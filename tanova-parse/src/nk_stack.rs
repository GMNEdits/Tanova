//! The Nuke `.nk` TCL stack machine.
//!
//! `.nk` files are TCL scripts: every node block pops its inputs off a stack
//! and pushes itself back on. `set N1 [stack 0]` captures a branch point and
//! `push $N1` re-uses it later, which is how Nuke expresses a DAG with a
//! linear-looking script.

use std::collections::HashMap;

use tanova_kernel::{Dag, Edge, KnobTable, KnobValue, NodeId, PortId, find_cartridge};

use crate::ParseError;
use crate::expr_tcl::{TclEvalEnv, eval_tcl_expr};

/// Classes that produce pixels without consuming an input.
const GENERATORS: &[&str] = &[
    "Root",
    "Constant",
    "Read",
    "BackdropNode",
    "StickyNote",
    "CheckerBoard2",
    "ColorBars",
    "ColorWheel",
];

/// Classes that never leave a node on the stack.
///
/// `Viewer` is intentionally **not** listed: it is a real cartridge now and
/// participates in the stack like any other node, so scripts can wire it up
/// and the loader can focus its `NodeId`.
const NON_PUSHING: &[&str] = &["Root", "BackdropNode", "StickyNote"];

/// One node recovered from a script.
#[derive(Debug, Clone)]
pub struct ParsedNode {
    /// Node id assigned while parsing.
    pub id: NodeId,
    /// Nuke class name (e.g. `Grade`).
    pub class: String,
    /// Value of the `name` knob, if the script had one.
    pub name: Option<String>,
    /// Knobs explicitly present in the script.
    pub knobs: KnobTable,
    /// Declared input count (ports `0..inputs`).
    pub inputs: usize,
    /// Upstream node for each input port; `None` means intentionally empty.
    pub input_nodes: Vec<Option<NodeId>>,
}

/// Everything the parser learned from one script.
#[derive(Debug, Clone, Default)]
pub struct ParsedScript {
    /// Nodes in the order they were parsed.
    pub nodes: Vec<ParsedNode>,
    /// Node name (`name` knob) to id.
    pub names: HashMap<String, NodeId>,
    /// Id to node name.
    pub node_names: HashMap<NodeId, String>,
    /// `NodeId` of the last `Viewer` block, when the script had one.
    ///
    /// The Viewer cartridge is a passthrough, so the app focuses this node
    /// rather than its upstream input.
    pub viewer_target: Option<NodeId>,
    /// Node left on top of the root stack.
    pub stack_top: Option<NodeId>,
}

impl ParsedScript {
    /// Resolves a `--node` argument, accepting a name or a numeric id.
    pub fn resolve(&self, name_or_id: &str) -> Option<NodeId> {
        let trimmed = name_or_id.trim();
        if let Some(id) = trimmed.strip_prefix('#') {
            return id.parse().ok().map(NodeId);
        }
        if let Ok(id) = trimmed.parse::<u64>() {
            return Some(NodeId(id));
        }
        self.names.get(trimmed).copied()
    }
}

/// Parses a script without touching a graph.
pub fn parse_nk_script(script: &str) -> Result<ParsedScript, ParseError> {
    Machine::new(None).run(script)
}

/// Parses a script and builds the graph in `dag`.
pub fn load_nk_into_dag(script: &str, dag: &mut Dag) -> Result<ParsedScript, ParseError> {
    Machine::new(Some(dag)).run(script)
}

struct Machine<'a> {
    dag: Option<&'a mut Dag>,
    script: ParsedScript,
    stacks: Vec<Vec<Option<NodeId>>>,
    vars: HashMap<String, Option<NodeId>>,
    next_id: u64,
    group_stack: Vec<NodeId>,
}

impl<'a> Machine<'a> {
    fn new(dag: Option<&'a mut Dag>) -> Self {
        Self {
            dag,
            script: ParsedScript::default(),
            stacks: vec![Vec::new()],
            vars: HashMap::new(),
            next_id: 1,
            group_stack: Vec::new(),
        }
    }

    fn run(mut self, script: &str) -> Result<ParsedScript, ParseError> {
        let lines: Vec<&str> = script.lines().collect();
        let mut index = 0;

        while index < lines.len() {
            let line_number = index + 1;
            let line = strip_comment(lines[index]).trim().to_string();
            index += 1;

            if line.is_empty() || line.starts_with("version ") {
                continue;
            }

            if line == "end_group" {
                self.end_group();
                continue;
            }

            if let Some(rest) = line.strip_prefix("set ") {
                self.handle_set(rest, line_number)?;
                continue;
            }

            if let Some(rest) = line.strip_prefix("push ") {
                self.handle_push(rest);
                continue;
            }

            if let Some(class) = line.strip_suffix('{').map(str::trim) {
                if class.is_empty() {
                    return Err(self.syntax(line_number, "missing class name before `{`"));
                }
                let body = self.read_block(&lines, &mut index, line_number, class)?;
                self.handle_block(class, &body)?;
                continue;
            }

            // Unknown top-level TCL commands are ignored so newer scripts keep
            // loading; node blocks and stack operations are what matter here.
        }

        self.script.stack_top = self
            .stacks
            .first()
            .and_then(|stack| stack.last().copied())
            .flatten();
        Ok(self.script)
    }

    /// Reads knob lines until the matching closing brace.
    fn read_block(
        &self,
        lines: &[&str],
        index: &mut usize,
        line_number: usize,
        class: &str,
    ) -> Result<Vec<(usize, String)>, ParseError> {
        let mut body = Vec::new();
        while *index < lines.len() {
            let body_line = *index + 1;
            let raw = strip_comment(lines[*index]);
            *index += 1;
            let trimmed = raw.trim();
            if trimmed == "}" {
                return Ok(body);
            }
            body.push((body_line, raw.to_string()));
        }
        Err(self.syntax(line_number, format!("unterminated `{class}` block")))
    }

    fn handle_set(&mut self, rest: &str, line_number: usize) -> Result<(), ParseError> {
        let Some(split) = rest.find(char::is_whitespace) else {
            return Err(self.syntax(line_number, "expected `set VAR [stack N]`"));
        };
        let variable = rest[..split].trim();
        let value = rest[split..].trim();
        let Some(index) = value
            .strip_prefix("[stack ")
            .and_then(|text| text.strip_suffix(']'))
        else {
            return Err(self.syntax(line_number, format!("unsupported `set` value `{value}`")));
        };
        let index: usize = index.trim().parse().map_err(|_| {
            self.syntax(
                line_number,
                format!("invalid stack index `{}`", index.trim()),
            )
        })?;
        let stack = self.stacks.last().expect("root stack always exists");
        let top = stack.iter().rev().nth(index).copied().flatten();
        self.vars.insert(variable.to_string(), top);
        Ok(())
    }

    fn handle_push(&mut self, rest: &str) {
        let token = rest.trim();
        let value = if token == "0" || token == "{}" {
            None
        } else if let Some(variable) = token.strip_prefix('$') {
            self.vars.get(variable).copied().flatten()
        } else {
            self.vars.get(token).copied().flatten()
        };
        self.stacks.last_mut().expect("root stack").push(value);
    }

    fn end_group(&mut self) {
        let Some(group) = self.group_stack.pop() else {
            return;
        };
        if self.stacks.len() > 1 {
            self.stacks.pop();
        }
        self.stacks
            .last_mut()
            .expect("root stack")
            .push(Some(group));
    }

    fn handle_block(&mut self, class: &str, body: &[(usize, String)]) -> Result<(), ParseError> {
        let defaults = find_cartridge(class).map(|descriptor| (descriptor.default_knobs)());
        let env = self.knob_env();

        let mut knobs = KnobTable::new();
        let mut name = None;
        let mut explicit_inputs = None;

        for (body_line, raw) in body {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                continue;
            }

            if let Some(rest) = trimmed.strip_prefix("inputs ") {
                explicit_inputs = Some(parse_input_count(rest).map_err(|message| {
                    self.syntax(*body_line, format!("invalid `inputs`: {message}"))
                })?);
                continue;
            }

            if let Some(rest) = trimmed.strip_prefix("addUserKnob ") {
                if let Some(user_knob) = parse_user_knob(rest) {
                    knobs.insert(user_knob, KnobValue::Float(0.0));
                }
                continue;
            }

            let Some((knob, raw_value)) = split_knob(trimmed) else {
                return Err(self.syntax(*body_line, format!("invalid knob line `{trimmed}`")));
            };
            let value = parse_knob_value(defaults.as_ref(), knob, raw_value, *body_line, &env)?;
            if knob == "name" {
                if let KnobValue::Text(text) = &value {
                    name = Some(text.clone());
                }
            }
            knobs.insert(knob, value);
        }

        let total_inputs = explicit_inputs.unwrap_or_else(|| default_input_count(class));
        self.create_node(class, total_inputs, knobs, name)
    }

    fn create_node(
        &mut self,
        class: &str,
        total_inputs: usize,
        knobs: KnobTable,
        name: Option<String>,
    ) -> Result<(), ParseError> {
        let id = NodeId(self.next_id);
        self.next_id += 1;

        let mut input_nodes = Vec::with_capacity(total_inputs);
        for _ in 0..total_inputs {
            let from = self
                .stacks
                .last_mut()
                .and_then(|stack| stack.pop())
                .flatten();
            input_nodes.push(from);
        }

        if let Some(dag) = self.dag.as_deref_mut() {
            dag.add_node(id, class)
                .map_err(|error| ParseError::Kernel(error.to_string()))?;
            for (knob, value) in knobs.iter() {
                dag.set_knob(id, knob, value.clone())
                    .map_err(|error| ParseError::Kernel(error.to_string()))?;
            }
            for (port, from) in input_nodes.iter().enumerate() {
                let Some(from) = from else {
                    continue;
                };
                dag.connect_edge(Edge::new(*from, PortId(0), id, PortId(port as u16)))
                    .map_err(|error| ParseError::Kernel(error.to_string()))?;
            }
        }

        self.script.nodes.push(ParsedNode {
            id,
            class: class.to_string(),
            name: name.clone(),
            knobs,
            inputs: total_inputs,
            input_nodes: input_nodes.clone(),
        });
        if let Some(name) = name {
            self.script.names.insert(name.clone(), id);
            self.script.node_names.insert(id, name);
        }

        if class == "Viewer" {
            // The Viewer cartridge is a passthrough node, so the app should
            // focus the node itself rather than its input.
            self.script.viewer_target = Some(id);
        }

        if class == "Group" {
            self.group_stack.push(id);
            self.stacks.push(Vec::new());
        } else if !NON_PUSHING.contains(&class) {
            self.stacks.last_mut().expect("root stack").push(Some(id));
        }

        Ok(())
    }

    /// Builds an expression environment from the nodes parsed so far.
    fn knob_env(&self) -> TclEvalEnv {
        let mut env = TclEvalEnv::with_frame(1.0);
        for node in &self.script.nodes {
            let Some(name) = &node.name else {
                continue;
            };
            for (knob, value) in node.knobs.iter() {
                if let Some(number) = knob_as_f64(value) {
                    env.set_knob(format!("{name}.{knob}"), number);
                }
            }
        }
        env
    }

    fn syntax(&self, line: usize, message: impl Into<String>) -> ParseError {
        ParseError::Syntax {
            line,
            message: message.into(),
        }
    }
}

fn default_input_count(class: &str) -> usize {
    if GENERATORS.contains(&class) { 0 } else { 1 }
}

fn parse_input_count(text: &str) -> Result<usize, String> {
    let mut total = 0usize;
    for part in text.trim().split('+') {
        let value: usize = part
            .trim()
            .parse()
            .map_err(|_| format!("`{}` is not a number", part.trim()))?;
        total += value;
    }
    Ok(total)
}

fn split_knob(line: &str) -> Option<(&str, &str)> {
    let split = line.find(char::is_whitespace)?;
    let knob = line[..split].trim();
    let value = line[split..].trim();
    if knob.is_empty() {
        None
    } else {
        Some((knob, value))
    }
}

fn parse_user_knob(rest: &str) -> Option<String> {
    let inner = rest.trim().trim_start_matches('{').trim_end_matches('}');
    let mut tokens = inner.split_whitespace();
    let _flags = tokens.next()?;
    let name = tokens.next()?;
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

fn parse_knob_value(
    defaults: Option<&KnobTable>,
    knob: &str,
    raw: &str,
    line: usize,
    env: &TclEvalEnv,
) -> Result<KnobValue, ParseError> {
    let default = defaults.and_then(|table| table.get(knob));
    let raw = raw.trim();

    if raw.is_empty() {
        return Ok(default
            .cloned()
            .unwrap_or_else(|| KnobValue::Text(String::new())));
    }

    if raw.starts_with('"') {
        let text = parse_quoted(raw, line)?;
        return match default {
            Some(KnobValue::Float(_)) => Ok(KnobValue::Float(eval_number(&text, env)?)),
            Some(KnobValue::Int(_)) => Ok(KnobValue::Int(eval_number(&text, env)? as i64)),
            Some(KnobValue::Bool(_)) => Ok(KnobValue::Bool(parse_bool(&text))),
            _ => Ok(KnobValue::Text(text)),
        };
    }

    if raw.starts_with('{') {
        let numbers = parse_number_list(raw, line)?;
        let alpha = match default {
            Some(KnobValue::Color(color)) => color[3],
            _ => 1.0,
        };
        return Ok(color_from_numbers(&numbers, alpha));
    }

    match default {
        Some(KnobValue::Color(color)) => {
            let numbers = parse_number_list(raw, line)?;
            if numbers.len() == 1 {
                let value = numbers[0] as f32;
                Ok(KnobValue::Color([value, value, value, value]))
            } else {
                Ok(color_from_numbers(&numbers, color[3]))
            }
        }
        Some(KnobValue::Bool(_)) => Ok(KnobValue::Bool(parse_bool(raw))),
        Some(KnobValue::Int(_)) => Ok(KnobValue::Int(eval_number(raw, env)? as i64)),
        Some(KnobValue::Float(_)) => Ok(KnobValue::Float(eval_number(raw, env)?)),
        Some(KnobValue::Text(_)) => Ok(KnobValue::Text(unquote(raw))),
        None => infer_value(raw, line),
    }
}

fn infer_value(raw: &str, line: usize) -> Result<KnobValue, ParseError> {
    if raw.starts_with('"') {
        return Ok(KnobValue::Text(parse_quoted(raw, line)?));
    }
    if raw.starts_with('{') {
        let numbers = parse_number_list(raw, line)?;
        return Ok(color_from_numbers(&numbers, 1.0));
    }
    match raw {
        "true" => return Ok(KnobValue::Bool(true)),
        "false" => return Ok(KnobValue::Bool(false)),
        _ => {}
    }
    if let Ok(value) = raw.parse::<i64>() {
        return Ok(KnobValue::Int(value));
    }
    if let Ok(value) = raw.parse::<f64>() {
        return Ok(KnobValue::Float(value));
    }
    Ok(KnobValue::Text(unquote(raw)))
}

fn color_from_numbers(numbers: &[f64], default_alpha: f32) -> KnobValue {
    match numbers {
        [] => KnobValue::Color([0.0, 0.0, 0.0, default_alpha]),
        [value] => {
            let value = *value as f32;
            KnobValue::Color([value, value, value, value])
        }
        // Two numbers are a 2D vector stored in a Color knob (`Floatx2`).
        [x, y] => KnobValue::Color([*x as f32, *y as f32, 0.0, default_alpha]),
        [red, green, blue] => {
            KnobValue::Color([*red as f32, *green as f32, *blue as f32, default_alpha])
        }
        [red, green, blue, alpha, ..] => {
            KnobValue::Color([*red as f32, *green as f32, *blue as f32, *alpha as f32])
        }
    }
}

fn parse_number_list(raw: &str, line: usize) -> Result<Vec<f64>, ParseError> {
    let inner = raw.trim().trim_start_matches('{').trim_end_matches('}');
    let mut numbers = Vec::new();
    for token in inner.split_whitespace() {
        let value: f64 = token.parse().map_err(|_| ParseError::Syntax {
            line,
            message: format!("`{token}` is not a number"),
        })?;
        numbers.push(value);
    }
    Ok(numbers)
}

fn eval_number(raw: &str, env: &TclEvalEnv) -> Result<f64, ParseError> {
    if let Ok(value) = raw.parse::<f64>() {
        return Ok(value);
    }
    eval_tcl_expr(raw, env)
}

fn parse_bool(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "true" | "1" | "yes" | "on"
    )
}

fn unquote(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"') {
        trimmed[1..trimmed.len() - 1].to_string()
    } else {
        trimmed.to_string()
    }
}

fn parse_quoted(raw: &str, line: usize) -> Result<String, ParseError> {
    let mut chars = raw.trim().chars();
    if chars.next() != Some('"') {
        return Err(ParseError::Syntax {
            line,
            message: "expected a quoted string".to_string(),
        });
    }

    let mut text = String::new();
    let mut escaped = false;
    for character in chars {
        if escaped {
            text.push(match character {
                'n' => '\n',
                't' => '\t',
                'r' => '\r',
                other => other,
            });
            escaped = false;
            continue;
        }
        match character {
            '\\' => escaped = true,
            '"' => return Ok(text),
            other => text.push(other),
        }
    }

    Err(ParseError::Syntax {
        line,
        message: "unterminated quoted string".to_string(),
    })
}

fn knob_as_f64(value: &KnobValue) -> Option<f64> {
    match value {
        KnobValue::Float(value) => Some(*value),
        KnobValue::Int(value) => Some(*value as f64),
        KnobValue::Bool(value) => Some(if *value { 1.0 } else { 0.0 }),
        KnobValue::Color(color) => Some(color[0] as f64),
        KnobValue::Text(_) => None,
    }
}

fn strip_comment(line: &str) -> &str {
    let mut in_quote = false;
    let mut escaped = false;
    for (index, character) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match character {
            '\\' if in_quote => escaped = true,
            '"' => in_quote = !in_quote,
            '#' if !in_quote => return &line[..index],
            _ => {}
        }
    }
    line
}

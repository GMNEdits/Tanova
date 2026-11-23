//! The Nuke `.nk` TCL stack machine.
//!
//! `.nk` files are TCL scripts: every node block pops its inputs off a stack
//! and pushes itself back on. `set N1 [stack 0]` captures a branch point and
//! `push $N1` re-uses it later, which is how Nuke expresses a DAG with a
//! linear-looking script.

use std::collections::HashMap;

use tanova_kernel::{Dag, Edge, KnobTable, KnobValue, NodeId, PortId, find_cartridge};

use crate::ParseError;
use crate::curve::{AnimationCurve, parse_nuke_curve};
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
    "Ramp",
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
    /// Canvas-space X position, when the script declared one.
    pub xpos: Option<f32>,
    /// Canvas-space Y position, when the script declared one.
    pub ypos: Option<f32>,
    /// `true` when the block carried Nuke's `disable true` line.
    ///
    /// Bypass state is graph state, not a knob, so it never enters
    /// [`ParsedNode::knobs`].
    pub disabled: bool,
    /// Knobs explicitly present in the script.
    pub knobs: KnobTable,
    /// Parsed `{curve ...}` animation curves keyed by knob name, one curve per
    /// knob channel (single-channel knobs store one curve).
    pub curves: HashMap<String, Vec<AnimationCurve>>,
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
    /// Tanova schema version from a `# tanova_schema N` header, when present.
    ///
    /// `None` for legacy `.tnv` files (v0.4–v0.6) and for external `.nk`
    /// scripts, both of which keep loading exactly as before.
    pub schema_version: Option<u32>,
    /// Original project path recorded in a recovery snapshot
    /// (`# tanova_recovery_source ...`), when present.
    ///
    /// A restored crash session is re-bound to this path, never to the hidden
    /// `autosave.tnv` file.
    pub recovery_source: Option<std::path::PathBuf>,
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

            // Tanova metadata lives in `#` comment lines, which
            // `strip_comment` would otherwise erase. Inspect the raw line
            // first so schema versioning and recovery provenance survive.
            let raw_line = lines[index].trim();
            if let Some(rest) = raw_line.strip_prefix("# tanova_schema ") {
                let version: u32 = rest.trim().parse().map_err(|_| {
                    self.syntax(
                        line_number,
                        format!("invalid Tanova schema version `{}`", rest.trim()),
                    )
                })?;
                if version == 0 || version > tanova_kernel::TANOVA_SCHEMA_VERSION {
                    return Err(self.syntax(
                        line_number,
                        format!(
                            "Unsupported Tanova schema version {version} (max supported: {})",
                            tanova_kernel::TANOVA_SCHEMA_VERSION
                        ),
                    ));
                }
                self.script.schema_version = Some(version);
                index += 1;
                continue;
            }
            if let Some(rest) = raw_line.strip_prefix("# tanova_recovery_source ") {
                let source = rest.trim();
                if !source.is_empty() {
                    self.script.recovery_source = Some(std::path::PathBuf::from(source));
                }
                index += 1;
                continue;
            }

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
        // Nuke writes plain `Backdrop`; normalize to our cartridge name so it
        // is treated as a 0-input, non-stack-pushing canvas container.
        let class = if class == "Backdrop" {
            "BackdropNode"
        } else {
            class
        };
        let defaults = find_cartridge(class).map(|descriptor| (descriptor.default_knobs)());
        let env = self.knob_env();

        let mut knobs = KnobTable::new();
        let mut curves = HashMap::new();
        let mut name = None;
        let mut explicit_inputs = None;
        let mut xpos: Option<f32> = None;
        let mut ypos: Option<f32> = None;
        let mut disabled = false;

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

            // xpos/ypos are layout data, never user knobs — intercept them
            // here so they never enter ParsedNode.knobs.
            if let Some(rest) = trimmed.strip_prefix("xpos ") {
                if let Ok(value) = rest.trim().parse::<f32>() {
                    xpos = Some(value);
                }
                continue;
            }
            if let Some(rest) = trimmed.strip_prefix("ypos ") {
                if let Ok(value) = rest.trim().parse::<f32>() {
                    ypos = Some(value);
                }
                continue;
            }

            // `disable` is Nuke's bypass flag — graph state, never a knob.
            // Accept the truthy spellings Nuke and hand-written scripts use.
            if let Some(rest) = trimmed.strip_prefix("disable ") {
                disabled = match rest.trim().to_ascii_lowercase().as_str() {
                    "true" | "1" => true,
                    "false" | "0" => false,
                    _ => disabled,
                };
                continue;
            }

            let Some((knob, raw_value)) = split_knob(trimmed) else {
                return Err(self.syntax(*body_line, format!("invalid knob line `{trimmed}`")));
            };
            let value = if let Some(animated) =
                parse_animated_knob(defaults.as_ref(), knob, raw_value, &mut curves)
            {
                animated
            } else {
                parse_knob_value(defaults.as_ref(), knob, raw_value, *body_line, &env)?
            };
            if knob == "name" {
                if let KnobValue::Text(text) = &value {
                    name = Some(text.clone());
                }
            }
            knobs.insert(knob, value);
        }

        let total_inputs = explicit_inputs.unwrap_or_else(|| default_input_count(class));
        self.create_node(
            class,
            total_inputs,
            knobs,
            curves,
            name,
            xpos,
            ypos,
            disabled,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn create_node(
        &mut self,
        class: &str,
        total_inputs: usize,
        knobs: KnobTable,
        curves: HashMap<String, Vec<AnimationCurve>>,
        name: Option<String>,
        xpos: Option<f32>,
        ypos: Option<f32>,
        disabled: bool,
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
            // Apply persisted canvas position before any knobs, so (0,0)
            // defaults match the DagNode initial state and non-zero values
            // round-trip through export_to_tcl.
            if let Some(x) = xpos {
                dag.set_position(id, x, ypos.unwrap_or(0.0))
                    .map_err(|error| ParseError::Kernel(error.to_string()))?;
            } else if let Some(y) = ypos {
                dag.set_position(id, 0.0, y)
                    .map_err(|error| ParseError::Kernel(error.to_string()))?;
            }
            for (knob, value) in knobs.iter() {
                dag.set_knob(id, knob, value.clone())
                    .map_err(|error| ParseError::Kernel(error.to_string()))?;
            }
            if disabled {
                dag.set_disabled(id, true)
                    .map_err(|error| ParseError::Kernel(error.to_string()))?;
            }
            // Animation curves live outside the knob table, so copy them onto
            // the node once its knobs exist (the frame-1.0 value was already
            // coerced into the knob type).
            if let Some(node) = dag.node_mut(id) {
                node.curves = curves
                    .iter()
                    .map(|(name, channel_curves)| (name.clone(), channel_curves.clone()))
                    .collect();
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
            xpos,
            ypos,
            disabled,
            knobs,
            curves,
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

/// Tries to read a knob's raw value as a Nuke `{curve ...}` animation — either
/// a single animated channel `{curve x1 0 x10 1}` or a nested multi-channel
/// tuple `{{curve ...} {curve ...} 1 1}`.
///
/// Returns the coerced static knob value at frame 1.0 and records every
/// parsed channel curve in `curves` so the node's animation survives the
/// parse. Returns `None` for plain static values, which fall through to the
/// existing knob parser untouched.
fn parse_animated_knob(
    defaults: Option<&KnobTable>,
    knob: &str,
    raw_value: &str,
    curves: &mut HashMap<String, Vec<AnimationCurve>>,
) -> Option<KnobValue> {
    let raw_value = raw_value.trim();

    // Case 1: a single `{curve ...}` channel.
    if let Some(curve) = parse_nuke_curve(raw_value) {
        let value = curve.evaluate(1.0);
        curves.insert(knob.to_string(), vec![curve]);
        return Some(coerce_curve_value(defaults, knob, value));
    }

    // Case 2: a nested `{ {curve ...} {curve ...} 1 1 }` multi-channel tuple:
    // one outer pair of knob-value braces around brace-depth-isolated channels.
    let inner = raw_value
        .strip_prefix('{')
        .and_then(|text| text.strip_suffix('}'))
        .map(str::trim)?;
    let elements = split_brace_depth_elements(inner);
    if elements.is_empty() {
        return None;
    }
    let mut channel_curves = Vec::with_capacity(elements.len());
    let mut any_animated = false;
    for element in elements {
        let element = element.trim();
        if element.is_empty() {
            continue;
        }
        if let Some(curve) = parse_nuke_curve(element) {
            any_animated = true;
            channel_curves.push(curve);
        } else if let Ok(number) = element.parse::<f64>() {
            channel_curves.push(AnimationCurve::from_constant(number));
        } else {
            // A non-curve, non-numeric channel: not an animated knob.
            return None;
        }
    }
    if !any_animated {
        return None;
    }
    let values: Vec<f64> = channel_curves
        .iter()
        .map(|curve| curve.evaluate(1.0))
        .collect();
    curves.insert(knob.to_string(), channel_curves);
    let default_alpha = defaults
        .and_then(|table| table.get(knob))
        .and_then(|value| match value {
            KnobValue::Color(color) => Some(color[3] as f64),
            _ => None,
        })
        .unwrap_or(1.0);
    Some(color_from_numbers(&values, default_alpha as f32))
}

/// Splits `text` into whitespace-separated elements, keeping braced groups
/// intact by tracking brace depth.
fn split_brace_depth_elements(text: &str) -> Vec<&str> {
    let mut elements = Vec::new();
    let mut depth: u32 = 0;
    let mut start: usize = 0;
    for (byte_index, character) in text.char_indices() {
        match character {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            _ if character.is_whitespace() && depth == 0 => {
                if byte_index > start {
                    elements.push(&text[start..byte_index]);
                }
                start = byte_index + 1;
            }
            _ => {}
        }
    }
    if start < text.len() {
        elements.push(&text[start..]);
    }
    elements
}

/// Coerces an evaluated curve value at frame 1.0 into the schema knob type
/// declared by `default_knobs()`, mirroring the static-number coercion rules:
/// Color defaults expand a single number across all channels, Int defaults
/// round, everything else stays a Float.
fn coerce_curve_value(defaults: Option<&KnobTable>, knob: &str, value: f64) -> KnobValue {
    let default = defaults.and_then(|table| table.get(knob));
    match default {
        Some(KnobValue::Color(_)) => {
            let channel = value as f32;
            KnobValue::Color([channel, channel, channel, channel])
        }
        Some(KnobValue::Int(_)) => KnobValue::Int(value.round() as i64),
        _ => KnobValue::Float(value),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xpos_ypos_round_trip_without_knob_collision() {
        // Build a graph: Constant at (140, 60), Grade at (140, 210).
        let mut dag = Dag::new();
        dag.add_node_at(NodeId(1), "Constant", 140.0, 60.0).unwrap();
        dag.add_node_at(NodeId(2), "Grade", 140.0, 210.0).unwrap();
        dag.connect_edge(Edge::new(NodeId(1), PortId(0), NodeId(2), PortId(0)))
            .unwrap();

        // Export to .nk TCL.
        let tcl = dag.export_to_tcl();

        // The exported script must contain xpos/ypos for non-zero positions.
        assert!(tcl.contains("xpos 140"), "missing xpos in export: {tcl}");
        assert!(tcl.contains("ypos 60"), "missing ypos in export: {tcl}");
        assert!(
            tcl.contains("ypos 210"),
            "missing ypos 210 in export: {tcl}"
        );

        // Load back into a fresh Dag.
        let mut dag2 = Dag::new();
        let parsed = load_nk_into_dag(&tcl, &mut dag2).expect("parse should succeed");

        // Both parsed nodes must carry xpos/ypos.
        let const_parsed = parsed
            .nodes
            .iter()
            .find(|n| n.id == NodeId(1))
            .expect("Constant #1 parsed");
        let grade_parsed = parsed
            .nodes
            .iter()
            .find(|n| n.id == NodeId(2))
            .expect("Grade #2 parsed");
        assert_eq!(const_parsed.xpos, Some(140.0));
        assert_eq!(const_parsed.ypos, Some(60.0));
        assert_eq!(grade_parsed.xpos, Some(140.0));
        assert_eq!(grade_parsed.ypos, Some(210.0));

        // Positions must round-trip into the Dag.
        let const_node = dag2.node(NodeId(1)).expect("Constant #1 in dag");
        let grade_node = dag2.node(NodeId(2)).expect("Grade #2 in dag");
        assert!((const_node.x - 140.0).abs() < 1e-4, "x: {:?}", const_node.x);
        assert!((const_node.y - 60.0).abs() < 1e-4, "y: {:?}", const_node.y);
        assert!((grade_node.x - 140.0).abs() < 1e-4, "x: {:?}", grade_node.x);
        assert!((grade_node.y - 210.0).abs() < 1e-4, "y: {:?}", grade_node.y);

        // Neither node may have "xpos" or "ypos" inside its knobs map.
        assert!(
            !const_node.knobs.contains_key("xpos"),
            "xpos must not be a knob"
        );
        assert!(
            !const_node.knobs.contains_key("ypos"),
            "ypos must not be a knob"
        );
        assert!(
            !grade_node.knobs.contains_key("xpos"),
            "xpos must not be a knob"
        );
        assert!(
            !grade_node.knobs.contains_key("ypos"),
            "ypos must not be a knob"
        );
    }

    #[test]
    fn zero_position_is_omitted_from_export() {
        // A node at (0, 0) must not emit xpos/ypos (backward compat).
        let mut dag = Dag::new();
        dag.add_node_at(NodeId(1), "Constant", 0.0, 0.0).unwrap();
        let tcl = dag.export_to_tcl();
        assert!(!tcl.contains("xpos"), "xpos must be omitted at zero: {tcl}");
        assert!(!tcl.contains("ypos"), "ypos must be omitted at zero: {tcl}");

        // Loading a script without xpos/ypos yields (0, 0).
        let mut dag2 = Dag::new();
        load_nk_into_dag(&tcl, &mut dag2).expect("parse should succeed");
        let node = dag2.node(NodeId(1)).expect("node #1");
        assert!((node.x - 0.0).abs() < 1e-4);
        assert!((node.y - 0.0).abs() < 1e-4);
    }

    #[test]
    fn disable_true_round_trips_without_knob_collision() {
        // Build a graph: Constant(1) -> Grade(2), then bypass Grade.
        let mut dag = Dag::new();
        dag.add_node_at(NodeId(1), "Constant", 0.0, 0.0).unwrap();
        dag.add_node_at(NodeId(2), "Grade", 0.0, 0.0).unwrap();
        dag.connect_edge(Edge::new(NodeId(1), PortId(0), NodeId(2), PortId(0)))
            .unwrap();
        dag.set_disabled(NodeId(2), true).unwrap();

        // Export to .nk TCL: `disable true` must appear, and `disable` must
        // never live inside `node.knobs`.
        let tcl = dag.export_to_tcl();
        assert!(
            tcl.contains("disable true"),
            "missing disable true in export: {tcl}"
        );
        let grade_node = dag.node(NodeId(2)).expect("Grade #2");
        assert!(
            !grade_node.knobs.contains_key("disable"),
            "disable must not be a knob"
        );

        // Load back into a fresh Dag: bypass state must round-trip.
        let mut dag2 = Dag::new();
        let parsed = load_nk_into_dag(&tcl, &mut dag2).expect("parse should succeed");
        let grade_parsed = parsed
            .nodes
            .iter()
            .find(|n| n.id == NodeId(2))
            .expect("Grade #2 parsed");
        assert!(
            grade_parsed.disabled,
            "parsed node must carry disabled=true"
        );
        assert!(
            !grade_parsed.knobs.contains_key("disable"),
            "disable must not be a parsed knob"
        );

        let reloaded = dag2.node(NodeId(2)).expect("Grade #2 in reloaded dag");
        assert!(reloaded.disabled, "reloaded node must be disabled");
    }

    /// A `BackdropNode` in the middle of a wired chain must not consume a
    /// stack slot nor push one, so downstream wiring survives the reload.
    #[test]
    fn backdrop_between_wired_nodes_never_corrupts_the_stack() {
        // Link the cartridges so `bdwidth`/`bdheight` parse with their real
        // `Float` default types instead of falling back to `Int`.
        tanova_nodes::ensure_linked();
        let script = "\
version 15.0 v5
Constant {
 inputs 0
 xpos 40
 ypos 40
}
set N1 [stack 0]
Backdrop {
 inputs 0
 xpos 4
 ypos -4
 label \"Group\"
 color teal
 bdwidth 240
 bdheight 272
}
set N2 [stack 0]
push $N1
Grade {
 inputs 1
 xpos 40
 ypos 164
}
set N3 [stack 0]
push $N3
Dot {
 inputs 1
 xpos 115
 ypos 254
}
set N4 [stack 0]
";

        let mut dag = Dag::new();
        let parsed = load_nk_into_dag(script, &mut dag).expect("parse should succeed");

        // `Backdrop` normalizes to the registered `BackdropNode` cartridge.
        let backdrop = parsed
            .nodes
            .iter()
            .find(|node| node.class == "BackdropNode")
            .expect("backdrop must be parsed");
        assert_eq!(backdrop.inputs, 0, "a backdrop has no image inputs");
        assert_eq!(backdrop.xpos, Some(4.0));
        assert_eq!(backdrop.ypos, Some(-4.0));
        assert_eq!(
            backdrop.knobs.get("label"),
            Some(&KnobValue::Text("Group".to_string()))
        );
        assert_eq!(
            backdrop.knobs.get("bdwidth"),
            Some(&KnobValue::Float(240.0))
        );
        assert_eq!(
            backdrop.knobs.get("bdheight"),
            Some(&KnobValue::Float(272.0))
        );

        // Both real wires must land exactly where the script says, proving the
        // backdrop neither popped an input nor pushed its own id.
        let id_of = |class: &str| {
            parsed
                .nodes
                .iter()
                .find(|node| node.class == class)
                .unwrap_or_else(|| panic!("{class} parsed"))
                .id
        };
        let constant_id = id_of("Constant");
        let grade_id = id_of("Grade");
        let dot_id = id_of("Dot");
        assert_eq!(
            dag.edges()
                .find(|edge| edge.to_node == grade_id && edge.to_port == PortId(0))
                .map(|edge| edge.from_node),
            Some(constant_id),
            "Constant -> Grade must survive a backdrop in between"
        );
        assert_eq!(
            dag.edges()
                .find(|edge| edge.to_node == dot_id && edge.to_port == PortId(0))
                .map(|edge| edge.from_node),
            Some(grade_id),
            "Grade -> Dot must survive a backdrop in between"
        );
    }

    #[test]
    fn nk_script_parses_single_and_multi_channel_animated_curves() {
        tanova_nodes::ensure_linked();
        let script = "\
version 15.0 v5
Constant {
 color {0.2 0.4 0.6 1}
}
Blur {
 size {curve x1 4 x11 24}
}
Grade {
 multiply {{curve l x1 0.5 x11 1.5} {curve l x1 1.0 x11 2.0} 1 1}
 mix {curve k x1 0.25 x10 1.0}
}
";
        let parsed = parse_nk_script(script).expect("parse should succeed");

        let blur = parsed
            .nodes
            .iter()
            .find(|node| node.class == "Blur")
            .expect("Blur parsed");
        assert_eq!(blur.curves["size"].len(), 1);
        assert_eq!(blur.curves["size"][0].evaluate(1.0), 4.0);
        assert_eq!(blur.curves["size"][0].evaluate(6.0), 14.0);
        assert_eq!(blur.knobs.get("size"), Some(&KnobValue::Float(4.0)));

        let grade = parsed
            .nodes
            .iter()
            .find(|node| node.class == "Grade")
            .expect("Grade parsed");
        let multiply_curves = grade
            .curves
            .get("multiply")
            .expect("multiply curves parsed");
        assert_eq!(multiply_curves.len(), 4);
        assert_eq!(multiply_curves[0].evaluate(6.0), 1.0);
        assert_eq!(multiply_curves[1].evaluate(6.0), 1.5);
        assert_eq!(
            grade.knobs.get("multiply"),
            Some(&KnobValue::Color([0.5, 1.0, 1.0, 1.0]))
        );
        assert_eq!(grade.knobs.get("mix"), Some(&KnobValue::Float(0.25)));
        assert_eq!(grade.curves.get("mix").expect("mix curve").len(), 1);

        let mut dag = Dag::new();
        load_nk_into_dag(script, &mut dag).expect("dag load should succeed");
    }

    #[test]
    fn ramp_is_a_generator_and_does_not_consume_the_upstream_pipe() {
        tanova_nodes::ensure_linked();
        let mut dag = Dag::new();
        let parsed = load_nk_into_dag(
            "Constant {\n name source\n}\nRamp {\n name ramp\n}\nGrade {\n name grade\n}\n",
            &mut dag,
        )
        .unwrap();
        let ramp = parsed
            .nodes
            .iter()
            .find(|node| node.class == "Ramp")
            .unwrap()
            .id;
        let grade = parsed
            .nodes
            .iter()
            .find(|node| node.class == "Grade")
            .unwrap()
            .id;
        assert!(!dag.edges().any(|edge| edge.to_node == ramp));
        assert!(dag.edges().any(|edge| edge.from_node == ramp
            && edge.to_node == grade
            && edge.to_port == PortId(0)));
    }

    #[test]
    fn animated_knob_evaluates_across_frames_in_dag_and_round_trips_tcl() {
        use tanova_kernel::{EvalContext, MemoryGovernor, RenderEpoch, Time};
        use tanova_render::Raster2D;

        tanova_nodes::ensure_linked();
        // Constant 0.2/0.4/0.6/1 graded by gain 2, with `mix` animated 0 -> 1.
        // At frame 1 mix is 0 (original color), at frame 11 mix is 1 (graded),
        // and frame 6 sits exactly halfway.
        let script = "\
version 15.0 v5
Constant {
 color {0.2 0.4 0.6 1}
 format_width 64
 format_height 64
}
Grade {
 gain {2 2 2 1}
 mix {curve l x1 0 x11 1}
}
";
        let mut dag = Dag::new();
        load_nk_into_dag(script, &mut dag).expect("dag load should succeed");

        // Constant is node 1, Grade is node 2 in load order.
        let grade = NodeId(2);
        assert_eq!(
            dag.node(grade).map(|node| node.curves.len()),
            Some(1),
            "the parsed mix curve must reach the DagNode"
        );

        let governor = MemoryGovernor::new(usize::MAX);
        let epoch = RenderEpoch::new();
        let target_epoch = epoch.bump();

        let sample = |dag: &mut Dag, frame: f64| -> [f32; 4] {
            let mut ctx = EvalContext::new();
            ctx.insert(Time(frame));
            let packet = dag
                .evaluate_pull(grade, &ctx, &governor, &epoch, target_epoch)
                .expect("Grade pulls");
            let raster = packet.get::<Raster2D>().expect("Grade emits a Raster2D");
            raster.sample_rgba(10, 10)
        };

        let assert_pixel = |actual: [f32; 4], expected: [f32; 4], frame: f64| {
            for (channel, (actual, expected)) in actual.iter().zip(expected.iter()).enumerate() {
                assert!(
                    (actual - expected).abs() < 1e-5,
                    "frame {frame} channel {channel}: expected {expected}, got {actual} (pixel {actual:?})"
                );
            }
        };

        let at_one = sample(&mut dag, 1.0);
        assert_pixel(at_one, [0.2, 0.4, 0.6, 1.0], 1.0);
        let at_six = sample(&mut dag, 6.0);
        assert_pixel(at_six, [0.3, 0.6, 0.9, 1.0], 6.0);
        let at_eleven = sample(&mut dag, 11.0);
        assert_pixel(at_eleven, [0.4, 0.8, 1.2, 1.0], 11.0);

        // The animated knob must survive an export/reload round trip.
        let exported = dag.export_to_tcl();
        assert!(
            exported.contains("mix {curve")
                && exported.contains("x1 0")
                && exported.contains("x11 1"),
            "mix must export as an animated curve: {exported}"
        );
        assert!(
            !exported.contains("mix 0.0") && !exported.contains("mix 0\n"),
            "an animated knob must not also export a static value: {exported}"
        );

        let mut reloaded = Dag::new();
        load_nk_into_dag(&exported, &mut reloaded).expect("exported script reloads");
        let mut ctx = EvalContext::new();
        ctx.insert(Time(6.0));
        let packet = reloaded
            .evaluate_pull(grade, &ctx, &governor, &epoch, target_epoch)
            .expect("reloaded Grade pulls");
        let raster = packet.get::<Raster2D>().expect("Grade emits a Raster2D");
        assert_pixel(raster.sample_rgba(10, 10), [0.3, 0.6, 0.9, 1.0], 6.0);
    }

    #[test]
    fn tanova_schema_header_accepts_v1_rejects_v0_and_v2() {
        let body = "\
set N1 [stack 0]
Constant {
 color {0.2 0.4 0.6 1}
}
push $N1
";

        // v1 (current) is accepted.
        let parsed = parse_nk_script(&format!("version 15.0 v5\n# tanova_schema 1\n{body}"))
            .expect("schema v1 must load");
        assert_eq!(parsed.schema_version, Some(1));

        // Missing header loads normally (legacy .tnv / external .nk).
        let parsed = parse_nk_script(&format!("version 15.0 v5\n{body}"))
            .expect("no schema header must load");
        assert_eq!(parsed.schema_version, None);

        // v0 is rejected.
        let err =
            parse_nk_script(&format!("version 15.0 v5\n# tanova_schema 0\n{body}")).unwrap_err();
        assert!(
            err.to_string()
                .contains("Unsupported Tanova schema version 0"),
            "unexpected error: {err}"
        );

        // v2 > max is rejected.
        let err =
            parse_nk_script(&format!("version 15.0 v5\n# tanova_schema 2\n{body}")).unwrap_err();
        assert!(
            err.to_string()
                .contains("Unsupported Tanova schema version 2 (max supported: 1)"),
            "unexpected error: {err}"
        );
    }
}

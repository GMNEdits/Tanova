//! A tiny TCL math-expression evaluator for `.nk` knob values.
//!
//! Nuke lets artists write `frame > 100 ? [value Grade1.mix] * 2 : 0.5` in a
//! numeric field. This evaluator covers the operators, ternaries, and bracketed
//! substitutions the parser needs, without pulling in a TCL interpreter.

use std::collections::HashMap;

use crate::ParseError;

/// Everything an expression may look up while it is evaluated.
#[derive(Debug, Clone, Default)]
pub struct TclEvalEnv {
    /// Current frame, exposed as `frame` and `[frame]`.
    pub frame: f64,
    /// Numeric knob values keyed by `"Node.knob"` or bare `"knob"`.
    pub knobs: HashMap<String, f64>,
    /// Environment variables used by `[getenv VAR]`.
    pub env: HashMap<String, String>,
}

impl TclEvalEnv {
    /// Creates an empty environment at frame 1.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates an environment pinned to one frame.
    pub fn with_frame(frame: f64) -> Self {
        Self {
            frame,
            ..Self::default()
        }
    }

    /// Registers a numeric knob.
    pub fn set_knob(&mut self, key: impl Into<String>, value: f64) {
        self.knobs.insert(key.into(), value);
    }

    /// Registers an environment variable.
    pub fn set_env(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.env.insert(key.into(), value.into());
    }

    /// Looks up a knob by exact key, then by the bare name after the last dot.
    pub fn knob(&self, key: &str) -> Option<f64> {
        if let Some(value) = self.knobs.get(key) {
            return Some(*value);
        }
        key.rsplit('.')
            .next()
            .and_then(|bare| self.knobs.get(bare))
            .copied()
    }
}

/// Evaluates a TCL/Nuke math expression.
pub fn eval_tcl_expr(expr: &str, ctx: &TclEvalEnv) -> Result<f64, ParseError> {
    let mut parser = ExprParser {
        chars: expr.chars().collect(),
        pos: 0,
        expr,
        ctx,
    };
    let value = parser.parse_ternary()?;
    parser.skip_ws();
    if parser.pos < parser.chars.len() {
        return Err(parser.error("unexpected trailing characters"));
    }
    Ok(value)
}

/// Expands bracketed TCL substitutions inside a string (paths, labels, ...).
pub fn expand_tcl_string(text: &str, ctx: &TclEvalEnv) -> Result<String, ParseError> {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(character) = chars.next() {
        if character != '[' {
            out.push(character);
            continue;
        }

        let mut inner = String::new();
        let mut depth = 1;
        for character in chars.by_ref() {
            match character {
                '[' => {
                    depth += 1;
                    inner.push(character);
                }
                ']' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                    inner.push(character);
                }
                _ => inner.push(character),
            }
        }
        if depth != 0 {
            return Err(ParseError::Expr {
                expr: text.to_string(),
                message: "unterminated `[`".to_string(),
            });
        }
        out.push_str(&eval_bracket_string(&inner, ctx)?);
    }
    Ok(out)
}

fn eval_bracket_string(inner: &str, ctx: &TclEvalEnv) -> Result<String, ParseError> {
    let tokens: Vec<&str> = inner.split_whitespace().collect();
    match tokens.as_slice() {
        ["getenv", rest @ ..] if !rest.is_empty() => {
            let key = rest.join(" ");
            ctx.env.get(&key).cloned().ok_or_else(|| ParseError::Expr {
                expr: inner.to_string(),
                message: format!("environment variable `{key}` is not set"),
            })
        }
        ["frame"] => Ok(format!("{}", ctx.frame)),
        ["value", rest @ ..] if !rest.is_empty() => {
            let key = rest.join(" ");
            ctx.knob(&key)
                .map(|value| format!("{value}"))
                .ok_or_else(|| ParseError::Expr {
                    expr: inner.to_string(),
                    message: format!("knob `{key}` has no numeric value"),
                })
        }
        _ => eval_tcl_expr(inner, ctx).map(|value| format!("{value}")),
    }
}

struct ExprParser<'a> {
    chars: Vec<char>,
    pos: usize,
    expr: &'a str,
    ctx: &'a TclEvalEnv,
}

impl<'a> ExprParser<'a> {
    fn error(&self, message: impl Into<String>) -> ParseError {
        ParseError::Expr {
            expr: self.expr.to_string(),
            message: message.into(),
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while self.peek().is_some_and(char::is_whitespace) {
            self.pos += 1;
        }
    }

    fn consume(&mut self, text: &str) -> bool {
        self.skip_ws();
        let length = text.chars().count();
        let end = self.pos + length;
        if end <= self.chars.len() && self.chars[self.pos..end].iter().copied().eq(text.chars()) {
            self.pos = end;
            true
        } else {
            false
        }
    }

    fn parse_ternary(&mut self) -> Result<f64, ParseError> {
        let condition = self.parse_comparison()?;
        if self.consume("?") {
            let then_value = self.parse_ternary()?;
            if !self.consume(":") {
                return Err(self.error("expected `:` in ternary"));
            }
            let else_value = self.parse_ternary()?;
            Ok(if condition != 0.0 {
                then_value
            } else {
                else_value
            })
        } else {
            Ok(condition)
        }
    }

    fn parse_comparison(&mut self) -> Result<f64, ParseError> {
        let mut left = self.parse_additive()?;
        loop {
            let operator = if self.consume("==") {
                Some("==")
            } else if self.consume("!=") {
                Some("!=")
            } else if self.consume("<=") {
                Some("<=")
            } else if self.consume(">=") {
                Some(">=")
            } else if self.consume("<") {
                Some("<")
            } else if self.consume(">") {
                Some(">")
            } else {
                None
            };
            let Some(operator) = operator else {
                break;
            };
            let right = self.parse_additive()?;
            let result = match operator {
                "==" => left == right,
                "!=" => left != right,
                "<=" => left <= right,
                ">=" => left >= right,
                "<" => left < right,
                _ => left > right,
            };
            left = if result { 1.0 } else { 0.0 };
        }
        Ok(left)
    }

    fn parse_additive(&mut self) -> Result<f64, ParseError> {
        let mut left = self.parse_multiplicative()?;
        loop {
            if self.consume("+") {
                left += self.parse_multiplicative()?;
            } else if self.consume("-") {
                left -= self.parse_multiplicative()?;
            } else {
                break;
            }
        }
        Ok(left)
    }

    fn parse_multiplicative(&mut self) -> Result<f64, ParseError> {
        let mut left = self.parse_unary()?;
        loop {
            if self.consume("*") {
                left *= self.parse_unary()?;
            } else if self.consume("/") {
                left /= self.parse_unary()?;
            } else if self.consume("%") {
                left %= self.parse_unary()?;
            } else {
                break;
            }
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<f64, ParseError> {
        if self.consume("-") {
            return Ok(-self.parse_unary()?);
        }
        if self.consume("!") {
            return Ok(if self.parse_unary()? == 0.0 { 1.0 } else { 0.0 });
        }
        if self.consume("+") {
            return self.parse_unary();
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<f64, ParseError> {
        self.skip_ws();
        let Some(character) = self.peek() else {
            return Err(self.error("unexpected end of expression"));
        };

        if character == '(' {
            self.pos += 1;
            let value = self.parse_ternary()?;
            if !self.consume(")") {
                return Err(self.error("expected `)`"));
            }
            return Ok(value);
        }

        if character == '[' {
            let inner = self.read_bracket()?;
            return self.eval_bracket(&inner);
        }

        if character.is_ascii_digit() || character == '.' {
            return self.read_number();
        }

        if character.is_ascii_alphabetic() || character == '_' {
            let identifier = self.read_identifier();
            return match identifier.as_str() {
                "frame" => Ok(self.ctx.frame),
                "true" => Ok(1.0),
                "false" => Ok(0.0),
                other => self
                    .ctx
                    .knob(other)
                    .ok_or_else(|| self.error(format!("unknown identifier `{other}`"))),
            };
        }

        Err(self.error("expected a number, `(`, `[`, or identifier"))
    }

    fn read_number(&mut self) -> Result<f64, ParseError> {
        let start = self.pos;
        while let Some(character) = self.peek() {
            if character.is_ascii_digit() || character == '.' {
                self.pos += 1;
            } else if matches!(character, 'e' | 'E') && self.pos > start {
                self.pos += 1;
                if self.peek().is_some_and(|next| next == '+' || next == '-') {
                    self.pos += 1;
                }
            } else {
                break;
            }
        }
        let text: String = self.chars[start..self.pos].iter().collect();
        text.parse::<f64>()
            .map_err(|_| self.error(format!("invalid number `{text}`")))
    }

    fn read_identifier(&mut self) -> String {
        let start = self.pos;
        while self
            .peek()
            .is_some_and(|character| character.is_ascii_alphanumeric() || character == '_')
        {
            self.pos += 1;
        }
        self.chars[start..self.pos].iter().collect()
    }

    fn read_bracket(&mut self) -> Result<String, ParseError> {
        self.pos += 1;
        let start = self.pos;
        let mut depth = 1;
        while let Some(character) = self.peek() {
            match character {
                '[' => depth += 1,
                ']' => {
                    depth -= 1;
                    if depth == 0 {
                        let text = self.chars[start..self.pos].iter().collect();
                        self.pos += 1;
                        return Ok(text);
                    }
                }
                _ => {}
            }
            self.pos += 1;
        }
        Err(self.error("unterminated `[`"))
    }

    fn eval_bracket(&mut self, inner: &str) -> Result<f64, ParseError> {
        let tokens: Vec<&str> = inner.split_whitespace().collect();
        match tokens.as_slice() {
            ["frame"] => Ok(self.ctx.frame),
            ["value", rest @ ..] if !rest.is_empty() => {
                let key = rest.join(" ");
                self.ctx
                    .knob(&key)
                    .ok_or_else(|| self.error(format!("knob `{key}` has no numeric value")))
            }
            ["getenv", rest @ ..] if !rest.is_empty() => {
                let key = rest.join(" ");
                let value = self.ctx.env.get(&key).ok_or_else(|| {
                    self.error(format!("environment variable `{key}` is not set"))
                })?;
                value
                    .parse::<f64>()
                    .map_err(|_| self.error(format!("environment variable `{key}` is not numeric")))
            }
            _ => eval_tcl_expr(inner, self.ctx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ternary_uses_frame_and_knob_substitutions() {
        let mut ctx = TclEvalEnv::with_frame(50.0);
        ctx.set_knob("Grade1.mix", 0.75);
        assert!(
            (eval_tcl_expr("frame > 100 ? [value Grade1.mix] * 2 : 0.5", &ctx).unwrap() - 0.5)
                .abs()
                < 1e-9
        );

        ctx.frame = 120.0;
        assert!(
            (eval_tcl_expr("frame > 100 ? [value Grade1.mix] * 2 : 0.5", &ctx).unwrap() - 1.5)
                .abs()
                < 1e-9
        );
    }

    #[test]
    fn precedence_and_unary_operators_follow_math_rules() {
        let ctx = TclEvalEnv::new();
        assert_eq!(eval_tcl_expr("1 + 2 * 3", &ctx).unwrap(), 7.0);
        assert_eq!(eval_tcl_expr("(1 + 2) * 3", &ctx).unwrap(), 9.0);
        assert_eq!(eval_tcl_expr("-2 * -3", &ctx).unwrap(), 6.0);
        assert_eq!(eval_tcl_expr("!(1 == 2)", &ctx).unwrap(), 1.0);
        assert_eq!(eval_tcl_expr("7 % 4", &ctx).unwrap(), 3.0);
    }

    #[test]
    fn string_expansion_replaces_getenv_and_frame() {
        let mut ctx = TclEvalEnv::with_frame(12.0);
        ctx.set_env("SHOW", "hero");
        assert_eq!(
            expand_tcl_string("/renders/[getenv SHOW]/frame_[frame].exr", &ctx).unwrap(),
            "/renders/hero/frame_12.exr"
        );
    }
}

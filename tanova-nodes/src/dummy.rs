//! The first cartridge: a text pass-through used to prove the plugin pipeline.
//!
//! Everything this node knows about lives in this one file. It binds to the
//! kernel only through `NodeTrait` and the `inventory` registry.

use std::sync::Arc;

use tanova_kernel::{
    EvalContext, KnobTable, KnobValue, NodeCartridgeDescriptor, NodeError, NodeTrait, Packet,
};

/// The node's output: a formatted text payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DummyPayload(pub String);

/// Message used when the `message` knob is missing or not text.
pub const DEFAULT_MESSAGE: &str = "hello from dummy";

/// Default knob values for a fresh [`DummyNode`].
pub fn default_knobs() -> KnobTable {
    let mut knobs = KnobTable::with_capacity(1);
    knobs.insert("message", KnobValue::Text(DEFAULT_MESSAGE.to_string()));
    knobs
}

/// A pass-through cartridge that echoes a message and any upstream payload.
#[derive(Debug, Clone)]
pub struct DummyNode {
    knobs: KnobTable,
}

impl Default for DummyNode {
    fn default() -> Self {
        Self {
            knobs: default_knobs(),
        }
    }
}

impl DummyNode {
    /// Creates a node with default knobs.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a node over an existing knob table.
    pub fn from_knobs(knobs: KnobTable) -> Self {
        Self { knobs }
    }

    /// Creates a node with a custom message.
    pub fn with_message(message: impl Into<String>) -> Self {
        let mut knobs = default_knobs();
        knobs.insert("message", KnobValue::Text(message.into()));
        Self { knobs }
    }

    /// Borrows the node's knobs.
    pub fn knobs(&self) -> &KnobTable {
        &self.knobs
    }

    /// The current message, falling back to [`DEFAULT_MESSAGE`].
    pub fn message(&self) -> &str {
        match self.knobs.get("message") {
            Some(KnobValue::Text(message)) => message,
            _ => DEFAULT_MESSAGE,
        }
    }
}

impl NodeTrait for DummyNode {
    fn evaluate(&self, ctx: &EvalContext, inputs: &[Packet]) -> Result<Packet, NodeError> {
        // The DAG injects live knobs into the context; standalone use falls
        // back to the node's own table.
        let knobs = ctx.knobs().unwrap_or(&self.knobs);
        let message = match knobs.get("message") {
            Some(KnobValue::Text(message)) => message.as_str(),
            _ => DEFAULT_MESSAGE,
        };

        let text = match inputs
            .iter()
            .find_map(|packet| packet.get::<DummyPayload>())
        {
            Some(DummyPayload(upstream)) => format!("{message} <- {upstream}"),
            None => message.to_string(),
        };

        let mut output = Packet::new();
        output.insert(DummyPayload(text));
        Ok(output)
    }

    fn serialize_to_tcl(&self) -> String {
        format!("Dummy {{\n message \"{}\"\n}}", escape_tcl(self.message()))
    }
}

/// Escapes text for a double-quoted TCL string.
fn escape_tcl(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\t' => escaped.push_str("\\t"),
            _ => escaped.push(character),
        }
    }
    escaped
}

fn factory() -> Arc<dyn NodeTrait> {
    Arc::new(DummyNode::new())
}

inventory::submit! {
    NodeCartridgeDescriptor {
        kind: "Dummy",
        default_knobs,
        factory,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_serialization_is_nuke_style_tcl() {
        let node = DummyNode::new();
        assert_eq!(
            node.serialize_to_tcl(),
            "Dummy {\n message \"hello from dummy\"\n}"
        );
    }

    #[test]
    fn serialization_escapes_quotes_and_newlines() {
        let node = DummyNode::with_message("say \"hi\"\nnow");
        assert_eq!(
            node.serialize_to_tcl(),
            "Dummy {\n message \"say \\\"hi\\\"\\nnow\"\n}"
        );
    }

    #[test]
    fn evaluate_emits_default_payload_without_inputs() {
        let node = DummyNode::new();
        let packet = node.evaluate(&EvalContext::new(), &[]).unwrap();
        assert_eq!(
            packet.get::<DummyPayload>(),
            Some(&DummyPayload(DEFAULT_MESSAGE.to_string()))
        );
    }

    #[test]
    fn evaluate_appends_the_first_upstream_payload() {
        let node = DummyNode::new();
        let mut upstream = Packet::new();
        upstream.insert(DummyPayload("upstream text".to_string()));

        let packet = node.evaluate(&EvalContext::new(), &[upstream]).unwrap();
        assert_eq!(
            packet.get::<DummyPayload>(),
            Some(&DummyPayload(
                "hello from dummy <- upstream text".to_string()
            ))
        );
    }

    #[test]
    fn missing_or_non_text_knob_falls_back_to_default() {
        let node = DummyNode::from_knobs(KnobTable::new());
        assert_eq!(node.message(), DEFAULT_MESSAGE);
    }
}

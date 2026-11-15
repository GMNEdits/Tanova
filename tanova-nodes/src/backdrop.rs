//! `BackdropNode`: a 0-input canvas container with a tinted fill and an
//! artist-editable label, used to visually group and annotate sections of a
//! B-pipe.

use std::sync::Arc;

use tanova_kernel::{
    EvalContext, KnobTable, KnobValue, NodeCartridgeDescriptor, NodeError, NodeTrait, Packet,
};

use crate::util::{float_knob, text_knob};

/// Default knob values for a fresh [`BackdropNode`].
pub fn default_knobs() -> KnobTable {
    let mut knobs = KnobTable::with_capacity(5);
    knobs.insert("label", KnobValue::Text("Backdrop".to_string()));
    knobs.insert("color", KnobValue::Text("slate".to_string()));
    knobs.insert("bdwidth", KnobValue::Float(320.0));
    knobs.insert("bdheight", KnobValue::Float(200.0));
    knobs
}

/// A 0-input canvas background container. It carries no pixel data and never
/// participates in the image pipeline; its only purpose is visual grouping.
#[derive(Debug, Clone)]
pub struct BackdropNode {
    knobs: KnobTable,
}

impl Default for BackdropNode {
    fn default() -> Self {
        Self {
            knobs: default_knobs(),
        }
    }
}

impl BackdropNode {
    /// Creates a node with default knobs.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a node over an existing knob table.
    pub fn from_knobs(knobs: KnobTable) -> Self {
        Self { knobs }
    }

    /// Borrows the node's knobs.
    pub fn knobs(&self) -> &KnobTable {
        &self.knobs
    }

    /// The backdrop's display label (default `"Backdrop"`).
    pub fn label(&self) -> String {
        text_knob(&self.knobs, "label", "Backdrop")
    }

    /// The backdrop's tint preset (default `"slate"`).
    pub fn color(&self) -> String {
        text_knob(&self.knobs, "color", "slate")
    }
}

impl NodeTrait for BackdropNode {
    fn evaluate(&self, _ctx: &EvalContext, _inputs: &[Packet]) -> Result<Packet, NodeError> {
        // A backdrop renders nothing into the pipeline; return an empty packet.
        Ok(Packet::new())
    }

    fn serialize_to_tcl(&self) -> String {
        format!(
            "BackdropNode {{\n label \"{}\"\n color \"{}\"\n bdwidth {}\n bdheight {}\n}}",
            self.label(),
            self.color(),
            float_knob(&self.knobs, "bdwidth", 320.0),
            float_knob(&self.knobs, "bdheight", 200.0),
        )
    }
}

fn factory() -> Arc<dyn NodeTrait> {
    Arc::new(BackdropNode::new())
}

inventory::submit! {
    NodeCartridgeDescriptor {
        kind: "BackdropNode",
        default_knobs,
        factory,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backdrop_default_knobs() {
        let node = BackdropNode::new();
        assert_eq!(node.label(), "Backdrop");
        assert_eq!(node.color(), "slate");
    }

    #[test]
    fn backdrop_evaluate_returns_empty_packet() {
        let node = BackdropNode::new();
        let output = node.evaluate(&EvalContext::new(), &[]).unwrap();
        assert!(output.is_empty());
    }
}

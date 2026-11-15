//! `Dot`: a zero-size routing elbow that passes its single input straight
//! through, useful for tidy B-pipe junctions.

use std::sync::Arc;

use tanova_kernel::{
    EvalContext, KnobTable, KnobValue, NodeCartridgeDescriptor, NodeError, NodeTrait, Packet,
};

/// Default knob values for a fresh [`DotNode`].
pub fn default_knobs() -> KnobTable {
    let mut knobs = KnobTable::with_capacity(1);
    knobs.insert("label", KnobValue::Text(String::new()));
    knobs
}

/// A 1-in/1-out routing elbow: its `evaluate` is a pure zero-copy passthrough
/// of `PortId(0)`.
#[derive(Debug, Clone)]
pub struct DotNode {
    knobs: KnobTable,
}

impl Default for DotNode {
    fn default() -> Self {
        Self {
            knobs: default_knobs(),
        }
    }
}

impl DotNode {
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

    /// The dot's optional label, defaulting to empty.
    pub fn label(&self) -> &str {
        match self.knobs.get("label") {
            Some(KnobValue::Text(value)) => value,
            _ => "",
        }
    }
}

impl NodeTrait for DotNode {
    fn evaluate(&self, _ctx: &EvalContext, inputs: &[Packet]) -> Result<Packet, NodeError> {
        // Pure passthrough: hand the upstream packet straight back with no
        // transformation, so a Dot never alters the image pipeline.
        Ok(inputs.first().cloned().unwrap_or_default())
    }

    fn serialize_to_tcl(&self) -> String {
        format!("Dot {{\n label \"{}\"\n}}", self.label())
    }
}

fn factory() -> Arc<dyn NodeTrait> {
    Arc::new(DotNode::new())
}

inventory::submit! {
    NodeCartridgeDescriptor {
        kind: "Dot",
        default_knobs,
        factory,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dot_passthrough_returns_upstream_packet() {
        let node = DotNode::new();
        let mut upstream = Packet::new();
        upstream.insert(tanova_render::Raster2D::for_format(
            tanova_render::Format::new("test", 8, 8, 1.0),
        ));
        let output = node.evaluate(&EvalContext::new(), &[upstream]).unwrap();
        assert!(output.get::<tanova_render::Raster2D>().is_some());
    }

    #[test]
    fn dot_with_no_inputs_returns_default_packet() {
        let node = DotNode::new();
        let output = node.evaluate(&EvalContext::new(), &[]).unwrap();
        assert!(output.is_empty());
    }

    #[test]
    fn dot_label_defaults_to_empty() {
        let node = DotNode::new();
        assert_eq!(node.label(), "");
    }
}

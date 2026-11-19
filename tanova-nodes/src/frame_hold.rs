//! `FrameHold`: freezes its input on one frame.
//!
//! Every timeline frame requests `first_frame` upstream, so downstream nodes
//! see a single held frame no matter where the playhead is parked.

use std::sync::Arc;

use tanova_kernel::{
    EvalContext, KnobTable, KnobValue, NodeCartridgeDescriptor, NodeError, NodeTrait, Packet,
    PortId, Time,
};

use crate::util::int_knob;

/// Default knob values for a fresh [`FrameHoldNode`].
pub fn default_knobs() -> KnobTable {
    let mut knobs = KnobTable::with_capacity(1);
    knobs.insert("first_frame", KnobValue::Int(1));
    knobs
}

/// A 1-in/1-out temporal shim that always samples one frame.
#[derive(Debug, Clone)]
pub struct FrameHoldNode {
    knobs: KnobTable,
}

impl Default for FrameHoldNode {
    fn default() -> Self {
        Self::from_knobs(default_knobs())
    }
}

impl FrameHoldNode {
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
}

impl NodeTrait for FrameHoldNode {
    fn evaluate(&self, _ctx: &EvalContext, inputs: &[Packet]) -> Result<Packet, NodeError> {
        Ok(inputs.first().cloned().unwrap_or_default())
    }

    fn upstream_inputs(
        &self,
        ctx: &EvalContext,
        connected_ports: &[PortId],
    ) -> Vec<(PortId, EvalContext)> {
        if !connected_ports.contains(&PortId(0)) {
            return Vec::new();
        }
        let held_frame = int_knob(ctx.knobs().unwrap_or(&self.knobs), "first_frame", 1) as f64;
        let mut held_ctx = ctx.clone();
        held_ctx.insert(Time(held_frame));
        vec![(PortId(0), held_ctx)]
    }

    fn serialize_to_tcl(&self) -> String {
        let held_frame = int_knob(&self.knobs, "first_frame", 1);
        format!("FrameHold {{\n first_frame {held_frame}\n}}")
    }
}

fn factory() -> Arc<dyn NodeTrait> {
    Arc::new(FrameHoldNode::new())
}

inventory::submit! {
    NodeCartridgeDescriptor {
        kind: "FrameHold",
        default_knobs,
        factory,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_hold_requests_the_held_frame() {
        let mut knobs = default_knobs();
        knobs.insert("first_frame", KnobValue::Int(2));
        let node = FrameHoldNode::from_knobs(knobs);

        let mut ctx = EvalContext::new();
        ctx.insert(Time(5.0));
        let requested = node.upstream_inputs(&ctx, &[PortId(0)]);
        assert_eq!(requested.len(), 1);
        assert_eq!(requested[0].0, PortId(0));
        assert_eq!(requested[0].1.time(), Some(2.0));
    }

    #[test]
    fn frame_hold_without_an_input_requests_nothing() {
        let node = FrameHoldNode::new();
        assert!(node.upstream_inputs(&EvalContext::new(), &[]).is_empty());
    }
}

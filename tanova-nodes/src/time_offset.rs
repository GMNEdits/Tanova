//! `TimeOffset`: evaluates its input at a shifted frame time.
//!
//! Nuke semantics: a positive `time_offset` delays the clip, so at timeline
//! frame `t` the upstream graph is sampled at `t - offset`. The shift travels
//! through [`EvalContext`], leaving `evaluate` a pure passthrough.

use std::sync::Arc;

use tanova_kernel::{
    EvalContext, KnobTable, KnobValue, NodeCartridgeDescriptor, NodeError, NodeTrait, Packet,
    PortId, Time,
};

use crate::util::int_knob;

/// Default knob values for a fresh [`TimeOffsetNode`].
pub fn default_knobs() -> KnobTable {
    let mut knobs = KnobTable::with_capacity(1);
    knobs.insert("time_offset", KnobValue::Int(0));
    knobs
}

/// A 1-in/1-out temporal shim that shifts the frame requested upstream.
#[derive(Debug, Clone)]
pub struct TimeOffsetNode {
    knobs: KnobTable,
}

impl Default for TimeOffsetNode {
    fn default() -> Self {
        Self::from_knobs(default_knobs())
    }
}

impl TimeOffsetNode {
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

impl NodeTrait for TimeOffsetNode {
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
        let offset = int_knob(ctx.knobs().unwrap_or(&self.knobs), "time_offset", 0) as f64;
        let current_time = ctx.time().unwrap_or(1.0);
        let mut shifted_ctx = ctx.clone();
        shifted_ctx.insert(Time(current_time - offset));
        vec![(PortId(0), shifted_ctx)]
    }

    fn serialize_to_tcl(&self) -> String {
        let offset = int_knob(&self.knobs, "time_offset", 0);
        format!("TimeOffset {{\n time_offset {offset}\n}}")
    }
}

fn factory() -> Arc<dyn NodeTrait> {
    Arc::new(TimeOffsetNode::new())
}

inventory::submit! {
    NodeCartridgeDescriptor {
        kind: "TimeOffset",
        default_knobs,
        factory,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_offset_shifts_the_upstream_frame() {
        let mut knobs = default_knobs();
        knobs.insert("time_offset", KnobValue::Int(3));
        let node = TimeOffsetNode::from_knobs(knobs);

        let mut ctx = EvalContext::new();
        ctx.insert(Time(5.0));
        let requested = node.upstream_inputs(&ctx, &[PortId(0)]);
        assert_eq!(requested.len(), 1);
        assert_eq!(requested[0].0, PortId(0));
        assert_eq!(requested[0].1.time(), Some(2.0));
    }

    #[test]
    fn time_offset_without_an_input_requests_nothing() {
        let node = TimeOffsetNode::new();
        assert!(node.upstream_inputs(&EvalContext::new(), &[]).is_empty());
    }
}

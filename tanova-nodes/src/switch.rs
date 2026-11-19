//! `Switch`: selects one of two upstream branches by knob.
//!
//! The unselected branch is never pulled: `upstream_inputs` requests only the
//! selected port, so a heavy or broken branch behind input `0` costs nothing
//! while input `1` is live (and vice versa).

use std::sync::Arc;

use tanova_kernel::{
    EvalContext, KnobTable, KnobValue, NodeCartridgeDescriptor, NodeError, NodeTrait, Packet,
    PortId,
};

use crate::util::int_knob;

/// Default knob values for a fresh [`SwitchNode`].
pub fn default_knobs() -> KnobTable {
    let mut knobs = KnobTable::with_capacity(1);
    knobs.insert("which", KnobValue::Int(0));
    knobs
}

/// A 2-in/1-out branch selector: `which == 0` reads `PortId(0)`,
/// `which == 1` reads `PortId(1)`.
#[derive(Debug, Clone)]
pub struct SwitchNode {
    knobs: KnobTable,
}

impl Default for SwitchNode {
    fn default() -> Self {
        Self::from_knobs(default_knobs())
    }
}

impl SwitchNode {
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

/// Reads `which` clamped to the two valid input indices.
fn selected_index(knobs: &KnobTable) -> usize {
    int_knob(knobs, "which", 0).clamp(0, 1) as usize
}

impl NodeTrait for SwitchNode {
    fn evaluate(&self, ctx: &EvalContext, inputs: &[Packet]) -> Result<Packet, NodeError> {
        let which = selected_index(ctx.knobs().unwrap_or(&self.knobs));
        Ok(inputs.get(which).cloned().unwrap_or_default())
    }

    fn upstream_inputs(
        &self,
        ctx: &EvalContext,
        connected_ports: &[PortId],
    ) -> Vec<(PortId, EvalContext)> {
        // The unselected branch is never pulled or evaluated.
        let selected_port = PortId(selected_index(ctx.knobs().unwrap_or(&self.knobs)) as u16);
        if connected_ports.contains(&selected_port) {
            vec![(selected_port, ctx.clone())]
        } else {
            Vec::new()
        }
    }

    fn serialize_to_tcl(&self) -> String {
        let which = int_knob(&self.knobs, "which", 0).clamp(0, 1);
        format!("Switch {{\n which {which}\n}}")
    }
}

fn factory() -> Arc<dyn NodeTrait> {
    Arc::new(SwitchNode::new())
}

inventory::submit! {
    NodeCartridgeDescriptor {
        kind: "Switch",
        default_knobs,
        factory,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switch_passes_the_selected_input_through() {
        let mut zero = Packet::new();
        zero.insert(0u32);
        let mut one = Packet::new();
        one.insert(1u32);

        let mut knobs = default_knobs();
        knobs.insert("which", KnobValue::Int(1));
        let node = SwitchNode::from_knobs(knobs);
        let output = node
            .evaluate(&EvalContext::new(), &[zero.clone(), one.clone()])
            .unwrap();
        assert_eq!(output.get::<u32>(), Some(&1));

        assert_eq!(
            node.evaluate(&EvalContext::new(), &[zero.clone()])
                .unwrap()
                .get::<u32>(),
            None,
            "a missing selected branch yields an empty packet"
        );
    }

    #[test]
    fn switch_requests_only_the_selected_port() {
        let node = SwitchNode::new();
        assert_eq!(
            node.upstream_inputs(&EvalContext::new(), &[PortId(0), PortId(1)])
                .iter()
                .map(|(port, _)| *port)
                .collect::<Vec<_>>(),
            vec![PortId(0)]
        );

        let mut knobs = default_knobs();
        knobs.insert("which", KnobValue::Int(1));
        let node = SwitchNode::from_knobs(knobs);
        assert_eq!(
            node.upstream_inputs(&EvalContext::new(), &[PortId(0), PortId(1)])
                .iter()
                .map(|(port, _)| *port)
                .collect::<Vec<_>>(),
            vec![PortId(1)]
        );
        assert!(
            node.upstream_inputs(&EvalContext::new(), &[PortId(0)])
                .is_empty(),
            "an unconnected selection requests nothing"
        );
    }

    #[test]
    fn switch_serializes_the_selected_branch() {
        let mut knobs = default_knobs();
        knobs.insert("which", KnobValue::Int(1));
        assert_eq!(
            SwitchNode::from_knobs(knobs).serialize_to_tcl(),
            "Switch {\n which 1\n}"
        );
    }
}

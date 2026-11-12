//! Reversible graph snapshots.
//!
//! `RemoveNode` is destructive: it drops the node, its edited knobs, and every
//! incident edge. To make `Ctrl+Z` exact, the command carries a full snapshot
//! captured at removal time. [`EdgeSnapshot`] is a plain value (it cannot borrow
//! from the DAG) and [`NodeSnapshot`] groups the node state with its wires.
//!
//! The kernel enriches UI-built commands from the live DAG before applying
//! them, so a stale UI mirror can never make undo lossy.

use crate::bus::{NodeId, PortId};
use crate::knobs::KnobTable;

/// One connection captured at removal time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EdgeSnapshot {
    /// Upstream node producing the data.
    pub from_node: NodeId,
    /// Output port on the upstream node.
    pub from_port: PortId,
    /// Downstream node consuming the data.
    pub to_node: NodeId,
    /// Input port on the downstream node.
    pub to_port: PortId,
}

impl EdgeSnapshot {
    /// Builds an edge snapshot.
    pub fn new(from_node: NodeId, from_port: PortId, to_node: NodeId, to_port: PortId) -> Self {
        Self {
            from_node,
            from_port,
            to_node,
            to_port,
        }
    }
}

/// Everything needed to recreate a node exactly as it was.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeSnapshot {
    /// Node identity.
    pub id: NodeId,
    /// Registered cartridge name.
    pub kind: String,
    /// Canvas-space X position.
    pub x: f32,
    /// Canvas-space Y position.
    pub y: f32,
    /// Every knob value, including user edits.
    pub knobs: KnobTable,
    /// Every incoming and outgoing edge.
    pub edges: Vec<EdgeSnapshot>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edge_snapshots_compare_by_value() {
        let edge = EdgeSnapshot::new(NodeId(1), PortId(0), NodeId(2), PortId(3));
        assert_eq!(
            edge,
            EdgeSnapshot::new(NodeId(1), PortId(0), NodeId(2), PortId(3))
        );
        assert_ne!(
            edge,
            EdgeSnapshot::new(NodeId(1), PortId(0), NodeId(2), PortId(0))
        );
    }
}

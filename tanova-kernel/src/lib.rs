//! Tanova microkernel.
//!
//! Owns the blackboard types (`Packet`, `EvalContext`), the `NodeTrait`
//! cartridge contract, the lazy-pull DAG with hash caching, and the
//! command/texture message bus. Deliberately blind to VFX math.

pub mod bus;
pub mod dag;
pub mod governor;
pub mod knobs;
pub mod registry;
pub mod types;

pub use bus::{
    FrameNotification, KernelEvent, KernelHandle, KernelMessage, NodeId, PortId, UndoStack,
    UndoableCommand,
};
pub use dag::{Dag, DagNode, Edge, OpHash, RamCache};
pub use governor::{MemoryGovernor, MemoryPermit, RenderEpoch, evaluate_with_airbag};
pub use knobs::{KnobTable, KnobValue};
pub use registry::{NodeCartridgeDescriptor, find_cartridge, registered_cartridge_kinds};
pub use types::{EvalContext, NodeError, NodeTrait, Packet, Time, TypeMap, View};

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    struct PanickingNode;

    impl NodeTrait for PanickingNode {
        fn evaluate(&self, _ctx: &EvalContext, _inputs: &[Packet]) -> Result<Packet, NodeError> {
            panic!("airbag test panic");
        }

        fn serialize_to_tcl(&self) -> String {
            "PanickingNode {}".to_string()
        }
    }

    fn next_event(kernel: &KernelHandle) -> KernelEvent {
        kernel
            .events()
            .recv_timeout(Duration::from_secs(5))
            .expect("kernel thread should emit an event")
    }

    #[test]
    fn airbag_catches_node_panics() {
        let node = PanickingNode;
        let error = evaluate_with_airbag(&node, &EvalContext::new(), &[])
            .expect_err("panicking node must not return a packet");

        match error {
            NodeError::Panic(message) => assert!(message.contains("airbag test panic")),
            other => panic!("expected NodeError::Panic, got {other:?}"),
        }
    }

    #[test]
    fn governor_caps_memory_and_permit_releases_it() {
        let governor = MemoryGovernor::new(100);

        let permit = governor.request_permit(60).expect("60 bytes fit");
        assert_eq!(permit.bytes(), 60);
        assert_eq!(governor.allocated_bytes(), 60);

        let rejected = governor.request_permit(50);
        assert!(
            matches!(
                rejected,
                Err(NodeError::MemoryExceeded {
                    requested: 50,
                    available: 40
                })
            ),
            "expected a memory rejection, got {rejected:?}"
        );

        drop(permit);
        assert_eq!(governor.allocated_bytes(), 0);
        assert_eq!(governor.available_bytes(), 100);

        let permit = governor.request_permit(50).expect("bytes were freed");
        drop(permit);
        assert_eq!(governor.allocated_bytes(), 0);
    }

    #[test]
    fn render_epoch_bumps_and_preempts_stale_renders() {
        let epoch = RenderEpoch::new();
        assert_eq!(epoch.current(), 0);

        assert_eq!(epoch.bump(), 1);
        assert!(epoch.is_current(1));
        assert!(!epoch.is_current(0));

        let shared = epoch.clone();
        assert_eq!(shared.bump(), 2);
        assert_eq!(epoch.current(), 2);
        assert!(!shared.is_current(1));
        assert!(shared.is_current(2));
    }

    #[test]
    fn background_kernel_applies_undoes_and_redoes_commands() {
        let kernel = KernelHandle::spawn().expect("kernel thread should spawn");

        let command = UndoableCommand::AddNode {
            id: NodeId(7),
            kind: "Dummy".to_string(),
        };

        kernel
            .send(KernelMessage::Execute(command.clone()))
            .expect("execute should reach the kernel");
        assert_eq!(next_event(&kernel), KernelEvent::Applied(command.clone()));

        kernel
            .send(KernelMessage::Undo)
            .expect("undo should reach the kernel");
        assert_eq!(next_event(&kernel), KernelEvent::Undone(command.clone()));

        kernel
            .send(KernelMessage::Redo)
            .expect("redo should reach the kernel");
        assert_eq!(next_event(&kernel), KernelEvent::Redone(command.clone()));

        kernel
            .send(KernelMessage::BumpEpoch)
            .expect("epoch bump should reach the kernel");
        assert_eq!(next_event(&kernel), KernelEvent::EpochChanged(1));

        kernel.shutdown();
    }

    #[test]
    fn undo_stack_inverts_commands() {
        let mut stack = UndoStack::new();
        assert!(!stack.can_undo());
        assert!(!stack.can_redo());

        let command = UndoableCommand::ConnectEdge {
            from_node: NodeId(1),
            from_port: PortId(0),
            to_node: NodeId(2),
            to_port: PortId(0),
        };

        stack.push(command.clone());
        assert!(stack.can_undo());

        assert_eq!(stack.undo(), Some(command.clone()));
        assert!(!stack.can_undo());
        assert!(stack.can_redo());

        assert_eq!(
            command.inverse(),
            UndoableCommand::DisconnectEdge {
                from_node: NodeId(1),
                from_port: PortId(0),
                to_node: NodeId(2),
                to_port: PortId(0),
            }
        );

        assert_eq!(stack.redo(), Some(command));
        assert!(stack.can_undo());
        assert!(!stack.can_redo());
    }
}

//! The kernel message bus, undo history, and background worker.
//!
//! The UI never mutates the graph directly. It sends [`KernelMessage`]s over
//! one channel and receives [`KernelEvent`]s over another, so the microkernel
//! can live on its own thread while the UI stays stateless.

use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, SendError, Sender, unbounded};

use crate::dag::Dag;
use crate::governor::{MemoryGovernor, RenderEpoch};
use crate::knobs::KnobValue;
use crate::types::{EvalContext, NodeError, Packet, Time};

/// Identifies a node instance in the graph.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(pub u64);

/// Identifies one input or output port on a node.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PortId(pub u16);

/// A state change the UI asks the kernel to perform.
///
/// Every variant carries enough state to build its own inverse, which is what
/// makes undo and redo mechanical instead of per-command special cases.
#[derive(Debug, Clone, PartialEq)]
pub enum UndoableCommand {
    /// Add a node of `kind` under `id`.
    AddNode {
        /// Identity the new node will use.
        id: NodeId,
        /// Registered cartridge name (e.g. `Blur`).
        kind: String,
    },
    /// Remove an existing node.
    RemoveNode {
        /// Node to remove.
        id: NodeId,
        /// Cartridge name, kept so undo can recreate the node.
        kind: String,
    },
    /// Connect an upstream output to a downstream input.
    ConnectEdge {
        /// Upstream node.
        from_node: NodeId,
        /// Upstream output port.
        from_port: PortId,
        /// Downstream node.
        to_node: NodeId,
        /// Downstream input port.
        to_port: PortId,
    },
    /// Remove an existing connection.
    DisconnectEdge {
        /// Upstream node.
        from_node: NodeId,
        /// Upstream output port.
        from_port: PortId,
        /// Downstream node.
        to_node: NodeId,
        /// Downstream input port.
        to_port: PortId,
    },
    /// Set a knob, remembering the previous value for undo.
    SetKnob {
        /// Owning node.
        node: NodeId,
        /// Knob name.
        knob: String,
        /// New value.
        value: KnobValue,
        /// Value before this command.
        previous: KnobValue,
    },
}

impl UndoableCommand {
    /// Returns the command that exactly reverses `self`.
    pub fn inverse(&self) -> Self {
        match self {
            Self::AddNode { id, kind } => Self::RemoveNode {
                id: *id,
                kind: kind.clone(),
            },
            Self::RemoveNode { id, kind } => Self::AddNode {
                id: *id,
                kind: kind.clone(),
            },
            Self::ConnectEdge {
                from_node,
                from_port,
                to_node,
                to_port,
            } => Self::DisconnectEdge {
                from_node: *from_node,
                from_port: *from_port,
                to_node: *to_node,
                to_port: *to_port,
            },
            Self::DisconnectEdge {
                from_node,
                from_port,
                to_node,
                to_port,
            } => Self::ConnectEdge {
                from_node: *from_node,
                from_port: *from_port,
                to_node: *to_node,
                to_port: *to_port,
            },
            Self::SetKnob {
                node,
                knob,
                value,
                previous,
            } => Self::SetKnob {
                node: *node,
                knob: knob.clone(),
                value: previous.clone(),
                previous: value.clone(),
            },
        }
    }
}

/// Work sent from the UI thread to the kernel thread.
#[derive(Debug, Clone)]
pub enum KernelMessage {
    /// Apply a reversible command.
    Execute(UndoableCommand),
    /// Undo the most recent command.
    Undo,
    /// Redo the most recently undone command.
    Redo,
    /// Invalidate in-flight renders.
    BumpEpoch,
    /// Replace the base render context (render state, GPU engine, ...).
    SetBaseContext(EvalContext),
    /// Choose which node the viewer displays; `None` clears the viewer.
    SetViewerNode(Option<NodeId>),
    /// Evaluate the active viewer target even if nothing else changed.
    RequestRender,
    /// A background worker produced new data; re-render without a graph edit.
    TriggerRender,
    /// Change the frame time used by time-dependent cartridges.
    SetTime(f64),
    /// Ask the kernel thread to exit.
    Shutdown,
}

/// Notifications sent from the kernel thread back to the UI.
#[derive(Debug, Clone, PartialEq)]
pub enum KernelEvent {
    /// A command was applied and recorded in the undo stack.
    Applied(UndoableCommand),
    /// A command was undone (the inverse is now in effect).
    Undone(UndoableCommand),
    /// A previously undone command was re-applied.
    Redone(UndoableCommand),
    /// A command was rejected and the graph is unchanged.
    CommandFailed {
        /// Command that could not be applied.
        command: UndoableCommand,
        /// Why it failed.
        message: String,
    },
    /// The render epoch advanced to the given value.
    EpochChanged(u64),
    /// A cartridge panicked; the kernel survived.
    NodeCrashed {
        /// Node that failed.
        node: NodeId,
        /// Captured panic message.
        message: String,
    },
}

/// Metadata for a rendered frame delivered off-screen to the UI.
///
/// The packet carries the cartridge output (for the current renderer, a
/// `Raster2D` behind an `Arc`); the kernel itself stays blind to its type.
#[derive(Debug, Clone)]
pub struct FrameNotification {
    /// Node the viewer asked to render.
    pub node: NodeId,
    /// Render epoch that produced the frame.
    pub epoch: u64,
    /// The evaluated packet, ready for the UI to inspect.
    pub packet: Arc<Packet>,
}

/// Linear undo/redo history of applied commands.
#[derive(Debug, Default)]
pub struct UndoStack {
    undo: Vec<UndoableCommand>,
    redo: Vec<UndoableCommand>,
}

impl UndoStack {
    /// Creates an empty history.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a newly applied command and drops any redo branch.
    ///
    /// Consecutive `SetKnob` commands for the same `(node, knob)` are coalesced
    /// so a continuous slider drag undoes in one step. Coalescing is skipped
    /// when a redo branch exists, because the previous undo entry may no longer
    /// describe the current value.
    pub fn push(&mut self, command: UndoableCommand) {
        let can_coalesce = self.redo.is_empty();
        self.redo.clear();

        if can_coalesce {
            if let UndoableCommand::SetKnob {
                node, knob, value, ..
            } = &command
            {
                if let Some(UndoableCommand::SetKnob {
                    node: last_node,
                    knob: last_knob,
                    value: last_value,
                    ..
                }) = self.undo.last_mut()
                {
                    if last_node == node && last_knob == knob {
                        *last_value = value.clone();
                        return;
                    }
                }
            }
        }

        self.undo.push(command);
    }

    /// Pops the last applied command, moving it onto the redo stack.
    ///
    /// The caller applies `command.inverse()` to the world.
    pub fn undo(&mut self) -> Option<UndoableCommand> {
        let command = self.undo.pop()?;
        self.redo.push(command.clone());
        Some(command)
    }

    /// Pops the last undone command, moving it back onto the undo stack.
    ///
    /// The caller re-applies the returned command to the world.
    pub fn redo(&mut self) -> Option<UndoableCommand> {
        let command = self.redo.pop()?;
        self.undo.push(command.clone());
        Some(command)
    }

    /// Returns `true` if there is something to undo.
    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    /// Returns `true` if there is something to redo.
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }
}

/// Owner-side handle to a running kernel thread.
///
/// Dropping the handle (or calling [`KernelHandle::shutdown`]) stops the
/// worker.
pub struct KernelHandle {
    messages: Sender<KernelMessage>,
    events: Receiver<KernelEvent>,
    frames: Receiver<FrameNotification>,
    thread: Option<JoinHandle<()>>,
}

impl KernelHandle {
    /// Spawns the `tanova-kernel` worker thread.
    pub fn spawn() -> std::io::Result<Self> {
        Self::spawn_with_governor(MemoryGovernor::new(usize::MAX))
    }

    /// Spawns the worker with a shared memory cap and fresh render epoch.
    pub fn spawn_with_governor(governor: MemoryGovernor) -> std::io::Result<Self> {
        let (message_tx, message_rx) = unbounded();
        let (event_tx, event_rx) = unbounded();
        let (frame_tx, frame_rx) = unbounded();

        let epoch = RenderEpoch::new();

        let thread = thread::Builder::new()
            .name("tanova-kernel".into())
            .spawn(move || kernel_loop(message_rx, event_tx, frame_tx, governor, epoch))?;

        Ok(Self {
            messages: message_tx,
            events: event_rx,
            frames: frame_rx,
            thread: Some(thread),
        })
    }

    /// Sends a message to the kernel thread.
    pub fn send(&self, message: KernelMessage) -> Result<(), SendError<KernelMessage>> {
        self.messages.send(message)
    }

    /// Clones the message sender so background workers can wake the kernel.
    pub fn sender(&self) -> Sender<KernelMessage> {
        self.messages.clone()
    }

    /// Receives kernel events on the UI thread.
    pub fn events(&self) -> &Receiver<KernelEvent> {
        &self.events
    }

    /// Receives off-screen frame notifications on the UI thread.
    pub fn frames(&self) -> &Receiver<FrameNotification> {
        &self.frames
    }

    /// Asks the worker to stop and waits for it to exit.
    pub fn shutdown(mut self) {
        let _ = self.messages.send(KernelMessage::Shutdown);
        self.join();
    }

    fn join(&mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for KernelHandle {
    fn drop(&mut self) {
        let _ = self.messages.send(KernelMessage::Shutdown);
        self.join();
    }
}

/// Body of the `tanova-kernel` thread.
fn kernel_loop(
    messages: Receiver<KernelMessage>,
    events: Sender<KernelEvent>,
    frames: Sender<FrameNotification>,
    governor: MemoryGovernor,
    epoch: RenderEpoch,
) {
    let mut undo = UndoStack::new();
    let mut dag = Dag::new();
    let mut base_ctx = EvalContext::new();
    let mut viewer_node: Option<NodeId> = None;
    let mut render_dirty = false;

    while let Ok(first) = messages.recv() {
        // Drain-then-render: apply every queued mutation before paying for a
        // render, so a burst of seed commands produces exactly one frame.
        let mut batch = vec![first];
        while let Ok(message) = messages.try_recv() {
            batch.push(message);
        }

        let mut shutdown = false;
        for message in batch {
            match message {
                KernelMessage::Execute(command) => match dag.apply_command(&command) {
                    Ok(()) => {
                        undo.push(command.clone());
                        render_dirty = true;
                        let _ = events.send(KernelEvent::Applied(command));
                    }
                    Err(error) => {
                        let _ = events.send(KernelEvent::CommandFailed {
                            command,
                            message: error.to_string(),
                        });
                    }
                },
                KernelMessage::Undo => {
                    if let Some(command) = undo.undo() {
                        match dag.undo_command(&command) {
                            Ok(()) => {
                                render_dirty = true;
                                let _ = events.send(KernelEvent::Undone(command));
                            }
                            Err(error) => {
                                let _ = events.send(KernelEvent::CommandFailed {
                                    command,
                                    message: error.to_string(),
                                });
                            }
                        }
                    }
                }
                KernelMessage::Redo => {
                    if let Some(command) = undo.redo() {
                        match dag.apply_command(&command) {
                            Ok(()) => {
                                render_dirty = true;
                                let _ = events.send(KernelEvent::Redone(command));
                            }
                            Err(error) => {
                                let _ = events.send(KernelEvent::CommandFailed {
                                    command,
                                    message: error.to_string(),
                                });
                            }
                        }
                    }
                }
                KernelMessage::BumpEpoch => {
                    let _ = events.send(KernelEvent::EpochChanged(epoch.bump()));
                }
                KernelMessage::SetBaseContext(context) => {
                    base_ctx = context;
                    render_dirty = true;
                }
                KernelMessage::SetViewerNode(node) => {
                    viewer_node = node;
                    render_dirty = true;
                }
                KernelMessage::RequestRender => {
                    render_dirty = true;
                }
                KernelMessage::TriggerRender => {
                    // External data (for example a disk frame) changed outside
                    // the graph. The op hash cannot see that, so drop cached
                    // packets before re-evaluating.
                    dag.cache_mut().clear();
                    render_dirty = true;
                }
                KernelMessage::SetTime(frame) => {
                    base_ctx.insert(Time(frame));
                    let _ = events.send(KernelEvent::EpochChanged(epoch.bump()));
                    render_dirty = true;
                }
                KernelMessage::Shutdown => shutdown = true,
            }
        }

        if shutdown {
            break;
        }
        if !render_dirty {
            continue;
        }
        render_dirty = false;

        let Some(target) = viewer_node else {
            continue;
        };
        if dag.node(target).is_none() {
            continue;
        }

        let current_epoch = epoch.bump();
        let context = base_ctx.clone();
        let last_render = Instant::now();
        match dag.evaluate_pull(target, &context, &governor, &epoch, current_epoch) {
            Ok(packet) => {
                let _ = frames.send(FrameNotification {
                    node: target,
                    epoch: current_epoch,
                    packet,
                });
            }
            Err(NodeError::Panic(message)) => {
                let _ = events.send(KernelEvent::NodeCrashed {
                    node: target,
                    message,
                });
            }
            Err(_) => {
                // Missing inputs, memory pressure, and preemption are normal
                // during interactive editing; the next change retries.
            }
        }

        // GPU queue contention: on integrated GPUs the render holds the device
        // queue while downloading pixels, starving the UI thread. Yielding for
        // one 60 FPS frame releases the queue so egui can redraw between
        // renders.
        let render_time = last_render.elapsed();
        debug_assert!(
            render_time < Duration::from_secs(30),
            "single frame render took {render_time:?}"
        );
        thread::sleep(Duration::from_millis(16));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use crate::knobs::KnobTable;
    use crate::registry::NodeCartridgeDescriptor;

    struct ConstantProbe;

    impl crate::types::NodeTrait for ConstantProbe {
        fn evaluate(&self, _ctx: &EvalContext, _inputs: &[Packet]) -> Result<Packet, NodeError> {
            let mut packet = Packet::new();
            packet.insert(7u32);
            Ok(packet)
        }

        fn serialize_to_tcl(&self) -> String {
            "Constant {}".to_string()
        }
    }

    fn constant_knobs() -> KnobTable {
        KnobTable::new()
    }

    fn constant_factory() -> Arc<dyn crate::types::NodeTrait> {
        Arc::new(ConstantProbe)
    }

    inventory::submit! {
        NodeCartridgeDescriptor {
            kind: "Constant",
            default_knobs: constant_knobs,
            factory: constant_factory,
        }
    }

    struct TimeProbe;

    impl crate::types::NodeTrait for TimeProbe {
        fn evaluate(&self, ctx: &EvalContext, _inputs: &[Packet]) -> Result<Packet, NodeError> {
            let mut packet = Packet::new();
            packet.insert(Time(ctx.time().unwrap_or(-1.0)));
            Ok(packet)
        }

        fn serialize_to_tcl(&self) -> String {
            "TimeProbe {}".to_string()
        }
    }

    fn time_probe_knobs() -> KnobTable {
        KnobTable::new()
    }

    fn time_probe_factory() -> Arc<dyn crate::types::NodeTrait> {
        Arc::new(TimeProbe)
    }

    static EXTERNAL_EVALS: AtomicUsize = AtomicUsize::new(0);

    struct ExternalProbe;

    impl crate::types::NodeTrait for ExternalProbe {
        fn evaluate(&self, _ctx: &EvalContext, _inputs: &[Packet]) -> Result<Packet, NodeError> {
            let value = EXTERNAL_EVALS.fetch_add(1, Ordering::SeqCst);
            let mut packet = Packet::new();
            packet.insert(value as u32);
            Ok(packet)
        }

        fn serialize_to_tcl(&self) -> String {
            "ExternalProbe {}".to_string()
        }
    }

    fn external_probe_factory() -> Arc<dyn crate::types::NodeTrait> {
        Arc::new(ExternalProbe)
    }

    inventory::submit! {
        NodeCartridgeDescriptor {
            kind: "ExternalProbe",
            default_knobs: time_probe_knobs,
            factory: external_probe_factory,
        }
    }

    fn next_event(kernel: &KernelHandle) -> KernelEvent {
        kernel
            .events()
            .recv_timeout(Duration::from_secs(5))
            .expect("kernel thread should emit an event")
    }

    inventory::submit! {
        NodeCartridgeDescriptor {
            kind: "TimeProbe",
            default_knobs: time_probe_knobs,
            factory: time_probe_factory,
        }
    }

    #[test]
    fn set_time_updates_the_render_context() {
        let kernel = KernelHandle::spawn().expect("kernel thread should spawn");
        let node = NodeId(9);

        kernel
            .send(KernelMessage::Execute(UndoableCommand::AddNode {
                id: node,
                kind: "TimeProbe".to_string(),
            }))
            .expect("add TimeProbe");
        assert!(matches!(
            next_event(&kernel),
            KernelEvent::Applied(UndoableCommand::AddNode { .. })
        ));

        kernel.send(KernelMessage::SetTime(42.0)).expect("set time");
        assert!(matches!(next_event(&kernel), KernelEvent::EpochChanged(_)));

        kernel
            .send(KernelMessage::SetViewerNode(Some(node)))
            .expect("bind viewer");

        let frame = kernel
            .frames()
            .recv_timeout(Duration::from_secs(5))
            .expect("the viewer should render with the new time");
        let time = frame.packet.get::<Time>().expect("probe emits Time");
        assert_eq!(time.0, 42.0);

        kernel.shutdown();
    }

    #[test]
    fn trigger_render_invalidates_external_data_cache() {
        let kernel = KernelHandle::spawn().expect("kernel thread should spawn");
        let node = NodeId(11);
        EXTERNAL_EVALS.store(0, Ordering::SeqCst);

        kernel
            .send(KernelMessage::Execute(UndoableCommand::AddNode {
                id: node,
                kind: "ExternalProbe".to_string(),
            }))
            .expect("add ExternalProbe");
        assert!(matches!(
            next_event(&kernel),
            KernelEvent::Applied(UndoableCommand::AddNode { .. })
        ));

        kernel
            .send(KernelMessage::SetViewerNode(Some(node)))
            .expect("bind viewer");

        let first = kernel
            .frames()
            .recv_timeout(Duration::from_secs(5))
            .expect("first frame");
        assert_eq!(first.packet.get::<u32>(), Some(&0));

        // A plain render request is served from the op-hash cache.
        kernel
            .send(KernelMessage::RequestRender)
            .expect("request render");
        let cached = kernel
            .frames()
            .recv_timeout(Duration::from_secs(5))
            .expect("cached frame");
        assert_eq!(cached.packet.get::<u32>(), Some(&0));

        // External data changed, so TriggerRender must drop the cache.
        kernel
            .send(KernelMessage::TriggerRender)
            .expect("trigger render");
        let refreshed = kernel
            .frames()
            .recv_timeout(Duration::from_secs(5))
            .expect("refreshed frame");
        assert_eq!(refreshed.packet.get::<u32>(), Some(&1));

        kernel.shutdown();
    }

    fn set_knob(name: &str, value: f32, previous: f32) -> UndoableCommand {
        UndoableCommand::SetKnob {
            node: NodeId(1),
            knob: name.to_string(),
            value: KnobValue::Float(value as f64),
            previous: KnobValue::Float(previous as f64),
        }
    }

    #[test]
    fn consecutive_set_knobs_coalesce_into_one_undo_step() {
        let mut stack = UndoStack::new();
        stack.push(set_knob("gain", 1.0, 0.0));
        stack.push(set_knob("gain", 2.0, 1.0));
        stack.push(set_knob("gain", 3.0, 2.0));

        let undone = stack.undo().expect("one coalesced step");
        match undone {
            UndoableCommand::SetKnob {
                value, previous, ..
            } => {
                assert_eq!(value, KnobValue::Float(3.0));
                assert_eq!(previous, KnobValue::Float(0.0));
            }
            other => panic!("expected SetKnob, got {other:?}"),
        }
        assert!(!stack.can_undo(), "the drag must undo in one step");
    }

    #[test]
    fn coalescing_stops_at_different_knobs_and_redo_branches() {
        let mut stack = UndoStack::new();
        stack.push(set_knob("gain", 1.0, 0.0));
        stack.push(set_knob("gamma", 2.0, 1.0));
        assert!(stack.undo().is_some());
        assert!(stack.undo().is_some());
        assert!(!stack.can_undo());

        // A redo branch means the previous entry no longer describes the world.
        stack.push(set_knob("gain", 1.0, 0.0));
        let _ = stack.undo();
        assert!(stack.can_redo());
        stack.push(set_knob("gain", 4.0, 1.0));
        assert!(!stack.can_redo(), "a new edit clears the redo branch");
        assert!(stack.undo().is_some());
        assert!(
            !stack.can_undo(),
            "the pre-undo command was already consumed"
        );
    }

    #[test]
    fn background_viewer_renders_after_set_viewer_node() {
        let kernel = KernelHandle::spawn().expect("kernel thread should spawn");

        kernel
            .send(KernelMessage::SetBaseContext(EvalContext::new()))
            .expect("base context");
        kernel
            .send(KernelMessage::Execute(UndoableCommand::AddNode {
                id: NodeId(1),
                kind: "Constant".to_string(),
            }))
            .expect("add node");
        kernel
            .send(KernelMessage::SetViewerNode(Some(NodeId(1))))
            .expect("set viewer");

        let frame = kernel
            .frames()
            .recv_timeout(Duration::from_secs(5))
            .expect("viewer should render automatically");

        assert_eq!(frame.node, NodeId(1));
        assert_eq!(frame.packet.get::<u32>(), Some(&7));

        kernel.shutdown();
    }
}

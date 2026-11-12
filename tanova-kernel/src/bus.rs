//! The kernel message bus, undo history, and background worker.
//!
//! The UI never mutates the graph directly. It sends [`KernelMessage`]s over
//! one channel and receives [`KernelEvent`]s over another, so the microkernel
//! can live on its own thread while the UI stays stateless.

use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_channel::{Receiver, SendError, Sender, unbounded};

use crate::command::EdgeSnapshot;
use crate::dag::Dag;
use crate::governor::{MemoryGovernor, RenderEpoch};
use crate::knobs::{KnobTable, KnobValue};
use crate::registry::find_cartridge;
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
    /// Add a node of `kind` under `id` at canvas coordinates `(x, y)`.
    AddNode {
        /// Identity the new node will use.
        id: NodeId,
        /// Registered cartridge name (e.g. `Blur`).
        kind: String,
        /// Canvas-space X position of the node.
        x: f32,
        /// Canvas-space Y position of the node.
        y: f32,
    },
    /// Remove an existing node.
    ///
    /// The command is a full snapshot so undo can recreate the node, its edited
    /// knobs, and every edge that touched it. The kernel replaces these fields
    /// with the live values before applying, so a stale UI mirror cannot make
    /// the undo lossy.
    RemoveNode {
        /// Node to remove.
        id: NodeId,
        /// Cartridge name, kept so undo can recreate the node.
        kind: String,
        /// Canvas-space X position, kept so undo restores the layout.
        x: f32,
        /// Canvas-space Y position, kept so undo restores the layout.
        y: f32,
        /// Full knob table captured at removal time.
        ///
        /// Boxed so the command enum (and every message on the bus) stays
        /// small; the knob table is the only large payload here.
        knobs: Box<KnobTable>,
        /// Every incoming and outgoing edge captured at removal time.
        edges: Vec<EdgeSnapshot>,
    },
    /// Recreate a removed node exactly (the inverse of [`Self::RemoveNode`]).
    RestoreNode {
        /// Node identity to recreate.
        id: NodeId,
        /// Cartridge name.
        kind: String,
        /// Canvas-space X position.
        x: f32,
        /// Canvas-space Y position.
        y: f32,
        /// Knob values to restore verbatim (boxed for the same reason).
        knobs: Box<KnobTable>,
        /// Edges to reconnect (best effort, since peers may restore later).
        edges: Vec<EdgeSnapshot>,
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
            Self::AddNode { id, kind, x, y } => Self::RemoveNode {
                id: *id,
                kind: kind.clone(),
                x: *x,
                y: *y,
                // A freshly added node still has its registry defaults and no
                // wires, so that is the exact state undo must restore to.
                knobs: Box::new(
                    find_cartridge(kind)
                        .map(|descriptor| (descriptor.default_knobs)())
                        .unwrap_or_default(),
                ),
                edges: Vec::new(),
            },
            Self::RemoveNode {
                id,
                kind,
                x,
                y,
                knobs,
                edges,
            } => Self::RestoreNode {
                id: *id,
                kind: kind.clone(),
                x: *x,
                y: *y,
                knobs: knobs.clone(),
                edges: edges.clone(),
            },
            Self::RestoreNode {
                id,
                kind,
                x,
                y,
                knobs,
                edges,
            } => Self::RemoveNode {
                id: *id,
                kind: kind.clone(),
                x: *x,
                y: *y,
                knobs: knobs.clone(),
                edges: edges.clone(),
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
    /// Atomically route a Nuke-style physical `Viewer` node at `target_node`.
    ///
    /// The kernel reuses (or creates) a `Viewer` cartridge, replaces the input
    /// on `PortId(0)`, binds the viewer, bumps the epoch, and renders — all in
    /// one message so immediate-mode UI cannot race itself.
    RouteNukeViewer {
        /// Node the physical Viewer should look through.
        target_node: NodeId,
        /// Canvas position used when a Viewer node has to be created.
        fallback_pos: (f32, f32),
    },
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
    /// The kernel's viewer target changed (including `None` when it was
    /// cleared because the node was deleted or routed elsewhere).
    ViewerChanged(Option<NodeId>),
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
///
/// A **cleared** notification is one whose packet is empty. The kernel sends
/// one whenever the viewer has nothing displayable (no target, dead target,
/// evaluation error, or a chain that produced no payload), so the UI never
/// keeps showing pixels that the graph no longer backs.
#[derive(Debug, Clone)]
pub struct FrameNotification {
    /// Node the viewer asked to render, or `None` when there is no target.
    pub node: Option<NodeId>,
    /// Render epoch that produced the frame.
    pub epoch: u64,
    /// The evaluated packet, ready for the UI to inspect.
    pub packet: Arc<Packet>,
}

impl FrameNotification {
    /// Builds a notification carrying a real payload.
    pub fn frame(node: NodeId, epoch: u64, packet: Arc<Packet>) -> Self {
        Self {
            node: Some(node),
            epoch,
            packet,
        }
    }

    /// Builds a notification that tells the UI to clear the viewer.
    pub fn cleared(node: Option<NodeId>, epoch: u64) -> Self {
        Self {
            node,
            epoch,
            packet: Arc::new(Packet::new()),
        }
    }

    /// `true` when this notification carries no displayable payload.
    pub fn is_cleared(&self) -> bool {
        self.packet.is_empty()
    }
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
    // What the UI is currently displaying; `None` means the viewer is blank.
    let mut displayed_target: Option<NodeId> = None;
    let mut render_dirty = false;

    while let Ok(first) = messages.recv() {
        // Drain-then-render: apply every queued mutation before paying for a
        // render, so a burst of seed commands produces exactly one frame.
        let mut batch = vec![first];
        while let Ok(message) = messages.try_recv() {
            batch.push(message);
        }

        let mut shutdown = false;
        // Coalesce consecutive SetKnob messages for the same (node, knob)
        // within the batch so a burst of drag events evaluates only the
        // latest value, keeping the render backlog low and the undo stack
        // clean.
        let mut coalesced: Vec<KernelMessage> = Vec::with_capacity(batch.len());
        for message in batch {
            if let KernelMessage::Execute(UndoableCommand::SetKnob {
                ref node,
                ref knob,
                ref value,
                ..
            }) = message
            {
                if let Some(&mut KernelMessage::Execute(UndoableCommand::SetKnob {
                    node: ref last_node,
                    knob: ref last_knob,
                    value: ref mut last_value,
                    ..
                })) = coalesced.last_mut()
                {
                    if last_node == node && last_knob == knob {
                        // Keep the latest `value`; `previous` stays from the
                        // first message in the run for correct undo semantics.
                        *last_value = value.clone();
                        continue;
                    }
                }
            }
            coalesced.push(message);
        }

        for message in coalesced {
            match message {
                KernelMessage::Execute(command) => {
                    // Invalidate downstream cache entries for knob changes so
                    // stale rendered pixels don't linger. Upstream caches
                    // (e.g. Constant) are preserved by op-hash keying.
                    if let UndoableCommand::SetKnob { node, .. } = &command {
                        dag.invalidate_downstream(*node);
                    }
                    if apply_and_record(&mut dag, &mut undo, &events, command) {
                        render_dirty = true;
                    }
                }
                KernelMessage::Undo => {
                    if let Some(command) = undo.undo() {
                        match dag.undo_command(&command) {
                            Ok(()) => {
                                // Topology may have changed: drop cached packets
                                // so the next pull reflects the restored wiring.
                                dag.cache_mut().clear();
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
                                dag.cache_mut().clear();
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
                    let _ = events.send(KernelEvent::ViewerChanged(node));
                    render_dirty = true;
                }
                KernelMessage::RouteNukeViewer {
                    target_node,
                    fallback_pos,
                } => {
                    if let Some(viewer) =
                        route_nuke_viewer(&mut dag, &mut undo, &events, target_node, fallback_pos)
                    {
                        viewer_node = Some(viewer);
                        let _ = events.send(KernelEvent::ViewerChanged(Some(viewer)));
                        let _ = events.send(KernelEvent::EpochChanged(epoch.bump()));
                        render_dirty = true;
                    }
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

        // A node can disappear through Execute, Undo, or Redo; `render_viewer`
        // unbinds a dead target and clears the image instead of freezing it.
        if let Some(notification) = render_viewer(
            &mut dag,
            &mut viewer_node,
            &mut displayed_target,
            &base_ctx,
            &governor,
            &epoch,
            &events,
        ) {
            let _ = frames.send(notification);
        }

        // GPU queue contention: on integrated GPUs the render holds the device
        // queue while downloading pixels, starving the UI thread. Yielding for
        // one 60 FPS frame releases the queue so egui can redraw between
        // renders.
        thread::sleep(Duration::from_millis(16));
    }
}

/// Evaluates the viewer and yields the notification the UI should receive.
///
/// Every dead end clears the viewer instead of leaving stale pixels behind:
/// a deleted target, a panic, a missing input, or a chain that produced no
/// payload all send an empty [`FrameNotification`]. `displayed_target` tracks
/// what the UI is currently showing so an unbound viewer only emits a cleared
/// frame when there is actually something to clear.
fn render_viewer(
    dag: &mut Dag,
    viewer_node: &mut Option<NodeId>,
    displayed_target: &mut Option<NodeId>,
    context: &EvalContext,
    governor: &MemoryGovernor,
    epoch: &RenderEpoch,
    events: &Sender<KernelEvent>,
) -> Option<FrameNotification> {
    let Some(target) = *viewer_node else {
        // Nothing is bound: clear only if the UI is still showing something.
        return displayed_target
            .take()
            .map(|_| FrameNotification::cleared(None, epoch.bump()));
    };

    if !dag.contains_node(target) {
        *viewer_node = None;
        *displayed_target = None;
        let _ = events.send(KernelEvent::ViewerChanged(None));
        return Some(FrameNotification::cleared(None, epoch.bump()));
    }

    let current_epoch = epoch.bump();
    match dag.evaluate_pull(target, context, governor, epoch, current_epoch) {
        Ok(packet) if !packet.is_empty() => {
            *displayed_target = Some(target);
            Some(FrameNotification::frame(target, current_epoch, packet))
        }
        Ok(_) => {
            // The chain exists but produced nothing displayable: an
            // unconnected `Viewer`, a detached upstream wire, or a
            // metadata-only packet.
            *displayed_target = None;
            Some(FrameNotification::cleared(Some(target), current_epoch))
        }
        Err(NodeError::Panic(message)) => {
            *displayed_target = None;
            let _ = events.send(KernelEvent::NodeCrashed {
                node: target,
                message,
            });
            Some(FrameNotification::cleared(Some(target), current_epoch))
        }
        Err(_) => {
            // Missing inputs, memory pressure, and preemption are normal
            // during interactive editing. Clear the image; the next change
            // retries.
            *displayed_target = None;
            Some(FrameNotification::cleared(Some(target), current_epoch))
        }
    }
}

/// Applies a command, records it for undo, and reports the outcome.
///
/// Destructive commands are enriched from the live DAG first, so the undo stack
/// always holds a full snapshot even if the UI's mirror was stale.
fn apply_and_record(
    dag: &mut Dag,
    undo: &mut UndoStack,
    events: &Sender<KernelEvent>,
    command: UndoableCommand,
) -> bool {
    let command = dag.enrich(command);
    match dag.apply_command(&command) {
        Ok(()) => {
            // Topology changes alter every downstream op hash. Dropping the
            // RAM cache guarantees the very next pull re-evaluates, so
            // detaching a wire updates the viewer immediately instead of
            // serving a packet cached for the old wiring.
            if changes_topology(&command) {
                dag.cache_mut().clear();
            }
            undo.push(command.clone());
            let _ = events.send(KernelEvent::Applied(command));
            true
        }
        Err(error) => {
            let _ = events.send(KernelEvent::CommandFailed {
                command,
                message: error.to_string(),
            });
            false
        }
    }
}

/// `true` for commands that add, remove, or rewire nodes.
///
/// These invalidate every cached op hash downstream of the change, so the
/// worker drops the packet cache before the next render.
fn changes_topology(command: &UndoableCommand) -> bool {
    matches!(
        command,
        UndoableCommand::AddNode { .. }
            | UndoableCommand::RemoveNode { .. }
            | UndoableCommand::RestoreNode { .. }
            | UndoableCommand::ConnectEdge { .. }
            | UndoableCommand::DisconnectEdge { .. }
    )
}

/// Atomically routes a physical `Viewer` node at `target_node`.
///
/// Returns the viewer id when the route succeeded. Every mutation goes through
/// [`apply_and_record`], so creating the Viewer, rewiring port 0, and the
/// resulting undo entries are one linear history.
fn route_nuke_viewer(
    dag: &mut Dag,
    undo: &mut UndoStack,
    events: &Sender<KernelEvent>,
    target_node: NodeId,
    fallback_pos: (f32, f32),
) -> Option<NodeId> {
    if !dag.contains_node(target_node) {
        return None;
    }

    // Routing a Viewer node at itself just binds it.
    if dag
        .node(target_node)
        .is_some_and(|node| node.kind == "Viewer")
    {
        return Some(target_node);
    }

    let existing_viewer = dag
        .nodes()
        .find(|node| node.kind == "Viewer")
        .map(|node| node.id);
    let viewer_id = match existing_viewer {
        Some(id) => id,
        None => {
            let id = dag.next_node_id();
            let command = UndoableCommand::AddNode {
                id,
                kind: "Viewer".to_string(),
                x: fallback_pos.0,
                y: fallback_pos.1,
            };
            if !apply_and_record(dag, undo, events, command) {
                return None;
            }
            id
        }
    };

    // Port 0 is the single input a Viewer accepts; replace whatever was there.
    let current_input = dag
        .edges()
        .find(|edge| edge.to_node == viewer_id && edge.to_port == PortId(0))
        .copied();
    if let Some(edge) = current_input {
        if edge.from_node == target_node && edge.from_port == PortId(0) {
            return Some(viewer_id);
        }
        let command = UndoableCommand::DisconnectEdge {
            from_node: edge.from_node,
            from_port: edge.from_port,
            to_node: edge.to_node,
            to_port: edge.to_port,
        };
        if !apply_and_record(dag, undo, events, command) {
            return None;
        }
    }

    let command = UndoableCommand::ConnectEdge {
        from_node: target_node,
        from_port: PortId(0),
        to_node: viewer_id,
        to_port: PortId(0),
    };
    // The DAG rejects cycles and dangling endpoints; surface that as a failed
    // command instead of binding the viewer to a broken route.
    if !apply_and_record(dag, undo, events, command) {
        return None;
    }

    Some(viewer_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use crate::dag::Edge;
    use crate::knobs::{KnobTable, KnobValue};
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
                x: 0.0,
                y: 0.0,
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
                x: 0.0,
                y: 0.0,
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
                x: 0.0,
                y: 0.0,
            }))
            .expect("add node");
        kernel
            .send(KernelMessage::SetViewerNode(Some(NodeId(1))))
            .expect("set viewer");

        let frame = kernel
            .frames()
            .recv_timeout(Duration::from_secs(5))
            .expect("viewer should render automatically");

        assert_eq!(frame.node, Some(NodeId(1)));
        assert_eq!(frame.packet.get::<u32>(), Some(&7));

        kernel.shutdown();
    }

    fn remove_node(id: NodeId, kind: &str) -> UndoableCommand {
        UndoableCommand::RemoveNode {
            id,
            kind: kind.to_string(),
            x: 0.0,
            y: 0.0,
            knobs: Box::new(KnobTable::new()),
            edges: Vec::new(),
        }
    }

    #[test]
    fn render_viewer_clears_when_the_upstream_wire_is_detached() {
        let (events_tx, _events_rx) = unbounded();
        let mut dag = Dag::new();
        dag.add_node(NodeId(1), "Constant").unwrap();
        dag.add_node(NodeId(2), "Viewer").unwrap();
        dag.connect_edge(Edge::new(NodeId(1), PortId(0), NodeId(2), PortId(0)))
            .unwrap();

        let mut viewer = Some(NodeId(2));
        let mut displayed = None;
        let governor = MemoryGovernor::new(usize::MAX);
        let epoch = RenderEpoch::new();
        let context = EvalContext::new();

        let frame = render_viewer(
            &mut dag,
            &mut viewer,
            &mut displayed,
            &context,
            &governor,
            &epoch,
            &events_tx,
        )
        .expect("a wired Viewer renders");
        assert_eq!(frame.node, Some(NodeId(2)));
        assert!(!frame.is_cleared(), "the Constant must reach the viewer");
        assert_eq!(frame.packet.get::<u32>(), Some(&7));
        assert_eq!(displayed, Some(NodeId(2)));

        // Detaching the upstream wire must clear the image instead of leaving
        // the last frame frozen on screen.
        dag.apply_command(&UndoableCommand::DisconnectEdge {
            from_node: NodeId(1),
            from_port: PortId(0),
            to_node: NodeId(2),
            to_port: PortId(0),
        })
        .unwrap();

        let cleared = render_viewer(
            &mut dag,
            &mut viewer,
            &mut displayed,
            &context,
            &governor,
            &epoch,
            &events_tx,
        )
        .expect("an unconnected Viewer clears");
        assert!(cleared.is_cleared(), "the viewer must go blank");
        assert_eq!(cleared.node, Some(NodeId(2)));
        assert_eq!(displayed, None);
    }

    #[test]
    fn render_viewer_clears_an_unbound_viewer_only_when_it_had_content() {
        let (events_tx, _events_rx) = unbounded();
        let mut dag = Dag::new();
        let mut viewer = None;
        let mut displayed = None;
        let governor = MemoryGovernor::new(usize::MAX);
        let epoch = RenderEpoch::new();
        let context = EvalContext::new();

        assert!(
            render_viewer(
                &mut dag,
                &mut viewer,
                &mut displayed,
                &context,
                &governor,
                &epoch,
                &events_tx,
            )
            .is_none(),
            "a viewer that never showed anything has nothing to clear"
        );

        // Pretend the UI is still showing a frame from a node that just lost
        // its binding: the next render must clear it.
        displayed = Some(NodeId(3));
        let cleared = render_viewer(
            &mut dag,
            &mut viewer,
            &mut displayed,
            &context,
            &governor,
            &epoch,
            &events_tx,
        )
        .expect("the stale image is cleared");
        assert!(cleared.is_cleared());
        assert_eq!(cleared.node, None);
        assert_eq!(displayed, None);
    }

    #[test]
    fn deleting_the_viewer_target_clears_the_kernel_viewer() {
        let kernel = KernelHandle::spawn().expect("kernel thread should spawn");
        let node = NodeId(41);

        kernel
            .send(KernelMessage::Execute(UndoableCommand::AddNode {
                id: node,
                kind: "Constant".to_string(),
                x: 0.0,
                y: 0.0,
            }))
            .expect("add node");
        assert!(matches!(next_event(&kernel), KernelEvent::Applied(_)));

        kernel
            .send(KernelMessage::SetViewerNode(Some(node)))
            .expect("bind viewer");
        match next_event(&kernel) {
            KernelEvent::ViewerChanged(Some(id)) => assert_eq!(id, node),
            other => panic!("expected ViewerChanged(Some), got {other:?}"),
        }

        let frame = kernel
            .frames()
            .recv_timeout(Duration::from_secs(5))
            .expect("live viewer frame");
        assert_eq!(frame.node, Some(node));

        // Deleting the viewed node must clear the binding, not panic or render.
        kernel
            .send(KernelMessage::Execute(remove_node(node, "Constant")))
            .expect("remove node");

        let cleared = kernel
            .frames()
            .recv_timeout(Duration::from_secs(5))
            .expect("the viewer should be cleared");
        assert_eq!(cleared.node, None);
        assert!(cleared.packet.is_empty());

        let events: Vec<KernelEvent> = kernel.events().try_iter().collect();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, KernelEvent::ViewerChanged(None))),
            "the UI must be told the viewer was cleared: {events:?}"
        );

        kernel.shutdown();
    }

    #[test]
    fn route_nuke_viewer_creates_reuses_and_rewires_a_viewer() {
        let (events_tx, events_rx) = unbounded();
        let mut dag = Dag::new();
        dag.add_node(NodeId(1), "Constant").unwrap();
        dag.add_node(NodeId(2), "Constant").unwrap();
        let mut undo = UndoStack::new();

        let viewer = route_nuke_viewer(&mut dag, &mut undo, &events_tx, NodeId(1), (5.0, 125.0))
            .expect("routing should create a Viewer");
        let node = dag.node(viewer).expect("Viewer node exists");
        assert_eq!(node.kind, "Viewer");
        assert_eq!((node.x, node.y), (5.0, 125.0));
        assert!(undo.can_undo(), "creating the Viewer is undoable");

        let edges: Vec<_> = dag
            .edges()
            .filter(|edge| edge.to_node == viewer)
            .copied()
            .collect();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].from_node, NodeId(1));
        assert_eq!(edges[0].to_port, PortId(0));

        let events: Vec<KernelEvent> = events_rx.try_iter().collect();
        assert!(
            events.iter().any(|event| matches!(
                event,
                KernelEvent::Applied(UndoableCommand::AddNode { kind, x, y, .. })
                    if kind == "Viewer" && *x == 5.0 && *y == 125.0
            )),
            "the created Viewer must be reported: {events:?}"
        );
        assert!(
            events.iter().any(|event| matches!(
                event,
                KernelEvent::Applied(UndoableCommand::ConnectEdge {
                    from_node,
                    to_node,
                    to_port,
                    ..
                }) if *from_node == NodeId(1) && *to_node == viewer && *to_port == PortId(0)
            )),
            "the Viewer must be wired to the target: {events:?}"
        );

        // Routing another node reuses the Viewer and replaces port 0.
        let again = route_nuke_viewer(&mut dag, &mut undo, &events_tx, NodeId(2), (99.0, 99.0))
            .expect("routing should reuse the Viewer");
        assert_eq!(again, viewer);
        assert_eq!(dag.node_count(), 3, "no second Viewer may be created");

        let inputs: Vec<_> = dag
            .edges()
            .filter(|edge| edge.to_node == viewer && edge.to_port == PortId(0))
            .copied()
            .collect();
        assert_eq!(inputs.len(), 1, "port 0 must have exactly one input");
        assert_eq!(inputs[0].from_node, NodeId(2));

        // Routing a Viewer at itself simply binds it.
        assert_eq!(
            route_nuke_viewer(&mut dag, &mut undo, &events_tx, viewer, (0.0, 0.0)),
            Some(viewer)
        );
        assert_eq!(dag.node_count(), 3);
    }

    #[test]
    fn route_nuke_viewer_binds_and_renders_the_physical_viewer() {
        let kernel = KernelHandle::spawn().expect("kernel thread should spawn");
        let target = NodeId(51);

        kernel
            .send(KernelMessage::Execute(UndoableCommand::AddNode {
                id: target,
                kind: "Constant".to_string(),
                x: 0.0,
                y: 0.0,
            }))
            .expect("add node");
        assert!(matches!(next_event(&kernel), KernelEvent::Applied(_)));

        kernel
            .send(KernelMessage::RouteNukeViewer {
                target_node: target,
                fallback_pos: (12.0, 132.0),
            })
            .expect("route viewer");

        let mut viewer = None;
        loop {
            match next_event(&kernel) {
                KernelEvent::Applied(UndoableCommand::AddNode { id, kind, x, y, .. })
                    if kind == "Viewer" =>
                {
                    assert_eq!((x, y), (12.0, 132.0));
                    viewer = Some(id);
                }
                KernelEvent::Applied(UndoableCommand::ConnectEdge {
                    from_node,
                    to_node,
                    to_port,
                    ..
                }) => {
                    assert_eq!(from_node, target);
                    assert_eq!(to_port, PortId(0));
                    assert_eq!(Some(to_node), viewer);
                }
                KernelEvent::ViewerChanged(Some(id)) => assert_eq!(Some(id), viewer),
                KernelEvent::EpochChanged(_) => break,
                _ => {}
            }
        }

        let frame = kernel
            .frames()
            .recv_timeout(Duration::from_secs(5))
            .expect("the physical Viewer should render");
        assert_eq!(frame.node, viewer);
        assert_eq!(frame.packet.get::<u32>(), Some(&7));

        kernel.shutdown();
    }

    /// A probe cartridge that echoes a Float knob as its u32 output, so knob
    /// changes are visible in rendered frames.
    struct KnobEchoProbe {
        value: f64,
    }

    impl crate::types::NodeTrait for KnobEchoProbe {
        fn evaluate(&self, ctx: &EvalContext, _inputs: &[Packet]) -> Result<Packet, NodeError> {
            let binding = KnobTable::new();
            let knobs = ctx.knobs().unwrap_or(&binding);
            let value = match knobs.get("value") {
                Some(KnobValue::Float(v)) => *v as u32,
                _ => 0,
            };
            let mut packet = Packet::new();
            packet.insert(value);
            Ok(packet)
        }

        fn serialize_to_tcl(&self) -> String {
            format!("KnobEcho {}", self.value)
        }
    }

    fn knob_echo_factory() -> Arc<dyn crate::types::NodeTrait> {
        Arc::new(KnobEchoProbe { value: 0.0 })
    }

    inventory::submit! {
        NodeCartridgeDescriptor {
            kind: "KnobEcho",
            default_knobs: || {
                let mut knobs = KnobTable::new();
                knobs.insert("value", KnobValue::Float(0.0));
                knobs
            },
            factory: knob_echo_factory,
        }
    }

    #[test]
    fn batch_drain_coalesces_consecutive_set_knobs() {
        let kernel = KernelHandle::spawn().expect("kernel thread should spawn");
        let node = NodeId(21);

        kernel
            .send(KernelMessage::Execute(UndoableCommand::AddNode {
                id: node,
                kind: "KnobEcho".to_string(),
                x: 0.0,
                y: 0.0,
            }))
            .expect("add KnobEcho");
        assert!(matches!(
            next_event(&kernel),
            KernelEvent::Applied(UndoableCommand::AddNode { .. })
        ));

        kernel
            .send(KernelMessage::SetViewerNode(Some(node)))
            .expect("bind viewer");

        let _first_frame = kernel
            .frames()
            .recv_timeout(Duration::from_secs(5))
            .expect("initial frame");

        // Fire 20 SetKnob messages in rapid succession. They should arrive in
        // a single batch and be coalesced so only the last value is evaluated.
        for i in 1..=20u32 {
            kernel
                .send(KernelMessage::Execute(UndoableCommand::SetKnob {
                    node,
                    knob: "value".to_string(),
                    value: KnobValue::Float(i as f64),
                    previous: KnobValue::Float((i - 1) as f64),
                }))
                .expect("send SetKnob");
        }

        // Drain all events: only one SetKnob application should be recorded
        // because consecutive SetKnobs for the same knob coalesce.
        let mut applied_knobs = 0u32;
        let mut final_value: Option<u32> = None;
        for _ in 0..50 {
            match kernel.events().recv_timeout(Duration::from_millis(100)) {
                Ok(KernelEvent::Applied(UndoableCommand::SetKnob {
                    node: n,
                    knob,
                    value,
                    ..
                })) if n == node && knob == "value" => {
                    applied_knobs += 1;
                    if let KnobValue::Float(v) = value {
                        final_value = Some(v as u32);
                    }
                }
                Ok(KernelEvent::EpochChanged(_)) => {}
                Ok(_) => {}
                Err(_) => break,
            }
        }

        assert_eq!(
            applied_knobs, 1,
            "20 rapid SetKnobs must coalesce into one applied event, got {applied_knobs}"
        );
        assert_eq!(
            final_value,
            Some(20),
            "final rendered value must be the 20th knob value, got {final_value:?}"
        );

        // The rendered frame must reflect the coalesced final value.
        let frame = kernel
            .frames()
            .recv_timeout(Duration::from_secs(5))
            .expect("coalesced render frame");
        assert_eq!(frame.packet.get::<u32>(), Some(&20));

        kernel.shutdown();
    }
}

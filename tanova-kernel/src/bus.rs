//! The kernel message bus, undo history, and background worker.
//!
//! The UI never mutates the graph directly. It sends [`KernelMessage`]s over
//! one channel and receives [`KernelEvent`]s over another, so the microkernel
//! can live on its own thread while the UI stays stateless.

use std::thread::{self, JoinHandle};

use crossbeam_channel::{Receiver, SendError, Sender, unbounded};

use crate::governor::{MemoryGovernor, RenderEpoch};

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
#[derive(Debug, Clone, PartialEq, Eq)]
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
        /// New serialized value.
        value: String,
        /// Value before this command.
        previous: String,
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KernelMessage {
    /// Apply a reversible command.
    Execute(UndoableCommand),
    /// Undo the most recent command.
    Undo,
    /// Redo the most recently undone command.
    Redo,
    /// Invalidate in-flight renders.
    BumpEpoch,
    /// Ask the kernel thread to exit.
    Shutdown,
}

/// Notifications sent from the kernel thread back to the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KernelEvent {
    /// A command was applied and recorded in the undo stack.
    Applied(UndoableCommand),
    /// A command was undone (the inverse is now in effect).
    Undone(UndoableCommand),
    /// A previously undone command was re-applied.
    Redone(UndoableCommand),
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
/// The pixels themselves will travel as shared `wgpu::TextureView` handles in
/// a later milestone; this is the typed notification side of that channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameNotification {
    /// Render epoch that produced the frame.
    pub epoch: u64,
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
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
    pub fn push(&mut self, command: UndoableCommand) {
        self.redo.clear();
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
    _frames: Sender<FrameNotification>,
    governor: MemoryGovernor,
    epoch: RenderEpoch,
) {
    let mut undo = UndoStack::new();
    // The governor is wired in now; DAG evaluations start reserving from it in
    // a later milestone.
    let _governor = governor;

    while let Ok(message) = messages.recv() {
        match message {
            KernelMessage::Execute(command) => {
                // Graph mutation lands with the DAG; the undo ledger is real
                // from day one so the UI can already round-trip commands.
                undo.push(command.clone());
                let _ = events.send(KernelEvent::Applied(command));
            }
            KernelMessage::Undo => {
                if let Some(command) = undo.undo() {
                    let _ = events.send(KernelEvent::Undone(command));
                }
            }
            KernelMessage::Redo => {
                if let Some(command) = undo.redo() {
                    let _ = events.send(KernelEvent::Redone(command));
                }
            }
            KernelMessage::BumpEpoch => {
                let _ = events.send(KernelEvent::EpochChanged(epoch.bump()));
            }
            KernelMessage::Shutdown => break,
        }
    }
}

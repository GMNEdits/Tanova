//! Tanova desktop application shell.
//!
//! The UI thread owns no graph state: it mirrors the kernel's graph from
//! [`KernelEvent`]s and sends every mutation back as a [`KernelMessage`].
//! The kernel thread evaluates the viewer target and delivers frames
//! off-screen through [`FrameNotification`]s.

use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui;
use tanova_io::DiskCache;
use tanova_kernel::{
    Dag, Edge, EvalContext, FrameNotification, KernelEvent, KernelHandle, KernelMessage, KnobValue,
    NodeId, Packet, PortId, ProjectFormat, UndoableCommand, find_cartridge,
};
use tanova_parse::load_nk_into_dag;
use tanova_render::{GpuComputeEngine, Raster2D};
use tanova_ui::{
    GraphMirror, NodeCanvas, NodeCanvasState, PropertiesPanel, ViewerPanel, ViewerWorkflow,
};

/// Errors crossing the app boundary (eframe requires `Send + Sync`).
type AppError = Box<dyn Error + Send + Sync>;

/// Maps an undo/redo shortcut to its kernel message.
///
/// Redo is matched first so `Ctrl+Shift+Z` can never fall through to the plain
/// `Ctrl+Z` undo branch. Both `command` (macOS ⌘ / Linux Ctrl) and raw `ctrl`
/// are accepted so the shortcut also works in headless tests.
pub fn shortcut_message(modifiers: egui::Modifiers, key: egui::Key) -> Option<KernelMessage> {
    if !(modifiers.command || modifiers.ctrl) {
        return None;
    }
    match key {
        egui::Key::Y => Some(KernelMessage::Redo),
        egui::Key::Z if modifiers.shift => Some(KernelMessage::Redo),
        egui::Key::Z => Some(KernelMessage::Undo),
        _ => None,
    }
}

/// Node ids of the starter comp.
const CONSTANT: NodeId = NodeId(1);
const GRADE: NodeId = NodeId(2);
/// Physical Nuke viewer that the starter comp renders through.
const VIEWER: NodeId = NodeId(3);

/// Starter constant color, graded by an identity Grade.
const STARTER_COLOR: [f32; 4] = [0.18, 0.35, 0.65, 1.0];
/// Small resolution keeps startup and the headless check fast.
const STARTER_SIZE: i64 = 256;

/// Pending action blocked by unsaved changes, resolved by the confirmation
/// modal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingUnsavedAction {
    NewScript,
    OpenScript,
    QuitApp,
}

/// In-app file browser modal for Open / Save As.
#[derive(Debug, Clone)]
pub enum FileModalState {
    Open {
        directory: PathBuf,
        file_input: String,
        /// True on the first frame the modal is shown; used to auto-focus
        /// and select-all the `File:` text input.
        just_opened: bool,
    },
    SaveAs {
        directory: PathBuf,
        file_input: String,
        follow_up: Option<PendingUnsavedAction>,
        /// True on the first frame the modal is shown; used to auto-focus
        /// and select-all the `File:` text input.
        just_opened: bool,
    },
}

/// Outcome of one file-modal frame, resolved after `egui::Window` closes.
///
/// Using `take()` + `ModalOutcome` guarantees that closing and chained
/// follow-up modals never collide: `self.file_modal` is set to `None` at the
/// start of the frame and only restored when the outcome is `KeepOpen`.
#[derive(Debug, Clone)]
enum ModalOutcome {
    KeepOpen,
    Cancel,
    ConfirmSave {
        path: PathBuf,
        follow_up: Option<PendingUnsavedAction>,
    },
    ConfirmOpen {
        path: PathBuf,
    },
}

/// GPU handles shared by the UI thread and the background render thread.
///
/// The `Arc` wrapper is deliberate: wgpu's `Device` and `Queue` are cheap to
/// clone internally, but a single shared instance keeps one GPU context for
/// the whole application.
#[derive(Clone)]
pub struct GpuContext {
    /// The single logical device every renderer targets.
    pub device: Arc<wgpu::Device>,
    /// The submission queue paired with `device`.
    pub queue: Arc<wgpu::Queue>,
}

/// Timeline state owned by the UI thread.
///
/// `current` is fractional so scrubbing stays smooth, but frame paths resolve
/// with `round()` before they hit disk.
#[derive(Debug, Clone, PartialEq)]
pub struct TimelineState {
    /// First frame of the play range.
    pub start: i64,
    /// Last frame of the play range.
    pub end: i64,
    /// Current frame (may be fractional while scrubbing).
    pub current: f64,
    /// Whether playback advances `current` automatically.
    pub playing: bool,
}

impl Default for TimelineState {
    fn default() -> Self {
        Self {
            start: 1,
            end: 100,
            current: 1.0,
            playing: false,
        }
    }
}

/// Requests a high-performance adapter and opens a device on it.
pub async fn init_gpu() -> Result<GpuContext, AppError> {
    let instance = wgpu::Instance::default();

    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
            apply_limit_buckets: false,
        })
        .await?;

    let (device, queue) = adapter
        .request_device(&wgpu::DeviceDescriptor {
            label: Some("tanova-device"),
            ..Default::default()
        })
        .await?;

    let info = adapter.get_info();
    println!("GPU: {} ({:?})", info.name, info.backend);

    Ok(GpuContext {
        device: Arc::new(device),
        queue: Arc::new(queue),
    })
}

fn main() -> Result<(), AppError> {
    tanova_nodes::ensure_linked();

    if std::env::args()
        .skip(1)
        .any(|arg| arg == "--check-headless")
    {
        run_headless_check()
    } else {
        run_desktop()
    }
}

/// Opens the interactive eframe window.
fn run_desktop() -> Result<(), AppError> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Tanova")
            .with_inner_size([1280.0, 720.0])
            .with_min_inner_size([720.0, 480.0])
            .with_active(true),
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };

    eframe::run_native(
        "Tanova",
        options,
        Box::new(|cc| {
            let gpu = gpu_from_render_state(cc)?;
            let kernel = KernelHandle::spawn().map_err(kernel_error)?;
            Ok(Box::new(TanovaApp::new(kernel, gpu)))
        }),
    )
    .map_err(|error| -> AppError { Box::new(std::io::Error::other(error.to_string())) })
}

/// Reuses eframe's wgpu device instead of creating a second one.
fn gpu_from_render_state(cc: &eframe::CreationContext<'_>) -> Result<GpuContext, AppError> {
    let render_state = cc
        .wgpu_render_state
        .as_ref()
        .ok_or_else(|| check_failed("eframe did not provide a wgpu render state"))?;

    let info = render_state.adapter.get_info();
    println!("GPU: {} ({:?})", info.name, info.backend);

    Ok(GpuContext {
        device: Arc::new(render_state.device.clone()),
        queue: Arc::new(render_state.queue.clone()),
    })
}

/// Boots GPU + kernel + UI state, verifies the starter comp renders, and exits.
fn run_headless_check() -> Result<(), AppError> {
    println!("check-headless: initializing GPU");
    let gpu = pollster::block_on(init_gpu())?;
    println!(
        "check-headless: GPU ready; holding {} shared handle(s)",
        Arc::strong_count(&gpu.device)
    );

    let mut app = TanovaApp::new(KernelHandle::spawn().map_err(kernel_error)?, gpu);
    let packet = app.wait_for_frame(VIEWER, Duration::from_secs(60))?;
    println!(
        "check-headless: viewer frame from #{}; nodes={} edges={} epoch={}",
        VIEWER.0,
        app.graph.node_count(),
        app.graph.edge_count(),
        app.epoch
    );

    if let Some(failure) = app.failure.as_deref() {
        return Err(check_failed(format!(
            "kernel reported a failure: {failure}"
        )));
    }
    if app.graph.node_count() != 3 {
        return Err(check_failed(format!(
            "expected 3 seeded nodes, found {}",
            app.graph.node_count()
        )));
    }
    if app.graph.edge_count() != 2 {
        return Err(check_failed(format!(
            "expected 2 seeded wires, found {}",
            app.graph.edge_count()
        )));
    }

    let wire = app.graph.edges()[0];
    if wire.from_node != CONSTANT || wire.to_node != GRADE {
        return Err(check_failed(format!(
            "expected a #1 -> #2 wire, found #{} -> #{}",
            wire.from_node.0, wire.to_node.0
        )));
    }
    let viewer_wire = app.graph.edges()[1];
    if viewer_wire.from_node != GRADE || viewer_wire.to_node != VIEWER {
        return Err(check_failed(format!(
            "expected a #2 -> #3 Viewer wire, found #{} -> #{}",
            viewer_wire.from_node.0, viewer_wire.to_node.0
        )));
    }
    if app.viewer.viewer_node() != Some(VIEWER) {
        return Err(check_failed(
            "the Nuke workflow must bind the physical Viewer node",
        ));
    }

    let raster = packet
        .get::<Raster2D>()
        .ok_or_else(|| check_failed("the viewer frame carried no Raster2D"))?;
    if raster.format.width != STARTER_SIZE as u32 || raster.format.height != STARTER_SIZE as u32 {
        return Err(check_failed(format!(
            "expected a {}x{} frame, found {}x{}",
            STARTER_SIZE, STARTER_SIZE, raster.format.width, raster.format.height
        )));
    }

    // Grade defaults are identity, so the graded pixel equals the Constant.
    let pixel = raster.sample_rgba(STARTER_SIZE as i32 / 2, STARTER_SIZE as i32 / 2);
    for (index, (actual, expected)) in pixel.iter().zip(STARTER_COLOR.iter()).enumerate() {
        if (actual - expected).abs() > 1e-3 {
            return Err(check_failed(format!(
                "graded channel {index} was {actual}, expected {expected}"
            )));
        }
    }
    println!("check-headless: graded pixel RGBA = {pixel:?}");

    app.canvas.layout_nodes(&app.graph);
    println!(
        "check-headless: canvas laid out {} node position(s); GPU handle(s) = {}",
        app.canvas.positions.len(),
        Arc::strong_count(&app.gpu.device)
    );

    app.shutdown();
    println!("check-headless: OK");
    Ok(())
}

/// The eframe application: visual state on the UI thread, graph state in the
/// kernel.
struct TanovaApp {
    kernel: KernelHandle,
    graph: GraphMirror,
    canvas: NodeCanvasState,
    viewer: ViewerPanel,
    /// Which viewer paradigm the UI presents (Nuke node vs Fusion target).
    workflow: ViewerWorkflow,
    timeline: TimelineState,
    epoch: u64,
    last_frame: Option<FrameNotification>,
    failure: Option<String>,
    gpu: GpuContext,
    /// True when the kernel is rendering a frame triggered by a UI message;
    /// keeps egui repainting until the new frame arrives so there's no visual
    /// lag on undo/redo or drag release.
    awaiting_kernel: bool,
    /// Current script file path, if any.
    pub script_path: Option<PathBuf>,
    /// Whether the graph has unsaved changes.
    pub is_dirty: bool,
    /// Last script load/save error, if any.
    pub last_script_error: Option<String>,
    /// Open file browser modal, if any.
    pub file_modal: Option<FileModalState>,
    /// Action blocked by unsaved changes, if any.
    pub pending_unsaved: Option<PendingUnsavedAction>,
    /// Whether the app is allowed to close (set after user confirms discard).
    pub allow_close: bool,
    /// Optional clipboard override for headless unit tests (avoids requiring
    /// an X11/Wayland display server).
    pub test_clipboard: Option<String>,
    /// In-app fallback clipboard buffer that always stores the latest copied
    /// text inside Tanova. Survives frames and never depends on the OS clipboard.
    pub internal_clipboard: Option<String>,
    /// Persistent OS clipboard handle, kept alive across frames so X11/Wayland
    /// never drops clipboard ownership when a temporary `Clipboard` is dropped.
    pub os_clipboard: Option<arboard::Clipboard>,
    /// Paste offset cascade: each `Ctrl+V` shifts the next paste by `40pt`.
    /// Resets to `0` on every `Ctrl+C`.
    pub paste_cascade: u32,
}

impl TanovaApp {
    fn new(kernel: KernelHandle, gpu: GpuContext) -> Self {
        let mut app = Self {
            kernel,
            graph: GraphMirror::default(),
            canvas: NodeCanvasState::default(),
            viewer: ViewerPanel::new(),
            workflow: ViewerWorkflow::default(),
            timeline: TimelineState::default(),
            epoch: 0,
            last_frame: None,
            failure: None,
            gpu,
            awaiting_kernel: false,
            script_path: None,
            is_dirty: false,
            last_script_error: None,
            file_modal: None,
            pending_unsaved: None,
            allow_close: false,
            test_clipboard: None,
            internal_clipboard: None,
            os_clipboard: arboard::Clipboard::new().ok(),
            paste_cascade: 0,
        };
        app.seed_graph();
        app
    }

    /// Queues the live starter comp: Constant -> Grade -> Viewer, viewed at #3.
    ///
    /// The starter graph ships a physical `Viewer` node so the default
    /// `ViewerWorkflow::Nuke` binding rule ("only a `Viewer` may be the viewer
    /// target") holds from the very first frame.
    fn seed_graph(&mut self) {
        let mut context = EvalContext::new();
        context.insert(GpuComputeEngine::new(
            self.gpu.device.clone(),
            self.gpu.queue.clone(),
        ));
        // One shared loader for every Read cartridge; the worker wakes the
        // kernel with TriggerRender when a frame becomes available.
        context.insert(DiskCache::new(self.kernel.sender()));
        // Project format: all source cartridges (Constant, Read, ...) produce
        // rasters at this resolution so downstream Transform/Merge2 stay
        // aligned with the viewer frame.
        context.insert(ProjectFormat::new(STARTER_SIZE as u32, STARTER_SIZE as u32));
        self.graph
            .set_project_format(ProjectFormat::new(STARTER_SIZE as u32, STARTER_SIZE as u32));

        let seed = [
            KernelMessage::SetBaseContext(context),
            KernelMessage::Execute(UndoableCommand::AddNode {
                id: CONSTANT,
                kind: "Constant".to_string(),
                x: 40.0,
                y: 40.0,
            }),
            KernelMessage::Execute(UndoableCommand::AddNode {
                id: GRADE,
                kind: "Grade".to_string(),
                x: 40.0,
                y: 164.0,
            }),
            KernelMessage::Execute(UndoableCommand::AddNode {
                id: VIEWER,
                kind: "Viewer".to_string(),
                x: 40.0,
                y: 288.0,
            }),
            KernelMessage::Execute(UndoableCommand::ConnectEdge {
                from_node: CONSTANT,
                from_port: PortId(0),
                to_node: GRADE,
                to_port: PortId(0),
            }),
            KernelMessage::Execute(UndoableCommand::ConnectEdge {
                from_node: GRADE,
                from_port: PortId(0),
                to_node: VIEWER,
                to_port: PortId(0),
            }),
            KernelMessage::Execute(UndoableCommand::SetKnob {
                node: CONSTANT,
                knob: "color".to_string(),
                value: KnobValue::Color(STARTER_COLOR),
                previous: KnobValue::Color([0.0, 0.0, 0.0, 1.0]),
            }),
            KernelMessage::Execute(UndoableCommand::SetKnob {
                node: CONSTANT,
                knob: "format_width".to_string(),
                value: KnobValue::Int(STARTER_SIZE),
                previous: KnobValue::Int(1920),
            }),
            KernelMessage::Execute(UndoableCommand::SetKnob {
                node: CONSTANT,
                knob: "format_height".to_string(),
                value: KnobValue::Int(STARTER_SIZE),
                previous: KnobValue::Int(1080),
            }),
            KernelMessage::SetViewerNode(Some(VIEWER)),
        ];

        self.viewer.bind(Some(VIEWER));
        for message in seed {
            if let Err(error) = self.kernel.send(message) {
                self.failure = Some(format!("failed to seed the kernel: {error}"));
            }
        }
        // A freshly seeded app starts clean.
        let _ = self.kernel.send(KernelMessage::MarkClean);
        self.is_dirty = false;
    }

    /// Non-blocking drain of kernel events and frame notifications.
    fn pump(&mut self) {
        let mut graph_updated = false;
        while let Ok(event) = self.kernel.events().try_recv() {
            if matches!(
                event,
                KernelEvent::Applied(_) | KernelEvent::Undone(_) | KernelEvent::Redone(_)
            ) {
                graph_updated = true;
            }
            self.fold_event(event);
        }
        let mut frame_received = false;
        while let Ok(frame) = self.kernel.frames().try_recv() {
            self.viewer.set_frame(&frame);
            self.last_frame = Some(frame);
            frame_received = true;
        }
        if frame_received || graph_updated {
            self.awaiting_kernel = false;
        }
    }

    /// Folds one kernel event into the UI mirror.
    fn fold_event(&mut self, event: KernelEvent) {
        match &event {
            KernelEvent::EpochChanged(epoch) => self.epoch = *epoch,
            KernelEvent::DirtyStateChanged { is_dirty } => {
                self.is_dirty = *is_dirty;
            }
            KernelEvent::ScriptSaved { .. } => {}
            KernelEvent::ScriptError { message } => {
                self.last_script_error = Some(message.clone());
            }
            // The kernel owns the viewer binding: it may clear it when the
            // target is deleted, or bind it when it creates a physical Viewer.
            // If the matching frame raced ahead of this event, replay it so the
            // viewer never sits empty until the next render.
            KernelEvent::ViewerChanged(node) => {
                self.viewer.bind(*node);
                if let Some(frame) = &self.last_frame {
                    if frame.node == *node {
                        self.viewer.set_frame(frame);
                    }
                }
            }
            KernelEvent::CommandFailed { message, .. } => {
                self.failure = Some(format!("command failed: {message}"));
            }
            _ => {}
        }
        self.graph.apply_event(&event);
    }

    /// Blocks (with a deadline) until the viewer delivers a frame from `node`.
    fn wait_for_frame(&mut self, node: NodeId, timeout: Duration) -> Result<Arc<Packet>, AppError> {
        let deadline = Instant::now() + timeout;

        loop {
            while let Ok(event) = self.kernel.events().try_recv() {
                self.fold_event(event);
            }
            while let Ok(frame) = self.kernel.frames().try_recv() {
                let matches = frame.node == Some(node);
                let packet = Arc::clone(&frame.packet);
                self.viewer.set_frame(&frame);
                self.last_frame = Some(frame);
                if matches {
                    return Ok(packet);
                }
            }
            if Instant::now() >= deadline {
                return Err(check_failed(format!(
                    "timed out waiting for a frame from #{}",
                    node.0
                )));
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Switches the viewer paradigm, enforcing the Nuke binding rule.
    ///
    /// Entering Nuke mode may only bind a physical `Viewer`. If the Fusion
    /// target was a plain node (Constant, Grade, ...) the binding is rerouted
    /// to an existing `Viewer`, or cleared so the panel goes blank instead of
    /// pretending a non-Viewer is the Nuke viewer.
    fn set_workflow(&mut self, workflow: ViewerWorkflow) {
        if self.workflow == workflow {
            return;
        }
        let previous = self.workflow;
        self.workflow = workflow;
        if previous == ViewerWorkflow::Fusion && workflow == ViewerWorkflow::Nuke {
            let message =
                tanova_ui::nuke_workflow_binding_message(&self.graph, self.viewer.viewer_node());
            self.dispatch(vec![message]);
        }
    }

    /// Applies global undo/redo shortcuts plus canvas copy/paste/duplicate.
    ///
    /// Text fields keep their own editing shortcuts, and an open canvas quick
    /// search owns the keyboard, so both suppress the global binding.
    ///
    /// Both `egui::Event::Copy` / `egui::Event::Paste` (emitted by eframe on
    /// Linux for `Ctrl+C` / `Ctrl+V`) and plain `Key::C` / `Key::V` with
    /// Ctrl/Command modifiers are handled, so copy/paste works on every
    /// backend.
    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        if ctx.egui_wants_keyboard_input()
            || self.canvas.is_search_open()
            || self.file_modal.is_some()
            || self.pending_unsaved.is_some()
        {
            return;
        }

        let mut messages = Vec::new();
        let mut do_copy = false;
        let mut do_paste = false;
        let mut do_duplicate = false;

        // Consume undo/redo key events first.
        ctx.input(|input| {
            for event in &input.events {
                if let egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } = event
                {
                    if !(modifiers.command || modifiers.ctrl) {
                        continue;
                    }
                    match key {
                        egui::Key::Z => {
                            if modifiers.shift {
                                messages.push(KernelMessage::Redo);
                            } else {
                                messages.push(KernelMessage::Undo);
                            }
                        }
                        egui::Key::Y => messages.push(KernelMessage::Redo),
                        _ => {}
                    }
                }
            }
        });

        // --- Copy: Ctrl+C / Cmd+C, either as a key event or as egui's
        // --- native `Event::Copy` (Linux eframe translates Ctrl+C into this).
        let copy_triggered = ctx.input_mut(|i| {
            let key_copy = i.consume_key(egui::Modifiers::COMMAND, egui::Key::C)
                || i.consume_key(egui::Modifiers::CTRL, egui::Key::C);
            let mut event_copy = false;
            i.events.retain(|e| {
                if matches!(e, egui::Event::Copy) {
                    event_copy = true;
                    false
                } else {
                    true
                }
            });
            key_copy || event_copy
        });

        if copy_triggered {
            do_copy = true;
        }

        // --- Paste: Ctrl+V / Cmd+V, either as a key event or as egui's
        // --- native `Event::Paste(String)` (Linux eframe translates Ctrl+V
        // --- into this). Drain any `Event::Paste` from the queue so it is
        // --- not processed twice.
        let pasted_event_text = ctx.input_mut(|i| {
            let mut pasted = None;
            i.events.retain(|e| {
                if let egui::Event::Paste(s) = e {
                    if pasted.is_none() {
                        pasted = Some(s.clone());
                    }
                    false
                } else {
                    true
                }
            });
            pasted
        });
        let key_v_pressed = ctx.input_mut(|i| {
            i.consume_key(egui::Modifiers::COMMAND, egui::Key::V)
                || i.consume_key(egui::Modifiers::CTRL, egui::Key::V)
        });

        if pasted_event_text.is_some() || key_v_pressed {
            do_paste = true;
        }

        // --- Duplicate: Ctrl+D / Cmd+D. Checked here so it never collides
        // --- with the canvas `D` bypass toggle.
        let key_d_pressed = ctx.input_mut(|i| {
            i.consume_key(egui::Modifiers::COMMAND, egui::Key::D)
                || i.consume_key(egui::Modifiers::CTRL, egui::Key::D)
        });
        if key_d_pressed {
            do_duplicate = true;
        }

        if do_copy {
            if let Some(tcl) = self.export_selection_to_tcl() {
                self.write_clipboard_text(ctx, tcl);
                self.paste_cascade = 0;
            }
        }
        if do_paste {
            // Resolve the TCL string to paste: prefer the non-empty native
            // `Event::Paste` text, otherwise fall back to the in-app / test
            // clipboard (which holds whatever was last copied inside Tanova).
            let tcl = pasted_event_text
                .filter(|s| !s.trim().is_empty())
                .or_else(|| self.read_clipboard_text());
            if let Some(tcl) = tcl {
                self.paste_cascade += 1;
                let off = 40.0 * self.paste_cascade as f32;
                let _ = self.paste_tcl_snippet(&tcl, (off, off));
            }
        }
        if do_duplicate {
            if let Some(tcl) = self.export_selection_to_tcl() {
                let _ = self.paste_tcl_snippet(&tcl, (40.0, 40.0));
            }
        }

        self.dispatch(messages);
    }

    /// Sends every message the UI asked for, syncing viewer state first.
    ///
    /// `SetKnob` and `MoveNode` commands are optimistically applied to the local
    /// `GraphMirror` before being sent to the kernel, so the UI stays in sync
    /// with zero jump-back on mouse release while the kernel round-trips.
    ///
    /// Sets `awaiting_kernel` so egui keeps repainting until the new frame
    /// arrives, eliminating visual lag on undo/redo and drag release.
    fn dispatch(&mut self, messages: Vec<KernelMessage>) {
        if !messages.is_empty() {
            self.awaiting_kernel = true;
        }
        for message in messages {
            if let KernelMessage::Execute(_) = &message {
                self.is_dirty = true;
            }
            if let KernelMessage::SetViewerNode(node) = &message {
                self.viewer.bind(*node);
            }
            if let KernelMessage::Execute(command) = &message {
                let optimistic = command.clone();
                self.apply_optimistic_mirror_update(&optimistic);
            }
            if let Err(error) = self.kernel.send(message) {
                self.failure = Some(format!("kernel send failed: {error}"));
            }
        }
    }

    /// Optimistically applies a command's effect to the local `GraphMirror`.
    ///
    /// Handles `SetKnob`, `MoveNode`, and `Batch` recursively so that the UI
    /// mirrors knob edits and node moves immediately, before the kernel
    /// round-trips.
    fn apply_optimistic_mirror_update(&mut self, command: &UndoableCommand) {
        match command {
            UndoableCommand::Batch(cmds) => {
                for cmd in cmds {
                    self.apply_optimistic_mirror_update(cmd);
                }
            }
            UndoableCommand::SetKnob { .. }
            | UndoableCommand::MoveNode { .. }
            | UndoableCommand::SetDisabled { .. }
            | UndoableCommand::AddNode { .. }
            | UndoableCommand::ConnectEdge { .. } => {
                self.graph.apply_command(command);
            }
            _ => {}
        }
    }

    /// Advances playback and asks the kernel for the new frame.
    fn advance_timeline(&mut self) {
        if !self.timeline.playing {
            return;
        }

        self.timeline.current += 1.0;
        if self.timeline.current > self.timeline.end as f64 {
            self.timeline.current = self.timeline.start as f64;
        }

        if let Err(error) = self
            .kernel
            .send(KernelMessage::SetTime(self.timeline.current))
        {
            self.failure = Some(format!("kernel send failed: {error}"));
        }
    }

    fn shutdown(self) {
        self.kernel.shutdown();
    }

    /// Starts a fresh empty script: clears the kernel DAG, mirror, canvas, and
    /// viewer, and resets the dirty state.
    pub fn execute_new_script(&mut self) {
        let _ = self.kernel.send(KernelMessage::NewScript);
        self.graph.clear();
        self.canvas.reset_for_script(&self.graph);
        self.viewer.clear_frame();
        self.script_path = None;
        self.is_dirty = false;
        self.last_script_error = None;
    }

    /// Opens a `.nk` script from `path`, replacing the current graph. Returns
    /// an error (and records it in `last_script_error`) without altering the
    /// current graph on read or parse failure.
    pub fn execute_open_path(&mut self, path: PathBuf) -> Result<(), String> {
        let content =
            std::fs::read_to_string(&path).map_err(|e| format!("failed to read {path:?}: {e}"))?;

        let mut dag = Dag::new();
        load_nk_into_dag(&content, &mut dag)
            .map_err(|e| format!("failed to parse {path:?}: {e}"))?;

        let viewer_target = match self.workflow {
            ViewerWorkflow::Nuke => dag
                .nodes()
                .find(|node| node.kind == "Viewer")
                .map(|node| node.id),
            ViewerWorkflow::Fusion => None,
        };

        self.graph = GraphMirror::from_dag(&dag, viewer_target, self.graph.project_format());
        self.canvas.reset_for_script(&self.graph);
        self.viewer.clear_frame();
        self.script_path = Some(path.clone());
        self.is_dirty = false;
        self.last_script_error = None;

        self.kernel
            .send(KernelMessage::ReplaceDag {
                dag: Box::new(dag),
                viewer_target,
            })
            .map_err(|e| format!("kernel send failed: {e}"))?;

        Ok(())
    }

    /// Saves the current graph to `path` (appending `.tnv` when no recognized
    /// extension is present) and marks the script clean.
    pub fn execute_save_to_path(&mut self, path: PathBuf) {
        let path = ensure_script_extension(path);
        self.script_path = Some(path.clone());
        self.is_dirty = false;
        let _ = self.kernel.send(KernelMessage::SaveScript { path });
    }
}

impl eframe::App for TanovaApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.draw(ui);
    }
}

impl TanovaApp {
    /// Draws one frame of the real panel layout.
    ///
    /// Split out of the `eframe::App` impl so tests can drive the app with a
    /// plain `egui::Ui` instead of an `eframe::Frame`.
    fn draw(&mut self, ui: &mut egui::Ui) {
        self.pump();

        // Bridge Ctrl+V into egui::Event::Paste for focused text inputs
        // (the file modal's `File:` field) on backends that don't emit it
        // automatically. On the plain canvas we do not inject `Event::Paste`:
        // `Ctrl+V` on the canvas is handled directly by `handle_shortcuts`
        // via `read_clipboard_text()`, so bridging here would only risk a
        // stray text event.
        if self.file_modal.is_some() || ui.ctx().egui_wants_keyboard_input() {
            self.bridge_clipboard_paste(ui.ctx());
        }

        // Handle OS window close request (the "X" button) with the dirty
        // guard.
        if ui.ctx().input(|i| i.viewport().close_requested()) {
            self.request_close(PendingUnsavedAction::QuitApp, ui.ctx());
        }

        // Global keyboard shortcuts for file operations.
        self.handle_file_shortcuts(ui.ctx());

        self.handle_shortcuts(ui.ctx());
        self.advance_timeline();

        // Top status bar: app name, File menu, script name with dirty marker,
        // stats, viewer mode.
        egui::Panel::top("tanova_status").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.strong("Tanova");
                ui.separator();
                self.draw_file_menu(ui);
                ui.separator();

                // High-contrast project pill badge.
                let script_name = self.script_display_name();
                let badge_text = if self.is_dirty {
                    format!("{} *", script_name)
                } else {
                    script_name
                };
                let (text_color, bg_color) = if self.is_dirty {
                    (
                        egui::Color32::from_rgb(255, 200, 70),
                        egui::Color32::from_rgb(72, 48, 18),
                    )
                } else {
                    (
                        egui::Color32::from_rgb(235, 240, 248),
                        egui::Color32::from_rgb(42, 52, 68),
                    )
                };
                let hover_text = format!(
                    "{} (epoch {})",
                    self.script_path
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "Unsaved Tanova Project (.tnv)".to_string()),
                    self.epoch
                );
                let badge_response = egui::Frame::new()
                    .fill(bg_color)
                    .inner_margin(egui::Margin::symmetric(8, 4))
                    .show(ui, |ui| {
                        ui.add(egui::Label::new(
                            egui::RichText::new(&badge_text).strong().color(text_color),
                        ))
                    });
                badge_response.response.on_hover_text(hover_text);
                ui.separator();
                ui.label(format!("{} node(s)", self.graph.node_count()));
                ui.label(format!("{} wire(s)", self.graph.edge_count()));
                if self.viewer.has_frame() {
                    if let Some(raster) = self
                        .last_frame
                        .as_ref()
                        .and_then(|frame| frame.packet.get::<Raster2D>())
                    {
                        ui.separator();
                        ui.label(format!(
                            "frame {}x{}",
                            raster.format.width, raster.format.height
                        ));
                    }
                } else {
                    ui.separator();
                    ui.label("frame --");
                }
                if let Some(failure) = &self.failure {
                    ui.separator();
                    ui.colored_label(egui::Color32::from_rgb(235, 90, 90), failure);
                }
                if let Some(error) = &self.last_script_error {
                    ui.separator();
                    ui.colored_label(egui::Color32::from_rgb(235, 90, 90), error);
                }
                ui.separator();
                ui.label("Viewer Mode:");
                let mut next_workflow = self.workflow;
                ui.selectable_value(&mut next_workflow, ViewerWorkflow::Nuke, "Nuke Node");
                ui.selectable_value(&mut next_workflow, ViewerWorkflow::Fusion, "Fusion Target");
                if next_workflow != self.workflow {
                    self.set_workflow(next_workflow);
                }
            });
        });

        // Nuke layout: Viewer on top, Properties on the right, graph below.
        egui::Panel::top("tanova_viewer")
            .resizable(true)
            .default_size(400.0)
            .min_size(200.0)
            .show(ui, |ui| {
                let mut messages = self
                    .viewer
                    .show(ui, &self.graph, self.canvas.primary_selected);
                messages.extend(self.viewer.show_timeline(
                    ui,
                    self.timeline.start,
                    self.timeline.end,
                    &mut self.timeline.current,
                    &mut self.timeline.playing,
                ));
                self.dispatch(messages);
            });

        egui::Panel::right("tanova_properties")
            .resizable(true)
            .default_size(300.0)
            .min_size(250.0)
            .show(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    let messages = PropertiesPanel::show(
                        ui,
                        &self.graph,
                        self.canvas.primary_selected,
                        self.viewer.viewer_node(),
                    );
                    self.dispatch(messages);
                });
            });

        let modal_open = self.file_modal.is_some() || self.pending_unsaved.is_some();
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ui, |ui| {
                let messages = NodeCanvas::show(
                    ui,
                    &mut self.canvas,
                    &self.graph,
                    self.viewer.viewer_node(),
                    self.workflow,
                    modal_open,
                );
                self.dispatch(messages);
            });

        // File browser modal (Open / Save As).
        if self.file_modal.is_some() {
            self.draw_file_modal(ui.ctx());
        }

        // Unsaved-changes confirmation modal.
        if let Some(action) = self.pending_unsaved.clone() {
            self.draw_unsaved_changes_modal(ui.ctx(), action);
        }

        // The kernel runs on its own thread; poll it at display cadence.
        if self.awaiting_kernel {
            ui.ctx().request_repaint();
        }
        ui.ctx().request_repaint_after(Duration::from_millis(16));
    }

    /// Returns the display name for the current script: the file name, or
    /// `"Untitled.tnv"` when no path is set (with `" *"` appended when dirty).
    fn script_display_name(&self) -> String {
        self.script_path
            .as_ref()
            .and_then(|p| p.file_name().and_then(|n| n.to_str()))
            .unwrap_or("Untitled.tnv")
            .to_string()
    }

    /// File menu in the top bar.
    fn draw_file_menu(&mut self, ui: &mut egui::Ui) {
        ui.menu_button("File", |ui| {
            if ui.button("New Script (Ctrl+N)").clicked() {
                self.request_new_script(ui.ctx());
                ui.close();
            }
            if ui.button("Open Script... (Ctrl+O)").clicked() {
                self.request_open_script(ui.ctx());
                ui.close();
            }
            if ui.button("Save (Ctrl+S)").clicked() {
                match &self.script_path {
                    Some(path) => self.execute_save_to_path(path.clone()),
                    None => self.request_save_as(ui.ctx(), None),
                }
                ui.close();
            }
            if ui.button("Save As... (Ctrl+Shift+S)").clicked() {
                self.request_save_as(ui.ctx(), None);
                ui.close();
            }
            ui.separator();
            if ui.button("Quit (Ctrl+Q)").clicked() {
                self.request_close(PendingUnsavedAction::QuitApp, ui.ctx());
                ui.close();
            }
        });
    }

    /// Handles Ctrl+N / Ctrl+O / Ctrl+S / Ctrl+Shift+S / Ctrl+Q shortcuts.
    fn handle_file_shortcuts(&mut self, ctx: &egui::Context) {
        if self.file_modal.is_some() || self.pending_unsaved.is_some() {
            return;
        }
        let input = ctx.input(|i| i.clone());
        for event in &input.events {
            if let egui::Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } = event
            {
                if !(modifiers.command || modifiers.ctrl) {
                    continue;
                }
                match key {
                    egui::Key::N => self.request_new_script(ctx),
                    egui::Key::O => self.request_open_script(ctx),
                    egui::Key::S if modifiers.shift => self.request_save_as(ctx, None),
                    egui::Key::S => match &self.script_path {
                        Some(path) => self.execute_save_to_path(path.clone()),
                        None => self.request_save_as(ctx, None),
                    },
                    egui::Key::Q => self.request_close(PendingUnsavedAction::QuitApp, ctx),
                    _ => {}
                }
            }
        }
    }

    /// Called when the user triggers New Script. Respects the dirty guard.
    fn request_new_script(&mut self, _ctx: &egui::Context) {
        if self.is_dirty && !self.allow_close {
            self.pending_unsaved = Some(PendingUnsavedAction::NewScript);
        } else {
            self.execute_new_script();
        }
    }

    /// Called when the user triggers Open Script. Respects the dirty guard.
    fn request_open_script(&mut self, _ctx: &egui::Context) {
        if self.is_dirty && !self.allow_close {
            self.pending_unsaved = Some(PendingUnsavedAction::OpenScript);
        } else {
            self.open_file_modal();
        }
    }

    /// Called when the user triggers Save As. Opens the file modal in SaveAs
    /// mode.
    fn request_save_as(&mut self, _ctx: &egui::Context, follow_up: Option<PendingUnsavedAction>) {
        let directory = self
            .script_path
            .as_ref()
            .and_then(|p| p.parent().map(PathBuf::from))
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let file_input = self
            .script_path
            .as_ref()
            .and_then(|p| p.file_name().and_then(|n| n.to_str()))
            .unwrap_or("Untitled.tnv")
            .to_string();
        self.file_modal = Some(FileModalState::SaveAs {
            directory,
            file_input,
            follow_up,
            just_opened: true,
        });
    }

    /// Opens the file modal in Open mode.
    fn open_file_modal(&mut self) {
        let directory = self
            .script_path
            .as_ref()
            .and_then(|p| p.parent().map(PathBuf::from))
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        self.file_modal = Some(FileModalState::Open {
            directory,
            file_input: String::new(),
            just_opened: true,
        });
    }

    /// Called when the user triggers Quit or the OS window close. Respects the
    /// dirty guard.
    fn request_close(&mut self, action: PendingUnsavedAction, ctx: &egui::Context) {
        if self.is_dirty && !self.allow_close {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.pending_unsaved = Some(action);
        } else if matches!(action, PendingUnsavedAction::QuitApp) {
            self.allow_close = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    /// Renders the in-app file browser modal using `take()` + `ModalOutcome`.
    ///
    /// `self.file_modal` is set to `None` at the start of the frame and only
    /// restored to `Some` when the outcome is `KeepOpen`, so closing and
    /// chained follow-up modals never collide.
    fn draw_file_modal(&mut self, ctx: &egui::Context) {
        let Some(mut modal) = self.file_modal.take() else {
            return;
        };
        let is_save_as = matches!(modal, FileModalState::SaveAs { .. });
        let title = if is_save_as { "Save As" } else { "Open Script" };

        let mut window_open = true;
        let mut outcome = ModalOutcome::KeepOpen;

        // Extract follow_up from SaveAs mode before the window borrows modal.
        // Clone so modal retains it for the KeepOpen case.
        let follow_up: Option<PendingUnsavedAction> = match &modal {
            FileModalState::SaveAs { follow_up, .. } => follow_up.clone(),
            _ => None,
        };

        // Snapshot any pre-existing error so the modal can display it.
        let last_script_error = self.last_script_error.clone();

        egui::Window::new(title)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .collapsible(false)
            .resizable(false)
            .default_width(420.0)
            .open(&mut window_open)
            .show(ctx, |ui| {
                match &mut modal {
                    FileModalState::Open {
                        directory,
                        file_input,
                        just_opened,
                    }
                    | FileModalState::SaveAs {
                        directory,
                        file_input,
                        just_opened,
                        ..
                    } => {
                        ui.label(format!("Directory: {}", directory.display()));
                        ui.separator();

                        // Read directory entries, skipping hidden dot-entries.
                        let mut entries: Vec<std::path::PathBuf> =
                            std::fs::read_dir(&mut *directory)
                                .ok()
                                .into_iter()
                                .flatten()
                                .filter_map(|e| e.ok())
                                .map(|e| e.path())
                                .collect();
                        entries.retain(|p| {
                            p.file_name()
                                .and_then(|n| n.to_str())
                                .is_some_and(|n| !n.starts_with('.'))
                        });

                        // Parent Directory button.
                        if directory.parent().is_some_and(|parent| parent != directory) {
                            if ui.button("[..] Parent Directory").clicked() {
                                if let Some(parent) = directory.parent() {
                                    *directory = parent.to_path_buf();
                                    self.last_script_error = None;
                                }
                                outcome = ModalOutcome::KeepOpen;
                            }
                            ui.separator();
                        }

                        egui::ScrollArea::vertical()
                            .max_height(300.0)
                            .show(ui, |ui| {
                                let mut sorted: Vec<_> = entries;
                                sorted.sort_by_key(|p| {
                                    p.file_name().unwrap_or_default().to_os_string()
                                });
                                for path in sorted {
                                    let name = path
                                        .file_name()
                                        .and_then(|n| n.to_str())
                                        .unwrap_or("")
                                        .to_string();

                                    if path.is_dir() {
                                        if ui.button(format!("[DIR] {}", name)).clicked() {
                                            *directory = path.clone();
                                            self.last_script_error = None;
                                            // Do not overwrite file_input with a
                                            // folder name in SaveAs mode.
                                            outcome = ModalOutcome::KeepOpen;
                                        }
                                    } else if is_supported_script_file(&path) {
                                        let label = if path
                                            .extension()
                                            .is_some_and(|e| e.eq_ignore_ascii_case("tnv"))
                                        {
                                            format!("[TNV] {}", name)
                                        } else {
                                            format!("[NK] {}", name)
                                        };
                                        let resp = ui.button(label);
                                        if resp.clicked() {
                                            *file_input = name;
                                            self.last_script_error = None;
                                            outcome = ModalOutcome::KeepOpen;
                                        }
                                        // Double-click immediately opens in Open mode.
                                        if !is_save_as && resp.double_clicked() {
                                            outcome =
                                                ModalOutcome::ConfirmOpen { path: path.clone() };
                                        }
                                    }
                                }
                            });

                        ui.separator();
                        ui.horizontal(|ui| {
                            ui.label("File:");
                            let text_edit_resp =
                                ui.add(egui::TextEdit::singleline(file_input).desired_width(200.0));
                            // Auto-focus and select-all on first frame so
                            // typing or Ctrl+V immediately replaces the
                            // default filename.
                            if *just_opened {
                                text_edit_resp.request_focus();
                                if let Some(mut state) =
                                    egui::TextEdit::load_state(ctx, text_edit_resp.id)
                                {
                                    let ccursor_range = egui::text::CCursorRange::two(
                                        egui::text::CCursor::new(0),
                                        egui::text::CCursor::new(file_input.chars().count()),
                                    );
                                    state.cursor.set_char_range(Some(ccursor_range));
                                    state.store(ctx, text_edit_resp.id);
                                }
                                *just_opened = false;
                            }
                            if text_edit_resp.lost_focus()
                                || ui.input(|i| i.key_pressed(egui::Key::Enter))
                            {
                                // The user typed a path/name directly.
                            }
                            // [Paste] button beside the File: input.
                            if ui
                                .button("[Paste]")
                                .on_hover_text("Paste filename from clipboard")
                                .clicked()
                            {
                                if let Some(pasted) = self.read_clipboard_text() {
                                    *file_input = pasted.trim().to_string();
                                }
                            }
                        });

                        // Sanitize pasted newlines so they never corrupt a
                        // filename.
                        file_input.retain(|c| c != '\n' && c != '\r');

                        ui.separator();
                        ui.horizontal(|ui| {
                            // Confirm button.
                            let confirm_clicked = if is_save_as {
                                ui.button("Save").clicked()
                            } else {
                                ui.button("Open").clicked()
                            };
                            let enter_pressed = ui.input(|i| i.key_pressed(egui::Key::Enter))
                                && !file_input.trim().is_empty();
                            if confirm_clicked || enter_pressed {
                                let trimmed = file_input.trim();
                                let full_path = if PathBuf::from(trimmed).is_relative() {
                                    directory.join(trimmed)
                                } else {
                                    PathBuf::from(trimmed)
                                };
                                if is_save_as {
                                    outcome = ModalOutcome::ConfirmSave {
                                        path: full_path,
                                        follow_up,
                                    };
                                } else {
                                    outcome = ModalOutcome::ConfirmOpen { path: full_path };
                                }
                            }
                            if ui.button("Cancel").clicked() {
                                outcome = ModalOutcome::Cancel;
                            }
                        });

                        // Display any script error inside the modal.
                        if let Some(err) = &last_script_error {
                            ui.separator();
                            ui.colored_label(
                                egui::Color32::from_rgb(235, 90, 90),
                                format!("Error: {err}"),
                            );
                        }
                    }
                }

                // Escape key closes the modal.
                if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
                    outcome = ModalOutcome::Cancel;
                }
            });

        // If the window X was clicked and no confirm/cancel happened, cancel.
        if !window_open && matches!(outcome, ModalOutcome::KeepOpen) {
            outcome = ModalOutcome::Cancel;
        }

        match outcome {
            ModalOutcome::KeepOpen => {
                self.file_modal = Some(modal);
            }
            ModalOutcome::Cancel => {
                // Leave self.file_modal = None.
                self.last_script_error = None;
            }
            ModalOutcome::ConfirmSave { path, follow_up } => {
                self.execute_save_to_path(path);
                if let Some(action) = follow_up {
                    match action {
                        PendingUnsavedAction::NewScript => self.execute_new_script(),
                        PendingUnsavedAction::OpenScript => self.open_file_modal(),
                        PendingUnsavedAction::QuitApp => {
                            self.allow_close = true;
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    }
                }
            }
            ModalOutcome::ConfirmOpen { path } => {
                if self.execute_open_path(path).is_err() {
                    self.file_modal = Some(modal);
                }
            }
        }
    }

    /// Renders the unsaved-changes confirmation modal.
    fn draw_unsaved_changes_modal(&mut self, ctx: &egui::Context, action: PendingUnsavedAction) {
        let script_name = self.script_display_name();
        let mut close = false;
        let mut do_save = false;
        let mut do_discard = false;

        egui::Window::new("Unsaved Changes")
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.label(format!("Save changes to {script_name} before continuing?"));
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() {
                        do_save = true;
                        close = true;
                    }
                    if ui.button("Discard Changes").clicked() {
                        do_discard = true;
                        close = true;
                    }
                    if ui.button("Cancel").clicked() {
                        self.pending_unsaved = None;
                        close = true;
                    }
                });
            });

        if close {
            self.pending_unsaved = None;
        }
        if do_save {
            match &self.script_path {
                Some(path) => {
                    self.execute_save_to_path(path.clone());
                    // Complete the pending action after save.
                    match action.clone() {
                        PendingUnsavedAction::NewScript => self.execute_new_script(),
                        PendingUnsavedAction::OpenScript => self.open_file_modal(),
                        PendingUnsavedAction::QuitApp => {
                            self.allow_close = true;
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    }
                }
                None => {
                    // Save As first, then complete the action.
                    self.request_save_as(ctx, Some(action.clone()));
                }
            }
        }
        if do_discard {
            match action {
                PendingUnsavedAction::NewScript => self.execute_new_script(),
                PendingUnsavedAction::OpenScript => self.open_file_modal(),
                PendingUnsavedAction::QuitApp => {
                    self.allow_close = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
    }

    /// Reads clipboard text, preferring the test override when set.
    ///
    /// Resolution order: test override, live OS clipboard, in-app fallback.
    pub fn read_clipboard_text(&mut self) -> Option<String> {
        if let Some(ref s) = self.test_clipboard {
            return Some(s.clone());
        }
        if let Some(ref mut cb) = self.os_clipboard {
            if let Ok(s) = cb.get_text() {
                if !s.trim().is_empty() {
                    return Some(s);
                }
            }
        }
        self.internal_clipboard.clone()
    }

    /// Writes clipboard text, preferring the test override when set.
    ///
    /// Keeps the OS `arboard::Clipboard` alive on `self` across frames so
    /// X11/Wayland never drops clipboard ownership when a temporary
    /// `Clipboard` is created and dropped. Also notifies `egui` via
    /// `ctx.copy_text()` so the native window clipboard stays synchronized.
    pub fn write_clipboard_text(&mut self, ctx: &egui::Context, text: String) {
        self.internal_clipboard = Some(text.clone());
        if self.test_clipboard.is_some() {
            self.test_clipboard = Some(text.clone());
        }
        if let Some(ref mut cb) = self.os_clipboard {
            let _ = cb.set_text(text.clone());
        }
        ctx.copy_text(text);
    }

    /// Bridges Ctrl+V into `egui::Event::Paste` on backends (e.g. Linux
    /// X11/Wayland) that don't emit `Event::Paste` automatically for `Ctrl+V`.
    ///
    /// Called at the top of `draw()` before any panel or modal so the paste
    /// reaches the focused text input. We only inject `Event::Paste` when it
    /// is absent from the event queue; `egui::TextEdit` does not consume
    /// `Event::Paste`, so injecting both an event and manually mutating the
    /// text would double-paste.
    pub fn bridge_clipboard_paste(&mut self, ctx: &egui::Context) {
        let should_paste = ctx.input(|i| {
            i.events.iter().any(|e| {
                matches!(
                    e,
                    egui::Event::Key {
                        key: egui::Key::V,
                        pressed: true,
                        modifiers,
                        ..
                    } if modifiers.ctrl || modifiers.command
                )
            }) && !i.events.iter().any(|e| matches!(e, egui::Event::Paste(_)))
        });
        if should_paste {
            if let Some(text) = self.read_clipboard_text() {
                if !text.is_empty() {
                    ctx.input_mut(|i| i.events.push(egui::Event::Paste(text)));
                }
            }
        }
    }

    /// Serialises the currently selected sub-graph (excluding `Viewer`) to a
    /// `.tk`/`.nk` TCL snippet suitable for the clipboard.
    ///
    /// Selected nodes are written at their live canvas positions with their
    /// edited knobs and bypass state; only edges whose both endpoints are
    /// selected are preserved, so pasting reproduces a self-contained chunk
    /// of the graph.
    pub fn export_selection_to_tcl(&self) -> Option<String> {
        let mut ids: Vec<NodeId> = self.canvas.selected_nodes.iter().copied().collect();
        ids.sort();
        // The physical Viewer is a binding, never part of a copied sub-graph.
        let ids: Vec<NodeId> = ids
            .into_iter()
            .filter(|id| {
                self.graph
                    .node(*id)
                    .is_none_or(|node| node.kind != "Viewer")
            })
            .collect();
        if ids.is_empty() {
            return None;
        }
        let selected: HashSet<NodeId> = ids.iter().copied().collect();

        let mut temp_dag = Dag::new();
        for id in &ids {
            let node = self.graph.node(*id)?;
            temp_dag
                .add_node_at(*id, node.kind.clone(), node.x, node.y)
                .ok()?;
            for (name, value) in node.knobs.iter() {
                temp_dag
                    .set_knob(*id, name.to_string(), (*value).clone())
                    .ok()?;
            }
            if node.disabled {
                temp_dag.set_disabled(*id, true).ok()?;
            }
        }
        for edge in self.graph.edges() {
            if selected.contains(&edge.from_node) && selected.contains(&edge.to_node) {
                let _ = temp_dag.connect_edge(Edge::new(
                    edge.from_node,
                    edge.from_port,
                    edge.to_node,
                    edge.to_port,
                ));
            }
        }
        Some(temp_dag.export_to_tcl())
    }

    /// Parses a TCL snippet, remaps its `NodeId`s to fresh never-reused ids,
    /// applies it as one atomic `UndoableCommand::Batch`, and selects the
    /// newly created nodes.
    ///
    /// `offset` shifts every pasted node so repeated pastes cascade down the
    /// canvas instead of stacking exactly on top of each other.
    pub fn paste_tcl_snippet(
        &mut self,
        tcl: &str,
        offset: (f32, f32),
    ) -> Result<Vec<NodeId>, String> {
        let mut clip_dag = Dag::new();
        tanova_parse::load_nk_into_dag(tcl, &mut clip_dag).map_err(|e| e.to_string())?;

        let mut clip_nodes: Vec<&tanova_kernel::DagNode> = clip_dag
            .nodes()
            .filter(|node| node.kind != "Viewer" && node.kind != "Root")
            .collect();
        clip_nodes.sort_by_key(|node| node.id);
        if clip_nodes.is_empty() {
            return Ok(Vec::new());
        }

        // Never reuse an id the mirror still knows about.
        let mirror_max = self
            .graph
            .nodes()
            .iter()
            .map(|node| node.id.0)
            .max()
            .unwrap_or(0);
        self.canvas.sync_next_node_id(mirror_max + 1);

        let mut id_map: HashMap<NodeId, NodeId> = HashMap::new();
        let mut cmds: Vec<UndoableCommand> = Vec::new();
        let mut new_ids: Vec<NodeId> = Vec::new();

        for old_node in clip_nodes {
            let new_id = self.canvas.allocate_node_id();
            id_map.insert(old_node.id, new_id);
            new_ids.push(new_id);

            cmds.push(UndoableCommand::AddNode {
                id: new_id,
                kind: old_node.kind.clone(),
                x: old_node.x + offset.0,
                y: old_node.y + offset.1,
            });

            let defaults = find_cartridge(&old_node.kind)
                .map(|descriptor| (descriptor.default_knobs)())
                .unwrap_or_default();
            for (knob_name, knob_val) in old_node.knobs.iter() {
                if knob_name == "name" {
                    continue;
                }
                let previous = defaults
                    .get(knob_name)
                    .cloned()
                    .unwrap_or(KnobValue::Float(0.0));
                cmds.push(UndoableCommand::SetKnob {
                    node: new_id,
                    knob: knob_name.to_string(),
                    value: (*knob_val).clone(),
                    previous,
                });
            }
            if old_node.disabled {
                cmds.push(UndoableCommand::SetDisabled {
                    node: new_id,
                    disabled: true,
                });
            }
        }

        for edge in clip_dag.edges() {
            let Some((&new_from, &new_to)) =
                id_map.get(&edge.from_node).zip(id_map.get(&edge.to_node))
            else {
                continue;
            };
            cmds.push(UndoableCommand::ConnectEdge {
                from_node: new_from,
                from_port: edge.from_port,
                to_node: new_to,
                to_port: edge.to_port,
            });
        }

        if let Some(batch_cmd) = UndoableCommand::from_vec(cmds) {
            self.dispatch(vec![KernelMessage::Execute(batch_cmd)]);
        }

        self.canvas.selected_nodes = new_ids.iter().copied().collect();
        self.canvas.primary_selected = new_ids.first().copied();

        Ok(new_ids)
    }
}

fn kernel_error(error: std::io::Error) -> AppError {
    Box::new(error)
}

fn check_failed(message: impl Into<String>) -> AppError {
    Box::new(std::io::Error::other(message.into()))
}

/// Ensures a script path carries a `.tnv` or `.nk` extension, preserving any
/// existing version-style suffixes (e.g. `shot_010.v01` -> `shot_010.v01.tnv`).
///
/// `Path::set_extension` would *replace* an existing extension like `.v01`,
/// so we instead append `.tnv` to the file stem when no recognized extension
/// is present.
pub fn ensure_script_extension(path: PathBuf) -> PathBuf {
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) if ext.eq_ignore_ascii_case("tnv") || ext.eq_ignore_ascii_case("nk") => path,
        _ => {
            let mut p = path;
            let file_name = p
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            p.set_file_name(format!("{}.tnv", file_name));
            p
        }
    }
}

/// Returns `true` when `path` has a `.tnv` or `.nk` extension (case-insensitive).
pub fn is_supported_script_file(path: &std::path::Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some(ext) if ext.eq_ignore_ascii_case("tnv") || ext.eq_ignore_ascii_case("nk")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Modifiers, PointerButton, Pos2, Rect, Vec2};

    fn ctrl() -> egui::Modifiers {
        egui::Modifiers {
            ctrl: true,
            command: true,
            ..Default::default()
        }
    }

    fn ctrl_shift() -> egui::Modifiers {
        egui::Modifiers {
            ctrl: true,
            command: true,
            shift: true,
            ..Default::default()
        }
    }

    #[test]
    fn ctrl_z_undoes_and_redo_shortcuts_redo() {
        assert!(matches!(
            shortcut_message(ctrl(), egui::Key::Z),
            Some(KernelMessage::Undo)
        ));
        assert!(matches!(
            shortcut_message(ctrl_shift(), egui::Key::Z),
            Some(KernelMessage::Redo)
        ));
        assert!(matches!(
            shortcut_message(ctrl(), egui::Key::Y),
            Some(KernelMessage::Redo)
        ));
    }

    #[test]
    fn plain_keys_never_undo_or_redo() {
        assert!(shortcut_message(egui::Modifiers::NONE, egui::Key::Z).is_none());
        assert!(shortcut_message(egui::Modifiers::NONE, egui::Key::Y).is_none());
        assert!(shortcut_message(egui::Modifiers::SHIFT, egui::Key::Z).is_none());
    }

    /// End-to-end test: Constant(blue) -> Merge2.B, Constant(red) ->
    /// Transform(scale=0.5) -> Merge2.A -> Viewer.
    /// End-to-end test: Constant(blue) -> Merge2.B, Constant(red) ->
    /// Transform(scale=0.5) -> Merge2.A -> Viewer.
    ///
    /// Verifies that the rendered DisplayFrame has a 128x128 red square at
    /// [0..128, 0..128] and blue everywhere else.
    #[test]
    fn merge_transform_display_frame_is_256x256_with_128x128_red_square() {
        use tanova_kernel::{KernelMessage, UndoableCommand};
        use tanova_render::Raster2D;

        let kernel = KernelHandle::spawn().expect("kernel thread should spawn");

        let mut context = EvalContext::new();
        context.insert(ProjectFormat::new(256, 256));

        kernel
            .send(KernelMessage::SetBaseContext(context))
            .expect("set base context");

        // Constant #1 (blue, 256x256 via project format).
        kernel
            .send(KernelMessage::Execute(UndoableCommand::AddNode {
                id: CONSTANT,
                kind: "Constant".to_string(),
                x: 40.0,
                y: 40.0,
            }))
            .expect("add Constant #1 (blue)");

        // Constant #1 is blue.
        kernel
            .send(KernelMessage::Execute(UndoableCommand::SetKnob {
                node: CONSTANT,
                knob: "color".to_string(),
                value: KnobValue::Color([0.0, 0.0, 1.0, 1.0]),
                previous: KnobValue::Color([0.0, 0.0, 0.0, 1.0]),
            }))
            .expect("set Constant #1 color to blue");

        // Viewer #3 (passthrough render target).
        kernel
            .send(KernelMessage::Execute(UndoableCommand::AddNode {
                id: VIEWER,
                kind: "Viewer".to_string(),
                x: 40.0,
                y: 288.0,
            }))
            .expect("add Viewer #3");

        // Build the graph exactly as the GUI would.
        kernel
            .send(KernelMessage::Execute(UndoableCommand::AddNode {
                id: NodeId(4),
                kind: "Merge2".to_string(),
                x: 40.0,
                y: 40.0,
            }))
            .expect("add Merge2");
        kernel
            .send(KernelMessage::Execute(UndoableCommand::AddNode {
                id: NodeId(5),
                kind: "Constant".to_string(),
                x: 40.0,
                y: 164.0,
            }))
            .expect("add Constant #5 (red)");
        kernel
            .send(KernelMessage::Execute(UndoableCommand::AddNode {
                id: NodeId(6),
                kind: "Transform".to_string(),
                x: 40.0,
                y: 288.0,
            }))
            .expect("add Transform #6");

        // Constant #5 is red.
        kernel
            .send(KernelMessage::Execute(UndoableCommand::SetKnob {
                node: NodeId(5),
                knob: "color".to_string(),
                value: KnobValue::Color([1.0, 0.0, 0.0, 1.0]),
                previous: KnobValue::Color([0.0, 0.0, 0.0, 1.0]),
            }))
            .expect("set Constant #5 color to red");

        // Transform #6 scale = 0.5.
        kernel
            .send(KernelMessage::Execute(UndoableCommand::SetKnob {
                node: NodeId(6),
                knob: "scale".to_string(),
                value: KnobValue::Float(0.5),
                previous: KnobValue::Float(1.0),
            }))
            .expect("set Transform #6 scale to 0.5");

        // Wire: Constant #1 (blue) -> Merge2 port 0 (B)
        kernel
            .send(KernelMessage::Execute(UndoableCommand::ConnectEdge {
                from_node: CONSTANT,
                from_port: PortId(0),
                to_node: NodeId(4),
                to_port: PortId(0),
            }))
            .expect("connect Constant #1 -> Merge2 B");

        // Wire: Constant #5 (red) -> Transform #6 port 0
        kernel
            .send(KernelMessage::Execute(UndoableCommand::ConnectEdge {
                from_node: NodeId(5),
                from_port: PortId(0),
                to_node: NodeId(6),
                to_port: PortId(0),
            }))
            .expect("connect Constant #5 -> Transform");

        // Wire: Transform #6 -> Merge2 port 1 (A)
        kernel
            .send(KernelMessage::Execute(UndoableCommand::ConnectEdge {
                from_node: NodeId(6),
                from_port: PortId(0),
                to_node: NodeId(4),
                to_port: PortId(1),
            }))
            .expect("connect Transform -> Merge2 A");

        // Wire: Merge2 -> Viewer #3 port 0
        kernel
            .send(KernelMessage::Execute(UndoableCommand::ConnectEdge {
                from_node: NodeId(4),
                from_port: PortId(0),
                to_node: VIEWER,
                to_port: PortId(0),
            }))
            .expect("connect Merge2 -> Viewer");

        // Bind the viewer.
        kernel
            .send(KernelMessage::SetViewerNode(Some(VIEWER)))
            .expect("set viewer");

        // Wait for the frame.
        let frame = kernel
            .frames()
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("should receive a frame from the viewer");

        let raster = frame
            .packet
            .get::<Raster2D>()
            .expect("frame should carry a Raster2D");

        // The raster should be 256x256 (project format).
        assert_eq!(
            raster.format.width, 256,
            "raster width should be 256, got {}",
            raster.format.width
        );
        assert_eq!(
            raster.format.height, 256,
            "raster height should be 256, got {}",
            raster.format.height
        );

        // Pixel (64, 64) is inside [0..128, 0..128]: must be red.
        let inside = raster.sample_rgba(64, 64);
        assert!(
            inside[0] > 0.9 && inside[1] < 0.1 && inside[2] < 0.1,
            "pixel (64, 64) must be red, got {:?}",
            inside
        );

        // Pixels outside [0..128, 0..128] must be blue (from Constant #1).
        for (x, y) in [(192, 192), (192, 64), (64, 192)] {
            let outside = raster.sample_rgba(x, y);
            assert!(
                outside[2] > 0.9 && outside[0] < 0.1,
                "pixel ({x}, {y}) must be blue, got {:?}",
                outside
            );
        }

        kernel.shutdown();
    }

    /// A `Constant` spawned via `AddNode` in a `ProjectFormat(256, 256)` graph
    /// should have its `format_width`/`format_height` knobs stamped to 256/256.
    /// Editing only `format_width` to 128 keeps `format_height` at 256.
    #[test]
    fn spawned_constant_inherits_project_format_knobs() {
        use tanova_kernel::{KnobValue, UndoableCommand};
        use tanova_ui::GraphMirror;

        let mut graph = GraphMirror::default();
        graph.set_project_format(tanova_kernel::ProjectFormat::new(256, 256));

        // Spawn a new Constant node via AddNode.
        graph.apply_command(&UndoableCommand::AddNode {
            id: NodeId(10),
            kind: "Constant".to_string(),
            x: 40.0,
            y: 40.0,
        });

        // Check the GraphMirror: format_width should be 256 (from project format),
        // not the cartridge default 1920.
        let constant_node = graph.node(NodeId(10)).expect("Constant #10 should exist");
        let format_width = constant_node
            .knobs
            .get("format_width")
            .and_then(|v| match v {
                KnobValue::Int(w) => Some(w),
                _ => None,
            });
        let format_height = constant_node
            .knobs
            .get("format_height")
            .and_then(|v| match v {
                KnobValue::Int(h) => Some(h),
                _ => None,
            });
        assert_eq!(
            format_width,
            Some(&256i64),
            "format_width should be 256 from project format, got {format_width:?}"
        );
        assert_eq!(
            format_height,
            Some(&256i64),
            "format_height should be 256 from project format, got {format_height:?}"
        );

        // Now edit only format_width to 128; format_height should stay 256.
        graph.apply_command(&UndoableCommand::SetKnob {
            node: NodeId(10),
            knob: "format_width".to_string(),
            value: KnobValue::Int(128),
            previous: KnobValue::Int(256),
        });

        let constant_node = graph.node(NodeId(10)).expect("Constant #10 should exist");
        let format_width = constant_node
            .knobs
            .get("format_width")
            .and_then(|v| match v {
                KnobValue::Int(w) => Some(w),
                _ => None,
            });
        let format_height = constant_node
            .knobs
            .get("format_height")
            .and_then(|v| match v {
                KnobValue::Int(h) => Some(h),
                _ => None,
            });
        assert_eq!(
            format_width,
            Some(&128i64),
            "format_width should be 128 after edit, got {format_width:?}"
        );
        assert_eq!(
            format_height,
            Some(&256i64),
            "format_height should stay 256 when only width is edited, got {format_height:?}"
        );
    }

    /// Draining the kernel event queue into a mirror, as `TanovaApp::pump` does.
    ///
    /// Keeps folding until the queue has been quiet for a moment, which is long
    /// enough for one command's `Applied`/`Undone`/`Redone` event to land.
    fn pump_into_mirror(kernel: &KernelHandle, mirror: &mut GraphMirror) {
        while let Ok(event) = kernel
            .events()
            .recv_timeout(std::time::Duration::from_millis(100))
        {
            mirror.apply_event(&event);
        }
    }

    /// A single-node and group drag must survive undo/redo in the mirror.
    ///
    /// Two consecutive `Batch` moves inside the coalescing window collapse into
    /// one undo step, so one `Undo` returns both nodes to their start.
    #[test]
    fn single_and_group_node_drag_undo_redo_roundtrip() {
        let kernel = KernelHandle::spawn().expect("kernel thread should spawn");
        let mut mirror = GraphMirror::default();

        kernel
            .send(KernelMessage::Execute(UndoableCommand::AddNode {
                id: NodeId(1),
                kind: "Constant".to_string(),
                x: 100.0,
                y: 100.0,
            }))
            .expect("add node #1");
        kernel
            .send(KernelMessage::Execute(UndoableCommand::AddNode {
                id: NodeId(2),
                kind: "Constant".to_string(),
                x: 300.0,
                y: 300.0,
            }))
            .expect("add node #2");
        pump_into_mirror(&kernel, &mut mirror);

        let start_1 = {
            let node = mirror.node(NodeId(1)).expect("node #1 in mirror");
            (node.x, node.y)
        };
        let start_2 = {
            let node = mirror.node(NodeId(2)).expect("node #2 in mirror");
            (node.x, node.y)
        };

        // First leg: +40,+20 for both nodes.
        kernel
            .send(KernelMessage::Execute(UndoableCommand::Batch(vec![
                UndoableCommand::MoveNode {
                    id: NodeId(1),
                    from_x: start_1.0,
                    from_y: start_1.1,
                    to_x: start_1.0 + 40.0,
                    to_y: start_1.1 + 20.0,
                },
                UndoableCommand::MoveNode {
                    id: NodeId(2),
                    from_x: start_2.0,
                    from_y: start_2.1,
                    to_x: start_2.0 + 40.0,
                    to_y: start_2.1 + 20.0,
                },
            ])))
            .expect("first group move");
        pump_into_mirror(&kernel, &mut mirror);

        // Second leg: another +60,+30 for both nodes.
        kernel
            .send(KernelMessage::Execute(UndoableCommand::Batch(vec![
                UndoableCommand::MoveNode {
                    id: NodeId(1),
                    from_x: start_1.0 + 40.0,
                    from_y: start_1.1 + 20.0,
                    to_x: start_1.0 + 100.0,
                    to_y: start_1.1 + 50.0,
                },
                UndoableCommand::MoveNode {
                    id: NodeId(2),
                    from_x: start_2.0 + 40.0,
                    from_y: start_2.1 + 20.0,
                    to_x: start_2.0 + 100.0,
                    to_y: start_2.1 + 50.0,
                },
            ])))
            .expect("second group move");
        pump_into_mirror(&kernel, &mut mirror);

        // Both legs coalesced, so ONE undo restores the start positions.
        kernel.send(KernelMessage::Undo).expect("undo");
        pump_into_mirror(&kernel, &mut mirror);

        let after_undo_1 = {
            let node = mirror.node(NodeId(1)).expect("node #1 in mirror");
            (node.x, node.y)
        };
        let after_undo_2 = {
            let node = mirror.node(NodeId(2)).expect("node #2 in mirror");
            (node.x, node.y)
        };
        assert!(
            (after_undo_1.0 - start_1.0).abs() < 1e-3 && (after_undo_1.1 - start_1.1).abs() < 1e-3,
            "one undo should restore node #1 to {start_1:?}, got {after_undo_1:?}"
        );
        assert!(
            (after_undo_2.0 - start_2.0).abs() < 1e-3 && (after_undo_2.1 - start_2.1).abs() < 1e-3,
            "one undo should restore node #2 to {start_2:?}, got {after_undo_2:?}"
        );

        // Redo replays the whole coalesced gesture.
        kernel.send(KernelMessage::Redo).expect("redo");
        pump_into_mirror(&kernel, &mut mirror);

        let after_redo_1 = {
            let node = mirror.node(NodeId(1)).expect("node #1 in mirror");
            (node.x, node.y)
        };
        let after_redo_2 = {
            let node = mirror.node(NodeId(2)).expect("node #2 in mirror");
            (node.x, node.y)
        };
        assert!(
            (after_redo_1.0 - (start_1.0 + 100.0)).abs() < 1e-3
                && (after_redo_1.1 - (start_1.1 + 50.0)).abs() < 1e-3,
            "redo should move node #1 to {:?}, got {after_redo_1:?}",
            (start_1.0 + 100.0, start_1.1 + 50.0)
        );
        assert!(
            (after_redo_2.0 - (start_2.0 + 100.0)).abs() < 1e-3
                && (after_redo_2.1 - (start_2.1 + 50.0)).abs() < 1e-3,
            "redo should move node #2 to {:?}, got {after_redo_2:?}",
            (start_2.0 + 100.0, start_2.1 + 50.0)
        );

        kernel.shutdown();
    }

    /// A spawned `Constant` in a 256x256 project that has its width edited to
    /// 128 must render 128x256, not 128x1080.
    ///
    /// The kernel has to stamp the project format onto the node's knobs when it
    /// spawns; otherwise `Constant` falls back to its static 1080 height as soon
    /// as the width knob stops matching the 1920x1080 default pair.
    #[test]
    fn spawned_constant_width_edit_renders_128x256_not_128x1080() {
        let kernel = KernelHandle::spawn().expect("kernel thread should spawn");

        let mut context = EvalContext::new();
        context.insert(ProjectFormat::new(256, 256));
        kernel
            .send(KernelMessage::SetBaseContext(context))
            .expect("set base context");

        // Constant #4, spawned under the 256x256 project format.
        kernel
            .send(KernelMessage::Execute(UndoableCommand::AddNode {
                id: NodeId(4),
                kind: "Constant".to_string(),
                x: 40.0,
                y: 40.0,
            }))
            .expect("add Constant #4");

        // Viewer #3.
        kernel
            .send(KernelMessage::Execute(UndoableCommand::AddNode {
                id: VIEWER,
                kind: "Viewer".to_string(),
                x: 40.0,
                y: 288.0,
            }))
            .expect("add Viewer #3");

        // Edit only the width knob to 128.
        kernel
            .send(KernelMessage::Execute(UndoableCommand::SetKnob {
                node: NodeId(4),
                knob: "format_width".to_string(),
                value: KnobValue::Int(128),
                previous: KnobValue::Int(256),
            }))
            .expect("set Constant #4 width to 128");

        kernel
            .send(KernelMessage::Execute(UndoableCommand::ConnectEdge {
                from_node: NodeId(4),
                from_port: PortId(0),
                to_node: VIEWER,
                to_port: PortId(0),
            }))
            .expect("connect Constant #4 -> Viewer #3");

        kernel
            .send(KernelMessage::SetViewerNode(Some(VIEWER)))
            .expect("set viewer");

        let frame = kernel
            .frames()
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("should receive a frame from the viewer");

        let raster = frame
            .packet
            .get::<Raster2D>()
            .expect("frame should carry a Raster2D");

        assert_eq!(
            raster.format.width, 128,
            "width knob 128 should win, got {}",
            raster.format.width
        );
        assert_eq!(
            raster.format.height, 256,
            "height should stay at the stamped project format 256, got {}",
            raster.format.height
        );

        kernel.shutdown();
    }

    /// Drives one full `TanovaApp` frame, panels and all.
    fn run_app_frame(
        ctx: &egui::Context,
        app: &mut TanovaApp,
        screen: Rect,
        events: Vec<egui::Event>,
    ) {
        ctx.run_pass(
            egui::RawInput {
                screen_rect: Some(screen),
                events,
                ..Default::default()
            },
            |ctx| {
                ctx.root_ui(|ui| app.draw(ui));
            },
        )
        .drop_without_applying_deltas();
    }

    /// End-to-end live path: a real `NodeCanvas::show` drag inside `TanovaApp`
    /// followed by `Ctrl+Z` must move the node back on screen.
    ///
    /// This is the regression the isolated tests missed: the mirror was correct
    /// after undo, but `NodeCanvasState::positions` kept the dragged value, so
    /// the canvas never re-laid the node out.
    #[test]
    fn live_app_ui_drag_and_ctrl_z_restores_node_position() {
        tanova_nodes::ensure_linked();

        let gpu = pollster::block_on(init_gpu()).expect("GPU should initialize");
        let kernel = KernelHandle::spawn().expect("kernel thread should spawn");
        let mut app = TanovaApp::new(kernel, gpu);

        let ctx = egui::Context::default();
        let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(1280.0, 720.0));

        // Frames 1..: lay the panels out and let the starter comp land.
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && app.graph.node_count() < 3 {
            run_app_frame(&ctx, &mut app, screen, vec![]);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            app.graph.node_count(),
            3,
            "the starter comp must reach the mirror"
        );

        let start = {
            let node = app.graph.node(NodeId(1)).expect("node #1 in mirror");
            (node.x, node.y)
        };
        let canvas_rect = app
            .canvas
            .last_canvas_rect
            .expect("the canvas must have been laid out");

        // Aim at a point inside node #1's body.
        let press = app
            .canvas
            .world_to_screen(canvas_rect, Pos2::new(start.0 + 20.0, start.1 + 20.0));
        let dragged = press + egui::vec2(40.0, 30.0);

        // Frame: press on the node body.
        run_app_frame(
            &ctx,
            &mut app,
            screen,
            vec![
                egui::Event::PointerMoved(press),
                egui::Event::PointerButton {
                    pos: press,
                    button: PointerButton::Primary,
                    pressed: true,
                    modifiers: Modifiers::NONE,
                },
            ],
        );

        // Frame: drag.
        run_app_frame(
            &ctx,
            &mut app,
            screen,
            vec![egui::Event::PointerMoved(dragged)],
        );

        // Frame: release.
        run_app_frame(
            &ctx,
            &mut app,
            screen,
            vec![egui::Event::PointerButton {
                pos: dragged,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            }],
        );

        let moved = {
            let node = app.graph.node(NodeId(1)).expect("node #1 in mirror");
            (node.x, node.y)
        };
        assert!(
            (moved.0 - start.0).abs() > 1.0 || (moved.1 - start.1).abs() > 1.0,
            "the drag must move node #1 from {start:?}, still at {moved:?}"
        );

        // Frame: Ctrl+Z.
        run_app_frame(
            &ctx,
            &mut app,
            screen,
            vec![egui::Event::Key {
                key: egui::Key::Z,
                physical_key: Some(egui::Key::Z),
                pressed: true,
                repeat: false,
                modifiers: ctrl(),
            }],
        );

        // The kernel round-trips asynchronously; keep pumping until it lands.
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let node = app.graph.node(NodeId(1)).expect("node #1 in mirror");
            if (node.x - start.0).abs() < 1e-3 && (node.y - start.1).abs() < 1e-3 {
                break;
            }
            run_app_frame(&ctx, &mut app, screen, vec![]);
            std::thread::sleep(Duration::from_millis(10));
        }

        let restored = {
            let node = app.graph.node(NodeId(1)).expect("node #1 in mirror");
            (node.x, node.y)
        };
        assert!(
            (restored.0 - start.0).abs() < 1e-3 && (restored.1 - start.1).abs() < 1e-3,
            "Ctrl+Z must restore node #1 to {start:?}, got {restored:?}"
        );

        // One more frame must re-lay the canvas out from the restored mirror,
        // otherwise the node stays visually stuck where it was dropped.
        run_app_frame(&ctx, &mut app, screen, vec![]);
        let laid_out = app
            .canvas
            .node_position(NodeId(1))
            .expect("node #1 must have a canvas position");
        assert!(
            (laid_out.x - start.0).abs() < 1e-3 && (laid_out.y - start.1).abs() < 1e-3,
            "the canvas must redraw node #1 at {start:?}, got {laid_out:?}"
        );
    }

    /// End-to-end: save a modified script to .tnv, clear with New Script, then
    /// open it back and verify positions and knobs round-trip.
    #[test]
    fn end_to_end_save_new_script_and_open_tnv_round_trip() {
        tanova_nodes::ensure_linked();

        let gpu = pollster::block_on(init_gpu()).expect("GPU should initialize");
        let kernel = KernelHandle::spawn().expect("kernel thread should spawn");
        let mut app = TanovaApp::new(kernel, gpu);

        let ctx = egui::Context::default();
        let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(1280.0, 720.0));

        // Pump until starter comp is loaded.
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && app.graph.node_count() < 3 {
            run_app_frame(&ctx, &mut app, screen, vec![]);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(app.graph.node_count(), 3, "starter comp must reach mirror");
        assert!(!app.is_dirty, "fresh app must start clean");

        // Modify Grade #2: mix = 0.35, move to (320, 280).
        app.dispatch(vec![
            KernelMessage::Execute(UndoableCommand::SetKnob {
                node: GRADE,
                knob: "mix".to_string(),
                value: KnobValue::Float(0.35),
                previous: KnobValue::Float(1.0),
            }),
            KernelMessage::Execute(UndoableCommand::MoveNode {
                id: GRADE,
                from_x: 40.0,
                from_y: 164.0,
                to_x: 320.0,
                to_y: 280.0,
            }),
        ]);
        // Pump until dirty state is confirmed.
        let dirty_deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < dirty_deadline && !app.is_dirty {
            app.pump();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(app.is_dirty, "after edits, app must be dirty");

        // Save to a temp .tnv file.
        let temp_path = std::env::temp_dir().join("tanova_test_comp.tnv");
        app.execute_save_to_path(temp_path.clone());
        // Wait for the kernel to process SaveScript and write the file.
        let save_deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < save_deadline && !temp_path.exists() {
            app.pump();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(temp_path.exists(), "saved file must exist on disk");
        // Pump until clean state is confirmed.
        let clean_deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < clean_deadline && app.is_dirty {
            app.pump();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!app.is_dirty, "after save, app must be clean");

        // New Script: clears everything.
        app.execute_new_script();
        // Pump until clean state is confirmed.
        let new_deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < new_deadline && app.is_dirty {
            app.pump();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            app.graph.node_count(),
            0,
            "mirror must be empty after new script"
        );
        assert!(!app.is_dirty, "after new script, app must be clean");

        // Open the saved .tnv file.
        app.execute_open_path(temp_path.clone())
            .expect("open should succeed");
        // Pump until open is processed.
        let open_deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < open_deadline && app.graph.node_count() != 3 {
            app.pump();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(app.graph.node_count(), 3, "must load 3 nodes");

        // Verify Grade #2 position and mix knob round-trip.
        let grade = app.graph.node(GRADE).expect("Grade #2 must exist");
        assert!((grade.x - 320.0).abs() < 1e-4, "Grade x: {:?}", grade.x);
        assert!((grade.y - 280.0).abs() < 1e-4, "Grade y: {:?}", grade.y);
        assert_eq!(
            grade.knobs.get("mix"),
            Some(&KnobValue::Float(0.35)),
            "Grade mix must be 0.35"
        );
        assert!(!app.is_dirty, "after open, app must be clean");

        // Clean up.
        let _ = std::fs::remove_file(&temp_path);
    }

    /// End-to-end: save a modified script to .nk (Nuke compatibility), clear
    /// with New Script, then open it back and verify positions and knobs
    /// round-trip.
    #[test]
    fn end_to_end_save_new_script_and_open_nk_round_trip() {
        tanova_nodes::ensure_linked();

        let gpu = pollster::block_on(init_gpu()).expect("GPU should initialize");
        let kernel = KernelHandle::spawn().expect("kernel thread should spawn");
        let mut app = TanovaApp::new(kernel, gpu);

        let ctx = egui::Context::default();
        let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(1280.0, 720.0));

        // Pump until starter comp is loaded.
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && app.graph.node_count() < 3 {
            run_app_frame(&ctx, &mut app, screen, vec![]);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(app.graph.node_count(), 3, "starter comp must reach mirror");
        assert!(!app.is_dirty, "fresh app must start clean");

        // Modify Grade #2: mix = 0.35, move to (320, 280).
        app.dispatch(vec![
            KernelMessage::Execute(UndoableCommand::SetKnob {
                node: GRADE,
                knob: "mix".to_string(),
                value: KnobValue::Float(0.35),
                previous: KnobValue::Float(1.0),
            }),
            KernelMessage::Execute(UndoableCommand::MoveNode {
                id: GRADE,
                from_x: 40.0,
                from_y: 164.0,
                to_x: 320.0,
                to_y: 280.0,
            }),
        ]);
        // Pump until dirty state is confirmed.
        let dirty_deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < dirty_deadline && !app.is_dirty {
            app.pump();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(app.is_dirty, "after edits, app must be dirty");

        // Save to a temp .nk file.
        let temp_path = std::env::temp_dir().join("tanova_test_comp.nk");
        app.execute_save_to_path(temp_path.clone());
        // Wait for the kernel to process SaveScript and write the file.
        let save_deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < save_deadline && !temp_path.exists() {
            app.pump();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(temp_path.exists(), "saved file must exist on disk");
        // Pump until clean state is confirmed.
        let clean_deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < clean_deadline && app.is_dirty {
            app.pump();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!app.is_dirty, "after save, app must be clean");

        // New Script: clears everything.
        app.execute_new_script();
        // Pump until clean state is confirmed.
        let new_deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < new_deadline && app.is_dirty {
            app.pump();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            app.graph.node_count(),
            0,
            "mirror must be empty after new script"
        );
        assert!(!app.is_dirty, "after new script, app must be clean");

        // Open the saved .nk file.
        app.execute_open_path(temp_path.clone())
            .expect("open should succeed");
        // Pump until open is processed.
        let open_deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < open_deadline && app.graph.node_count() != 3 {
            app.pump();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(app.graph.node_count(), 3, "must load 3 nodes");

        // Verify Grade #2 position and mix knob round-trip.
        let grade = app.graph.node(GRADE).expect("Grade #2 must exist");
        assert!((grade.x - 320.0).abs() < 1e-4, "Grade x: {:?}", grade.x);
        assert!((grade.y - 280.0).abs() < 1e-4, "Grade y: {:?}", grade.y);
        assert_eq!(
            grade.knobs.get("mix"),
            Some(&KnobValue::Float(0.35)),
            "Grade mix must be 0.35"
        );
        assert!(!app.is_dirty, "after open, app must be clean");

        // Clean up.
        let _ = std::fs::remove_file(&temp_path);
    }

    /// Unit tests for `ensure_script_extension`.
    #[test]
    fn ensure_script_extension_appends_tnv() {
        assert_eq!(
            ensure_script_extension(std::path::PathBuf::from("my_comp")),
            std::path::PathBuf::from("my_comp.tnv")
        );
        // Preserves version-style suffixes: `.v02` is kept, `.tnv` appended.
        assert_eq!(
            ensure_script_extension(std::path::PathBuf::from("shot_010.v02")),
            std::path::PathBuf::from("shot_010.v02.tnv")
        );
        // Recognized extensions are preserved unchanged.
        assert_eq!(
            ensure_script_extension(std::path::PathBuf::from("scene.tnv")),
            std::path::PathBuf::from("scene.tnv")
        );
        assert_eq!(
            ensure_script_extension(std::path::PathBuf::from("legacy_nuke.nk")),
            std::path::PathBuf::from("legacy_nuke.nk")
        );
        // Case-insensitive extension matching.
        assert_eq!(
            ensure_script_extension(std::path::PathBuf::from("clip.TNV")),
            std::path::PathBuf::from("clip.TNV")
        );
        assert_eq!(
            ensure_script_extension(std::path::PathBuf::from("clip.NK")),
            std::path::PathBuf::from("clip.NK")
        );
    }

    /// Unit tests for `is_supported_script_file`.
    #[test]
    fn is_supported_script_file_accepts_tnv_and_nk() {
        assert!(is_supported_script_file(std::path::Path::new("foo.tnv")));
        assert!(is_supported_script_file(std::path::Path::new("foo.TNV")));
        assert!(is_supported_script_file(std::path::Path::new("foo.tNv")));
        assert!(is_supported_script_file(std::path::Path::new("foo.nk")));
        assert!(is_supported_script_file(std::path::Path::new("foo.NK")));
        assert!(is_supported_script_file(std::path::Path::new("foo.nK")));
        assert!(!is_supported_script_file(std::path::Path::new("foo.exr")));
        assert!(!is_supported_script_file(std::path::Path::new("foo.txt")));
        assert!(!is_supported_script_file(std::path::Path::new("foo")));
        assert!(!is_supported_script_file(std::path::Path::new("foo.")));
    }

    /// When the app is dirty and the user triggers Quit, the pending unsaved
    /// action is set and close is not allowed.
    #[test]
    fn unsaved_changes_close_guard_sets_pending() {
        tanova_nodes::ensure_linked();

        let gpu = pollster::block_on(init_gpu()).expect("GPU should initialize");
        let kernel = KernelHandle::spawn().expect("kernel thread should spawn");
        let mut app = TanovaApp::new(kernel, gpu);

        // Make the app dirty by editing a knob.
        app.dispatch(vec![KernelMessage::Execute(UndoableCommand::SetKnob {
            node: CONSTANT,
            knob: "color".to_string(),
            value: KnobValue::Color([1.0, 0.0, 0.0, 1.0]),
            previous: KnobValue::Color(STARTER_COLOR),
        })]);
        // Pump until dirty state is confirmed (MarkClean event from seed may
        // arrive asynchronously and reset the optimistic dirty flag).
        let dirty_deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < dirty_deadline && !app.is_dirty {
            app.pump();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(app.is_dirty, "app must be dirty after edit");

        // Trigger QuitApp while dirty.
        let ctx = egui::Context::default();
        app.request_close(PendingUnsavedAction::QuitApp, &ctx);

        assert_eq!(
            app.pending_unsaved,
            Some(PendingUnsavedAction::QuitApp),
            "pending unsaved must be set"
        );
        assert!(!app.allow_close, "app must not allow close while dirty");
    }

    /// Pressing Escape while the file modal is open closes it (`file_modal` ->
    /// `None`).
    #[test]
    fn file_modal_cancel_and_escape_close_modal() {
        tanova_nodes::ensure_linked();

        let gpu = pollster::block_on(init_gpu()).expect("GPU should initialize");
        let kernel = KernelHandle::spawn().expect("kernel thread should spawn");
        let mut app = TanovaApp::new(kernel, gpu);

        let ctx = egui::Context::default();
        let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(1280.0, 720.0));

        // Open the SaveAs modal.
        app.request_save_as(&ctx, None);
        assert!(app.file_modal.is_some(), "file modal should be open");

        // Press Escape while the modal is open.
        ctx.run_pass(
            egui::RawInput {
                screen_rect: Some(screen),
                events: vec![egui::Event::Key {
                    key: egui::Key::Escape,
                    physical_key: Some(egui::Key::Escape),
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                ..Default::default()
            },
            |ctx| {
                app.draw_file_modal(ctx);
            },
        )
        .drop_without_applying_deltas();

        assert!(
            app.file_modal.is_none(),
            "file modal should be closed after Escape"
        );
        assert!(
            app.last_script_error.is_none(),
            "error should be cleared on cancel"
        );
    }

    /// SaveAs confirm closes the modal and writes the file; Open confirm
    /// re-opens it and closes again on success.
    #[test]
    fn file_modal_save_and_open_close_modal_on_success() {
        tanova_nodes::ensure_linked();

        let gpu = pollster::block_on(init_gpu()).expect("GPU should initialize");
        let kernel = KernelHandle::spawn().expect("kernel thread should spawn");
        let mut app = TanovaApp::new(kernel, gpu);

        let ctx = egui::Context::default();
        let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(1280.0, 720.0));

        let temp_dir = std::env::temp_dir();
        let temp_path = temp_dir.join("tanova_modal_test.tnv");

        // --- SaveAs path ---
        app.request_save_as(&ctx, None);
        assert!(app.file_modal.is_some(), "SaveAs modal should be open");

        // Set the file_input to the temp path.
        if let Some(FileModalState::SaveAs { file_input, .. }) = &mut app.file_modal {
            *file_input = temp_path.to_string_lossy().to_string();
        }

        // Press Enter to confirm Save.
        ctx.run_pass(
            egui::RawInput {
                screen_rect: Some(screen),
                events: vec![egui::Event::Key {
                    key: egui::Key::Enter,
                    physical_key: Some(egui::Key::Enter),
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                ..Default::default()
            },
            |ctx| {
                app.draw_file_modal(ctx);
            },
        )
        .drop_without_applying_deltas();

        assert!(
            app.file_modal.is_none(),
            "SaveAs modal should close on successful save"
        );

        // Wait for the kernel to write the file.
        let save_deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < save_deadline && !temp_path.exists() {
            app.pump();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(temp_path.exists(), "saved .tnv file must exist on disk");

        // --- Open path ---
        app.request_open_script(&ctx);
        assert!(app.file_modal.is_some(), "Open modal should be open");

        // Set directory and file_input to the saved file.
        if let Some(FileModalState::Open {
            directory,
            file_input,
            ..
        }) = &mut app.file_modal
        {
            *directory = temp_dir.clone();
            *file_input = temp_path.file_name().unwrap().to_string_lossy().to_string();
        }

        // Press Enter to confirm Open.
        ctx.run_pass(
            egui::RawInput {
                screen_rect: Some(screen),
                events: vec![egui::Event::Key {
                    key: egui::Key::Enter,
                    physical_key: Some(egui::Key::Enter),
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                ..Default::default()
            },
            |ctx| {
                app.draw_file_modal(ctx);
            },
        )
        .drop_without_applying_deltas();

        assert!(
            app.file_modal.is_none(),
            "Open modal should close on successful open"
        );

        // Clean up.
        let _ = std::fs::remove_file(&temp_path);
    }

    /// All badge and file-browser labels are pure ASCII (no missing-glyph
    /// `▯` boxes from non-ASCII symbols).
    #[test]
    fn project_badge_and_file_modal_labels_are_pure_ascii() {
        tanova_nodes::ensure_linked();

        let gpu = pollster::block_on(init_gpu()).expect("GPU should initialize");
        let kernel = KernelHandle::spawn().expect("kernel thread should spawn");
        let mut app = TanovaApp::new(kernel, gpu);

        // Clean badge: "Untitled.tnv" is pure ASCII.
        let clean_name = app.script_display_name();
        assert_eq!(clean_name, "Untitled.tnv");
        assert!(clean_name.is_ascii());

        // Dirty badge: "Untitled.tnv *" is pure ASCII.
        app.is_dirty = true;
        let dirty_name = format!("{} *", app.script_display_name());
        assert_eq!(dirty_name, "Untitled.tnv *");
        assert!(dirty_name.is_ascii());

        // File browser labels are all pure ASCII.
        assert!("[..] Parent Directory".is_ascii());
        assert!("[DIR] some_folder".is_ascii());
        assert!("[TNV] project.tnv".is_ascii());
        assert!("[NK] legacy.nk".is_ascii());

        // `is_supported_script_file` only matches ASCII extensions.
        assert!(is_supported_script_file(std::path::Path::new("foo.tnv")));
        assert!(is_supported_script_file(std::path::Path::new("foo.nk")));
        assert!(!is_supported_script_file(std::path::Path::new("foo.exr")));
        assert!(!is_supported_script_file(std::path::Path::new("foo.txt")));
    }

    /// Modal windows are anchored to the center of the viewport, not the
    /// top-left corner, and `just_opened` is consumed on the first render.
    #[test]
    fn modals_are_centered_in_viewport() {
        tanova_nodes::ensure_linked();

        let gpu = pollster::block_on(init_gpu()).expect("GPU should initialize");
        let kernel = KernelHandle::spawn().expect("kernel thread should spawn");
        let mut app = TanovaApp::new(kernel, gpu);

        let ctx = egui::Context::default();
        let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(1280.0, 720.0));

        // Open the SaveAs modal.
        app.request_save_as(&ctx, None);
        assert!(app.file_modal.is_some(), "file modal should be open");
        if let Some(FileModalState::SaveAs { just_opened, .. }) = &app.file_modal {
            assert!(*just_opened, "just_opened should be true initially");
        }

        // Run a frame with the modal rendered.
        ctx.run_pass(
            egui::RawInput {
                screen_rect: Some(screen),
                ..Default::default()
            },
            |ctx| {
                app.draw_file_modal(ctx);
            },
        )
        .drop_without_applying_deltas();

        // just_opened should be false after the first render frame.
        if let Some(FileModalState::SaveAs { just_opened, .. }) = &app.file_modal {
            assert!(!*just_opened, "just_opened should be false after render");
        } else {
            panic!("file modal should still be open");
        }
    }

    /// Ctrl+V replaces the default filename exactly once, without double-pasting.
    #[test]
    fn ctrl_v_replaces_default_filename_once_without_double_paste() {
        tanova_nodes::ensure_linked();

        let gpu = pollster::block_on(init_gpu()).expect("GPU should initialize");
        let kernel = KernelHandle::spawn().expect("kernel thread should spawn");
        let mut app = TanovaApp::new(kernel, gpu);

        let ctx = egui::Context::default();
        let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(1280.0, 720.0));

        // Open the SaveAs modal; file_input starts as "Untitled.tnv".
        app.request_save_as(&ctx, None);
        assert!(app.file_modal.is_some(), "SaveAs modal should be open");
        if let Some(FileModalState::SaveAs { file_input, .. }) = &app.file_modal {
            assert_eq!(file_input, "Untitled.tnv");
        }

        // Set the test clipboard override.
        app.test_clipboard = Some("shot_01.tnv".to_string());

        // Frame 1: render the modal so the TextEdit acquires focus and selects
        // "Untitled.tnv".
        ctx.run_pass(
            egui::RawInput {
                screen_rect: Some(screen),
                ..Default::default()
            },
            |ctx| {
                app.draw_file_modal(ctx);
            },
        )
        .drop_without_applying_deltas();

        // Frame 2: send a raw Ctrl+V key event (without Event::Paste) through
        // the clipboard bridge and modal UI.
        ctx.run_pass(
            egui::RawInput {
                screen_rect: Some(screen),
                events: vec![egui::Event::Key {
                    key: egui::Key::V,
                    physical_key: Some(egui::Key::V),
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::CTRL,
                }],
                ..Default::default()
            },
            |ctx| {
                app.bridge_clipboard_paste(ctx);
                app.draw_file_modal(ctx);
            },
        )
        .drop_without_applying_deltas();

        // The clipboard text should have replaced "Untitled.tnv" exactly once.
        if let Some(FileModalState::SaveAs { file_input, .. }) = &app.file_modal {
            assert_eq!(
                file_input, "shot_01.tnv",
                "Ctrl+V should replace the default filename once, got {:?}",
                file_input
            );
        } else {
            panic!("file modal should still be open");
        }
    }

    /// Copy a sub-graph (Constant #1 + disabled Grade #2) to TCL, paste it at
    /// an offset, then undo in a single step — the whole paste vanishes while
    /// the originals stay untouched.
    #[test]
    fn sub_graph_copy_paste_and_single_step_undo() {
        tanova_nodes::ensure_linked();

        let gpu = pollster::block_on(init_gpu()).expect("GPU should initialize");
        let kernel = KernelHandle::spawn().expect("kernel thread should spawn");
        let mut app = TanovaApp::new(kernel, gpu);

        // Let the seeded starter comp (Constant #1 -> Grade #2 -> Viewer #3)
        // round-trip into the mirror (the kernel works on a background thread).
        let seed_deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < seed_deadline && app.graph.node_count() != 3 {
            app.pump();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            app.graph.node_count(),
            3,
            "starter comp must land in mirror"
        );

        // Edit Grade #2's mix to 0.25 and disable it.
        app.kernel
            .send(KernelMessage::Execute(UndoableCommand::SetKnob {
                node: GRADE,
                knob: "mix".to_string(),
                value: KnobValue::Float(0.25),
                previous: KnobValue::Float(1.0),
            }))
            .expect("set Grade #2 mix to 0.25");
        app.kernel
            .send(KernelMessage::Execute(UndoableCommand::SetDisabled {
                node: GRADE,
                disabled: true,
            }))
            .expect("disable Grade #2");
        // Pump until the mirror reflects the edit.
        let edit_deadline = Instant::now() + Duration::from_secs(10);
        loop {
            app.pump();
            let grade = app.graph.node(GRADE);
            if grade
                .is_some_and(|g| g.disabled && g.knobs.get("mix") == Some(&KnobValue::Float(0.25)))
            {
                break;
            }
            if Instant::now() >= edit_deadline {
                panic!("mirror did not reflect Grade #2 edit");
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        let grade = app.graph.node(GRADE).expect("Grade #2");
        assert_eq!(
            grade.knobs.get("mix"),
            Some(&KnobValue::Float(0.25)),
            "Grade #2 mix must be 0.25"
        );
        assert!(grade.disabled, "Grade #2 must be disabled");

        // Select Constant #1 and Grade #2 (Viewer #3 is never part of a copy).
        app.canvas.selected_nodes = HashSet::from([CONSTANT, GRADE]);

        // Copy selection to a TCL snippet.
        let tcl = app
            .export_selection_to_tcl()
            .expect("selection should export to TCL");
        assert!(
            tcl.contains("Constant"),
            "export must contain Constant: {tcl}"
        );
        assert!(tcl.contains("Grade"), "export must contain Grade: {tcl}");
        assert!(
            tcl.contains("disable true"),
            "export must carry disable true: {tcl}"
        );
        assert!(!tcl.contains("Viewer"), "export must exclude Viewer: {tcl}");

        // Paste at (+40, +40): expects new nodes #4 and #5 plus edge #4 -> #5.
        let new_ids = app
            .paste_tcl_snippet(&tcl, (40.0, 40.0))
            .expect("paste should succeed");
        assert_eq!(new_ids, vec![NodeId(4), NodeId(5)]);
        assert_eq!(
            app.canvas.selected_nodes,
            HashSet::from([NodeId(4), NodeId(5)])
        );
        assert_eq!(app.canvas.primary_selected, Some(NodeId(4)));

        let const4 = app.graph.node(NodeId(4)).expect("Constant #4");
        let grade5 = app.graph.node(NodeId(5)).expect("Grade #5");
        assert!(
            (const4.x - 80.0).abs() < 1e-4,
            "Constant #4 at x=80, got {}",
            const4.x
        );
        assert!(
            (const4.y - 80.0).abs() < 1e-4,
            "Constant #4 at y=80, got {}",
            const4.y
        );
        assert!(
            (grade5.x - 80.0).abs() < 1e-4,
            "Grade #5 at x=80, got {}",
            grade5.x
        );
        assert!(
            (grade5.y - 204.0).abs() < 1e-4,
            "Grade #5 at y=204, got {}",
            grade5.y
        );
        assert_eq!(
            grade5.knobs.get("mix"),
            Some(&KnobValue::Float(0.25)),
            "pasted Grade #5 must keep mix 0.25"
        );
        assert!(grade5.disabled, "pasted Grade #5 must be disabled");

        // Edge #4 -> #5 must exist.
        assert_eq!(
            app.graph
                .incoming_edge(NodeId(5), PortId(0))
                .map(|e| e.from_node),
            Some(NodeId(4)),
            "edge #4 -> #5 must exist"
        );

        // Original nodes untouched.
        assert!(app.graph.node(CONSTANT).is_some());
        assert!(app.graph.node(GRADE).is_some());
        assert_eq!(app.graph.node_count(), 5);

        // One Undo must remove the whole pasted sub-graph in a single step.
        app.kernel.send(KernelMessage::Undo).expect("undo");
        let undo_deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < undo_deadline && app.graph.node_count() != 3 {
            app.pump();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            app.graph.node_count(),
            3,
            "paste must be undone in one step"
        );
        assert!(
            !app.graph.node(NodeId(4)).is_some(),
            "Constant #4 must be undone"
        );
        assert!(
            !app.graph.node(NodeId(5)).is_some(),
            "Grade #5 must be undone"
        );
        assert!(
            app.graph.incoming_edge(NodeId(5), PortId(0)).is_none(),
            "edge #4 -> #5 must be undone"
        );
        assert!(
            app.graph.node(CONSTANT).is_some(),
            "Constant #1 must remain"
        );
        assert!(app.graph.node(GRADE).is_some(), "Grade #2 must remain");
        assert_eq!(app.graph.node_count(), 3);
    }

    /// Duplicate (`Ctrl+D`) creates a remapped copy at a fixed offset without
    /// touching the clipboard, and canvas `F` frames the result on screen.
    #[test]
    fn duplicate_and_frame_nodes() {
        tanova_nodes::ensure_linked();

        let gpu = pollster::block_on(init_gpu()).expect("GPU should initialize");
        let kernel = KernelHandle::spawn().expect("kernel thread should spawn");
        let mut app = TanovaApp::new(kernel, gpu);
        // Let the seeded starter comp round-trip into the mirror.
        let seed_deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < seed_deadline && app.graph.node_count() != 3 {
            app.pump();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            app.graph.node_count(),
            3,
            "starter comp must land in mirror"
        );

        app.canvas.selected_nodes = HashSet::from([GRADE]);

        // Sentinel clipboard must survive duplicate untouched.
        app.test_clipboard = Some("sentinel".to_string());

        // Ctrl+D: export + paste at (40, 40) without touching the clipboard.
        let tcl = app
            .export_selection_to_tcl()
            .expect("Grade #2 should export to TCL");
        let _ = app.paste_tcl_snippet(&tcl, (40.0, 40.0));

        assert_eq!(
            app.test_clipboard,
            Some("sentinel".to_string()),
            "duplicate must not touch the clipboard"
        );
        let new_grade = app.graph.node(NodeId(4)).expect("new Grade #4");
        assert_eq!(new_grade.kind, "Grade");
        assert!((new_grade.x - 80.0).abs() < 1e-4, "Grade #4 at x=80");
        assert!((new_grade.y - 204.0).abs() < 1e-4, "Grade #4 at y=204");
        assert_eq!(app.canvas.primary_selected, Some(NodeId(4)));
        assert_eq!(app.canvas.selected_nodes, HashSet::from([NodeId(4)]));

        // Canvas `F` frames the single node dead-center of the viewport.
        //
        // `frame_nodes` centers the node's bounds center; the card is
        // 168x72 so its center is 84px right and 36px down from its top-left.
        let viewport = Rect::from_min_size(Pos2::ZERO, Vec2::new(1280.0, 720.0));
        app.canvas.frame_nodes(&app.graph, viewport);
        assert_ne!(app.canvas.zoom, 1.0, "frame should zoom to fit the node");
        let card_center = Pos2::new(new_grade.x + 84.0, new_grade.y + 36.0);
        let screen_pos = app.canvas.world_to_screen(viewport, card_center);
        assert!(
            (screen_pos.x - viewport.center().x).abs() < 2.0,
            "node x must frame to viewport center"
        );
        assert!(
            (screen_pos.y - viewport.center().y).abs() < 2.0,
            "node y must frame to viewport center"
        );
    }

    /// Copy/paste via egui's native `Event::Copy` / `Event::Paste` events
    /// (no `Key::C` / `Key::V`), exactly as eframe emits them on Linux.
    #[test]
    fn canvas_copy_and_paste_via_egui_copy_and_paste_events() {
        tanova_nodes::ensure_linked();

        let gpu = pollster::block_on(init_gpu()).expect("GPU should initialize");
        let kernel = KernelHandle::spawn().expect("kernel thread should spawn");
        let mut app = TanovaApp::new(kernel, gpu);

        // Let the seeded starter comp round-trip into the mirror.
        let seed_deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < seed_deadline && app.graph.node_count() != 3 {
            app.pump();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            app.graph.node_count(),
            3,
            "starter comp must land in mirror"
        );

        // Select Constant #1 and Grade #2 (Viewer #3 is never copied).
        app.canvas.selected_nodes = HashSet::from([CONSTANT, GRADE]);

        // Frame 1: emit an egui `Event::Copy` (no `Key::C`).
        let ctx = egui::Context::default();
        let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(1280.0, 720.0));
        ctx.run_pass(
            egui::RawInput {
                screen_rect: Some(screen),
                events: vec![egui::Event::Copy],
                ..Default::default()
            },
            |ctx| {
                app.handle_shortcuts(ctx);
            },
        )
        .drop_without_applying_deltas();

        assert!(
            app.internal_clipboard.is_some(),
            "internal_clipboard must hold the copied TCL"
        );
        let copied = app.internal_clipboard.clone().unwrap();
        assert!(copied.contains("Constant"), "copy must contain Constant");
        assert!(copied.contains("Grade"), "copy must contain Grade");

        // Frame 2: emit an egui `Event::Paste` carrying the copied text
        // (no `Key::V`).
        ctx.run_pass(
            egui::RawInput {
                screen_rect: Some(screen),
                events: vec![egui::Event::Paste(copied.clone())],
                ..Default::default()
            },
            |ctx| {
                app.handle_shortcuts(ctx);
            },
        )
        .drop_without_applying_deltas();

        // Pump until the kernel applies the pasted sub-graph.
        let paste_deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < paste_deadline && app.graph.node_count() != 5 {
            app.pump();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(app.graph.node_count(), 5, "paste must spawn two new nodes");
        assert!(
            app.graph.node(NodeId(4)).is_some(),
            "Constant #4 must exist"
        );
        assert!(app.graph.node(NodeId(5)).is_some(), "Grade #5 must exist");
        assert_eq!(
            app.graph
                .incoming_edge(NodeId(5), PortId(0))
                .map(|e| e.from_node),
            Some(NodeId(4)),
            "edge #4 -> #5 must exist"
        );
    }

    /// Copy/paste via `Key::C` / `Key::V` key events (no egui `Event::Copy` /
    /// `Event::Paste`), using the in-app `internal_clipboard` fallback.
    #[test]
    fn canvas_copy_and_paste_via_ctrl_c_and_ctrl_v_key_events() {
        tanova_nodes::ensure_linked();

        let gpu = pollster::block_on(init_gpu()).expect("GPU should initialize");
        let kernel = KernelHandle::spawn().expect("kernel thread should spawn");
        let mut app = TanovaApp::new(kernel, gpu);

        // Let the seeded starter comp round-trip into the mirror.
        let seed_deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < seed_deadline && app.graph.node_count() != 3 {
            app.pump();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            app.graph.node_count(),
            3,
            "starter comp must land in mirror"
        );

        // Select Grade #2 only.
        app.canvas.selected_nodes = HashSet::from([GRADE]);

        let ctx = egui::Context::default();
        let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(1280.0, 720.0));

        // Frame 1: Ctrl+C key event (no `Event::Copy`).
        ctx.run_pass(
            egui::RawInput {
                screen_rect: Some(screen),
                events: vec![egui::Event::Key {
                    key: egui::Key::C,
                    physical_key: Some(egui::Key::C),
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::CTRL,
                }],
                ..Default::default()
            },
            |ctx| {
                app.handle_shortcuts(ctx);
            },
        )
        .drop_without_applying_deltas();

        // The in-app clipboard must now hold the copied TCL.
        assert!(
            app.internal_clipboard.is_some(),
            "internal_clipboard must hold the copied TCL after Ctrl+C"
        );

        // Frame 2: Ctrl+V key event (no `Event::Paste`).
        ctx.run_pass(
            egui::RawInput {
                screen_rect: Some(screen),
                events: vec![egui::Event::Key {
                    key: egui::Key::V,
                    physical_key: Some(egui::Key::V),
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::CTRL,
                }],
                ..Default::default()
            },
            |ctx| {
                app.handle_shortcuts(ctx);
            },
        )
        .drop_without_applying_deltas();

        // Pump until the kernel applies the pasted duplicate.
        let paste_deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < paste_deadline && app.graph.node_count() != 4 {
            app.pump();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(app.graph.node_count(), 4, "paste must spawn one new node");
        let dup = app.graph.node(NodeId(4)).expect("duplicate Grade #4");
        assert_eq!(dup.kind, "Grade");
        assert!((dup.x - 80.0).abs() < 1e-4, "Grade #4 at x=80");
        assert!((dup.y - 204.0).abs() < 1e-4, "Grade #4 at y=204");
    }
}

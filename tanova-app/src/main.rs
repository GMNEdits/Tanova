//! Tanova desktop application shell.
//!
//! The UI thread owns no graph state: it mirrors the kernel's graph from
//! [`KernelEvent`]s and sends every mutation back as a [`KernelMessage`].
//! The kernel thread evaluates the viewer target and delivers frames
//! off-screen through [`FrameNotification`]s.

use std::error::Error;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui;
use tanova_io::DiskCache;
use tanova_kernel::{
    EvalContext, FrameNotification, KernelEvent, KernelHandle, KernelMessage, KnobValue, NodeId,
    Packet, PortId, ProjectFormat, UndoableCommand,
};
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

    /// Applies global undo/redo shortcuts.
    ///
    /// Text fields keep their own editing shortcuts, and an open canvas quick
    /// search owns the keyboard, so both suppress the global binding.
    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        if ctx.egui_wants_keyboard_input() || self.canvas.is_search_open() {
            return;
        }

        let mut messages = Vec::new();
        ctx.input(|input| {
            for event in &input.events {
                if let egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } = event
                {
                    if let Some(message) = shortcut_message(*modifiers, *key) {
                        messages.push(message);
                    }
                }
            }
        });
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
            UndoableCommand::SetKnob { .. } | UndoableCommand::MoveNode { .. } => {
                self.graph.apply_command(command);
            }
            UndoableCommand::Batch(cmds) => {
                for cmd in cmds {
                    self.apply_optimistic_mirror_update(cmd);
                }
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
}

impl eframe::App for TanovaApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.pump();
        self.handle_shortcuts(ui.ctx());
        self.advance_timeline();

        egui::Panel::top("tanova_status").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.strong("Tanova");
                ui.separator();
                ui.label(format!("epoch {}", self.epoch));
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
        // The panels are declared before the central panel so the canvas only
        // ever receives the space that is left over.
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

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ui, |ui| {
                let messages = NodeCanvas::show(
                    ui,
                    &mut self.canvas,
                    &self.graph,
                    self.viewer.viewer_node(),
                    self.workflow,
                );
                self.dispatch(messages);
            });

        // The kernel runs on its own thread; poll it at display cadence.
        if self.awaiting_kernel {
            // Keep egui repainting until the kernel finishes rendering so the
            // new frame appears immediately instead of waiting for the next
            // mouse event or timer tick.
            ui.ctx().request_repaint();
        }
        ui.ctx().request_repaint_after(Duration::from_millis(16));
    }
}

fn kernel_error(error: std::io::Error) -> AppError {
    Box::new(error)
}

fn check_failed(message: impl Into<String>) -> AppError {
    Box::new(std::io::Error::other(message.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

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
}

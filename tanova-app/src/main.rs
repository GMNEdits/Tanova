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
use tanova_kernel::{
    EvalContext, FrameNotification, KernelEvent, KernelHandle, KernelMessage, KnobValue, NodeId,
    Packet, PortId, UndoableCommand, registered_cartridge_kinds,
};
use tanova_render::{GpuComputeEngine, Raster2D};
use tanova_ui::{GraphMirror, NodeCanvas, NodeCanvasState, PropertiesPanel, ViewerPanel};

/// Errors crossing the app boundary (eframe requires `Send + Sync`).
type AppError = Box<dyn Error + Send + Sync>;

/// Node ids of the starter comp.
const CONSTANT: NodeId = NodeId(1);
const GRADE: NodeId = NodeId(2);

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
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([720.0, 480.0]),
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
    let packet = app.wait_for_frame(GRADE, Duration::from_secs(60))?;
    println!(
        "check-headless: viewer frame from #{}; nodes={} edges={} epoch={}",
        GRADE.0,
        app.graph.node_count(),
        app.graph.edge_count(),
        app.epoch
    );

    if let Some(failure) = app.failure.as_deref() {
        return Err(check_failed(format!(
            "kernel reported a failure: {failure}"
        )));
    }
    if app.graph.node_count() != 2 {
        return Err(check_failed(format!(
            "expected 2 seeded nodes, found {}",
            app.graph.node_count()
        )));
    }
    if app.graph.edge_count() != 1 {
        return Err(check_failed(format!(
            "expected 1 seeded wire, found {}",
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
    cartridge_kinds: Vec<&'static str>,
    epoch: u64,
    last_frame: Option<FrameNotification>,
    failure: Option<String>,
    gpu: GpuContext,
}

impl TanovaApp {
    fn new(kernel: KernelHandle, gpu: GpuContext) -> Self {
        let mut app = Self {
            kernel,
            graph: GraphMirror::default(),
            canvas: NodeCanvasState::default(),
            viewer: ViewerPanel::new(),
            cartridge_kinds: registered_cartridge_kinds(),
            epoch: 0,
            last_frame: None,
            failure: None,
            gpu,
        };
        app.seed_graph();
        app
    }

    /// Queues the live starter comp: Constant -> Grade, viewed at #2.
    fn seed_graph(&mut self) {
        let mut context = EvalContext::new();
        context.insert(GpuComputeEngine::new(
            self.gpu.device.clone(),
            self.gpu.queue.clone(),
        ));

        let seed = [
            KernelMessage::SetBaseContext(context),
            KernelMessage::Execute(UndoableCommand::AddNode {
                id: CONSTANT,
                kind: "Constant".to_string(),
            }),
            KernelMessage::Execute(UndoableCommand::AddNode {
                id: GRADE,
                kind: "Grade".to_string(),
            }),
            KernelMessage::Execute(UndoableCommand::ConnectEdge {
                from_node: CONSTANT,
                from_port: PortId(0),
                to_node: GRADE,
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
            KernelMessage::SetViewerNode(Some(GRADE)),
        ];

        self.viewer.bind(Some(GRADE));
        for message in seed {
            if let Err(error) = self.kernel.send(message) {
                self.failure = Some(format!("failed to seed the kernel: {error}"));
            }
        }
    }

    /// Non-blocking drain of kernel events and frame notifications.
    fn pump(&mut self) {
        while let Ok(event) = self.kernel.events().try_recv() {
            self.fold_event(event);
        }
        while let Ok(frame) = self.kernel.frames().try_recv() {
            self.viewer.set_frame(&frame);
            self.last_frame = Some(frame);
        }
    }

    /// Folds one kernel event into the UI mirror.
    fn fold_event(&mut self, event: KernelEvent) {
        match &event {
            KernelEvent::EpochChanged(epoch) => self.epoch = *epoch,
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
                let matches = frame.node == node;
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

    /// Sends every message the UI asked for, syncing viewer state first.
    fn dispatch(&mut self, messages: Vec<KernelMessage>) {
        for message in messages {
            if let KernelMessage::SetViewerNode(node) = &message {
                self.viewer.bind(*node);
            }
            if let Err(error) = self.kernel.send(message) {
                self.failure = Some(format!("kernel send failed: {error}"));
            }
        }
    }

    fn shutdown(self) {
        self.kernel.shutdown();
    }
}

impl eframe::App for TanovaApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.pump();

        egui::Panel::top("tanova_status").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.strong("Tanova");
                ui.separator();
                ui.label(format!("epoch {}", self.epoch));
                ui.separator();
                ui.label(format!("{} node(s)", self.graph.node_count()));
                ui.label(format!("{} wire(s)", self.graph.edge_count()));
                if let Some(frame) = &self.last_frame {
                    if let Some(raster) = frame.packet.get::<Raster2D>() {
                        ui.separator();
                        ui.label(format!(
                            "frame {}x{}",
                            raster.format.width, raster.format.height
                        ));
                    }
                }
                if let Some(failure) = &self.failure {
                    ui.separator();
                    ui.colored_label(egui::Color32::from_rgb(235, 90, 90), failure);
                }
            });
        });

        // Nuke layout: Viewer on top, Properties on the right, graph below.
        egui::Panel::top("tanova_viewer")
            .resizable(true)
            .default_size(380.0)
            .min_size(160.0)
            .show(ui, |ui| {
                let messages = self.viewer.show(ui, &self.graph, self.canvas.selected);
                self.dispatch(messages);
            });

        egui::Panel::right("tanova_properties")
            .resizable(true)
            .default_size(320.0)
            .min_size(180.0)
            .show(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    let messages = PropertiesPanel::show(
                        ui,
                        &self.graph,
                        self.canvas.selected,
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
                    &self.cartridge_kinds,
                    self.viewer.viewer_node(),
                );
                self.dispatch(messages);
            });

        // The kernel runs on its own thread; poll it at display cadence.
        ui.ctx().request_repaint_after(Duration::from_millis(16));
    }
}

fn kernel_error(error: std::io::Error) -> AppError {
    Box::new(error)
}

fn check_failed(message: impl Into<String>) -> AppError {
    Box::new(std::io::Error::other(message.into()))
}

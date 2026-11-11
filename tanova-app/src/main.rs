//! Tanova desktop application shell.
//!
//! The UI thread owns no graph state: it mirrors the kernel's graph from
//! [`KernelEvent`]s and sends every mutation back as a [`KernelMessage`].

use std::error::Error;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui;
use tanova_kernel::{
    FrameNotification, KernelEvent, KernelHandle, KernelMessage, NodeId, PortId, UndoableCommand,
    registered_cartridge_kinds,
};
use tanova_ui::{GraphMirror, NodeCanvas, NodeCanvasState};

/// Errors crossing the app boundary (eframe requires `Send + Sync`).
type AppError = Box<dyn Error + Send + Sync>;

/// Seeded commands: two `Dummy` nodes plus one B-pipe wire, then an epoch bump.
const SEED_EVENTS: usize = 4;

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

/// Boots GPU + kernel + UI state, verifies the seeded graph, and exits.
fn run_headless_check() -> Result<(), AppError> {
    println!("check-headless: initializing GPU");
    let gpu = pollster::block_on(init_gpu())?;
    println!(
        "check-headless: GPU ready; holding {} shared handle(s)",
        Arc::strong_count(&gpu.device)
    );

    let mut app = TanovaApp::new(KernelHandle::spawn().map_err(kernel_error)?, gpu);
    let folded = app.wait_for_events(SEED_EVENTS, Duration::from_secs(10))?;
    println!("check-headless: folded {folded} kernel event(s)");

    app.pump();
    app.canvas.layout_nodes(&app.graph);

    let nodes = app.graph.node_count();
    let edges = app.graph.edge_count();
    println!(
        "check-headless: nodes={nodes} edges={edges} epoch={}",
        app.epoch
    );

    if let Some(failure) = app.failure.as_deref() {
        return Err(check_failed(format!(
            "kernel reported a failure: {failure}"
        )));
    }
    if nodes != 2 {
        return Err(check_failed(format!(
            "expected 2 seeded nodes, found {nodes}"
        )));
    }
    if edges != 1 {
        return Err(check_failed(format!(
            "expected 1 seeded wire, found {edges}"
        )));
    }
    if app.epoch != 1 {
        return Err(check_failed(format!(
            "expected epoch 1 after the seed bump, found {}",
            app.epoch
        )));
    }

    let wire = app.graph.edges()[0];
    if wire.from_node != NodeId(1) || wire.to_node != NodeId(2) {
        return Err(check_failed(format!(
            "expected a #1 -> #2 wire, found #{} -> #{}",
            wire.from_node.0, wire.to_node.0
        )));
    }

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
            cartridge_kinds: registered_cartridge_kinds(),
            epoch: 0,
            last_frame: None,
            failure: None,
            gpu,
        };
        app.seed_graph();
        app
    }

    /// Queues the two-node starter graph the window opens with.
    fn seed_graph(&mut self) {
        let seed = [
            KernelMessage::Execute(UndoableCommand::AddNode {
                id: NodeId(1),
                kind: "Dummy".to_string(),
            }),
            KernelMessage::Execute(UndoableCommand::AddNode {
                id: NodeId(2),
                kind: "Dummy".to_string(),
            }),
            KernelMessage::Execute(UndoableCommand::ConnectEdge {
                from_node: NodeId(1),
                from_port: PortId(0),
                to_node: NodeId(2),
                to_port: PortId(0),
            }),
            KernelMessage::BumpEpoch,
        ];

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

    /// Blocks (with a deadline) until `expected` events have been folded.
    fn wait_for_events(&mut self, expected: usize, timeout: Duration) -> Result<usize, AppError> {
        let deadline = Instant::now() + timeout;
        let mut folded = 0;

        while folded < expected {
            while let Ok(event) = self.kernel.events().try_recv() {
                self.fold_event(event);
                folded += 1;
            }
            if folded >= expected {
                break;
            }
            if Instant::now() >= deadline {
                return Err(check_failed(format!(
                    "timed out after {folded}/{expected} kernel event(s)"
                )));
            }
            std::thread::sleep(Duration::from_millis(2));
        }

        Ok(folded)
    }

    /// Sends every message the canvas asked for.
    fn dispatch(&mut self, messages: Vec<KernelMessage>) {
        for message in messages {
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
                if let Some(frame) = self.last_frame {
                    ui.separator();
                    ui.label(format!("frame {}x{}", frame.width, frame.height));
                }
                if let Some(failure) = &self.failure {
                    ui.separator();
                    ui.colored_label(egui::Color32::from_rgb(235, 90, 90), failure);
                }
            });
        });

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ui, |ui| {
                let messages =
                    NodeCanvas::show(ui, &mut self.canvas, &self.graph, &self.cartridge_kinds);
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

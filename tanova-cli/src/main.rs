//! Headless Tanova render farm executable.
//!
//! Loads a Nuke `.nk` script through the TCL stack machine, evaluates the
//! requested node on the CPU/GPU render core, and optionally writes a
//! display-transformed PPM. Deliberately independent of `tanova-ui`/`eframe`.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::time::Instant;

use tanova_io::{resolve_sequence_path, write_exr_to_path};
use tanova_kernel::{
    Dag, DagNode, Edge, EvalContext, KnobValue, MemoryGovernor, NodeError, NodeId, PortId,
    RenderEpoch, Time, View, find_cartridge,
};
use tanova_parse::{ParsedScript, load_nk_into_dag};
use tanova_render::Raster2D;

type CliError = Box<dyn Error + Send + Sync>;

/// Embedded script exercised by `--check-headless`.
///
/// Constant -> Grade -> unknown Foundry node feeds Merge2's `A` input, while
/// the Constant also feeds `B` through a `set N1 [stack 0]` branch. Grade
/// multiplies by 2, so the opaque `over` result is `[0.4, 0.8, 1.2, 1.0]`.
const CHECK_SCRIPT: &str = r#"
version 15.0 v5
Root {
 inputs 0
 name Root
 format "4 4 0 0 4 4 1 square"
}
Constant {
 inputs 0
 name Constant1
 color {0.2 0.4 0.6 1}
 format_width 4
 format_height 4
}
set N1 [stack 0]
Grade {
 inputs 1
 name Grade1
 multiply 2
}
Foundry_CameraShake {
 inputs 1
 name Shake1
 secret_knob 42
}
set N2 [stack 0]
push 0
push $N2
push $N1
Merge2 {
 inputs 2+1
 name Merge1
 operation "over"
 mix 1.0
}
set N3 [stack 0]
Viewer {
 inputs 1
 name Viewer1
}
"#;

fn main() {
    tanova_nodes::ensure_linked();
    if let Err(error) = run() {
        eprintln!("tanova-cli: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), CliError> {
    let args = Args::parse()?;
    if args.check_headless {
        return run_headless_check();
    }
    if args.bench_1080p {
        return run_bench_1080p();
    }

    let script_path = args
        .script
        .ok_or("missing --script <path.nk> (or use --check-headless)")?;
    let script = std::fs::read_to_string(&script_path)?;
    let (raster, _parsed, target) = render_script(&script, args.frame, args.node.as_deref())?;

    println!(
        "rendered node #{} ({}x{}) at frame {}",
        target.0, raster.format.width, raster.format.height, args.frame
    );

    if let Some(path) = args.out_ppm {
        write_ppm(&path, &raster)?;
        println!("wrote {}", path.display());
    }
    if let Some(path) = args.out_exr {
        let resolved =
            resolve_sequence_path(path.to_str().ok_or("invalid --out-exr path")?, args.frame);
        let resolved_path = PathBuf::from(resolved);
        write_exr_to_path(&resolved_path, &raster, "all")?;
        println!("wrote {}", resolved_path.display());
    }
    Ok(())
}

/// Command-line flags.
struct Args {
    script: Option<PathBuf>,
    frame: f64,
    node: Option<String>,
    out_ppm: Option<PathBuf>,
    out_exr: Option<PathBuf>,
    check_headless: bool,
    bench_1080p: bool,
}

impl Args {
    fn parse() -> Result<Self, CliError> {
        let mut args = Self {
            script: None,
            frame: 1.0,
            node: None,
            out_ppm: None,
            out_exr: None,
            check_headless: false,
            bench_1080p: false,
        };

        let mut iter = std::env::args().skip(1);
        while let Some(argument) = iter.next() {
            match argument.as_str() {
                "--script" => {
                    args.script = Some(iter.next().ok_or("--script needs a path")?.into());
                }
                "--frame" => {
                    args.frame = iter
                        .next()
                        .ok_or("--frame needs a number")?
                        .parse()
                        .map_err(|_| "--frame must be a number")?;
                }
                "--node" => {
                    args.node = Some(iter.next().ok_or("--node needs a name or id")?);
                }
                "--out-ppm" => {
                    args.out_ppm = Some(iter.next().ok_or("--out-ppm needs a path")?.into());
                }
                "--out-exr" => {
                    args.out_exr = Some(iter.next().ok_or("--out-exr needs a path")?.into());
                }
                "--check-headless" => args.check_headless = true,
                "--bench-1080p" => args.bench_1080p = true,
                "--help" | "-h" => {
                    print_usage();
                    std::process::exit(0);
                }
                other => return Err(format!("unknown argument `{other}`").into()),
            }
        }

        Ok(args)
    }
}

fn print_usage() {
    println!(
        "tanova-cli {}\n\
         \n\
         Usage:\n\
           tanova-cli --script <path.nk> [--frame <f64>] [--node <name|id>] [--out-ppm <path>] [--out-exr <path>]\n\
           tanova-cli --check-headless\n",
        env!("CARGO_PKG_VERSION")
    );
}

/// Loads a script, resolves the target, and evaluates one frame.
fn render_script(
    script: &str,
    frame: f64,
    node: Option<&str>,
) -> Result<(Raster2D, ParsedScript, NodeId), CliError> {
    let mut dag = Dag::new();
    let parsed = load_nk_into_dag(script, &mut dag)?;
    let target = resolve_target(&parsed, node)?;

    let mut ctx = EvalContext::new();
    ctx.insert(Time(frame));
    ctx.insert(View("main".to_string()));

    let governor = MemoryGovernor::new(512 * 1024 * 1024);
    let epoch = RenderEpoch::new();
    let packet = dag.evaluate_pull(target, &ctx, &governor, &epoch, epoch.current())?;
    let raster = packet
        .get::<Raster2D>()
        .cloned()
        .ok_or("target node produced no Raster2D")?;

    Ok((raster, parsed, target))
}

fn resolve_target(parsed: &ParsedScript, node: Option<&str>) -> Result<NodeId, CliError> {
    if let Some(node) = node {
        return parsed
            .resolve(node)
            .ok_or_else(|| format!("unknown node `{node}`").into());
    }

    parsed
        .viewer_target
        .or(parsed.stack_top)
        .or_else(|| parsed.nodes.last().map(|node| node.id))
        .ok_or_else(|| "the script contains no renderable node".into())
}

/// Writes a binary portable pixmap (P6) from the display-transformed frame.
fn write_ppm(path: &Path, raster: &Raster2D) -> Result<(), CliError> {
    let width = raster.format.width as usize;
    let height = raster.format.height as usize;
    let rgba = raster.to_top_left_rgba8(raster.format.to_bbox());

    let mut out = Vec::with_capacity(width * height * 3 + 32);
    out.extend_from_slice(format!("P6\n{width} {height}\n255\n").as_bytes());
    for pixel in rgba.chunks_exact(4) {
        out.extend_from_slice(&pixel[..3]);
    }

    std::fs::write(path, out)?;
    Ok(())
}

fn run_headless_check() -> Result<(), CliError> {
    println!("check-headless: parsing the embedded multi-node script");
    let (raster, parsed, target) = render_script(CHECK_SCRIPT, 1.0, None)?;

    let merge = parsed
        .names
        .get("Merge1")
        .copied()
        .ok_or("Merge1 was not parsed")?;
    let viewer = parsed
        .names
        .get("Viewer1")
        .copied()
        .ok_or("Viewer1 was not parsed")?;
    if parsed.viewer_target != Some(viewer) || target != viewer {
        return Err(format!(
            "expected the Viewer node (#{}) to be the render target, got viewer={:?} target=#{}",
            viewer.0, parsed.viewer_target, target.0
        )
        .into());
    }
    let viewer_node = parsed
        .nodes
        .iter()
        .find(|node| node.id == viewer)
        .ok_or("Viewer1 descriptor missing")?;
    if viewer_node.input_nodes.first().copied().flatten() != Some(merge) {
        return Err(format!("expected Viewer1 to be wired to Merge1 (#{})", merge.0).into());
    }

    let shake = parsed
        .names
        .get("Shake1")
        .copied()
        .ok_or("Shake1 was not parsed")?;
    let shake_node = parsed
        .nodes
        .iter()
        .find(|node| node.id == shake)
        .ok_or("Shake1 descriptor missing")?;
    if shake_node.class != "Foundry_CameraShake" {
        return Err(format!("expected Foundry_CameraShake, got {}", shake_node.class).into());
    }
    if shake_node.knobs.get("secret_knob") != Some(&KnobValue::Int(42)) {
        return Err("the unknown node lost `secret_knob 42`".into());
    }

    if raster.format.width != 4 || raster.format.height != 4 {
        return Err(format!(
            "expected a 4x4 frame, got {}x{}",
            raster.format.width, raster.format.height
        )
        .into());
    }

    let pixel = raster.sample_rgba(1, 1);
    let expected = [0.4f32, 0.8, 1.2, 1.0];
    for (index, (actual, expected)) in pixel.iter().zip(expected.iter()).enumerate() {
        if (actual - expected).abs() > 1e-3 {
            return Err(format!("graded channel {index} was {actual}, expected {expected}").into());
        }
    }

    println!(
        "check-headless: nodes={} unknown={} final RGBA={pixel:?}",
        parsed.nodes.len(),
        shake_node.class
    );
    println!("check-headless: OK");
    Ok(())
}

/// Builds `CheckerBoard2 -> Viewer` at `width x height` and times one pull so
/// the source-generation cost is visible on its own row.
fn bench_checker_source(width: u32, height: u32) -> Result<(u64, usize), CliError> {
    let mut dag = Dag::new();
    let ctx = EvalContext::new();
    let governor = MemoryGovernor::new(2 * 1024 * 1024 * 1024); // 2.0 GiB cap
    let epoch = RenderEpoch::new();

    let checker = NodeId(1);
    let viewer = NodeId(2);

    dag.insert_node(
        DagNode::new(checker, "CheckerBoard2").with_implementation((find_cartridge(
            "CheckerBoard2",
        )
        .ok_or("CheckerBoard2 cartridge not registered")?
        .factory)()),
    )?;
    dag.set_knob(checker, "format_width", KnobValue::Int(width as i64))?;
    dag.set_knob(checker, "format_height", KnobValue::Int(height as i64))?;
    dag.set_knob(checker, "boxsize", KnobValue::Int(64))?;

    dag.insert_node(
        tanova_kernel::DagNode::new(viewer, "Viewer").with_implementation((find_cartridge(
            "Viewer",
        )
        .ok_or("Viewer cartridge not registered")?
        .factory)()),
    )?;
    dag.connect_edge(Edge::new(checker, PortId(0), viewer, PortId(0)))?;

    let start = Instant::now();
    let _packet = dag.evaluate_pull(viewer, &ctx, &governor, &epoch, epoch.current())?;
    let elapsed_us = start.elapsed().as_micros() as u64;
    let peak_bytes = governor.allocated_bytes();

    Ok((elapsed_us, peak_bytes))
}

/// Builds `CheckerBoard2 -> op -> Viewer` at `width x height`, evaluates the
/// source once to warm the DAG cache, then times the downstream pull so the
/// reported `ms` measures the op itself rather than source generation.
fn bench_op(
    op_kind: &str,
    width: u32,
    height: u32,
    set_knobs: impl FnOnce(&mut Dag, NodeId) -> Result<(), NodeError>,
) -> Result<(u64, usize), CliError> {
    let mut dag = Dag::new();
    let ctx = EvalContext::new();
    let governor = MemoryGovernor::new(2 * 1024 * 1024 * 1024); // 2.0 GiB cap
    let epoch = RenderEpoch::new();

    let checker = NodeId(1);
    let op = NodeId(2);
    let viewer = NodeId(3);

    dag.insert_node(
        DagNode::new(checker, "CheckerBoard2").with_implementation((find_cartridge(
            "CheckerBoard2",
        )
        .ok_or("CheckerBoard2 cartridge not registered")?
        .factory)()),
    )?;
    dag.set_knob(checker, "format_width", KnobValue::Int(width as i64))?;
    dag.set_knob(checker, "format_height", KnobValue::Int(height as i64))?;
    dag.set_knob(checker, "boxsize", KnobValue::Int(64))?;

    dag.insert_node(
        tanova_kernel::DagNode::new(op, op_kind).with_implementation((find_cartridge(op_kind)
            .ok_or(format!("{op_kind} cartridge not registered"))?
            .factory)()),
    )?;
    set_knobs(&mut dag, op)?;

    dag.insert_node(
        tanova_kernel::DagNode::new(viewer, "Viewer").with_implementation((find_cartridge(
            "Viewer",
        )
        .ok_or("Viewer cartridge not registered")?
        .factory)()),
    )?;
    dag.connect_edge(Edge::new(checker, PortId(0), op, PortId(0)))?;
    dag.connect_edge(Edge::new(op, PortId(0), viewer, PortId(0)))?;

    // Warm the source so the timed pull is the op, not checker generation.
    let _ = dag.evaluate_pull(checker, &ctx, &governor, &epoch, epoch.current())?;
    let start = Instant::now();
    let _packet = dag.evaluate_pull(viewer, &ctx, &governor, &epoch, epoch.current())?;
    let elapsed_us = start.elapsed().as_micros() as u64;
    let peak_bytes = governor.allocated_bytes();

    Ok((elapsed_us, peak_bytes))
}

/// Runs the 1920x1080 speed benchmark gate: the CheckerBoard2 source on its
/// own row, then Grade, Blur, Defocus, CornerPin2D and Glow with the source
/// pre-warmed, plus a single-frame 256x256 Defocus row.
fn run_bench_1080p() -> Result<(), CliError> {
    let workers = tanova_render::pool::worker_thread_count();
    let tiles = tanova_render::Format::hd_1080()
        .to_bbox()
        .tile_coords()
        .len();
    println!("tanova-cli --bench-1080p");
    println!("  reference machine: i5-10310U (8 threads -> N-2 workers)");
    println!("  worker_thread_count() = {workers}");
    println!("  source: CheckerBoard2 1920x1080 ({tiles} tiles)");
    println!("  governor cap: 2.0 GiB");
    println!("  each op row warms the source first, so it measures the op only");
    println!();

    let mut results: Vec<(&str, u64, usize)> = Vec::new();

    let source = bench_checker_source(1920, 1080)?;
    results.push(("CheckerBoard2", source.0, source.1));

    let grade = bench_op("Grade", 1920, 1080, |dag, op| {
        dag.set_knob(op, "multiply", KnobValue::Color([1.2; 4]))
    })?;
    results.push(("Grade", grade.0, grade.1));

    let blur = bench_op("Blur", 1920, 1080, |dag, op| {
        dag.set_knob(op, "size", KnobValue::Float(12.0))
    })?;
    results.push(("Blur (size=12)", blur.0, blur.1));

    let defocus = bench_op("Defocus", 1920, 1080, |dag, op| {
        dag.set_knob(op, "defocus", KnobValue::Float(8.0))?;
        dag.set_knob(op, "aspect", KnobValue::Float(1.0))?;
        dag.set_knob(op, "mix", KnobValue::Float(1.0))
    })?;
    results.push(("Defocus (8.0)", defocus.0, defocus.1));

    let corner_pin = bench_op("CornerPin2D", 1920, 1080, |dag, op| {
        dag.set_knob(op, "to1", KnobValue::Color([0.0, 0.0, 0.0, 0.0]))?;
        dag.set_knob(op, "to2", KnobValue::Color([1920.0, 0.0, 0.0, 0.0]))?;
        dag.set_knob(op, "to3", KnobValue::Color([1800.0, 1080.0, 0.0, 0.0]))?;
        dag.set_knob(op, "to4", KnobValue::Color([120.0, 1080.0, 0.0, 0.0]))?;
        dag.set_knob(op, "invert", KnobValue::Bool(false))
    })?;
    results.push(("CornerPin2D", corner_pin.0, corner_pin.1));

    let glow = bench_op("Glow", 1920, 1080, |dag, op| {
        dag.set_knob(op, "size", KnobValue::Float(12.0))?;
        dag.set_knob(op, "tolerance", KnobValue::Float(0.2))?;
        dag.set_knob(op, "mix", KnobValue::Float(1.0))
    })?;
    results.push(("Glow (size=12)", glow.0, glow.1));

    let defocus_256 = bench_op("Defocus", 256, 256, |dag, op| {
        dag.set_knob(op, "defocus", KnobValue::Float(4.0))?;
        dag.set_knob(op, "aspect", KnobValue::Float(1.0))?;
        dag.set_knob(op, "mix", KnobValue::Float(1.0))
    })?;
    results.push(("Defocus 256 (4.0)", defocus_256.0, defocus_256.1));

    println!("{:<18} {:>8} {:>10}", "OP", "ms", "peak MiB");
    let mut all_ok = true;
    for (name, us, bytes) in &results {
        let ms = *us as f64 / 1000.0;
        let mib = *bytes as f64 / (1024.0 * 1024.0);
        if mib > 2048.0 {
            all_ok = false;
        }
        println!("{:<18} {ms:8.2} {mib:10.2}", name);
    }
    println!();
    if all_ok {
        println!("bench-1080p: all ops within 2.0 GiB cap — OK");
        Ok(())
    } else {
        Err("bench-1080p: an op exceeded the 2.0 GiB governor cap".into())
    }
}

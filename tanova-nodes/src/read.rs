//! `Read`: an EXR sequence source backed by the disk cache.
//!
//! The cartridge never blocks the render thread. A cache miss returns a
//! neutral gray frame immediately while the single I/O worker loads the file;
//! the worker's `TriggerRender` message brings the real pixels in on the next
//! evaluation. Missing or corrupt files follow the `missing_frames` knob.

use std::path::Path;
use std::sync::Arc;

use tanova_io::{CacheStatus, DiskCache, load_exr, resolve_sequence_path};
use tanova_kernel::{
    EvalContext, KnobTable, KnobValue, NodeCartridgeDescriptor, NodeError, NodeTrait, Packet,
};
use tanova_render::{Channel, ChannelPlane, Format, Raster2D, TileBuffer};

use crate::util::{int_knob, text_knob};

/// Default `file` knob.
pub const DEFAULT_FILE: &str = "";
/// Default first frame.
pub const DEFAULT_FIRST: i64 = 1;
/// Default last frame.
pub const DEFAULT_LAST: i64 = 100;
/// Default missing-frame policy.
pub const DEFAULT_MISSING_FRAMES: &str = "error";

/// Neutral gray shown while a frame is still loading.
pub const LOADING_GRAY: [f32; 4] = [0.5, 0.5, 0.5, 1.0];
/// Opaque black shown for missing frames in `black` mode.
pub const MISSING_BLACK: [f32; 4] = [0.0, 0.0, 0.0, 1.0];

/// Default knob values for a fresh [`ReadNode`].
pub fn default_knobs() -> KnobTable {
    let mut knobs = KnobTable::with_capacity(4);
    knobs.insert("file", KnobValue::Text(DEFAULT_FILE.to_string()));
    knobs.insert("first", KnobValue::Int(DEFAULT_FIRST));
    knobs.insert("last", KnobValue::Int(DEFAULT_LAST));
    knobs.insert(
        "missing_frames",
        KnobValue::Text(DEFAULT_MISSING_FRAMES.to_string()),
    );
    knobs
}

/// An EXR sequence reader.
#[derive(Debug, Clone)]
pub struct ReadNode {
    knobs: KnobTable,
}

impl Default for ReadNode {
    fn default() -> Self {
        Self {
            knobs: default_knobs(),
        }
    }
}

impl ReadNode {
    /// Creates a reader with default knobs.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a reader over an existing knob table.
    pub fn from_knobs(knobs: KnobTable) -> Self {
        Self { knobs }
    }

    /// Borrows the node's knobs.
    pub fn knobs(&self) -> &KnobTable {
        &self.knobs
    }
}

impl NodeTrait for ReadNode {
    fn evaluate(&self, ctx: &EvalContext, _inputs: &[Packet]) -> Result<Packet, NodeError> {
        let knobs = ctx.knobs().unwrap_or(&self.knobs);
        let file = text_knob(knobs, "file", DEFAULT_FILE);
        if file.is_empty() {
            return Err(NodeError::Other("EXR read failed".to_string()));
        }

        let first = int_knob(knobs, "first", DEFAULT_FIRST);
        let last = int_knob(knobs, "last", DEFAULT_LAST);
        let (first, last) = if first <= last {
            (first, last)
        } else {
            (last, first)
        };
        let missing = text_knob(knobs, "missing_frames", DEFAULT_MISSING_FRAMES);
        let time = ctx.time().unwrap_or(first as f64);
        let path = resolve_frame_path(&file, time, first, last, &missing);
        let governor = ctx.governor().cloned();

        let Some(cache) = ctx.get::<DiskCache>() else {
            // Headless/CLI fallback: there is no worker to wake, so load
            // synchronously and report failures through the missing policy.
            return match load_exr(&path, governor.as_ref()) {
                Ok(raster) => Ok(packet_from_raster(raster)),
                Err(_) => missing_packet(&missing),
            };
        };

        if let CacheStatus::Ready(raster) = cache.status(&path) {
            return Ok(packet_from_raster((*raster).clone()));
        }

        if !Path::new(&path).exists() {
            return missing_packet(&missing);
        }

        let _ = cache.get_or_load(path.clone(), governor);

        match cache.status(&path) {
            CacheStatus::Ready(raster) => Ok(packet_from_raster((*raster).clone())),
            CacheStatus::Failed => missing_packet(&missing),
            CacheStatus::Loading | CacheStatus::Missing => Ok(packet_from_raster(uniform_raster(
                Format::hd_1080(),
                LOADING_GRAY,
            ))),
        }
    }

    fn serialize_to_tcl(&self) -> String {
        format!(
            "Read {{\n file \"{}\"\n first {}\n last {}\n missing_frames \"{}\"\n}}",
            text_knob(&self.knobs, "file", DEFAULT_FILE),
            int_knob(&self.knobs, "first", DEFAULT_FIRST),
            int_knob(&self.knobs, "last", DEFAULT_LAST),
            text_knob(&self.knobs, "missing_frames", DEFAULT_MISSING_FRAMES)
        )
    }
}

/// Resolves the requested frame, optionally searching for the nearest file.
fn resolve_frame_path(file: &str, time: f64, first: i64, last: i64, missing: &str) -> String {
    if missing != "nearest" {
        return resolve_sequence_path(file, time);
    }

    let clamped = (time.round() as i64).clamp(first, last);
    let span = last.saturating_sub(first);
    for offset in 0..=span {
        for candidate in [
            clamped.saturating_sub(offset),
            clamped.saturating_add(offset),
        ] {
            if candidate < first || candidate > last {
                continue;
            }
            let path = resolve_sequence_path(file, candidate as f64);
            if Path::new(&path).exists() {
                return path;
            }
        }
    }

    resolve_sequence_path(file, clamped as f64)
}

fn missing_packet(missing: &str) -> Result<Packet, NodeError> {
    if missing == "black" {
        Ok(packet_from_raster(uniform_raster(
            Format::hd_1080(),
            MISSING_BLACK,
        )))
    } else {
        Err(NodeError::Other("EXR read failed".to_string()))
    }
}

fn packet_from_raster(raster: Raster2D) -> Packet {
    let mut packet = Packet::new();
    packet.insert(raster);
    packet
}

/// Builds a solid raster out of collapsed uniform tiles.
fn uniform_raster(format: Format, color: [f32; 4]) -> Raster2D {
    let mut raster = Raster2D::for_format(format);
    let mut red = ChannelPlane::new();
    let mut green = ChannelPlane::new();
    let mut blue = ChannelPlane::new();
    let mut alpha = ChannelPlane::new();

    for tile in raster.bbox.tile_coords() {
        red.insert(tile, Arc::new(TileBuffer::new_uniform(color[0])));
        green.insert(tile, Arc::new(TileBuffer::new_uniform(color[1])));
        blue.insert(tile, Arc::new(TileBuffer::new_uniform(color[2])));
        alpha.insert(tile, Arc::new(TileBuffer::new_uniform(color[3])));
    }

    raster.insert_channel(Channel::RgbaRed, red);
    raster.insert_channel(Channel::RgbaGreen, green);
    raster.insert_channel(Channel::RgbaBlue, blue);
    raster.insert_channel(Channel::RgbaAlpha, alpha);
    raster
}

fn factory() -> Arc<dyn NodeTrait> {
    Arc::new(ReadNode::new())
}

inventory::submit! {
    NodeCartridgeDescriptor {
        kind: "Read",
        default_knobs,
        factory,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    use crossbeam_channel::unbounded;
    use exr::prelude::write_rgba_file;

    use super::*;

    static NEXT_FILE: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(tag: &str) -> PathBuf {
        let id = NEXT_FILE.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("tanova-read-{}-{tag}-{id}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn read_knobs(file: &str, first: i64, last: i64, missing: &str) -> KnobTable {
        let mut knobs = KnobTable::with_capacity(4);
        knobs.insert("file", KnobValue::Text(file.to_string()));
        knobs.insert("first", KnobValue::Int(first));
        knobs.insert("last", KnobValue::Int(last));
        knobs.insert("missing_frames", KnobValue::Text(missing.to_string()));
        knobs
    }

    fn context(knobs: KnobTable, time: f64) -> EvalContext {
        let mut ctx = EvalContext::new();
        ctx.insert(knobs);
        ctx.insert(tanova_kernel::Time(time));
        ctx
    }

    fn wait_until(mut predicate: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if predicate() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        predicate()
    }

    fn solid_raster(color: [f32; 4]) -> Raster2D {
        uniform_raster(Format::new("test", 1, 1, 1.0), color)
    }

    #[test]
    fn missing_file_errors_by_default() {
        let dir = temp_dir("missing-error");
        let path = dir.join("frame.%04d.exr");
        let ctx = context(
            read_knobs(path.to_str().expect("utf-8 path"), 1, 10, "error"),
            1.0,
        );

        let result = ReadNode::new().evaluate(&ctx, &[]);
        let _ = std::fs::remove_dir_all(&dir);

        assert!(matches!(
            result,
            Err(NodeError::Other(message)) if message == "EXR read failed"
        ));
    }

    #[test]
    fn missing_file_can_be_black() {
        let dir = temp_dir("missing-black");
        let path = dir.join("frame.%04d.exr");
        let ctx = context(
            read_knobs(path.to_str().expect("utf-8 path"), 1, 10, "black"),
            1.0,
        );

        let packet = ReadNode::new()
            .evaluate(&ctx, &[])
            .expect("black fallback should succeed");
        let raster = packet.get::<Raster2D>().expect("raster");
        let pixel = raster.sample_rgba(0, 0);
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(pixel, MISSING_BLACK);
    }

    #[test]
    fn nearest_resolves_to_an_existing_frame() {
        let dir = temp_dir("nearest");
        let template = dir.join("plate.%04d.exr");

        for (frame, color) in [(5_i32, (1.0_f32, 0.0, 0.0, 1.0)), (7, (0.0, 1.0, 0.0, 1.0))] {
            let path = resolve_sequence_path(template.to_str().expect("utf-8 path"), frame as f64);
            write_rgba_file(&path, 2, 2, |_, _| color).expect("write test EXR");
        }

        let ctx = context(
            read_knobs(template.to_str().expect("utf-8 path"), 1, 10, "nearest"),
            6.0,
        );
        let packet = ReadNode::new()
            .evaluate(&ctx, &[])
            .expect("nearest frame should load");
        let raster = packet.get::<Raster2D>().expect("raster");
        let pixel = raster.sample_rgba(0, 0);
        let _ = std::fs::remove_dir_all(&dir);

        // Frame 5 is one step away from frame 6; frame 7 is also one step away.
        // The search checks the lower frame first.
        assert_eq!(pixel, [1.0, 0.0, 0.0, 1.0]);
    }

    #[test]
    fn a_ready_cache_entry_is_returned_directly() {
        let (kernel_tx, _kernel_rx) = unbounded();
        let cache = DiskCache::with_loader(kernel_tx, |_path, _governor| {
            Ok(solid_raster([0.25, 0.5, 0.75, 1.0]))
        });

        let path = "/virtual/ready.exr".to_string();
        let _ = cache.get_or_load(path.clone(), None);
        assert!(wait_until(|| matches!(
            cache.status(&path),
            CacheStatus::Ready(_)
        )));

        let mut ctx = context(read_knobs(&path, 1, 1, "error"), 1.0);
        ctx.insert(cache);

        let packet = ReadNode::new()
            .evaluate(&ctx, &[])
            .expect("ready cache entry should evaluate");
        let raster = packet.get::<Raster2D>().expect("raster");
        assert_eq!(raster.sample_rgba(0, 0), [0.25, 0.5, 0.75, 1.0]);
    }

    #[test]
    fn a_loading_frame_returns_fifty_percent_gray() {
        let dir = temp_dir("loading");
        let path = dir.join("loading.exr");
        std::fs::write(&path, b"not an exr yet").expect("create placeholder file");

        let (kernel_tx, _kernel_rx) = unbounded();
        let cache = DiskCache::with_loader(kernel_tx, |_path, _governor| {
            std::thread::sleep(Duration::from_millis(100));
            Ok(solid_raster([0.25, 0.5, 0.75, 1.0]))
        });

        let mut ctx = context(
            read_knobs(path.to_str().expect("utf-8 path"), 1, 1, "error"),
            1.0,
        );
        ctx.insert(cache.clone());

        let packet = ReadNode::new()
            .evaluate(&ctx, &[])
            .expect("loading fallback should succeed");
        let raster = packet.get::<Raster2D>().expect("raster");
        assert_eq!(raster.sample_rgba(0, 0), LOADING_GRAY);

        assert!(wait_until(|| {
            matches!(
                cache.status(path.to_str().expect("utf-8 path")),
                CacheStatus::Ready(_)
            )
        }));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

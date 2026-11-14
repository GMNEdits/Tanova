//! `Write`: renders a frame sequence to multi-channel OpenEXR on disk.
//!
//! The cartridge is a **pure zero-copy passthrough** of `PortId(0)` during
//! interactive evaluation, so wiring `Grade -> Write -> Viewer` lets the
//! artist view the `Write` node's output without writing files. File output
//! only happens when the kernel processes `KernelMessage::ExecuteWriteNode`.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use tanova_io::{resolve_sequence_path, write_exr_to_path};
use tanova_kernel::{
    EvalContext, KnobTable, KnobValue, NodeCartridgeDescriptor, NodeError, NodeTrait, Packet,
};
use tanova_render::Raster2D;

use crate::util::{int_knob, text_knob};

/// Default `file` knob: a padded sequence pattern.
pub const DEFAULT_FILE: &str = "render.%04d.exr";
/// Default `channels` knob.
pub const DEFAULT_CHANNELS: &str = "rgba";
/// Default first frame.
pub const DEFAULT_FIRST: i64 = 1;
/// Default last frame.
pub const DEFAULT_LAST: i64 = 1;

/// Default knob values for a fresh [`WriteNode`].
pub fn default_knobs() -> KnobTable {
    let mut knobs = KnobTable::with_capacity(4);
    knobs.insert("file", KnobValue::Text(DEFAULT_FILE.to_string()));
    knobs.insert("channels", KnobValue::Text(DEFAULT_CHANNELS.to_string()));
    knobs.insert("first", KnobValue::Int(DEFAULT_FIRST));
    knobs.insert("last", KnobValue::Int(DEFAULT_LAST));
    knobs
}

/// A `Write` node: passes its input through interactively, writes to disk on
/// `ExecuteWriteNode`.
#[derive(Debug, Clone)]
pub struct WriteNode {
    knobs: KnobTable,
}

impl Default for WriteNode {
    fn default() -> Self {
        Self {
            knobs: default_knobs(),
        }
    }
}

impl WriteNode {
    /// Creates a writer with default knobs.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a writer over an existing knob table.
    pub fn from_knobs(knobs: KnobTable) -> Self {
        Self { knobs }
    }

    /// Borrows the node's knobs.
    pub fn knobs(&self) -> &KnobTable {
        &self.knobs
    }
}

impl NodeTrait for WriteNode {
    fn evaluate(&self, _ctx: &EvalContext, inputs: &[Packet]) -> Result<Packet, NodeError> {
        // Interactive evaluation is a pure passthrough: no file I/O happens
        // here, so scratching sliders or viewing the `Write` node never
        // touches disk.
        Ok(inputs.first().cloned().unwrap_or_default())
    }

    fn serialize_to_tcl(&self) -> String {
        format!(
            "Write {{\n file \"{}\"\n channels \"{}\"\n first {}\n last {}\n}}",
            text_knob(&self.knobs, "file", DEFAULT_FILE),
            text_knob(&self.knobs, "channels", DEFAULT_CHANNELS),
            int_knob(&self.knobs, "first", DEFAULT_FIRST),
            int_knob(&self.knobs, "last", DEFAULT_LAST),
        )
    }
}

/// Exports a single evaluated frame to disk using the `Write` node's knobs.
///
/// Resolves the `"file"` pattern via [`resolve_sequence_path`] for `frame`
/// when it contains `%` or `#` tokens. For plain filenames (e.g. `shot.exr`),
/// auto-numbers each frame as `{base_stem}.{:04}.{ext}` so rendering frames
/// `1..=3` produces distinct files (`shot.0001.exr`, `shot.0002.exr`,
/// `shot.0003.exr`) instead of overwriting the same file.
pub fn export_packet_with_write_knobs(
    packet: &Packet,
    knobs: &KnobTable,
    frame: f64,
) -> Result<PathBuf, NodeError> {
    let raster = packet
        .get::<Raster2D>()
        .ok_or_else(|| NodeError::ExecutionFailed("Write input has no Raster2D image".into()))?;

    let file = text_knob(knobs, "file", DEFAULT_FILE);
    if file.is_empty() {
        return Err(NodeError::ExecutionFailed(
            "Write `file` knob is empty".into(),
        ));
    }
    let channels = text_knob(knobs, "channels", DEFAULT_CHANNELS);

    let resolved_path = if file.contains('%') || file.contains('#') {
        // Nuke-style sequence token: resolve via the standard resolver.
        let resolved_str = resolve_sequence_path(&file, frame);
        let mut resolved_path = PathBuf::from(resolved_str);
        if resolved_path.extension().is_none() {
            resolved_path.set_extension("exr");
        }
        resolved_path
    } else {
        // Plain filename: auto-number each frame as {base_stem}.{:04}.{ext}.
        let path = Path::new(&file);
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let stem = path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("exr");

        // Strip trailing frame suffix (dot+digits or underscore+digits) from
        // the stem so the user can select an existing frame and still get
        // clean auto-numbering.
        let base_stem = strip_trailing_frame_suffix(&stem);

        parent.join(format!("{base_stem}.{:04}.{ext}", frame as i32))
    };

    write_exr_to_path(&resolved_path, raster, &channels)?;
    Ok(resolved_path)
}

/// Strips a trailing frame suffix from a stem: either `.<digits>` or
/// `_<digits>` at the end. Returns the stem unchanged if no such suffix
/// exists.
fn strip_trailing_frame_suffix(stem: &str) -> String {
    // Check for `_<digits>` suffix first (e.g. `shot_001` -> `shot`).
    if let Some(underscore_pos) = stem.rfind('_') {
        let after = &stem[underscore_pos + 1..];
        if !after.is_empty() && after.chars().all(|c| c.is_ascii_digit()) {
            return stem[..underscore_pos].to_string();
        }
    }
    // Check for `.<digits>` suffix (e.g. `shot.0001` -> `shot`).
    if let Some(dot_pos) = stem.rfind('.') {
        let after = &stem[dot_pos + 1..];
        if !after.is_empty() && after.chars().all(|c| c.is_ascii_digit()) {
            return stem[..dot_pos].to_string();
        }
    }
    stem.to_string()
}

fn factory() -> Arc<dyn NodeTrait> {
    Arc::new(WriteNode::new())
}

inventory::submit! {
    NodeCartridgeDescriptor {
        kind: "Write",
        default_knobs,
        factory,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_write_knobs() {
        let knobs = default_knobs();
        assert_eq!(
            knobs.get("file"),
            Some(&KnobValue::Text(DEFAULT_FILE.to_string()))
        );
        assert_eq!(
            knobs.get("channels"),
            Some(&KnobValue::Text(DEFAULT_CHANNELS.to_string()))
        );
        assert_eq!(knobs.get("first"), Some(&KnobValue::Int(DEFAULT_FIRST)));
        assert_eq!(knobs.get("last"), Some(&KnobValue::Int(DEFAULT_LAST)));
    }

    #[test]
    fn write_is_passthrough() {
        let node = WriteNode::new();
        let packet = tanova_kernel::Packet::new();
        let result = node.evaluate(&EvalContext::new(), &[packet]);
        assert!(result.is_ok());
        assert!(result.unwrap().is_empty());
    }

    #[test]
    fn serialization_lists_the_nuke_knobs() {
        let node = WriteNode::new();
        assert_eq!(
            node.serialize_to_tcl(),
            "Write {\n file \"render.%04d.exr\"\n channels \"rgba\"\n first 1\n last 1\n}"
        );
    }
}

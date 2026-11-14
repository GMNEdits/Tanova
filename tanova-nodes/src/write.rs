//! `Write`: renders a frame sequence to multi-channel OpenEXR on disk.
//!
//! The cartridge is a **pure zero-copy passthrough** of `PortId(0)` during
//! interactive evaluation, so wiring `Grade -> Write -> Viewer` lets the
//! artist view the `Write` node's output without writing files. File output
//! only happens when the kernel processes `KernelMessage::ExecuteWriteNode`.

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
/// Resolves the `"file"` pattern via [`resolve_sequence_path`] for `frame`,
/// appends `.exr` if the resolved path has no extension, and writes via
/// [`write_exr_to_path`].
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

    let resolved_str = resolve_sequence_path(&file, frame);
    let mut resolved_path = PathBuf::from(resolved_str);
    if resolved_path.extension().is_none() {
        resolved_path.set_extension("exr");
    }

    write_exr_to_path(&resolved_path, raster, &channels)?;
    Ok(resolved_path)
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

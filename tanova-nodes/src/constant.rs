//! `Constant`: a solid-color source with no upstream inputs.
//!
//! Every tile is emitted as a collapsed [`TileBuffer::new_uniform`], so a
//! 1920x1080 constant costs no dense tile memory at all.

use std::sync::Arc;

use tanova_kernel::{
    EvalContext, KnobTable, KnobValue, NodeCartridgeDescriptor, NodeError, NodeTrait, Packet,
};
use tanova_render::{Channel, ChannelPlane, Format, Raster2D, TileBuffer};

use crate::util::{color_knob, format_color, int_knob};

/// Default solid color: opaque black.
pub const DEFAULT_COLOR: [f32; 4] = [0.0, 0.0, 0.0, 1.0];

/// Default width in pixels.
pub const DEFAULT_WIDTH: i64 = 1920;

/// Default height in pixels.
pub const DEFAULT_HEIGHT: i64 = 1080;

/// Default knob values for a fresh [`ConstantNode`].
pub fn default_knobs() -> KnobTable {
    let mut knobs = KnobTable::with_capacity(3);
    knobs.insert("color", KnobValue::Color(DEFAULT_COLOR));
    knobs.insert("format_width", KnobValue::Int(DEFAULT_WIDTH));
    knobs.insert("format_height", KnobValue::Int(DEFAULT_HEIGHT));
    knobs
}

/// A solid-color image source.
#[derive(Debug, Clone)]
pub struct ConstantNode {
    knobs: KnobTable,
}

impl Default for ConstantNode {
    fn default() -> Self {
        Self {
            knobs: default_knobs(),
        }
    }
}

impl ConstantNode {
    /// Creates a node with default knobs.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a node over an existing knob table.
    pub fn from_knobs(knobs: KnobTable) -> Self {
        Self { knobs }
    }

    /// Borrows the node's knobs.
    pub fn knobs(&self) -> &KnobTable {
        &self.knobs
    }
}

impl NodeTrait for ConstantNode {
    fn evaluate(&self, ctx: &EvalContext, _inputs: &[Packet]) -> Result<Packet, NodeError> {
        let knobs = ctx.knobs().unwrap_or(&self.knobs);
        let color = color_knob(knobs, "color", DEFAULT_COLOR);

        // Use the project format from EvalContext when available, so every
        // Constant in the graph shares the active project resolution
        // (e.g. 256x256) instead of its own default (1920x1080).
        let (width, height) = match ctx.project_format() {
            Some(fmt) => (fmt.width, fmt.height),
            None => {
                let width =
                    int_knob(knobs, "format_width", DEFAULT_WIDTH).clamp(1, i32::MAX as i64) as u32;
                let height = int_knob(knobs, "format_height", DEFAULT_HEIGHT)
                    .clamp(1, i32::MAX as i64) as u32;
                (width, height)
            }
        };

        let format = Format::new("Constant", width, height, 1.0);
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

        let mut packet = Packet::new();
        packet.insert(raster);
        Ok(packet)
    }

    fn serialize_to_tcl(&self) -> String {
        let color = color_knob(&self.knobs, "color", DEFAULT_COLOR);
        format!(
            "Constant {{\n color {{{}}}\n format_width {}\n format_height {}\n}}",
            format_color(color),
            int_knob(&self.knobs, "format_width", DEFAULT_WIDTH),
            int_knob(&self.knobs, "format_height", DEFAULT_HEIGHT)
        )
    }
}

fn factory() -> Arc<dyn NodeTrait> {
    Arc::new(ConstantNode::new())
}

inventory::submit! {
    NodeCartridgeDescriptor {
        kind: "Constant",
        default_knobs,
        factory,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_constant_emits_uniform_tiles() {
        let packet = ConstantNode::new()
            .evaluate(&EvalContext::new(), &[])
            .expect("constant should evaluate");
        let raster = packet.get::<Raster2D>().expect("raster");

        assert_eq!(raster.bbox.width(), 1920);
        assert_eq!(raster.bbox.height(), 1080);
        assert_eq!(raster.channels.len(), 4);

        let red = raster.channel(&Channel::RgbaRed).expect("red plane");
        assert!(
            red.iter().all(|(_, tile)| tile.is_uniform() == Some(0.0)),
            "every constant tile stays collapsed"
        );
        let alpha = raster.channel(&Channel::RgbaAlpha).expect("alpha plane");
        assert!(alpha.iter().all(|(_, tile)| tile.is_uniform() == Some(1.0)));
    }

    #[test]
    fn serialization_lists_the_nuke_knobs() {
        let node = ConstantNode::new();
        assert_eq!(
            node.serialize_to_tcl(),
            "Constant {\n color {0 0 0 1}\n format_width 1920\n format_height 1080\n}"
        );
    }
}

//! Checkerboard source in bottom-left world coordinates.

use std::sync::Arc;

use crate::constant::{DEFAULT_HEIGHT, DEFAULT_WIDTH};
use crate::util::{color_knob, format_color, int_knob};
use tanova_kernel::{
    EvalContext, KnobTable, KnobValue, NodeCartridgeDescriptor, NodeError, NodeTrait, Packet,
};
use tanova_render::{Channel, ChannelPlane, Format, Raster2D, TILE_PIXELS, TILE_SIZE, TileBuffer};

const COLOR0: [f32; 4] = [0.1, 0.1, 0.1, 1.0];
const COLOR1: [f32; 4] = [0.5, 0.5, 0.5, 1.0];

pub fn default_knobs() -> KnobTable {
    let mut knobs = KnobTable::with_capacity(5);
    knobs.insert("boxsize", KnobValue::Int(64));
    knobs.insert("color0", KnobValue::Color(COLOR0));
    knobs.insert("color1", KnobValue::Color(COLOR1));
    knobs.insert("format_width", KnobValue::Int(DEFAULT_WIDTH));
    knobs.insert("format_height", KnobValue::Int(DEFAULT_HEIGHT));
    knobs
}

#[derive(Debug, Clone)]
pub struct CheckerBoardNode {
    knobs: KnobTable,
}

impl Default for CheckerBoardNode {
    fn default() -> Self {
        Self::from_knobs(default_knobs())
    }
}

impl CheckerBoardNode {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_knobs(knobs: KnobTable) -> Self {
        Self { knobs }
    }
}

struct CheckerBoardOp {
    boxsize: i64,
    colors: [[f32; 4]; 2],
    width: i64,
    height: i64,
}

impl CheckerBoardOp {
    fn from_knobs(knobs: &KnobTable) -> Self {
        Self {
            boxsize: int_knob(knobs, "boxsize", 64),
            colors: [
                color_knob(knobs, "color0", COLOR0),
                color_knob(knobs, "color1", COLOR1),
            ],
            width: int_knob(knobs, "format_width", DEFAULT_WIDTH),
            height: int_knob(knobs, "format_height", DEFAULT_HEIGHT),
        }
    }

    fn dimensions(&self, ctx: &EvalContext) -> (u32, u32) {
        if (self.width, self.height) == (DEFAULT_WIDTH, DEFAULT_HEIGHT)
            && let Some(format) = ctx.project_format()
        {
            return (format.width, format.height);
        }
        (
            self.width.clamp(1, 8192) as u32,
            self.height.clamp(1, 8192) as u32,
        )
    }
}

impl NodeTrait for CheckerBoardNode {
    fn evaluate(&self, ctx: &EvalContext, _inputs: &[Packet]) -> Result<Packet, NodeError> {
        let op = CheckerBoardOp::from_knobs(ctx.knobs().unwrap_or(&self.knobs));
        let (width, height) = op.dimensions(ctx);
        let mut output = Raster2D::for_format(Format::new("CheckerBoard2", width, height, 1.0));
        let mut planes: [ChannelPlane; 4] = std::array::from_fn(|_| ChannelPlane::new());
        let uniform: [_; 4] = std::array::from_fn(|c| {
            (op.colors[0][c].to_bits() == op.colors[1][c].to_bits())
                .then(|| Arc::new(TileBuffer::new_uniform(op.colors[0][c])))
        });
        let boxsize = op.boxsize.max(1);
        for tile in output.bbox.tile_coords() {
            let mut samples: [Vec<f32>; 4] = std::array::from_fn(|c| {
                if uniform[c].is_some() {
                    Vec::new()
                } else {
                    Vec::with_capacity(TILE_PIXELS)
                }
            });
            if uniform.iter().any(Option::is_none) {
                let (ox, oy) = tile.origin_world();
                for y in 0..i64::from(TILE_SIZE) {
                    let row = (i64::from(oy) + y).div_euclid(boxsize);
                    for x in 0..i64::from(TILE_SIZE) {
                        let column = (i64::from(ox) + x).div_euclid(boxsize);
                        let color = op.colors[(row + column).rem_euclid(2) as usize];
                        for ((samples, uniform), value) in
                            samples.iter_mut().zip(&uniform).zip(color)
                        {
                            if uniform.is_none() {
                                samples.push(value);
                            }
                        }
                    }
                }
            }
            for ((plane, samples), uniform) in planes.iter_mut().zip(samples).zip(&uniform) {
                let buffer = match uniform {
                    Some(buffer) => Arc::clone(buffer),
                    None => Arc::new(TileBuffer::from_vec(samples, ctx.governor())?),
                };
                plane.insert(tile, buffer);
            }
        }
        for (channel, plane) in [
            Channel::RgbaRed,
            Channel::RgbaGreen,
            Channel::RgbaBlue,
            Channel::RgbaAlpha,
        ]
        .into_iter()
        .zip(planes)
        {
            output.insert_channel(channel, plane);
        }
        let mut packet = Packet::new();
        packet.insert(output);
        Ok(packet)
    }

    fn serialize_to_tcl(&self) -> String {
        let op = CheckerBoardOp::from_knobs(&self.knobs);
        format!(
            "CheckerBoard2 {{\n boxsize {}\n color0 {{{}}}\n color1 {{{}}}\n format_width {}\n format_height {}\n}}",
            op.boxsize,
            format_color(op.colors[0]),
            format_color(op.colors[1]),
            op.width,
            op.height
        )
    }
}

inventory::submit! {
    NodeCartridgeDescriptor {
        kind: "CheckerBoard2", default_knobs, factory: || Arc::new(CheckerBoardNode::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tanova_kernel::{MemoryGovernor, ProjectFormat};

    #[test]
    fn checks_follow_world_coordinates_across_tiles() {
        let mut ctx = EvalContext::new();
        ctx.insert(ProjectFormat::new(260, 70));
        let mut knobs = default_knobs();
        knobs.insert("boxsize", KnobValue::Int(65));
        let packet = CheckerBoardNode::from_knobs(knobs)
            .evaluate(&ctx, &[])
            .unwrap();
        let raster = packet.get::<Raster2D>().unwrap();
        assert_eq!((raster.format.width, raster.format.height), (260, 70));
        assert_eq!(raster.sample_rgba(0, 0), COLOR0);
        assert_eq!(raster.sample_rgba(65, 0), COLOR1);
        assert_eq!(raster.sample_rgba(255, 0), COLOR1);
        assert_eq!(raster.sample_rgba(256, 0), COLOR1);
        assert_eq!(raster.sample_rgba(256, 65), COLOR0);
    }

    #[test]
    fn matching_colors_need_no_dense_memory() {
        let mut ctx = EvalContext::new();
        let governor = MemoryGovernor::new(0);
        ctx.insert(governor.clone());
        let mut knobs = default_knobs();
        knobs.insert("color1", KnobValue::Color(COLOR0));
        let packet = CheckerBoardNode::from_knobs(knobs)
            .evaluate(&ctx, &[])
            .unwrap();
        let raster = packet.get::<Raster2D>().unwrap();
        assert_eq!(raster.sample_rgba(1919, 1079), COLOR0);
        assert_eq!(governor.allocated_bytes(), 0);
    }
}

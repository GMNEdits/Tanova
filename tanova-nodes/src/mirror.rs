//! Pixel-exact reflections around the input format.

use std::sync::Arc;

use crate::util::{bool_knob, raster_input};
use tanova_kernel::{
    EvalContext, KnobTable, KnobValue, NodeCartridgeDescriptor, NodeError, NodeTrait, Packet,
};
use tanova_render::{BBox, Channel, TILE_PIXELS, TILE_SIZE, TileBuffer};

const RGBA: [Channel; 4] = [
    Channel::RgbaRed,
    Channel::RgbaGreen,
    Channel::RgbaBlue,
    Channel::RgbaAlpha,
];

pub fn default_knobs() -> KnobTable {
    let mut knobs = KnobTable::with_capacity(2);
    knobs.insert("flip", KnobValue::Bool(false));
    knobs.insert("flop", KnobValue::Bool(false));
    knobs
}

#[derive(Debug, Clone)]
pub struct MirrorNode {
    knobs: KnobTable,
}

impl Default for MirrorNode {
    fn default() -> Self {
        Self::from_knobs(default_knobs())
    }
}

impl MirrorNode {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_knobs(knobs: KnobTable) -> Self {
        Self { knobs }
    }
}

struct MirrorOp {
    flip: bool,
    flop: bool,
}

impl MirrorOp {
    fn from_knobs(knobs: &KnobTable) -> Self {
        Self {
            flip: bool_knob(knobs, "flip", false),
            flop: bool_knob(knobs, "flop", false),
        }
    }

    fn map_bbox(&self, bbox: BBox, width: i32, height: i32) -> BBox {
        let (min_x, max_x) = if self.flop {
            (width - bbox.max_x, width - bbox.min_x)
        } else {
            (bbox.min_x, bbox.max_x)
        };
        let (min_y, max_y) = if self.flip {
            (height - bbox.max_y, height - bbox.min_y)
        } else {
            (bbox.min_y, bbox.max_y)
        };
        BBox::new(min_x, min_y, max_x, max_y)
    }
}

impl NodeTrait for MirrorNode {
    fn evaluate(&self, ctx: &EvalContext, inputs: &[Packet]) -> Result<Packet, NodeError> {
        let input = raster_input(inputs, 0).ok_or(NodeError::MissingInput("main"))?;
        let op = MirrorOp::from_knobs(ctx.knobs().unwrap_or(&self.knobs));
        if !op.flip && !op.flop {
            return Ok(inputs[0].clone());
        }

        let width = input.format.width as i32;
        let height = input.format.height as i32;
        let bbox = op.map_bbox(input.bbox, width, height);
        let mut output = input.clone();
        output.bbox = bbox;
        for channel in RGBA {
            output.channels.insert(channel, Default::default());
        }

        for tile in bbox.tile_coords() {
            let region = tile.to_bbox().intersection(&bbox);
            let source_tiles = op.map_bbox(region, width, height).tile_coords();
            let uniform = RGBA.map(|channel| {
                let Some(plane) = input.channel(&channel) else {
                    return Some(if channel == Channel::RgbaAlpha {
                        1.0
                    } else {
                        0.0
                    });
                };
                let value_at = |tile| match plane.get(tile) {
                    Some(buffer) => buffer.is_uniform(),
                    None => Some(0.0),
                };
                let first = value_at(&source_tiles[0])?;
                source_tiles
                    .iter()
                    .all(|tile| value_at(tile).map(f32::to_bits) == Some(first.to_bits()))
                    .then_some(first)
            });

            let mut dense = uniform.map(|value| value.is_none().then(|| vec![0.0; TILE_PIXELS]));
            if dense.iter().any(Option::is_some) {
                let (ox, oy) = tile.origin_world();
                for y in region.min_y..region.max_y {
                    let sy = if op.flip { height - 1 - y } else { y };
                    for x in region.min_x..region.max_x {
                        let sx = if op.flop { width - 1 - x } else { x };
                        let index = (y - oy) as usize * TILE_SIZE as usize + (x - ox) as usize;
                        for (plane, value) in dense.iter_mut().zip(input.sample_rgba(sx, sy)) {
                            if let Some(values) = plane {
                                values[index] = value;
                            }
                        }
                    }
                }
            }

            for ((channel, value), samples) in RGBA.into_iter().zip(uniform).zip(dense) {
                let buffer = match samples {
                    Some(samples) => TileBuffer::from_vec(samples, ctx.governor())?,
                    None => TileBuffer::new_uniform(value.expect("uniform source plane")),
                };
                output
                    .channels
                    .entry(channel)
                    .or_default()
                    .insert(tile, Arc::new(buffer));
            }
        }

        let mut packet = Packet::new();
        packet.insert(output);
        Ok(packet)
    }

    fn serialize_to_tcl(&self) -> String {
        let op = MirrorOp::from_knobs(&self.knobs);
        format!("Mirror {{\n flip {}\n flop {}\n}}", op.flip, op.flop)
    }
}

inventory::submit! {
    NodeCartridgeDescriptor {
        kind: "Mirror", default_knobs, factory: || Arc::new(MirrorNode::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tanova_kernel::MemoryGovernor;
    use tanova_render::{EdgeMode, Format, Raster2D};

    #[test]
    fn reflects_cropped_half_open_bounds_and_round_trips_pixels() {
        let bbox = BBox::new(254, -1, 258, 2);
        let mut input = Raster2D::new(
            Format::new("test", 300, 10, 1.0),
            bbox,
            EdgeMode::BlackOutside,
        );
        for y in bbox.min_y..bbox.max_y {
            for x in bbox.min_x..bbox.max_x {
                input
                    .set_pixel(Channel::RgbaRed, x, y, (10 * x + y) as f32, None)
                    .unwrap();
            }
        }
        let mut packet = Packet::new();
        packet.insert(input.clone());
        for (flip, flop, expected_bbox) in [
            (true, false, BBox::new(254, 8, 258, 11)),
            (false, true, BBox::new(42, -1, 46, 2)),
            (true, true, BBox::new(42, 8, 46, 11)),
        ] {
            let mut knobs = default_knobs();
            knobs.insert("flip", KnobValue::Bool(flip));
            knobs.insert("flop", KnobValue::Bool(flop));
            let node = MirrorNode::from_knobs(knobs);
            let ctx = EvalContext::new();
            let reflected = node.evaluate(&ctx, std::slice::from_ref(&packet)).unwrap();
            let raster = reflected.get::<Raster2D>().unwrap();
            assert_eq!(raster.bbox, expected_bbox);
            assert_eq!(
                raster.sample_rgba(if flop { 45 } else { 254 }, if flip { 10 } else { -1 }),
                input.sample_rgba(254, -1)
            );

            let restored = node.evaluate(&ctx, &[reflected]).unwrap();
            let raster = restored.get::<Raster2D>().unwrap();
            assert_eq!(raster.bbox, bbox);
            for y in bbox.min_y..bbox.max_y {
                for x in bbox.min_x..bbox.max_x {
                    assert_eq!(raster.sample_rgba(x, y), input.sample_rgba(x, y));
                }
            }
        }
    }

    #[test]
    fn disabled_reflections_reuse_dense_tiles_without_reserving_bytes() {
        let mut input = Raster2D::for_format(Format::new("test", 16, 16, 1.0));
        input.set_pixel(Channel::RgbaRed, 2, 3, 0.75, None).unwrap();
        let tile = input.bbox.tile_coords()[0];
        let buffer = input.channels[&Channel::RgbaRed]
            .get(&tile)
            .unwrap()
            .clone();
        let mut packet = Packet::new();
        packet.insert(input);
        let governor = MemoryGovernor::new(0);
        let mut ctx = EvalContext::new();
        ctx.insert(governor.clone());

        let output = MirrorNode::new().evaluate(&ctx, &[packet]).unwrap();
        let raster = output.get::<Raster2D>().unwrap();
        assert!(Arc::ptr_eq(
            &buffer,
            raster.channels[&Channel::RgbaRed].get(&tile).unwrap()
        ));
        assert_eq!(governor.allocated_bytes(), 0);
    }
}

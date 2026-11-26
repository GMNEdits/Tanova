//! Spatial cropping with shared interior tiles and cropped AOV planes.

use std::sync::Arc;

use crate::util::{bool_knob, color_knob, format_color, raster_input};
use tanova_kernel::{
    EvalContext, KnobTable, KnobValue, NodeCartridgeDescriptor, NodeError, NodeTrait, Packet,
    ProjectFormat,
};
use tanova_render::{
    BBox, ChannelPlane, EdgeMode, Format, Raster2D, TILE_PIXELS, TILE_SIZE, TileBuffer,
};

const DEFAULT_MIN: [f32; 4] = [0.0; 4];
const DEFAULT_MAX: [f32; 4] = [1920.0, 1080.0, 0.0, 0.0];

pub fn default_knobs() -> KnobTable {
    let mut knobs = KnobTable::new();
    knobs.insert("box_min", KnobValue::Color(DEFAULT_MIN));
    knobs.insert("box_max", KnobValue::Color(DEFAULT_MAX));
    knobs.insert("reformat", KnobValue::Bool(false));
    knobs.insert("intersect", KnobValue::Bool(false));
    knobs
}

#[derive(Debug, Clone)]
pub struct CropNode {
    knobs: KnobTable,
}

impl Default for CropNode {
    fn default() -> Self {
        Self::from_knobs(default_knobs())
    }
}

impl CropNode {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_knobs(knobs: KnobTable) -> Self {
        Self { knobs }
    }
}

struct CropOp {
    box_min: [f32; 4],
    box_max: [f32; 4],
    reformat: bool,
    intersect: bool,
}

impl CropOp {
    fn from_knobs(knobs: &KnobTable) -> Self {
        Self {
            box_min: color_knob(knobs, "box_min", DEFAULT_MIN),
            box_max: color_knob(knobs, "box_max", DEFAULT_MAX),
            reformat: bool_knob(knobs, "reformat", false),
            intersect: bool_knob(knobs, "intersect", false),
        }
    }

    fn crop_bbox(&self, project_format: Option<ProjectFormat>, scale: f32) -> BBox {
        let [min_x, min_y, ..] = self.box_min;
        let [mut max_x, mut max_y, ..] = self.box_max;
        if self.box_min == DEFAULT_MIN
            && self.box_max == DEFAULT_MAX
            && let Some(format) = project_format
        {
            max_x = format.width as f32;
            max_y = format.height as f32;
        }
        let pixel = |coord: f32| (coord * scale).round() as i32;
        let min_x = pixel(min_x);
        let min_y = pixel(min_y);
        BBox::new(
            min_x,
            min_y,
            pixel(max_x).max(min_x),
            pixel(max_y).max(min_y),
        )
    }
}

impl NodeTrait for CropNode {
    fn evaluate(&self, ctx: &EvalContext, inputs: &[Packet]) -> Result<Packet, NodeError> {
        let input = raster_input(inputs, 0).ok_or(NodeError::MissingInput("main"))?;
        let op = CropOp::from_knobs(ctx.knobs().unwrap_or(&self.knobs));
        let scale = ctx.proxy_scale();
        let fallback_format = ctx.project_format().filter(|_| ctx.knobs().is_none());
        let crop_bbox = op.crop_bbox(fallback_format, scale);
        let bbox = if op.intersect {
            input.bbox.intersection(&crop_bbox)
        } else {
            crop_bbox
        };
        let format = if op.reformat && !bbox.is_empty() {
            let format = Format::new(
                "Crop",
                bbox.width() as u32,
                bbox.height() as u32,
                input.format.pixel_aspect,
            );
            if scale == 1.0 {
                format
            } else {
                let logical_crop = op.crop_bbox(fallback_format, 1.0);
                let logical = if !op.intersect {
                    logical_crop
                } else {
                    let (w, h) = input.format.logical_size(scale);
                    let logical_input = if input.bbox == input.format.to_bbox() {
                        BBox::new(0, 0, w as i32, h as i32)
                    } else {
                        input
                            .bbox
                            .transform(&tanova_render::Mat3x3::scale(scale.recip(), scale.recip()))
                    };
                    logical_crop.intersection(&logical_input)
                };
                format.with_logical_size((
                    logical.width().max(1) as u32,
                    logical.height().max(1) as u32,
                ))
            }
        } else {
            input.format.clone()
        };
        let mut output = Raster2D::new(format, bbox, EdgeMode::BlackOutside);
        for channel in input.channels.keys() {
            output.insert_channel(channel.clone(), ChannelPlane::new());
        }

        for tile in bbox.tile_coords() {
            let bounds = tile.to_bbox();
            let inside = bounds.intersection(&crop_bbox);
            let source_overlap = inside.intersection(&input.bbox);
            if inside == bounds && (op.intersect || source_overlap == bounds) {
                for (source, target) in input.channels.values().zip(output.channels.values_mut()) {
                    let buffer = source
                        .get(&tile)
                        .cloned()
                        .unwrap_or_else(|| Arc::new(TileBuffer::new_uniform(0.0)));
                    target.insert(tile, buffer);
                }
                continue;
            }
            if source_overlap.is_empty() && input.edge_mode == EdgeMode::BlackOutside {
                let zero = Arc::new(TileBuffer::new_uniform(0.0));
                for plane in output.channels.values_mut() {
                    plane.insert(tile, Arc::clone(&zero));
                }
                continue;
            }

            let mut planes: Vec<_> = input
                .channels
                .values()
                .map(|plane| (plane, Vec::with_capacity(TILE_PIXELS)))
                .collect();
            for y in 0..TILE_SIZE as i32 {
                for x in 0..TILE_SIZE as i32 {
                    let wx = bounds.min_x + x;
                    let wy = bounds.min_y + y;
                    let in_crop = crop_bbox.contains(wx, wy);
                    for (source, values) in &mut planes {
                        values.push(if in_crop {
                            source.sample(wx, wy, input.bbox, input.edge_mode)
                        } else {
                            0.0
                        });
                    }
                }
            }
            for (target, (_, values)) in output.channels.values_mut().zip(planes) {
                target.insert(
                    tile,
                    Arc::new(TileBuffer::from_vec(values, ctx.governor())?),
                );
            }
        }
        let mut packet = Packet::new();
        packet.insert(output);
        Ok(packet)
    }

    fn serialize_to_tcl(&self) -> String {
        let op = CropOp::from_knobs(&self.knobs);
        format!(
            "Crop {{\n box_min {{{}}}\n box_max {{{}}}\n reformat {}\n intersect {}\n}}",
            format_color(op.box_min),
            format_color(op.box_max),
            op.reformat,
            op.intersect,
        )
    }
}

inventory::submit! {
    NodeCartridgeDescriptor { kind: "Crop", default_knobs, factory: || Arc::new(CropNode::new()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tanova_render::{Channel, TileCoord};

    #[test]
    fn default_box_uses_project_format_and_invalid_extents_collapse() {
        let mut knobs = default_knobs();
        let project = Some(ProjectFormat::new(640, 480));
        assert_eq!(
            CropOp::from_knobs(&knobs).crop_bbox(project, 1.0),
            BBox::new(0, 0, 640, 480)
        );
        knobs.insert("box_min", KnobValue::Color([-1.6, 1081.0, 0.0, 0.0]));
        assert_eq!(
            CropOp::from_knobs(&knobs).crop_bbox(project, 1.0),
            BBox::new(-2, 1081, 1920, 1081)
        );
    }

    #[test]
    fn crop_intersects_reformats_and_shares_interior_aov_tiles() {
        let mut raster = Raster2D::for_format(Format::new("test", 512, 256, 1.5));
        let mut depth = ChannelPlane::new();
        let buffer = Arc::new(TileBuffer::new_uniform(42.0));
        depth.insert(TileCoord::new(0, 0), Arc::clone(&buffer));
        depth.insert(TileCoord::new(1, 0), Arc::clone(&buffer));
        raster.insert_channel(Channel::DepthZ, depth);
        let mut input = Packet::new();
        input.insert(raster);
        let mut knobs = default_knobs();
        knobs.insert("box_min", KnobValue::Color([-5.0, 0.0, 0.0, 0.0]));
        knobs.insert("box_max", KnobValue::Color([258.0, 256.0, 0.0, 0.0]));
        knobs.insert("intersect", KnobValue::Bool(true));
        knobs.insert("reformat", KnobValue::Bool(true));
        let output = CropNode::from_knobs(knobs)
            .evaluate(&EvalContext::new(), &[input])
            .unwrap();
        let raster = output.get::<Raster2D>().unwrap();
        assert_eq!(raster.bbox, BBox::new(0, 0, 258, 256));
        assert_eq!(raster.format, Format::new("Crop", 258, 256, 1.5));
        let depth = raster.channel(&Channel::DepthZ).unwrap();
        assert!(Arc::ptr_eq(
            &buffer,
            depth.get(&TileCoord::new(0, 0)).unwrap()
        ));
        let boundary = depth.get(&TileCoord::new(1, 0)).unwrap();
        assert_eq!(boundary.get(1, 0), 42.0);
        assert_eq!(boundary.get(2, 0), 0.0);
    }
}

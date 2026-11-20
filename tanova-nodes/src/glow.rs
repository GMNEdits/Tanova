//! Thresholded RGB bloom with Gaussian tile aprons and unchanged base alpha.

use std::sync::Arc;

use crate::util::{bool_knob, float_knob, format_float, raster_input};
use tanova_kernel::{
    EvalContext, KnobTable, KnobValue, NodeCartridgeDescriptor, NodeError, NodeTrait, Packet,
};
use tanova_render::ops::{uniform_mask_value, uniform_rgba};
use tanova_render::{
    BBox, Channel, ChannelPlane, EvalContextGpu, Raster2D, TILE_PIXELS, TILE_SIZE, TileBuffer,
    TileCoord, blur_padded_tile, extract_padded_tile, is_rgba_channel, mask_value,
};

const RGB: [Channel; 3] = [Channel::RgbaRed, Channel::RgbaGreen, Channel::RgbaBlue];
const RGBA: [Channel; 4] = [
    Channel::RgbaRed,
    Channel::RgbaGreen,
    Channel::RgbaBlue,
    Channel::RgbaAlpha,
];

pub fn default_knobs() -> KnobTable {
    let mut knobs = KnobTable::with_capacity(5);
    knobs.insert("tolerance", KnobValue::Float(0.5));
    knobs.insert("brightness", KnobValue::Float(1.0));
    knobs.insert("size", KnobValue::Float(10.0));
    knobs.insert("effect_only", KnobValue::Bool(false));
    knobs.insert("mix", KnobValue::Float(1.0));
    knobs
}

#[derive(Debug, Clone)]
pub struct GlowNode {
    knobs: KnobTable,
}

impl Default for GlowNode {
    fn default() -> Self {
        Self::from_knobs(default_knobs())
    }
}

impl GlowNode {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_knobs(knobs: KnobTable) -> Self {
        Self { knobs }
    }
}

struct GlowOp {
    tolerance: f32,
    brightness: f32,
    size: f32,
    effect_only: bool,
    mix: f32,
}

impl GlowOp {
    fn from_knobs(knobs: &KnobTable) -> Self {
        Self {
            tolerance: float_knob(knobs, "tolerance", 0.5),
            brightness: float_knob(knobs, "brightness", 1.0),
            size: float_knob(knobs, "size", 10.0),
            effect_only: bool_knob(knobs, "effect_only", false),
            mix: float_knob(knobs, "mix", 1.0),
        }
    }

    fn extract_highlights(
        &self,
        input: &Raster2D,
        ctx: &EvalContext,
    ) -> Result<Option<Raster2D>, NodeError> {
        let mut highlights = Raster2D::new(input.format.clone(), input.bbox, input.edge_mode);
        let mut planes: [ChannelPlane; 3] = std::array::from_fn(|_| ChannelPlane::new());
        let sources = RGB.map(|channel| input.channel(&channel));
        for tile in input.bbox.tile_coords() {
            let buffers = sources.map(|plane| plane.and_then(|plane| plane.get(&tile)));
            let uniform = buffers.map(|buffer| {
                buffer
                    .map_or(Some(0.0), |buffer| buffer.is_uniform())
                    .map(|value| (value - self.tolerance).max(0.0))
            });
            let mut dense: [Option<Vec<f32>>; 3] = [None, None, None];
            if uniform.iter().any(Option::is_none) {
                let dense_sources: [_; 3] =
                    std::array::from_fn(|c| buffers[c].filter(|_| uniform[c].is_none()));
                let (ox, oy) = tile.origin_world();
                let region = tile.to_bbox().intersection(&input.bbox);
                for y in region.min_y..region.max_y {
                    for x in region.min_x..region.max_x {
                        let (lx, ly) = ((x - ox) as u32, (y - oy) as u32);
                        for (values, source) in dense.iter_mut().zip(dense_sources) {
                            if let Some(source) = source {
                                let value = (source.get(lx, ly) - self.tolerance).max(0.0);
                                if value > 0.0 {
                                    let values =
                                        values.get_or_insert_with(|| vec![0.0; TILE_PIXELS]);
                                    values[ly as usize * TILE_SIZE as usize + lx as usize] = value;
                                }
                            }
                        }
                    }
                }
            }
            for ((plane, uniform), values) in planes.iter_mut().zip(uniform).zip(dense) {
                let buffer = match (uniform, values) {
                    (Some(value), _) if value > 0.0 => TileBuffer::new_uniform(value),
                    (_, Some(values)) => TileBuffer::from_vec(values, ctx.governor())?,
                    _ => continue,
                };
                plane.insert(tile, Arc::new(buffer));
            }
        }
        let any = planes.iter().any(|plane| !plane.is_empty());
        for (channel, plane) in RGB.into_iter().zip(planes) {
            highlights.insert_channel(channel, plane);
        }
        Ok(any.then_some(highlights))
    }

    fn composite(&self, mut base: [f32; 4], glow: [f32; 3], mask: f32) -> [f32; 4] {
        let mix = (self.mix * mask).clamp(0.0, 1.0);
        if mix == 0.0 {
            return base;
        }
        for (base, highlight) in base[..3].iter_mut().zip(glow) {
            let bloom = highlight * self.brightness;
            *base = if self.effect_only {
                if mix == 1.0 {
                    bloom
                } else {
                    *base * (1.0 - mix) + bloom * mix
                }
            } else {
                *base + bloom * mix
            };
        }
        base
    }
}

enum BloomTile {
    Uniform([f32; 3]),
    Dense(Vec<f32>),
}

impl BloomTile {
    fn new(
        highlights: &Raster2D,
        tile: TileCoord,
        radius: u32,
        sigma: f32,
        ctx: &EvalContext,
    ) -> Result<Self, NodeError> {
        if radius == 0 {
            let uniform = RGB.map(|channel| {
                highlights.channels[&channel]
                    .get(&tile)
                    .map_or(Some(0.0), |buffer| buffer.is_uniform())
            });
            if let [Some(r), Some(g), Some(b)] = uniform {
                return Ok(Self::Uniform([r, g, b]));
            }
        }

        let [red, green, blue] = RGB.map(|channel| {
            extract_padded_tile(
                &highlights.channels[&channel],
                tile,
                radius,
                highlights.bbox,
                highlights.edge_mode,
            )
        });
        let uniform = [&red, &green, &blue].map(|values| {
            let first = values[0];
            values.iter().all(|&value| value == first).then_some(first)
        });
        if let [Some(r), Some(g), Some(b)] = uniform {
            return Ok(Self::Uniform([r, g, b]));
        }

        let padded = red
            .into_iter()
            .zip(green)
            .zip(blue)
            .map(|((r, g), b)| [r, g, b, 0.0]);
        let pixels = if radius == 0 {
            padded.flatten().collect()
        } else {
            blur_padded_tile(
                &padded.collect::<Vec<_>>(),
                radius,
                sigma,
                TILE_SIZE + 2 * radius,
                ctx.gpu(),
            )?
        };
        Ok(Self::Dense(pixels))
    }

    fn sample(&self, index: usize) -> [f32; 3] {
        match self {
            Self::Uniform(value) => *value,
            Self::Dense(pixels) => {
                let i = index * 4;
                [pixels[i], pixels[i + 1], pixels[i + 2]]
            }
        }
    }
}

fn output_with_aovs(input: &Raster2D, bbox: BBox) -> Raster2D {
    let mut output = Raster2D::new(input.format.clone(), bbox, input.edge_mode);
    for (channel, plane) in &input.channels {
        if !is_rgba_channel(channel) {
            output.insert_channel(channel.clone(), plane.clone());
        }
    }
    for channel in RGBA {
        output.insert_channel(channel, ChannelPlane::new());
    }
    output
}

impl NodeTrait for GlowNode {
    fn evaluate(&self, ctx: &EvalContext, inputs: &[Packet]) -> Result<Packet, NodeError> {
        let input = raster_input(inputs, 0).ok_or(NodeError::MissingInput("main"))?;
        let op = GlowOp::from_knobs(ctx.knobs().unwrap_or(&self.knobs));
        if op.mix <= 0.0 || (!op.effect_only && op.brightness <= 0.0) {
            return Ok(inputs[0].clone());
        }

        let mut packet = Packet::new();
        let Some(highlights) = op.extract_highlights(input, ctx)? else {
            if !op.effect_only {
                return Ok(inputs[0].clone());
            }
            let mut output = output_with_aovs(input, input.bbox);
            let zero = Arc::new(TileBuffer::new_uniform(0.0));
            for tile in output.bbox.tile_coords() {
                for channel in RGBA {
                    output
                        .channels
                        .entry(channel)
                        .or_default()
                        .insert(tile, Arc::clone(&zero));
                }
            }
            packet.insert(output);
            return Ok(packet);
        };

        let mask = raster_input(inputs, 1);
        let size = op.size.clamp(0.0, 250.0);
        let radius = size.ceil() as u32;
        let sigma = (size / 3.0).max(1e-5);
        let bbox = input.bbox.pad(radius as i32);
        let mut output = output_with_aovs(input, bbox);

        for tile in bbox.tile_coords() {
            let glow = BloomTile::new(&highlights, tile, radius, sigma, ctx)?;
            let region = tile.to_bbox().intersection(&bbox);
            let overlap = region.intersection(&input.bbox);
            let base_at = |x, y| {
                if input.bbox.contains(x, y) {
                    input.sample_rgba(x, y)
                } else {
                    [0.0; 4]
                }
            };
            let uniform_base = if overlap.is_empty() {
                Some([0.0; 4])
            } else if overlap == region {
                uniform_rgba(input, tile, region).map(|mut rgba| {
                    if input.channel(&Channel::RgbaAlpha).is_none() {
                        rgba[3] = 1.0;
                    }
                    rgba
                })
            } else {
                None
            };
            let uniform_mask = match mask {
                Some(mask) if region.intersection(&mask.bbox).is_empty() => Some(0.0),
                Some(mask) if region.intersection(&mask.bbox) != region => None,
                _ => uniform_mask_value(mask, tile, region),
            };
            if let (Some(base), BloomTile::Uniform(glow), Some(mask)) =
                (uniform_base, &glow, uniform_mask)
            {
                for (channel, value) in RGBA.into_iter().zip(op.composite(base, *glow, mask)) {
                    output
                        .channels
                        .entry(channel)
                        .or_default()
                        .insert(tile, Arc::new(TileBuffer::new_uniform(value)));
                }
                continue;
            }

            let (ox, oy) = tile.origin_world();
            let tile_index =
                |x: i32, y: i32| (y - oy) as usize * TILE_SIZE as usize + (x - ox) as usize;
            let mut first = None;
            let mut uniform = [true; 4];
            let mut values: [Vec<f32>; 4] = std::array::from_fn(|_| vec![0.0; TILE_PIXELS]);
            for y in region.min_y..region.max_y {
                for x in region.min_x..region.max_x {
                    let index = tile_index(x, y);
                    let rgba =
                        op.composite(base_at(x, y), glow.sample(index), mask_value(mask, x, y));
                    let first = first.get_or_insert(rgba);
                    for (c, value) in rgba.into_iter().enumerate() {
                        values[c][index] = value;
                        uniform[c] &= value.to_bits() == first[c].to_bits();
                    }
                }
            }
            let first = first.expect("nonempty output tile");
            for (c, (channel, values)) in RGBA.into_iter().zip(values).enumerate() {
                let buffer = if uniform[c] {
                    TileBuffer::new_uniform(first[c])
                } else {
                    TileBuffer::from_vec(values, ctx.governor())?
                };
                output
                    .channels
                    .entry(channel)
                    .or_default()
                    .insert(tile, Arc::new(buffer));
            }
        }
        packet.insert(output);
        Ok(packet)
    }

    fn serialize_to_tcl(&self) -> String {
        let op = GlowOp::from_knobs(&self.knobs);
        format!(
            "Glow {{\n tolerance {}\n brightness {}\n size {}\n effect_only {}\n mix {}\n}}",
            format_float(op.tolerance),
            format_float(op.brightness),
            format_float(op.size),
            op.effect_only,
            format_float(op.mix),
        )
    }
}

inventory::submit! {
    NodeCartridgeDescriptor {
        kind: "Glow", default_knobs, factory: || Arc::new(GlowNode::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tanova_kernel::MemoryGovernor;
    use tanova_render::Format;

    #[test]
    fn below_threshold_bypasses_or_emits_transparent_uniform_tiles() {
        let mut input = Raster2D::for_format(Format::new("test", 8, 8, 1.0));
        input.set_pixel(Channel::RgbaRed, 2, 2, 0.5, None).unwrap();
        let tile = TileCoord::new(0, 0);
        let red = input.channels[&Channel::RgbaRed]
            .get(&tile)
            .unwrap()
            .clone();
        let mut packet = Packet::new();
        packet.insert(input);
        let governor = MemoryGovernor::new(0);
        let mut ctx = EvalContext::new();
        ctx.insert(governor.clone());

        let output = GlowNode::new()
            .evaluate(&ctx, std::slice::from_ref(&packet))
            .unwrap();
        let raster = output.get::<Raster2D>().unwrap();
        assert!(Arc::ptr_eq(
            &red,
            raster.channels[&Channel::RgbaRed].get(&tile).unwrap()
        ));

        let mut knobs = default_knobs();
        knobs.insert("effect_only", KnobValue::Bool(true));
        let output = GlowNode::from_knobs(knobs)
            .evaluate(&ctx, &[packet])
            .unwrap();
        let raster = output.get::<Raster2D>().unwrap();
        assert_eq!(raster.bbox, BBox::new(0, 0, 8, 8));
        assert_eq!(raster.sample_rgba(2, 2), [0.0; 4]);
        assert_eq!(governor.allocated_bytes(), 0);
    }

    #[test]
    fn highlight_spreads_across_a_tile_boundary_and_preserves_depth_and_alpha() {
        let mut input = Raster2D::for_format(Format::new("test", 256, 16, 1.0));
        let tile = TileCoord::new(0, 0);
        let depth = Arc::new(TileBuffer::new_uniform(42.0));
        let mut plane = ChannelPlane::new();
        plane.insert(tile, Arc::clone(&depth));
        input.insert_channel(Channel::DepthZ, plane);
        input
            .set_pixel(Channel::RgbaRed, 255, 8, 2.0, None)
            .unwrap();
        let mut packet = Packet::new();
        packet.insert(input);
        let mut knobs = default_knobs();
        knobs.insert("size", KnobValue::Float(3.0));
        let output = GlowNode::from_knobs(knobs)
            .evaluate(&EvalContext::new(), &[packet])
            .unwrap();
        let raster = output.get::<Raster2D>().unwrap();
        assert_eq!(raster.bbox, BBox::new(-3, -3, 259, 19));
        let center = raster.sample_rgba(255, 8);
        let inner = raster.sample_rgba(254, 8);
        let halo = raster.sample_rgba(256, 8);
        let outer = raster.sample_rgba(257, 8);
        assert!(center[0] > 2.0);
        assert!(inner[0] > outer[0] && outer[0] > 0.0);
        assert_eq!(halo, [inner[0], 0.0, 0.0, 0.0]);
        assert_eq!(center[3], 1.0);
        assert!(Arc::ptr_eq(
            &depth,
            raster.channels[&Channel::DepthZ].get(&tile).unwrap()
        ));
    }
}

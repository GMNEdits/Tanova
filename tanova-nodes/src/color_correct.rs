//! Nuke-style ColorCorrect cartridge with governed tiles and zero-copy AOVs.
use crate::util::{float_knob, raster_input};
use std::sync::Arc;
use tanova_kernel::{
    EvalContext, KnobTable, KnobValue, NodeCartridgeDescriptor, NodeError, NodeTrait, Packet,
};
use tanova_render::ops::{uniform_mask_value, uniform_rgba};
use tanova_render::{Channel, ChannelPlane, TILE_PIXELS, TILE_SIZE, TileBuffer};
/// Default knob values for a fresh cartridge.
pub fn default_knobs() -> KnobTable {
    let mut knobs = KnobTable::new();
    knobs.insert("saturation", KnobValue::Float(1.0));
    knobs.insert("contrast", KnobValue::Float(1.0));
    knobs.insert("gamma", KnobValue::Float(1.0));
    knobs.insert("gain", KnobValue::Float(1.0));
    knobs.insert("offset", KnobValue::Float(0.0));
    knobs.insert("unpremult", KnobValue::Bool(false));
    knobs.insert("mix", KnobValue::Float(1.0));
    knobs
}
/// Nuke-style `ColorCorrect` image operation.
#[derive(Debug, Clone)]
pub struct ColorCorrectNode {
    knobs: KnobTable,
}
impl Default for ColorCorrectNode {
    fn default() -> Self {
        Self {
            knobs: default_knobs(),
        }
    }
}
impl ColorCorrectNode {
    /// Creates a node with default knobs.
    pub fn new() -> Self {
        Self::default()
    }
    /// Creates a node over an existing knob table.
    pub fn from_knobs(knobs: KnobTable) -> Self {
        Self { knobs }
    }
    /// Borrows the stored knobs; evaluation honors live context overrides.
    pub fn knobs(&self) -> &KnobTable {
        &self.knobs
    }
}
/// Parsed parameters; pixel evaluation never accesses a knob table.
#[derive(Debug, Clone, Copy)]
pub struct ColorCorrectOp {
    pub saturation: f32,
    pub contrast: f32,
    pub gamma: f32,
    pub gain: f32,
    pub offset: f32,
    pub unpremult: bool,
    pub mix: f32,
    inverse_gamma: f32,
}
impl ColorCorrectOp {
    pub fn from_knobs(knobs: &KnobTable) -> Self {
        let gamma = float_knob(knobs, "gamma", 1.0);
        Self {
            saturation: float_knob(knobs, "saturation", 1.0),
            contrast: float_knob(knobs, "contrast", 1.0),
            gamma,
            gain: float_knob(knobs, "gain", 1.0),
            offset: float_knob(knobs, "offset", 0.0),
            unpremult: crate::util::bool_knob(knobs, "unpremult", false),
            mix: float_knob(knobs, "mix", 1.0),
            inverse_gamma: if gamma > 1e-6 { 1.0 / gamma } else { 1.0 },
        }
    }
    pub fn apply_pixel(&self, rgba: [f32; 4], mask: f32) -> [f32; 4] {
        let corrected = {
            let mut out = rgba;
            let unpremult = self.unpremult && rgba[3].abs() > 1e-5;
            if unpremult {
                for value in &mut out[..3] {
                    *value /= rgba[3];
                }
            }
            let luma = 0.2126 * out[0] + 0.7152 * out[1] + 0.0722 * out[2];
            for value in &mut out[..3] {
                *value = luma + (*value - luma) * self.saturation;
                *value = 0.18 + (*value - 0.18) * self.contrast;
                let gamma = self.gamma;
                if *value > 0.0 && gamma > 1e-6 {
                    *value = value.powf(self.inverse_gamma);
                }
                *value = *value * self.gain + self.offset;
                if unpremult {
                    *value *= rgba[3];
                }
            }
            out
        };
        let mix = (self.mix * mask).clamp(0.0, 1.0);
        if mix == 0.0 {
            return rgba;
        }
        if mix == 1.0 {
            return corrected;
        }
        let mut out = rgba;
        for c in 0..3 {
            out[c] = rgba[c] + (corrected[c] - rgba[c]) * mix;
        }
        out
    }
}
impl NodeTrait for ColorCorrectNode {
    fn evaluate(&self, ctx: &EvalContext, inputs: &[Packet]) -> Result<Packet, NodeError> {
        let input = raster_input(inputs, 0).ok_or(NodeError::MissingInput("main"))?;
        let mask = raster_input(inputs, 1);
        let op = ColorCorrectOp::from_knobs(ctx.knobs().unwrap_or(&self.knobs));
        // Untouched alpha and AOV tiles retain their original Arcs.
        let mut output = input.clone();
        let active = [true, true, true, false];
        let mut planes: [ChannelPlane; 4] = std::array::from_fn(|_| ChannelPlane::new());
        if op.mix == 0.0 || !active.iter().any(|&enabled| enabled) {
            let mut packet = Packet::new();
            packet.insert(output);
            return Ok(packet);
        }
        for tile in input.bbox.tile_coords() {
            let region = tile.to_bbox().intersection(&input.bbox);
            let uniform =
                uniform_rgba(input, tile, region).zip(uniform_mask_value(mask, tile, region));
            if let Some((rgba, factor)) = uniform {
                let out = op.apply_pixel(rgba, factor);
                for c in 0..4 {
                    if active[c] {
                        planes[c].insert(tile, Arc::new(TileBuffer::new_uniform(out[c])));
                    }
                }
                continue;
            }
            // Gather all active planes in one pass: sample and evaluate each pixel once.
            let mut values: [Vec<f32>; 4] = std::array::from_fn(|c| {
                if active[c] {
                    Vec::with_capacity(TILE_PIXELS)
                } else {
                    Vec::new()
                }
            });
            let bounds = tile.to_bbox();
            for y in 0..TILE_SIZE {
                for x in 0..TILE_SIZE {
                    let wx = bounds.min_x + x as i32;
                    let wy = bounds.min_y + y as i32;
                    let rgba = input.sample_rgba(wx, wy);
                    let factor = tanova_render::mask_value(mask, wx, wy);
                    let out = op.apply_pixel(rgba, factor);
                    for c in 0..4 {
                        if active[c] {
                            values[c].push(out[c]);
                        }
                    }
                }
            }
            for c in 0..4 {
                if active[c] {
                    let buffer =
                        TileBuffer::from_vec(std::mem::take(&mut values[c]), ctx.governor())?;
                    planes[c].insert(tile, Arc::new(buffer));
                }
            }
        }
        for (c, channel) in [
            Channel::RgbaRed,
            Channel::RgbaGreen,
            Channel::RgbaBlue,
            Channel::RgbaAlpha,
        ]
        .into_iter()
        .enumerate()
        {
            if active[c] {
                output.insert_channel(channel, std::mem::take(&mut planes[c]));
            }
        }
        let mut packet = Packet::new();
        packet.insert(output);
        Ok(packet)
    }
    fn serialize_to_tcl(&self) -> String {
        let op = ColorCorrectOp::from_knobs(&self.knobs);
        format!(
            "ColorCorrect {{\n saturation {}\n contrast {}\n gamma {}\n gain {}\n offset {}\n unpremult {}\n mix {}\n}}",
            crate::util::format_float(op.saturation),
            crate::util::format_float(op.contrast),
            crate::util::format_float(op.gamma),
            crate::util::format_float(op.gain),
            crate::util::format_float(op.offset),
            op.unpremult,
            crate::util::format_float(op.mix)
        )
    }
}
fn factory() -> Arc<dyn NodeTrait> {
    Arc::new(ColorCorrectNode::new())
}
inventory::submit! { NodeCartridgeDescriptor { kind: "ColorCorrect", default_knobs, factory } }

#[cfg(test)]
mod tests {
    use super::*;
    use tanova_kernel::MemoryGovernor;
    use tanova_render::{BBox, Format, Raster2D, TileCoord};
    fn input(color: [f32; 4]) -> Packet {
        let mut raster = Raster2D::new(
            Format::new("test", 256, 256, 1.0),
            BBox::new(0, 0, 256, 256),
            Default::default(),
        );
        for (c, v) in [
            Channel::RgbaRed,
            Channel::RgbaGreen,
            Channel::RgbaBlue,
            Channel::RgbaAlpha,
            Channel::DepthZ,
        ]
        .into_iter()
        .zip(color.into_iter().chain([42.0]))
        {
            let mut plane = ChannelPlane::new();
            plane.insert(TileCoord::new(0, 0), Arc::new(TileBuffer::new_uniform(v)));
            raster.insert_channel(c, plane);
        }
        let mut p = Packet::new();
        p.insert(raster);
        p
    }
    #[test]
    fn saturation_and_negative_gamma_are_safe() {
        let mut knobs = default_knobs();
        knobs.insert("saturation", KnobValue::Float(0.0));
        let p = ColorCorrectNode::from_knobs(knobs)
            .evaluate(&EvalContext::new(), &[input([1.0, 0.0, 0.0, 0.5])])
            .unwrap();
        for c in &p.get::<Raster2D>().unwrap().sample_rgba(0, 0)[..3] {
            assert!((*c - 0.2126).abs() < 1e-6);
        }
        let mut knobs = default_knobs();
        knobs.insert("gamma", KnobValue::Float(2.2));
        let p = ColorCorrectNode::from_knobs(knobs)
            .evaluate(&EvalContext::new(), &[input([-0.5, -0.2, -1.0, 0.0])])
            .unwrap();
        for (actual, expected) in p
            .get::<Raster2D>()
            .unwrap()
            .sample_rgba(0, 0)
            .into_iter()
            .zip([-0.5, -0.2, -1.0, 0.0])
        {
            assert!(actual.is_finite());
            assert!((actual - expected).abs() < 1e-6);
        }
    }
    #[test]
    fn correction_order_unpremult_and_mask_mix() {
        let mut knobs = default_knobs();
        knobs.insert("unpremult", KnobValue::Bool(true));
        knobs.insert("contrast", KnobValue::Float(2.0));
        knobs.insert("gamma", KnobValue::Float(2.0));
        knobs.insert("gain", KnobValue::Float(1.5));
        knobs.insert("offset", KnobValue::Float(0.1));
        knobs.insert("mix", KnobValue::Float(0.5));
        let rgba = [0.25, 0.25, 0.25, 0.5];
        let corrected = ((0.18_f32 + (0.5 - 0.18) * 2.0).sqrt() * 1.5 + 0.1) * 0.5;
        let result = ColorCorrectOp::from_knobs(&knobs).apply_pixel(rgba, 0.5);
        assert!((result[0] - (0.25 + (corrected - 0.25) * 0.25)).abs() < 1e-6);
        assert_eq!(result[3], rgba[3]);
        assert_eq!(
            ColorCorrectOp::from_knobs(&knobs).apply_pixel(rgba, -1.0),
            rgba
        );
        knobs.insert("gamma", KnobValue::Float(0.0));
        assert!(
            ColorCorrectOp::from_knobs(&knobs)
                .apply_pixel(rgba, 1.0)
                .into_iter()
                .all(f32::is_finite)
        );
    }
    #[test]
    fn uniform_costs_zero() {
        let governor = MemoryGovernor::new(0);
        let mut ctx = EvalContext::new();
        ctx.insert(governor.clone());
        let p = ColorCorrectNode::new()
            .evaluate(&ctx, &[input([0.2, 0.4, 0.8, 0.5])])
            .unwrap();
        assert_eq!(governor.allocated_bytes(), 0);
        assert!(
            p.get::<Raster2D>()
                .unwrap()
                .channels
                .values()
                .all(|p| p.iter().all(|(_, b)| b.is_uniform().is_some()))
        );
    }
}

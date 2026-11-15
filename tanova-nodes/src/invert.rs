//! Nuke-style Invert cartridge with governed tiles and zero-copy AOVs.
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
    knobs.insert("channels", KnobValue::Text("rgb".into()));
    knobs.insert("mix", KnobValue::Float(1.0));
    knobs
}
/// Nuke-style `Invert` image operation.
#[derive(Debug, Clone)]
pub struct InvertNode {
    knobs: KnobTable,
}
impl Default for InvertNode {
    fn default() -> Self {
        Self {
            knobs: default_knobs(),
        }
    }
}
impl InvertNode {
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
/// Channels selected for inversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvertChannels {
    Rgb,
    Rgba,
    Alpha,
    None,
}
impl InvertChannels {
    fn as_str(self) -> &'static str {
        match self {
            Self::Rgb => "rgb",
            Self::Rgba => "rgba",
            Self::Alpha => "alpha",
            Self::None => "none",
        }
    }
    fn active(self) -> [bool; 4] {
        match self {
            Self::Rgb => [true, true, true, false],
            Self::Rgba => [true; 4],
            Self::Alpha => [false, false, false, true],
            Self::None => [false; 4],
        }
    }
}
/// Parsed parameters; pixel evaluation never accesses a knob table.
#[derive(Debug, Clone, Copy)]
pub struct InvertOp {
    pub channels: InvertChannels,
    pub mix: f32,
}
impl InvertOp {
    pub fn from_knobs(knobs: &KnobTable) -> Self {
        let channels = match crate::util::text_knob(knobs, "channels", "rgb").as_str() {
            "rgb" => InvertChannels::Rgb,
            "rgba" => InvertChannels::Rgba,
            "alpha" => InvertChannels::Alpha,
            _ => InvertChannels::None,
        };
        Self {
            channels,
            mix: float_knob(knobs, "mix", 1.0),
        }
    }
    pub fn apply_pixel(&self, rgba: [f32; 4], mask: f32) -> [f32; 4] {
        let mix = (self.mix * mask).clamp(0.0, 1.0);
        let active = self.channels.active();
        std::array::from_fn(|c| {
            if !active[c] || mix == 0.0 {
                rgba[c]
            } else if mix == 1.0 {
                1.0 - rgba[c]
            } else {
                rgba[c] + (1.0 - rgba[c] - rgba[c]) * mix
            }
        })
    }
}
impl NodeTrait for InvertNode {
    fn evaluate(&self, ctx: &EvalContext, inputs: &[Packet]) -> Result<Packet, NodeError> {
        let input = raster_input(inputs, 0).ok_or(NodeError::MissingInput("main"))?;
        let mask = raster_input(inputs, 1);
        let op = InvertOp::from_knobs(ctx.knobs().unwrap_or(&self.knobs));
        // Untouched alpha and AOV tiles retain their original Arcs.
        let mut output = input.clone();
        let active = op.channels.active();
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
        let op = InvertOp::from_knobs(&self.knobs);
        format!(
            "Invert {{\n channels {}\n mix {}\n}}",
            op.channels.as_str(),
            crate::util::format_float(op.mix)
        )
    }
}
fn factory() -> Arc<dyn NodeTrait> {
    Arc::new(InvertNode::new())
}
inventory::submit! { NodeCartridgeDescriptor { kind: "Invert", default_knobs, factory } }

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
    fn channel_modes_preserve_depth() {
        for (mode, expected) in [
            ("rgb", [0.8, 0.6, 0.2, 0.25]),
            ("alpha", [0.2, 0.4, 0.8, 0.75]),
            ("rgba", [0.8, 0.6, 0.2, 0.75]),
        ] {
            let governor = MemoryGovernor::new(0);
            let mut ctx = EvalContext::new();
            ctx.insert(governor.clone());
            let mut knobs = default_knobs();
            knobs.insert("channels", KnobValue::Text(mode.into()));
            let p = input([0.2, 0.4, 0.8, 0.25]);
            let before = p.get::<Raster2D>().unwrap().clone();
            let p = InvertNode::from_knobs(knobs).evaluate(&ctx, &[p]).unwrap();
            let out = p.get::<Raster2D>().unwrap();
            for (a, b) in out.sample_rgba(0, 0).into_iter().zip(expected) {
                assert!((a - b).abs() < 1e-6);
            }
            assert!(Arc::ptr_eq(
                before
                    .channel(&Channel::DepthZ)
                    .unwrap()
                    .get(&TileCoord::new(0, 0))
                    .unwrap(),
                out.channel(&Channel::DepthZ)
                    .unwrap()
                    .get(&TileCoord::new(0, 0))
                    .unwrap()
            ));
            assert_eq!(governor.allocated_bytes(), 0);
        }
    }
}

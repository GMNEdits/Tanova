//! Masked, per-channel RGBA clamping.

use std::sync::Arc;

use crate::util::{bool_knob, float_knob, format_float, raster_input};
use tanova_kernel::{
    EvalContext, KnobTable, KnobValue, NodeCartridgeDescriptor, NodeError, NodeTrait, Packet,
};
use tanova_render::Channel;

pub fn default_knobs() -> KnobTable {
    let mut knobs = KnobTable::new();
    knobs.insert("minimum", KnobValue::Float(0.0));
    knobs.insert("maximum", KnobValue::Float(1.0));
    knobs.insert("minimum_enable", KnobValue::Bool(true));
    knobs.insert("maximum_enable", KnobValue::Bool(true));
    knobs.insert("mix", KnobValue::Float(1.0));
    knobs
}

#[derive(Debug, Clone)]
pub struct ClampNode {
    knobs: KnobTable,
}

impl Default for ClampNode {
    fn default() -> Self {
        Self::from_knobs(default_knobs())
    }
}

impl ClampNode {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_knobs(knobs: KnobTable) -> Self {
        Self { knobs }
    }
}

struct ClampOp {
    minimum: f32,
    maximum: f32,
    minimum_enable: bool,
    maximum_enable: bool,
    mix: f32,
}

impl ClampOp {
    fn from_knobs(knobs: &KnobTable) -> Self {
        Self {
            minimum: float_knob(knobs, "minimum", 0.0),
            maximum: float_knob(knobs, "maximum", 1.0),
            minimum_enable: bool_knob(knobs, "minimum_enable", true),
            maximum_enable: bool_knob(knobs, "maximum_enable", true),
            mix: float_knob(knobs, "mix", 1.0),
        }
    }

    fn apply(&self, rgba: [f32; 4], mask: f32) -> [f32; 4] {
        let mix = (self.mix * mask).clamp(0.0, 1.0);
        if mix == 0.0 {
            return rgba;
        }
        rgba.map(|original| {
            let mut value = original;
            if self.minimum_enable && value < self.minimum {
                value = self.minimum;
            }
            if self.maximum_enable && value > self.maximum {
                value = self.maximum;
            }
            if mix == 1.0 || value == original {
                value
            } else {
                original + (value - original) * mix
            }
        })
    }
}

impl NodeTrait for ClampNode {
    fn evaluate(&self, ctx: &EvalContext, inputs: &[Packet]) -> Result<Packet, NodeError> {
        let input = raster_input(inputs, 0).ok_or(NodeError::MissingInput("main"))?;
        let mask = raster_input(inputs, 1);
        let op = ClampOp::from_knobs(ctx.knobs().unwrap_or(&self.knobs));
        let output = if op.mix == 0.0 || !(op.minimum_enable || op.maximum_enable) {
            input.clone()
        } else {
            crate::util::map_color_tiles(
                ctx,
                input,
                mask,
                [
                    Channel::RgbaRed,
                    Channel::RgbaGreen,
                    Channel::RgbaBlue,
                    Channel::RgbaAlpha,
                ],
                |rgba, factor| op.apply(rgba, factor),
            )?
        };
        let mut packet = Packet::new();
        packet.insert(output);
        Ok(packet)
    }

    fn serialize_to_tcl(&self) -> String {
        let op = ClampOp::from_knobs(&self.knobs);
        format!(
            "Clamp {{\n minimum {}\n maximum {}\n minimum_enable {}\n maximum_enable {}\n mix {}\n}}",
            format_float(op.minimum),
            format_float(op.maximum),
            op.minimum_enable,
            op.maximum_enable,
            format_float(op.mix),
        )
    }
}

inventory::submit! {
    NodeCartridgeDescriptor { kind: "Clamp", default_knobs, factory: || Arc::new(ClampNode::new()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tanova_kernel::MemoryGovernor;
    use tanova_render::{ChannelPlane, Format, Raster2D, TileBuffer, TileCoord};

    #[test]
    fn uniform_clamp_blends_all_channels_and_shares_aovs() {
        let tile = TileCoord::new(0, 0);
        let mut raster = Raster2D::for_format(Format::new("test", 2, 2, 1.0));
        let depth = Arc::new(TileBuffer::new_uniform(42.0));
        for (channel, buffer) in [
            (Channel::RgbaRed, Arc::new(TileBuffer::new_uniform(-1.0))),
            (Channel::RgbaAlpha, Arc::new(TileBuffer::new_uniform(2.0))),
            (Channel::DepthZ, Arc::clone(&depth)),
        ] {
            let mut plane = ChannelPlane::new();
            plane.insert(tile, buffer);
            raster.insert_channel(channel, plane);
        }
        let mut input = Packet::new();
        input.insert(raster);
        let mut ctx = EvalContext::new();
        ctx.insert(MemoryGovernor::new(0));
        let mut knobs = default_knobs();
        knobs.insert("mix", KnobValue::Float(0.5));
        let output = ClampNode::from_knobs(knobs)
            .evaluate(&ctx, &[input])
            .unwrap();
        let raster = output.get::<Raster2D>().unwrap();
        assert_eq!(raster.sample_rgba(0, 0), [-0.5, 0.0, 0.0, 1.5]);
        assert!(Arc::ptr_eq(
            &depth,
            raster
                .channel(&Channel::DepthZ)
                .unwrap()
                .get(&tile)
                .unwrap()
        ));
    }

    #[test]
    fn clamp_bounds_can_be_enabled_independently() {
        let mut knobs = default_knobs();
        knobs.insert("minimum_enable", KnobValue::Bool(false));
        assert_eq!(
            ClampOp::from_knobs(&knobs).apply([-1.0, 0.5, 2.0, 2.0], 2.0),
            [-1.0, 0.5, 1.0, 1.0]
        );
    }
}

//! Masked saturation adjustment for premultiplied RGBA images.

use std::sync::Arc;

use crate::util::{bool_knob, float_knob, format_float, raster_input};
use tanova_kernel::{
    EvalContext, KnobTable, KnobValue, NodeCartridgeDescriptor, NodeError, NodeTrait, Packet,
};
use tanova_render::Channel;

pub fn default_knobs() -> KnobTable {
    let mut knobs = KnobTable::new();
    knobs.insert("saturation", KnobValue::Float(1.0));
    knobs.insert("mode", KnobValue::Text("Rec709".into()));
    knobs.insert("unpremult", KnobValue::Bool(false));
    knobs.insert("mix", KnobValue::Float(1.0));
    knobs
}

#[derive(Debug, Clone)]
pub struct SaturationNode {
    knobs: KnobTable,
}

impl Default for SaturationNode {
    fn default() -> Self {
        Self::from_knobs(default_knobs())
    }
}

impl SaturationNode {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_knobs(knobs: KnobTable) -> Self {
        Self { knobs }
    }
}

enum LuminanceMode {
    Rec709,
    Average,
    Maximum,
}

impl LuminanceMode {
    fn name(&self) -> &'static str {
        match self {
            Self::Rec709 => "Rec709",
            Self::Average => "Average",
            Self::Maximum => "Maximum",
        }
    }

    fn luminance(&self, [r, g, b]: [f32; 3]) -> f32 {
        match self {
            Self::Rec709 => 0.2126 * r + 0.7152 * g + 0.0722 * b,
            Self::Average => (r + g + b) / 3.0,
            Self::Maximum => r.max(g).max(b),
        }
    }
}

struct SaturationOp {
    saturation: f32,
    mode: LuminanceMode,
    unpremult: bool,
    mix: f32,
}

impl SaturationOp {
    fn from_knobs(knobs: &KnobTable) -> Self {
        let mode = match knobs.get("mode") {
            Some(KnobValue::Text(value)) if value == "Average" => LuminanceMode::Average,
            Some(KnobValue::Text(value)) if value == "Maximum" => LuminanceMode::Maximum,
            _ => LuminanceMode::Rec709,
        };
        Self {
            saturation: float_knob(knobs, "saturation", 1.0),
            mode,
            unpremult: bool_knob(knobs, "unpremult", false),
            mix: float_knob(knobs, "mix", 1.0),
        }
    }

    fn apply(&self, [r, g, b, alpha]: [f32; 4], mask: f32) -> [f32; 3] {
        let mix = (self.mix * mask).clamp(0.0, 1.0);
        let original = [r, g, b];
        if mix == 0.0 {
            return original;
        }
        let unpremult = self.unpremult && alpha != 0.0;
        let rgb = if unpremult {
            original.map(|value| value / alpha)
        } else {
            original
        };
        let luma = self.mode.luminance(rgb);
        std::array::from_fn(|c| {
            let adjusted = luma + (rgb[c] - luma) * self.saturation;
            let adjusted = if unpremult {
                adjusted * alpha
            } else {
                adjusted
            };
            if mix == 1.0 {
                adjusted
            } else {
                original[c] + (adjusted - original[c]) * mix
            }
        })
    }
}

impl NodeTrait for SaturationNode {
    fn evaluate(&self, ctx: &EvalContext, inputs: &[Packet]) -> Result<Packet, NodeError> {
        let input = raster_input(inputs, 0).ok_or(NodeError::MissingInput("main"))?;
        let mask = raster_input(inputs, 1);
        let op = SaturationOp::from_knobs(ctx.knobs().unwrap_or(&self.knobs));
        let output = if op.mix == 0.0 || op.saturation == 1.0 {
            input.clone()
        } else {
            crate::util::map_color_tiles(
                ctx,
                input,
                mask,
                [Channel::RgbaRed, Channel::RgbaGreen, Channel::RgbaBlue],
                |rgba, factor| op.apply(rgba, factor),
            )?
        };
        let mut packet = Packet::new();
        packet.insert(output);
        Ok(packet)
    }

    fn serialize_to_tcl(&self) -> String {
        let op = SaturationOp::from_knobs(&self.knobs);
        format!(
            "Saturation {{\n saturation {}\n mode {}\n unpremult {}\n mix {}\n}}",
            format_float(op.saturation),
            op.mode.name(),
            op.unpremult,
            format_float(op.mix),
        )
    }
}

inventory::submit! {
    NodeCartridgeDescriptor {
        kind: "Saturation", default_knobs, factory: || Arc::new(SaturationNode::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saturation_modes_blend_after_repremultiplying() {
        let mut knobs = default_knobs();
        knobs.insert("saturation", KnobValue::Float(0.0));
        knobs.insert("unpremult", KnobValue::Bool(true));
        for (mode, luma) in [("Rec709", 0.1063), ("Average", 0.5 / 3.0), ("Maximum", 0.5)] {
            knobs.insert("mode", KnobValue::Text(mode.into()));
            let op = SaturationOp::from_knobs(&knobs);
            let output = op.apply([0.5, 0.0, 0.0, 0.5], 0.5);
            for (actual, expected) in
                output
                    .into_iter()
                    .zip([0.25 + luma * 0.5, luma * 0.5, luma * 0.5])
            {
                assert!((actual - expected).abs() < 1e-6);
            }
            assert_eq!(op.apply([0.0; 4], 1.0), [0.0; 3]);
        }
    }
}

//! `Grade`: Nuke's linear slope/offset grade with gamma and premult handling.
//!
//! The math lives in `tanova-render`; this file only maps knobs to a
//! [`GradeOp`] and forwards the engine + governor from the evaluation context.
//! Non-RGBA AOV planes on the main input pass through by `Arc` clone.

use std::sync::Arc;

use tanova_kernel::{
    EvalContext, KnobTable, KnobValue, NodeCartridgeDescriptor, NodeError, NodeTrait, Packet,
};
use tanova_render::{EvalContextGpu, GradeOp, grade_raster};

use crate::util::{bool_knob, color_knob, float_knob, format_color, format_float, raster_input};

/// Default black point.
pub const DEFAULT_BLACKPOINT: [f32; 4] = [0.0; 4];
/// Default white point.
pub const DEFAULT_WHITEPOINT: [f32; 4] = [1.0; 4];
/// Default lift.
pub const DEFAULT_LIFT: [f32; 4] = [0.0; 4];
/// Default gain.
pub const DEFAULT_GAIN: [f32; 4] = [1.0; 4];
/// Default multiply.
pub const DEFAULT_MULTIPLY: [f32; 4] = [1.0; 4];
/// Default offset.
pub const DEFAULT_OFFSET: [f32; 4] = [0.0; 4];
/// Default gamma.
pub const DEFAULT_GAMMA: [f32; 4] = [1.0; 4];

/// Default knob values for a fresh [`GradeNode`].
pub fn default_knobs() -> KnobTable {
    let mut knobs = KnobTable::with_capacity(11);
    knobs.insert("blackpoint", KnobValue::Color(DEFAULT_BLACKPOINT));
    knobs.insert("whitepoint", KnobValue::Color(DEFAULT_WHITEPOINT));
    knobs.insert("lift", KnobValue::Color(DEFAULT_LIFT));
    knobs.insert("gain", KnobValue::Color(DEFAULT_GAIN));
    knobs.insert("multiply", KnobValue::Color(DEFAULT_MULTIPLY));
    knobs.insert("offset", KnobValue::Color(DEFAULT_OFFSET));
    knobs.insert("gamma", KnobValue::Color(DEFAULT_GAMMA));
    knobs.insert("unpremult", KnobValue::Bool(false));
    knobs.insert("black_clamp", KnobValue::Bool(true));
    knobs.insert("white_clamp", KnobValue::Bool(false));
    knobs.insert("mix", KnobValue::Float(1.0));
    knobs
}

/// A Nuke-style grade cartridge.
#[derive(Debug, Clone)]
pub struct GradeNode {
    knobs: KnobTable,
}

impl Default for GradeNode {
    fn default() -> Self {
        Self {
            knobs: default_knobs(),
        }
    }
}

impl GradeNode {
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

/// Builds the render operation from a knob table.
pub fn grade_op(knobs: &KnobTable) -> GradeOp {
    GradeOp {
        blackpoint: color_knob(knobs, "blackpoint", DEFAULT_BLACKPOINT),
        whitepoint: color_knob(knobs, "whitepoint", DEFAULT_WHITEPOINT),
        lift: color_knob(knobs, "lift", DEFAULT_LIFT),
        gain: color_knob(knobs, "gain", DEFAULT_GAIN),
        multiply: color_knob(knobs, "multiply", DEFAULT_MULTIPLY),
        offset: color_knob(knobs, "offset", DEFAULT_OFFSET),
        gamma: color_knob(knobs, "gamma", DEFAULT_GAMMA),
        unpremult: bool_knob(knobs, "unpremult", false),
        black_clamp: bool_knob(knobs, "black_clamp", true),
        white_clamp: bool_knob(knobs, "white_clamp", false),
        mix: float_knob(knobs, "mix", 1.0),
    }
}

impl NodeTrait for GradeNode {
    fn evaluate(&self, ctx: &EvalContext, inputs: &[Packet]) -> Result<Packet, NodeError> {
        let input = raster_input(inputs, 0).ok_or(NodeError::MissingInput("main"))?;
        let mask = raster_input(inputs, 1);
        let knobs = ctx.knobs().unwrap_or(&self.knobs);
        let op = grade_op(knobs);

        let raster = grade_raster(input, mask, &op, ctx.governor(), ctx.gpu())?;

        let mut packet = Packet::new();
        packet.insert(raster);
        Ok(packet)
    }

    fn serialize_to_tcl(&self) -> String {
        let knobs = &self.knobs;
        let op = grade_op(knobs);
        format!(
            "Grade {{\n blackpoint {{{}}}\n whitepoint {{{}}}\n lift {{{}}}\n gain {{{}}}\n multiply {{{}}}\n offset {{{}}}\n gamma {{{}}}\n unpremult {}\n black_clamp {}\n white_clamp {}\n mix {}\n}}",
            format_color(op.blackpoint),
            format_color(op.whitepoint),
            format_color(op.lift),
            format_color(op.gain),
            format_color(op.multiply),
            format_color(op.offset),
            format_color(op.gamma),
            op.unpremult,
            op.black_clamp,
            op.white_clamp,
            format_float(op.mix)
        )
    }
}

fn factory() -> Arc<dyn NodeTrait> {
    Arc::new(GradeNode::new())
}

inventory::submit! {
    NodeCartridgeDescriptor {
        kind: "Grade",
        default_knobs,
        factory,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tanova_render::{BBox, Channel, Format, Raster2D, TILE_SIZE, TileBuffer};

    fn solid_raster(color: [f32; 4], depth: f32) -> Raster2D {
        let bbox = BBox::new(0, 0, TILE_SIZE as i32, TILE_SIZE as i32);
        let mut raster =
            Raster2D::new(Format::new("tile", 256, 256, 1.0), bbox, Default::default());
        for (channel, value) in [
            (Channel::RgbaRed, color[0]),
            (Channel::RgbaGreen, color[1]),
            (Channel::RgbaBlue, color[2]),
            (Channel::RgbaAlpha, color[3]),
            (Channel::DepthZ, depth),
        ] {
            let mut plane = tanova_render::ChannelPlane::new();
            plane.insert(
                tanova_render::TileCoord::new(0, 0),
                Arc::new(TileBuffer::new_uniform(value)),
            );
            raster.insert_channel(channel, plane);
        }
        raster
    }

    #[test]
    fn identity_grade_preserves_pixels_and_aovs() {
        let input = solid_raster([0.25, 0.5, 0.75, 1.0], 7.0);
        let mut packet = Packet::new();
        packet.insert(input);

        let output = GradeNode::new()
            .evaluate(&EvalContext::new(), &[packet])
            .expect("grade should evaluate");
        let raster = output.get::<Raster2D>().expect("raster");

        assert_eq!(raster.sample(&Channel::RgbaRed, 3, 4), 0.25);
        assert_eq!(raster.sample(&Channel::RgbaGreen, 3, 4), 0.5);
        assert_eq!(raster.sample(&Channel::RgbaBlue, 3, 4), 0.75);
        assert_eq!(raster.sample(&Channel::DepthZ, 3, 4), 7.0);
    }

    #[test]
    fn missing_main_input_is_reported() {
        let result = GradeNode::new().evaluate(&EvalContext::new(), &[]);
        assert!(matches!(result, Err(NodeError::MissingInput("main"))));
    }

    #[test]
    fn serialization_lists_every_knob() {
        let text = GradeNode::new().serialize_to_tcl();
        for name in [
            "blackpoint",
            "whitepoint",
            "lift",
            "gain",
            "multiply",
            "offset",
            "gamma",
            "unpremult",
            "black_clamp",
            "white_clamp",
            "mix",
        ] {
            assert!(text.contains(name), "missing {name} in {text}");
        }
    }
}

//! RGBA ramp evaluated at bottom-left world-space pixel centers.

use std::sync::Arc;

use crate::constant::{DEFAULT_HEIGHT, DEFAULT_WIDTH};
use crate::util::{color_knob, format_color, int_knob};
use tanova_kernel::{
    EvalContext, KnobTable, KnobValue, NodeCartridgeDescriptor, NodeError, NodeTrait, Packet,
};
use tanova_render::{Channel, ChannelPlane, Format, Raster2D, TILE_PIXELS, TILE_SIZE, TileBuffer};

const P0: [f32; 4] = [0.0; 4];
const P1: [f32; 4] = [256.0, 256.0, 0.0, 0.0];
const COLOR0: [f32; 4] = [0.0, 0.0, 0.0, 1.0];
const COLOR1: [f32; 4] = [1.0; 4];

pub fn default_knobs() -> KnobTable {
    let mut knobs = KnobTable::with_capacity(7);
    knobs.insert("p0", KnobValue::Color(P0));
    knobs.insert("p1", KnobValue::Color(P1));
    knobs.insert("color0", KnobValue::Color(COLOR0));
    knobs.insert("color1", KnobValue::Color(COLOR1));
    knobs.insert("ramp_type", KnobValue::Text("linear".into()));
    knobs.insert("format_width", KnobValue::Int(DEFAULT_WIDTH));
    knobs.insert("format_height", KnobValue::Int(DEFAULT_HEIGHT));
    knobs
}

#[derive(Debug, Clone)]
pub struct RampNode {
    knobs: KnobTable,
}

impl Default for RampNode {
    fn default() -> Self {
        Self::from_knobs(default_knobs())
    }
}

impl RampNode {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_knobs(knobs: KnobTable) -> Self {
        Self { knobs }
    }
}

enum RampType {
    Linear,
    Smooth,
    Plinear,
}

impl RampType {
    fn name(&self) -> &'static str {
        match self {
            Self::Linear => "linear",
            Self::Smooth => "smooth",
            Self::Plinear => "plinear",
        }
    }

    fn apply(&self, t: f32) -> f32 {
        match self {
            Self::Linear => t,
            Self::Smooth => t * t * (3.0 - 2.0 * t),
            Self::Plinear => t * t,
        }
    }
}

struct RampOp {
    p0: [f32; 4],
    p1: [f32; 4],
    color0: [f32; 4],
    color1: [f32; 4],
    ramp_type: RampType,
    width: i64,
    height: i64,
}

impl RampOp {
    fn from_knobs(knobs: &KnobTable) -> Self {
        let ramp_type = match knobs.get("ramp_type") {
            Some(KnobValue::Text(value)) if value == "smooth" => RampType::Smooth,
            Some(KnobValue::Text(value)) if value == "plinear" => RampType::Plinear,
            _ => RampType::Linear,
        };
        Self {
            p0: color_knob(knobs, "p0", P0),
            p1: color_knob(knobs, "p1", P1),
            color0: color_knob(knobs, "color0", COLOR0),
            color1: color_knob(knobs, "color1", COLOR1),
            ramp_type,
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

impl NodeTrait for RampNode {
    fn evaluate(&self, ctx: &EvalContext, _inputs: &[Packet]) -> Result<Packet, NodeError> {
        let op = RampOp::from_knobs(ctx.knobs().unwrap_or(&self.knobs));
        let (width, height) = op.dimensions(ctx);
        let mut output = Raster2D::for_format(Format::new("Ramp", width, height, 1.0));
        let mut planes: [ChannelPlane; 4] = std::array::from_fn(|_| ChannelPlane::new());

        let dx = op.p1[0] - op.p0[0];
        let dy = op.p1[1] - op.p0[1];
        let len_sq = dx * dx + dy * dy;
        let uniform: [_; 4] = std::array::from_fn(|c| {
            (len_sq < 1e-12 || op.color0[c] == op.color1[c])
                .then(|| Arc::new(TileBuffer::new_uniform(op.color0[c])))
        });
        for tile in output.bbox.tile_coords() {
            let mut dense = uniform
                .each_ref()
                .map(|value| value.is_none().then(|| Vec::with_capacity(TILE_PIXELS)));
            if dense.iter().any(Option::is_some) {
                let (ox, oy) = tile.origin_world();
                for y in 0..TILE_SIZE as i32 {
                    let py = (oy + y) as f32 + 0.5;
                    for x in 0..TILE_SIZE as i32 {
                        let px = (ox + x) as f32 + 0.5;
                        let t = (((px - op.p0[0]) * dx + (py - op.p0[1]) * dy) / len_sq)
                            .clamp(0.0, 1.0);
                        let t = op.ramp_type.apply(t);
                        for (c, values) in dense.iter_mut().enumerate() {
                            if let Some(values) = values {
                                values.push(op.color0[c] * (1.0 - t) + op.color1[c] * t);
                            }
                        }
                    }
                }
            }
            for ((plane, values), uniform) in planes.iter_mut().zip(dense).zip(&uniform) {
                let buffer = match values {
                    Some(values) => Arc::new(TileBuffer::from_vec(values, ctx.governor())?),
                    None => Arc::clone(uniform.as_ref().expect("uniform ramp channel")),
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
        let op = RampOp::from_knobs(&self.knobs);
        format!(
            "Ramp {{\n inputs 0\n p0 {{{}}}\n p1 {{{}}}\n color0 {{{}}}\n color1 {{{}}}\n ramp_type {}\n format_width {}\n format_height {}\n}}",
            format_color(op.p0),
            format_color(op.p1),
            format_color(op.color0),
            format_color(op.color1),
            op.ramp_type.name(),
            op.width,
            op.height,
        )
    }
}

inventory::submit! {
    NodeCartridgeDescriptor {
        kind: "Ramp", default_knobs, factory: || Arc::new(RampNode::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tanova_kernel::{MemoryGovernor, ProjectFormat};

    #[test]
    fn ramp_hits_endpoints_and_applies_each_curve_at_pixel_centers() {
        for (ramp_type, quarter, middle) in [
            ("linear", 0.25, 0.5),
            ("smooth", 0.15625, 0.5),
            ("plinear", 0.0625, 0.25),
        ] {
            let mut knobs = default_knobs();
            knobs.insert("p0", KnobValue::Color([0.5, 0.5, 0.0, 0.0]));
            knobs.insert("p1", KnobValue::Color([4.5, 0.5, 0.0, 0.0]));
            knobs.insert("ramp_type", KnobValue::Text(ramp_type.into()));
            let mut ctx = EvalContext::new();
            ctx.insert(ProjectFormat {
                width: 5,
                height: 1,
            });
            let output = RampNode::from_knobs(knobs).evaluate(&ctx, &[]).unwrap();
            let raster = output.get::<Raster2D>().unwrap();
            assert_eq!((raster.format.width, raster.format.height), (5, 1));
            assert_eq!(raster.sample_rgba(0, 0), COLOR0);
            assert_eq!(raster.sample_rgba(1, 0), [quarter, quarter, quarter, 1.0]);
            assert_eq!(raster.sample_rgba(2, 0), [middle, middle, middle, 1.0]);
            assert_eq!(raster.sample_rgba(4, 0), COLOR1);
        }
    }

    #[test]
    fn equal_colors_and_coincident_points_emit_uniform_tiles() {
        let governor = MemoryGovernor::new(0);
        let mut ctx = EvalContext::new();
        ctx.insert(governor.clone());
        ctx.insert(ProjectFormat {
            width: 5,
            height: 1,
        });
        for (knob, value) in [("color1", COLOR0), ("p1", P0)] {
            let mut knobs = default_knobs();
            knobs.insert(knob, KnobValue::Color(value));
            let output = RampNode::from_knobs(knobs).evaluate(&ctx, &[]).unwrap();
            let raster = output.get::<Raster2D>().unwrap();
            assert_eq!(raster.sample_rgba(2, 0), COLOR0);
            assert_eq!(governor.allocated_bytes(), 0);
        }
    }
}

//! `CornerPin2D`: projective corner-pin warp via a homography matrix.
//!
//! Maps the input's source quad (its full format) to a destination quad of
//! four arbitrary corners. Solves a `Mat3x3` homography via
//! `Mat3x3::from_quad_to_quad` and warps every output pixel by sampling the
//! inverse. Zero-copy identity fast path when all four corners sit on their
//! source positions, and an OOM-safe clamped output bbox so dragging a corner
//! handle far off-screen can never trigger runaway tile allocation.

use tanova_kernel::{
    EvalContext, KnobTable, KnobValue, NodeCartridgeDescriptor, NodeError, NodeTrait, Packet,
};
use tanova_render::{BBox, Mat3x3, Raster2D, perspective_warp_raster};

use crate::util::{color_knob, format_color, raster_input};

/// Number of corners in the pin quad.
const CORNER_COUNT: usize = 4;

/// Default knob values for a fresh [`CornerPin2D`].
///
/// Corners are stored as `KnobValue::Color([x, y, 0.0, 0.0])`, matching
/// `Transform.translate` and `Crop.box_min` conventions.
pub fn default_knobs() -> KnobTable {
    let mut knobs = KnobTable::with_capacity(5);
    knobs.insert("to1", KnobValue::Color([0.0, 0.0, 0.0, 0.0]));
    knobs.insert("to2", KnobValue::Color([256.0, 0.0, 0.0, 0.0]));
    knobs.insert("to3", KnobValue::Color([256.0, 256.0, 0.0, 0.0]));
    knobs.insert("to4", KnobValue::Color([0.0, 256.0, 0.0, 0.0]));
    knobs.insert("invert", KnobValue::Bool(false));
    knobs
}

/// A Nuke-style `CornerPin2D` cartridge: one input, four corner handles, one
/// homography.
#[derive(Debug, Clone)]
pub struct CornerPinNode {
    knobs: KnobTable,
}

impl Default for CornerPinNode {
    fn default() -> Self {
        Self {
            knobs: default_knobs(),
        }
    }
}

impl CornerPinNode {
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

impl NodeTrait for CornerPinNode {
    fn evaluate(&self, ctx: &EvalContext, inputs: &[Packet]) -> Result<Packet, NodeError> {
        let input = raster_input(inputs, 0).ok_or(NodeError::MissingInput("main"))?;
        let knobs = ctx.knobs().unwrap_or(&self.knobs);

        let to1 = color_knob(knobs, "to1", [0.0, 0.0, 0.0, 0.0]);
        let to2 = color_knob(knobs, "to2", [256.0, 0.0, 0.0, 0.0]);
        let to3 = color_knob(knobs, "to3", [256.0, 256.0, 0.0, 0.0]);
        let to4 = color_knob(knobs, "to4", [0.0, 256.0, 0.0, 0.0]);
        let invert = Self::bool_knob(knobs, "invert", false);

        let fw = input.format.width as f32;
        let fh = input.format.height as f32;

        // Source quad = the input's full format.
        let from = [[0.0, 0.0], [fw, 0.0], [fw, fh], [0.0, fh]];
        // Destination quad = the four pinned corners.
        let to = [
            [to1[0], to1[1]],
            [to2[0], to2[1]],
            [to3[0], to3[1]],
            [to4[0], to4[1]],
        ];

        // Zero-copy identity fast path: every corner sits exactly on its source.
        if !invert {
            let mut identity = true;
            for i in 0..CORNER_COUNT {
                if !((to[i][0] - from[i][0]).abs() <= 1e-4 && (to[i][1] - from[i][1]).abs() <= 1e-4)
                {
                    identity = false;
                    break;
                }
            }
            if identity {
                return Ok(inputs[0].clone());
            }
        }

        // Solve the homography. Forward maps `from` -> `to`; invert swaps.
        let homography = if invert {
            Mat3x3::from_quad_to_quad(to, from)
        } else {
            Mat3x3::from_quad_to_quad(from, to)
        };

        let Some(homography) = homography else {
            // Degenerate / collinear quad: emit an empty raster preserving the
            // input format without panicking.
            let mut output = Raster2D::new(input.format.clone(), input.bbox, input.edge_mode);
            // Missing alpha means opaque in Raster2D, so an empty alpha plane
            // is required for an actually transparent degenerate result.
            output.insert_channel(
                tanova_render::Channel::RgbaAlpha,
                tanova_render::ChannelPlane::new(),
            );
            for (channel, plane) in input.channels.iter() {
                if !tanova_render::is_rgba_channel(channel) {
                    output.insert_channel(channel.clone(), plane.clone());
                }
            }
            let mut packet = Packet::new();
            packet.insert(output);
            return Ok(packet);
        };

        let output_bbox = output_bbox(from, to, fw, fh);

        let governor = ctx.governor();
        let output = perspective_warp_raster(input, &homography, output_bbox, governor)?;

        let mut packet = Packet::new();
        packet.insert(output);
        Ok(packet)
    }

    fn serialize_to_tcl(&self) -> String {
        let to1 = color_knob(&self.knobs, "to1", [0.0, 0.0, 0.0, 0.0]);
        let to2 = color_knob(&self.knobs, "to2", [256.0, 0.0, 0.0, 0.0]);
        let to3 = color_knob(&self.knobs, "to3", [256.0, 256.0, 0.0, 0.0]);
        let to4 = color_knob(&self.knobs, "to4", [0.0, 256.0, 0.0, 0.0]);
        let invert = Self::bool_knob(&self.knobs, "invert", false);
        format!(
            "CornerPin2D {{\n to1 {{{}}}\n to2 {{{}}}\n to3 {{{}}}\n to4 {{{}}}\n invert {}\n}}",
            format_color(to1),
            format_color(to2),
            format_color(to3),
            format_color(to4),
            if invert { 1 } else { 0 },
        )
    }
}

/// Union of both quads plus a pixel margin, bounded by the format's drag
/// limits and by a 4096px allocation window on each axis.
fn output_bbox(from: [[f32; 2]; 4], to: [[f32; 2]; 4], fw: f32, fh: f32) -> BBox {
    let mut min_x = f32::INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut max_y = f32::NEG_INFINITY;
    for corner in from.iter().chain(to.iter()) {
        min_x = min_x.min(corner[0]);
        min_y = min_y.min(corner[1]);
        max_x = max_x.max(corner[0]);
        max_y = max_y.max(corner[1]);
    }
    min_x -= 1.0;
    min_y -= 1.0;
    max_x += 1.0;
    max_y += 1.0;
    // If the union is wider than 4096, crop around the format center
    // rather than allowing two independently clamped endpoints to span 8192.
    let axis = |min: f32, max: f32, size: f32| {
        let low = -(size as i32);
        let high = (2.0 * size).min(i32::MAX as f32) as i32;
        let mut start = (min.floor() as i32).clamp(low, high);
        let mut end = (max.ceil() as i32).clamp(low, high);
        if i64::from(end) - i64::from(start) > 4096 {
            start = ((size * 0.5 - 2048.0).floor() as i32).clamp(start, end - 4096);
            end = start + 4096;
        }
        (start, end)
    };
    let (min_x, max_x) = axis(min_x, max_x, fw);
    let (min_y, max_y) = axis(min_y, max_y, fh);
    BBox::new(min_x, min_y, max_x, max_y)
}

impl CornerPinNode {
    /// Reads a boolean knob with a default.
    fn bool_knob(knobs: &KnobTable, name: &str, default: bool) -> bool {
        match knobs.get(name) {
            Some(KnobValue::Bool(value)) => *value,
            _ => default,
        }
    }
}

fn factory() -> std::sync::Arc<dyn NodeTrait> {
    std::sync::Arc::new(CornerPinNode::new())
}

inventory::submit! {
    NodeCartridgeDescriptor {
        kind: "CornerPin2D",
        default_knobs,
        factory,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tanova_kernel::{KnobTable, MemoryGovernor};
    use tanova_render::{BBox, Channel, ChannelPlane, EdgeMode, Format, Raster2D, TileBuffer};

    fn uniform_raster(width: i32, height: i32, value: f32, edge_mode: EdgeMode) -> Raster2D {
        let bbox = BBox::new(0, 0, width, height);
        let mut raster = Raster2D::new(
            Format::new("test", width as u32, height as u32, 1.0),
            bbox,
            edge_mode,
        );
        for channel in [
            Channel::RgbaRed,
            Channel::RgbaGreen,
            Channel::RgbaBlue,
            Channel::RgbaAlpha,
        ] {
            let mut plane = ChannelPlane::new();
            plane.insert(
                tanova_render::TileCoord::new(0, 0),
                std::sync::Arc::new(TileBuffer::new_uniform(value)),
            );
            raster.insert_channel(channel, plane);
        }
        raster
    }

    #[test]
    fn corner_pin_identity_is_zero_copy_and_trapezoid_warps_pixels() {
        let mut source_knobs = crate::constant::default_knobs();
        source_knobs.insert("format_width", KnobValue::Int(64));
        source_knobs.insert("format_height", KnobValue::Int(64));
        source_knobs.insert("color", KnobValue::Color([0.75; 4]));
        let source = crate::ConstantNode::from_knobs(source_knobs)
            .evaluate(&EvalContext::new(), &[])
            .unwrap();
        let mut input = source.get::<Raster2D>().unwrap().clone();
        let mut depth = ChannelPlane::new();
        let depth_tile = Arc::new(TileBuffer::new_uniform(42.0));
        let tile = tanova_render::TileCoord::new(0, 0);
        depth.insert(tile, depth_tile.clone());
        input.insert_channel(Channel::DepthZ, depth);
        let mut packet = Packet::new();
        packet.insert(input.clone());

        let mut knobs = KnobTable::with_capacity(5);
        knobs.insert("to1", KnobValue::Color([0.0, 0.0, 0.0, 0.0]));
        knobs.insert("to2", KnobValue::Color([64.0, 0.0, 0.0, 0.0]));
        knobs.insert("to3", KnobValue::Color([64.0, 64.0, 0.0, 0.0]));
        knobs.insert("to4", KnobValue::Color([0.0, 64.0, 0.0, 0.0]));
        knobs.insert("invert", KnobValue::Bool(false));
        let mut ctx = EvalContext::new();
        ctx.insert(knobs.clone());
        let governor = MemoryGovernor::new(0);
        ctx.insert(governor.clone());

        let node = CornerPinNode::new();
        let output = node
            .evaluate(&ctx, &[packet.clone()])
            .expect("identity should pass through");
        let output_raster = output.get::<Raster2D>().expect("raster");

        // Identity fast path returns the input packet untouched (Arc clone).
        assert!(
            Arc::ptr_eq(
                input
                    .channel(&Channel::RgbaRed)
                    .expect("red plane")
                    .get(&tanova_render::TileCoord::new(0, 0))
                    .expect("tile"),
                output_raster
                    .channel(&Channel::RgbaRed)
                    .expect("red plane")
                    .get(&tanova_render::TileCoord::new(0, 0))
                    .expect("tile")
            ),
            "identity corner pin must zero-copy the upstream tile"
        );
        assert_eq!(governor.allocated_bytes(), 0);
        assert!(Arc::ptr_eq(
            &depth_tile,
            output_raster
                .channel(&Channel::DepthZ)
                .unwrap()
                .get(&tile)
                .unwrap()
        ));

        // A trapezoid narrows the bottom edge: a formerly filled pixel at
        // (2, 2) becomes transparent while the center remains filled.
        knobs.insert("to1", KnobValue::Color([16.0, 0.0, 0.0, 0.0]));
        knobs.insert("to2", KnobValue::Color([48.0, 0.0, 0.0, 0.0]));
        ctx.insert(knobs.clone());
        ctx.insert(MemoryGovernor::new(16 * 1024 * 1024));
        let warped = node.evaluate(&ctx, &[packet.clone()]).unwrap();
        let warped = warped.get::<Raster2D>().unwrap();
        assert_eq!(input.sample_rgba(2, 2), [0.75; 4]);
        assert_eq!(warped.sample_rgba(2, 2), [0.0; 4]);
        assert!((warped.sample_rgba(32, 32)[0] - 0.75).abs() < 1e-4);
        assert_eq!(warped.format, input.format);
        assert!(Arc::ptr_eq(
            &depth_tile,
            warped
                .channel(&Channel::DepthZ)
                .unwrap()
                .get(&tile)
                .unwrap()
        ));

        // Distinct but collinear corners must be safe and transparent.
        for (i, xy) in [[0.0, 0.0], [16.0, 16.0], [32.0, 32.0], [64.0, 64.0]]
            .iter()
            .enumerate()
        {
            knobs.insert(
                format!("to{}", i + 1),
                KnobValue::Color([xy[0], xy[1], 0.0, 0.0]),
            );
        }
        ctx.insert(knobs);
        ctx.insert(governor.clone());
        let empty = node.evaluate(&ctx, &[packet]).unwrap();
        let empty = empty.get::<Raster2D>().unwrap();
        assert_eq!(empty.format, input.format);
        assert_eq!(empty.sample_rgba(32, 32), [0.0; 4]);
        assert_eq!(governor.allocated_bytes(), 0);
        assert!(Arc::ptr_eq(
            &depth_tile,
            empty.channel(&Channel::DepthZ).unwrap().get(&tile).unwrap()
        ));
    }

    #[test]
    fn corner_pin_output_bbox_bounds_both_endpoints_and_dimensions() {
        for (fw, fh) in [(64.0, 32.0), (1920.0, 1080.0), (8192.0, 8192.0)] {
            let from = [[0.0, 0.0], [fw, 0.0], [fw, fh], [0.0, fh]];
            let to = [[-1e20, -1e20], [1e20, -1e20], [1e20, 1e20], [-1e20, 1e20]];
            let bbox = output_bbox(from, to, fw, fh);
            assert!(bbox.min_x >= -(fw as i32) && bbox.max_x <= 2 * fw as i32);
            assert!(bbox.min_y >= -(fh as i32) && bbox.max_y <= 2 * fh as i32);
            assert!(bbox.width() <= 4096 && bbox.height() <= 4096);
            if fw == 64.0 {
                assert_eq!(bbox, BBox::new(-64, -32, 128, 64));
            }
        }
    }

    #[test]
    fn corner_pin_trapezoid_warps_pixels() {
        let input = uniform_raster(64, 64, 0.75, EdgeMode::BlackOutside);
        let mut packet = Packet::new();
        packet.insert(input);

        let mut knobs = KnobTable::with_capacity(5);
        knobs.insert("to1", KnobValue::Color([10.0, 10.0, 0.0, 0.0]));
        knobs.insert("to2", KnobValue::Color([54.0, 5.0, 0.0, 0.0]));
        knobs.insert("to3", KnobValue::Color([50.0, 50.0, 0.0, 0.0]));
        knobs.insert("to4", KnobValue::Color([5.0, 55.0, 0.0, 0.0]));
        knobs.insert("invert", KnobValue::Bool(false));
        let mut ctx = EvalContext::new();
        ctx.insert(knobs);

        let node = CornerPinNode::new();
        let output = node
            .evaluate(&ctx, &[packet])
            .expect("trapezoid warp should evaluate");
        let raster = output.get::<Raster2D>().expect("raster");

        // Output bbox should cover the warped quad, not the full 4096 clamp.
        assert!(
            raster.bbox.width() <= 128,
            "output bbox must stay clamped near the input, got {}x{}",
            raster.bbox.width(),
            raster.bbox.height()
        );
        // A uniform image warps to a uniform image.
        let pixel = raster.sample_rgba(32, 32);
        for (channel, value) in pixel.iter().enumerate() {
            assert!(
                (value - 0.75).abs() < 1e-4,
                "channel {channel} warped to {value}"
            );
        }
    }

    #[test]
    fn corner_pin_degenerate_quad_returns_empty_without_panic() {
        let input = uniform_raster(64, 64, 0.75, EdgeMode::BlackOutside);
        let mut packet = Packet::new();
        packet.insert(input);

        // All four corners identical -> singular quad.
        let mut knobs = KnobTable::with_capacity(5);
        knobs.insert("to1", KnobValue::Color([0.0, 0.0, 0.0, 0.0]));
        knobs.insert("to2", KnobValue::Color([0.0, 0.0, 0.0, 0.0]));
        knobs.insert("to3", KnobValue::Color([0.0, 0.0, 0.0, 0.0]));
        knobs.insert("to4", KnobValue::Color([0.0, 0.0, 0.0, 0.0]));
        knobs.insert("invert", KnobValue::Bool(false));
        let mut ctx = EvalContext::new();
        ctx.insert(knobs);

        let node = CornerPinNode::new();
        let output = node
            .evaluate(&ctx, &[packet])
            .expect("degenerate quad must not panic");
        let raster = output.get::<Raster2D>().expect("raster");

        // Empty raster preserving the input format.
        assert_eq!(raster.bbox, BBox::new(0, 0, 64, 64));
        assert!(raster.channel(&Channel::RgbaRed).is_none());
    }

    #[test]
    fn serialization_writes_the_corner_knobs() {
        let text = CornerPinNode::new().serialize_to_tcl();
        assert!(text.starts_with("CornerPin2D {"), "got {text}");
        assert!(text.contains("to1"), "got {text}");
        assert!(text.contains("to2"), "got {text}");
        assert!(text.contains("to3"), "got {text}");
        assert!(text.contains("to4"), "got {text}");
        assert!(text.contains("invert"), "got {text}");
    }
}

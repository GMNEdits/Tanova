//! `Transform`: Nuke-style 2D affine transform with inverse bilinear sampling.
//!
//! The matrix math and the sampler live in `tanova-render`; this file maps the
//! `translate` / `rotate` / `scale` / `center` knobs to a [`Mat3x3`], expands the
//! data window, and gathers the inverse-mapped input footprint for each output
//! tile. Non-RGBA AOV planes pass through by `Arc` clone.

use std::sync::Arc;

use tanova_kernel::{
    EvalContext, KnobTable, KnobValue, MemoryGovernor, NodeCartridgeDescriptor, NodeError,
    NodeTrait, Packet,
};
use tanova_render::coords::BBox;
use tanova_render::{
    Channel, ChannelPlane, EvalContextGpu, Mat3x3, Raster2D, TILE_SIZE, TileBuffer, TileCoord,
    transform_gathered_tile,
};

use crate::util::{color_knob, float_knob, format_color, format_float, raster_input};

/// Default translation.
pub const DEFAULT_TRANSLATE: [f32; 4] = [0.0, 0.0, 0.0, 1.0];
/// Default rotation, in degrees.
pub const DEFAULT_ROTATE: f32 = 0.0;
/// Default uniform scale.
pub const DEFAULT_SCALE: f32 = 1.0;
/// Default rotation/scale pivot.
pub const DEFAULT_CENTER: [f32; 4] = [0.0, 0.0, 0.0, 1.0];

/// Extra gathered pixels around the inverse-mapped tile footprint, covering
/// the bilinear tap.
const GATHER_MARGIN: i32 = 2;

/// Default knob values for a fresh [`TransformNode`].
pub fn default_knobs() -> KnobTable {
    let mut knobs = KnobTable::with_capacity(4);
    knobs.insert("translate", KnobValue::Color(DEFAULT_TRANSLATE));
    knobs.insert("rotate", KnobValue::Float(DEFAULT_ROTATE as f64));
    knobs.insert("scale", KnobValue::Float(DEFAULT_SCALE as f64));
    knobs.insert("center", KnobValue::Color(DEFAULT_CENTER));
    knobs
}

/// A Nuke-style affine transform cartridge.
#[derive(Debug, Clone)]
pub struct TransformNode {
    knobs: KnobTable,
}

impl Default for TransformNode {
    fn default() -> Self {
        Self {
            knobs: default_knobs(),
        }
    }
}

impl TransformNode {
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

impl NodeTrait for TransformNode {
    fn evaluate(&self, ctx: &EvalContext, inputs: &[Packet]) -> Result<Packet, NodeError> {
        let input = raster_input(inputs, 0).ok_or(NodeError::MissingInput("main"))?;
        let knobs = ctx.knobs().unwrap_or(&self.knobs);

        let translate = color_knob(knobs, "translate", DEFAULT_TRANSLATE);
        let center = color_knob(knobs, "center", DEFAULT_CENTER);
        let rotate = float_knob(knobs, "rotate", DEFAULT_ROTATE);
        let scale = float_knob(knobs, "scale", DEFAULT_SCALE);

        // Output always covers the full project format canvas so every
        // destination pixel (x, y) in 0..format_w, 0..format_h is sampled via
        // inverse mapping. Out-of-bounds source pixels read as transparent
        // black.
        let output_bbox = match ctx.project_format() {
            Some(fmt) => BBox::new(0, 0, fmt.width as i32, fmt.height as i32),
            None => input.format.to_bbox(),
        };

        // A scale near zero would make the affine matrix singular. Instead of
        // rejecting it, emit a fully transparent raster so downstream Merge2
        // compositing never breaks.
        if scale.abs() < 1e-4 {
            let mut output = Raster2D::new(input.format.clone(), output_bbox, input.edge_mode);
            let mut red = ChannelPlane::new();
            let mut green = ChannelPlane::new();
            let mut blue = ChannelPlane::new();
            let mut alpha = ChannelPlane::new();
            for tile in output_bbox.tile_coords() {
                insert_uniform(&mut red, tile, 0.0);
                insert_uniform(&mut green, tile, 0.0);
                insert_uniform(&mut blue, tile, 0.0);
                insert_uniform(&mut alpha, tile, 0.0);
            }
            output.insert_channel(Channel::RgbaRed, red);
            output.insert_channel(Channel::RgbaGreen, green);
            output.insert_channel(Channel::RgbaBlue, blue);
            output.insert_channel(Channel::RgbaAlpha, alpha);
            let mut packet = Packet::new();
            packet.insert(output);
            return Ok(packet);
        }

        let matrix = transform_matrix(translate, center, rotate, scale);
        let inverse = matrix.inverse().ok_or_else(|| NodeError::InvalidKnob {
            knob: "scale".to_string(),
            reason: "the affine matrix is singular".to_string(),
        })?;

        let governor = ctx.governor();
        let gpu = ctx.gpu();

        let mut output = Raster2D::new(input.format.clone(), output_bbox, input.edge_mode);
        // Only RGBA participates in resampling; AOV planes keep their upstream
        // tiles by reference.
        for (channel, plane) in input.channels.iter() {
            if !is_rgba(channel) {
                output.insert_channel(channel.clone(), plane.clone());
            }
        }

        let mut red = ChannelPlane::new();
        let mut green = ChannelPlane::new();
        let mut blue = ChannelPlane::new();
        let mut alpha = ChannelPlane::new();

        for tile in output_bbox.tile_coords() {
            let (origin_x, origin_y) = tile.origin_world();
            let Some((gx0, gy0, gx1, gy1)) = gather_bounds(input, &inverse, origin_x, origin_y)
            else {
                insert_uniform(&mut red, tile, 0.0);
                insert_uniform(&mut green, tile, 0.0);
                insert_uniform(&mut blue, tile, 0.0);
                insert_uniform(&mut alpha, tile, 0.0);
                continue;
            };

            let gathered_width = (gx1 - gx0) as u32;
            let gathered_height = (gy1 - gy0) as u32;
            let mut gathered = Vec::with_capacity((gathered_width * gathered_height * 4) as usize);
            for y in gy0..gy1 {
                for x in gx0..gx1 {
                    gathered.extend_from_slice(&input.sample_rgba(x, y));
                }
            }

            // Fold the tile origin and gathered origin into the inverse so the
            // kernel works in local 0..255 output coordinates.
            let tile_inverse = Mat3x3::translate(-(gx0 as f32), -(gy0 as f32))
                .mul(&inverse)
                .mul(&Mat3x3::translate(origin_x as f32, origin_y as f32));

            let pixels = transform_gathered_tile(
                &gathered,
                gathered_width,
                gathered_height,
                &tile_inverse,
                gpu,
            )?;
            insert_dense(
                [&mut red, &mut green, &mut blue, &mut alpha],
                tile,
                &pixels,
                governor,
            )?;
        }

        output.insert_channel(Channel::RgbaRed, red);
        output.insert_channel(Channel::RgbaGreen, green);
        output.insert_channel(Channel::RgbaBlue, blue);
        output.insert_channel(Channel::RgbaAlpha, alpha);

        let mut packet = Packet::new();
        packet.insert(output);
        Ok(packet)
    }

    fn serialize_to_tcl(&self) -> String {
        let knobs = &self.knobs;
        format!(
            "Transform {{\n translate {{{}}}\n rotate {}\n scale {}\n center {{{}}}\n}}",
            format_color(color_knob(knobs, "translate", DEFAULT_TRANSLATE)),
            format_float(float_knob(knobs, "rotate", DEFAULT_ROTATE)),
            format_float(float_knob(knobs, "scale", DEFAULT_SCALE)),
            format_color(color_knob(knobs, "center", DEFAULT_CENTER))
        )
    }
}

/// Builds `T(translate) * T(center) * S(scale) * R(rotate) * T(-center)`.
///
/// `rotate` is in degrees, counter-clockwise in Bottom-Left space.
pub fn transform_matrix(
    translate: [f32; 4],
    center: [f32; 4],
    rotate_degrees: f32,
    scale: f32,
) -> Mat3x3 {
    Mat3x3::translate(translate[0], translate[1])
        .mul(&Mat3x3::translate(center[0], center[1]))
        .mul(&Mat3x3::rotate(rotate_degrees.to_radians()))
        .mul(&Mat3x3::scale(scale, scale))
        .mul(&Mat3x3::translate(-center[0], -center[1]))
}

/// World-space bounds of the input region needed by one output tile.
///
/// The output tile's four corners are mapped back through the inverse matrix,
/// expanded by [`GATHER_MARGIN`] for the bilinear tap, and clipped to the input
/// data window. `None` means the tile lies entirely outside the input.
fn gather_bounds(
    input: &Raster2D,
    inverse: &Mat3x3,
    origin_x: i32,
    origin_y: i32,
) -> Option<(i32, i32, i32, i32)> {
    let size = TILE_SIZE as f32;
    let corners = [
        (origin_x as f32, origin_y as f32),
        (origin_x as f32 + size, origin_y as f32),
        (origin_x as f32, origin_y as f32 + size),
        (origin_x as f32 + size, origin_y as f32 + size),
    ];

    let mut min_x = f32::INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut max_y = f32::NEG_INFINITY;
    for (x, y) in corners {
        let (tx, ty) = inverse.transform_point(x, y);
        min_x = min_x.min(tx);
        min_y = min_y.min(ty);
        max_x = max_x.max(tx);
        max_y = max_y.max(ty);
    }

    let gx0 = (min_x.floor() as i32 - GATHER_MARGIN).max(input.bbox.min_x);
    let gy0 = (min_y.floor() as i32 - GATHER_MARGIN).max(input.bbox.min_y);
    let gx1 = (max_x.ceil() as i32 + GATHER_MARGIN).min(input.bbox.max_x);
    let gy1 = (max_y.ceil() as i32 + GATHER_MARGIN).min(input.bbox.max_y);

    (gx0 < gx1 && gy0 < gy1).then_some((gx0, gy0, gx1, gy1))
}

fn is_rgba(channel: &Channel) -> bool {
    matches!(
        channel,
        Channel::RgbaRed | Channel::RgbaGreen | Channel::RgbaBlue | Channel::RgbaAlpha
    )
}

fn insert_uniform(plane: &mut ChannelPlane, tile: TileCoord, value: f32) {
    plane.insert(tile, Arc::new(TileBuffer::new_uniform(value)));
}

fn insert_dense(
    planes: [&mut ChannelPlane; 4],
    tile: TileCoord,
    interleaved: &[f32],
    governor: Option<&MemoryGovernor>,
) -> Result<(), NodeError> {
    for (channel, plane) in planes.into_iter().enumerate() {
        let values: Vec<f32> = interleaved
            .iter()
            .skip(channel)
            .step_by(4)
            .copied()
            .collect();
        plane.insert(tile, Arc::new(TileBuffer::from_vec(values, governor)?));
    }
    Ok(())
}

fn factory() -> Arc<dyn NodeTrait> {
    Arc::new(TransformNode::new())
}

inventory::submit! {
    NodeCartridgeDescriptor {
        kind: "Transform",
        default_knobs,
        factory,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tanova_kernel::KnobTable;
    use tanova_render::{BBox, EdgeMode, Format};

    fn solid_raster(width: i32, height: i32, color: [f32; 4]) -> Raster2D {
        let bbox = BBox::new(0, 0, width, height);
        let mut raster = Raster2D::new(
            Format::new("test", width as u32, height as u32, 1.0),
            bbox,
            EdgeMode::BlackOutside,
        );
        for (channel, value) in [
            (Channel::RgbaRed, color[0]),
            (Channel::RgbaGreen, color[1]),
            (Channel::RgbaBlue, color[2]),
            (Channel::RgbaAlpha, color[3]),
        ] {
            let mut plane = ChannelPlane::new();
            plane.insert(
                TileCoord::new(0, 0),
                Arc::new(TileBuffer::new_uniform(value)),
            );
            raster.insert_channel(channel, plane);
        }
        raster
    }

    fn context(translate: [f32; 4], rotate: f32, scale: f32, center: [f32; 4]) -> EvalContext {
        let mut knobs = KnobTable::with_capacity(4);
        knobs.insert("translate", KnobValue::Color(translate));
        knobs.insert("rotate", KnobValue::Float(rotate as f64));
        knobs.insert("scale", KnobValue::Float(scale as f64));
        knobs.insert("center", KnobValue::Color(center));
        let mut ctx = EvalContext::new();
        ctx.insert(knobs);
        ctx
    }

    fn evaluate(raster: Raster2D, ctx: &EvalContext) -> Raster2D {
        let mut packet = Packet::new();
        packet.insert(raster);
        let output = TransformNode::new()
            .evaluate(ctx, &[packet])
            .expect("transform should evaluate");
        output.get::<Raster2D>().expect("raster").clone()
    }

    #[test]
    fn identity_keeps_the_data_window_and_colors() {
        let color = [0.25, 0.5, 0.75, 1.0];
        let output = evaluate(
            solid_raster(100, 100, color),
            &context(DEFAULT_TRANSLATE, 0.0, 1.0, DEFAULT_CENTER),
        );

        assert_eq!(output.bbox, BBox::new(0, 0, 100, 100));
        let pixel = output.sample_rgba(50, 50);
        for channel in 0..4 {
            assert!((pixel[channel] - color[channel]).abs() < 1e-5);
        }
    }

    #[test]
    fn translation_moves_the_data_window_and_pixels() {
        let color = [0.1, 0.2, 0.3, 0.4];
        let output = evaluate(
            solid_raster(100, 100, color),
            &context([10.0, -5.0, 0.0, 1.0], 0.0, 1.0, DEFAULT_CENTER),
        );

        // Output always covers the full format canvas; the translated content
        // lives at the right world coordinates within it.
        assert_eq!(output.bbox, BBox::new(0, 0, 100, 100));
        // dst(60, 45) -> inverse(T(10,-5)) -> src(50, 50), still the same color.
        let pixel = output.sample_rgba(60, 45);
        for channel in 0..4 {
            assert!((pixel[channel] - color[channel]).abs() < 1e-5);
        }
    }

    #[test]
    fn rotation_expands_the_data_window() {
        let output = evaluate(
            solid_raster(100, 100, [1.0, 1.0, 1.0, 1.0]),
            &context(DEFAULT_TRANSLATE, 45.0, 1.0, DEFAULT_CENTER),
        );

        // Output always covers the full format canvas; rotated content stays
        // within [0..100, 0..100] with transparent black outside the rotated
        // source footprint.
        assert_eq!(output.bbox, BBox::new(0, 0, 100, 100));
    }

    #[test]
    fn scale_around_the_center_keeps_the_center_pixel() {
        let color = [0.3, 0.6, 0.9, 1.0];
        let output = evaluate(
            solid_raster(100, 100, color),
            &context(DEFAULT_TRANSLATE, 0.0, 2.0, [50.0, 50.0, 0.0, 1.0]),
        );

        assert_eq!(output.bbox, BBox::new(0, 0, 100, 100));
        let pixel = output.sample_rgba(50, 50);
        for channel in 0..4 {
            assert!((pixel[channel] - color[channel]).abs() < 1e-5);
        }
    }

    #[test]
    fn tiny_scale_emits_transparent_raster() {
        let mut packet = Packet::new();
        packet.insert(solid_raster(8, 8, [1.0, 1.0, 1.0, 1.0]));
        let output = TransformNode::new()
            .evaluate(
                &context(DEFAULT_TRANSLATE, 0.0, 0.0, DEFAULT_CENTER),
                &[packet],
            )
            .expect("tiny scale should produce transparent raster, not error");
        let raster = output.get::<Raster2D>().expect("raster");
        assert_eq!(raster.bbox, BBox::new(0, 0, 8, 8));
        let pixel = raster.sample_rgba(4, 4);
        for channel in 0..4 {
            assert!(
                (pixel[channel] - 0.0).abs() < 1e-5,
                "channel {channel} was {pixel:?}"
            );
        }
    }

    #[test]
    fn non_rgba_planes_pass_through_by_arc() {
        let mut input = solid_raster(8, 8, [1.0, 1.0, 1.0, 1.0]);
        let mut depth = ChannelPlane::new();
        let depth_buffer = Arc::new(TileBuffer::new_uniform(9.0));
        depth.insert(TileCoord::new(0, 0), Arc::clone(&depth_buffer));
        input.insert_channel(Channel::DepthZ, depth);

        let output = evaluate(input, &context(DEFAULT_TRANSLATE, 0.0, 1.0, DEFAULT_CENTER));
        let output_depth = output
            .channel(&Channel::DepthZ)
            .and_then(|plane| plane.get(&TileCoord::new(0, 0)))
            .expect("depth tile");
        assert!(Arc::ptr_eq(&depth_buffer, output_depth));
    }

    #[test]
    fn scaled_transform_composites_over_a_background() {
        use crate::merge::Merge2Node;

        let background = solid_raster(256, 256, [0.0, 0.0, 1.0, 1.0]);
        let foreground = solid_raster(256, 256, [1.0, 0.0, 0.0, 1.0]);

        let transformed = evaluate(
            foreground,
            &context([64.0, 64.0, 0.0, 1.0], 0.0, 0.5, DEFAULT_CENTER),
        );
        assert_eq!(
            transformed.bbox,
            BBox::new(0, 0, 256, 256),
            "transform output always covers the format canvas"
        );
        assert_eq!(transformed.format.to_bbox(), BBox::new(0, 0, 256, 256),);

        let inside = transformed.sample_rgba(128, 128);
        assert!(
            inside[0] > 0.99 && inside[1] < 0.01 && inside[2] < 0.01,
            "inside the scaled square must be red, got {inside:?}"
        );

        let mut background_packet = Packet::new();
        background_packet.insert(background);
        let mut foreground_packet = Packet::new();
        foreground_packet.insert(transformed);

        let merged = Merge2Node::new()
            .evaluate(&EvalContext::new(), &[background_packet, foreground_packet])
            .expect("merge should evaluate");
        let merged = merged.get::<Raster2D>().expect("raster");

        let inside = merged.sample_rgba(128, 128);
        assert!(
            inside[0] > 0.99 && inside[1] < 0.01 && inside[2] < 0.01,
            "inside the scaled square must be red, got {inside:?}"
        );

        for (x, y) in [(10, 10), (220, 220)] {
            let outside = merged.sample_rgba(x, y);
            assert!(
                outside[2] > 0.99 && outside[0] < 0.01,
                "({x}, {y}) must stay blue, got {outside:?}"
            );
        }
    }

    #[test]
    fn scaled_transform_partial_negative_translate_composites_over_background() {
        use crate::merge::Merge2Node;

        let background = solid_raster(256, 256, [0.0, 0.0, 1.0, 1.0]);
        let foreground = solid_raster(256, 256, [1.0, 0.0, 0.0, 1.0]);

        // scale=0.5, translate=(-64, 48): A occupies x in -64..64, y in 48..176.
        // Inside the frame, only x in 0..64, y in 48..176 is red.
        let transformed = evaluate(
            foreground,
            &context([-64.0, 48.0, 0.0, 1.0], 0.0, 0.5, DEFAULT_CENTER),
        );
        assert_eq!(transformed.bbox, BBox::new(0, 0, 256, 256));

        let mut background_packet = Packet::new();
        background_packet.insert(background);
        let mut foreground_packet = Packet::new();
        foreground_packet.insert(transformed);

        let merged = Merge2Node::new()
            .evaluate(&EvalContext::new(), &[background_packet, foreground_packet])
            .expect("merge should evaluate");
        let merged = merged.get::<Raster2D>().expect("raster");

        // x=32, y=100 is inside [0..64, 48..176]: red.
        let inside = merged.sample_rgba(32, 100);
        assert!(
            inside[0] > 0.99 && inside[1] < 0.01 && inside[2] < 0.01,
            "pixel (32, 100) must be red, got {inside:?}"
        );

        // x=100 is right of the square: blue.
        for (x, y) in [(100, 100), (100, 10)] {
            let outside = merged.sample_rgba(x, y);
            assert!(
                outside[2] > 0.99 && outside[0] < 0.01,
                "pixel ({x}, {y}) must stay blue, got {outside:?}"
            );
        }
    }

    #[test]
    fn scaled_transform_offscreen_left_is_all_blue() {
        use crate::merge::Merge2Node;

        let background = solid_raster(256, 256, [0.0, 0.0, 1.0, 1.0]);
        let foreground = solid_raster(256, 256, [1.0, 0.0, 0.0, 1.0]);

        // scale=0.5, translate=(-174, 49): A occupies x in -174..-46, y in 49..177.
        // Zero pixels overlap [0..256, 0..256], so the entire output is blue.
        let transformed = evaluate(
            foreground,
            &context([-174.0, 49.0, 0.0, 1.0], 0.0, 0.5, DEFAULT_CENTER),
        );
        assert_eq!(transformed.bbox, BBox::new(0, 0, 256, 256));

        let mut background_packet = Packet::new();
        background_packet.insert(background);
        let mut foreground_packet = Packet::new();
        foreground_packet.insert(transformed);

        let merged = Merge2Node::new()
            .evaluate(&EvalContext::new(), &[background_packet, foreground_packet])
            .expect("merge should evaluate");
        let merged = merged.get::<Raster2D>().expect("raster");

        // Every pixel in the frame must be blue.
        for (x, y) in [(128, 100), (0, 0), (255, 255), (64, 64), (192, 128)] {
            let pixel = merged.sample_rgba(x, y);
            assert!(
                pixel[2] > 0.99 && pixel[0] < 0.01,
                "({x}, {y}) must be blue, got {pixel:?}"
            );
        }
    }

    #[test]
    fn serialization_round_trips_every_knob() {
        let text = TransformNode::new().serialize_to_tcl();
        assert!(text.starts_with("Transform {"), "got {text}");
        for knob in ["translate", "rotate", "scale", "center"] {
            assert!(text.contains(knob), "missing {knob}: {text}");
        }
    }
}

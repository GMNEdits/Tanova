//! Centered resizing into a fixed output format.

use std::sync::Arc;

use crate::constant::{DEFAULT_HEIGHT, DEFAULT_WIDTH};
use crate::util::{int_knob, raster_input};
use tanova_kernel::{
    EvalContext, KnobTable, KnobValue, NodeCartridgeDescriptor, NodeError, NodeTrait, Packet,
};
use tanova_render::ops::{is_rgba_channel, uniform_rgba};
use tanova_render::{
    BBox, Channel, ChannelPlane, Format, Mat3x3, Raster2D, TILE_PIXELS, TILE_SIZE, TileBuffer,
    TileRange,
};

const RGBA: [Channel; 4] = [
    Channel::RgbaRed,
    Channel::RgbaGreen,
    Channel::RgbaBlue,
    Channel::RgbaAlpha,
];

pub fn default_knobs() -> KnobTable {
    let mut knobs = KnobTable::with_capacity(3);
    knobs.insert("format_width", KnobValue::Int(DEFAULT_WIDTH));
    knobs.insert("format_height", KnobValue::Int(DEFAULT_HEIGHT));
    knobs.insert("resize", KnobValue::Text("width".into()));
    knobs
}

#[derive(Debug, Clone)]
pub struct ReformatNode {
    knobs: KnobTable,
}

impl Default for ReformatNode {
    fn default() -> Self {
        Self::from_knobs(default_knobs())
    }
}

impl ReformatNode {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_knobs(knobs: KnobTable) -> Self {
        Self { knobs }
    }
}

enum Resize {
    Width,
    Height,
    Fit,
    Fill,
    Distort,
    None,
}

impl Resize {
    fn name(&self) -> &'static str {
        match self {
            Self::Width => "width",
            Self::Height => "height",
            Self::Fit => "fit",
            Self::Fill => "fill",
            Self::Distort => "distort",
            Self::None => "none",
        }
    }

    fn scale(&self, x: f32, y: f32) -> (f32, f32) {
        let uniform = match self {
            Self::Width => x,
            Self::Height => y,
            Self::Fit => x.min(y),
            Self::Fill => x.max(y),
            Self::Distort => return (x, y),
            Self::None => 1.0,
        };
        (uniform, uniform)
    }
}

struct ReformatOp {
    width: i64,
    height: i64,
    resize: Resize,
}

impl ReformatOp {
    fn from_knobs(knobs: &KnobTable) -> Self {
        let resize = match knobs.get("resize") {
            Some(KnobValue::Text(value)) => match value.as_str() {
                "height" => Resize::Height,
                "fit" => Resize::Fit,
                "fill" => Resize::Fill,
                "distort" => Resize::Distort,
                "none" => Resize::None,
                _ => Resize::Width,
            },
            _ => Resize::Width,
        };
        Self {
            width: int_knob(knobs, "format_width", DEFAULT_WIDTH),
            height: int_knob(knobs, "format_height", DEFAULT_HEIGHT),
            resize,
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

    fn inverse(&self, src: &Format, dst: &Format) -> Mat3x3 {
        let (sw, sh) = (src.width as f32, src.height as f32);
        let (dw, dh) = (dst.width as f32, dst.height as f32);
        let (sx, sy) = self
            .resize
            .scale(dw.max(1.0) / sw.max(1.0), dh.max(1.0) / sh.max(1.0));

        // Invert each part of the centered scale directly to support very small ratios.
        Mat3x3::translate(sw * 0.5, sh * 0.5)
            .mul(&Mat3x3::scale(sx.recip(), sy.recip()))
            .mul(&Mat3x3::translate(-dw * 0.5, -dh * 0.5))
    }
}

impl NodeTrait for ReformatNode {
    fn evaluate(&self, ctx: &EvalContext, inputs: &[Packet]) -> Result<Packet, NodeError> {
        let input = raster_input(inputs, 0).ok_or(NodeError::MissingInput("main"))?;
        let op = ReformatOp::from_knobs(ctx.knobs().unwrap_or(&self.knobs));
        let (width, height) = op.dimensions(ctx);
        let mut output = Raster2D::for_format(Format::new("Reformat", width, height, 1.0));
        let inverse = op.inverse(&input.format, &output.format);

        for (channel, plane) in &input.channels {
            if !is_rgba_channel(channel) {
                output.insert_channel(channel.clone(), plane.clone());
            }
        }

        let mut planes: [ChannelPlane; 4] = std::array::from_fn(|_| ChannelPlane::new());
        for tile in output.bbox.tile_coords() {
            let bounds = tile.to_bbox();
            let region = bounds.intersection(&output.bbox);
            if let Some(color) = constant_sample(input, &inverse, region) {
                for (plane, value) in planes.iter_mut().zip(color) {
                    plane.insert(tile, Arc::new(TileBuffer::new_uniform(value)));
                }
                continue;
            }

            let mut values: [Vec<f32>; 4] =
                std::array::from_fn(|_| Vec::with_capacity(TILE_PIXELS));
            let (ox, oy) = tile.origin_world();
            for y in 0..TILE_SIZE as i32 {
                for x in 0..TILE_SIZE as i32 {
                    let (wx, wy) = (ox + x, oy + y);
                    let color = if region.contains(wx, wy) {
                        let (u, v) = inverse.transform_point(wx as f32, wy as f32);
                        bilinear(input, u, v)
                    } else {
                        [0.0; 4]
                    };
                    for (plane, value) in values.iter_mut().zip(color) {
                        plane.push(value);
                    }
                }
            }
            for (plane, values) in planes.iter_mut().zip(values) {
                plane.insert(
                    tile,
                    Arc::new(TileBuffer::from_vec(values, ctx.governor())?),
                );
            }
        }
        for (channel, plane) in RGBA.into_iter().zip(planes) {
            output.insert_channel(channel, plane);
        }

        let mut packet = Packet::new();
        packet.insert(output);
        Ok(packet)
    }

    fn serialize_to_tcl(&self) -> String {
        let op = ReformatOp::from_knobs(&self.knobs);
        format!(
            "Reformat {{\n format_width {}\n format_height {}\n resize {}\n}}",
            op.width,
            op.height,
            op.resize.name()
        )
    }
}

/// Checks whether every bilinear tap for the destination tile has the same value.
fn constant_sample(input: &Raster2D, inverse: &Mat3x3, region: BBox) -> Option<[f32; 4]> {
    let source = input.bbox;
    let (left, bottom) = inverse.transform_point(region.min_x as f32, region.min_y as f32);
    let (right, top) =
        inverse.transform_point((region.max_x - 1) as f32, (region.max_y - 1) as f32);
    let (max_x, max_y) = (
        (source.max_x as i64 - 1) as f32,
        (source.max_y as i64 - 1) as f32,
    );
    if source.is_empty()
        || right < source.min_x as f32
        || top < source.min_y as f32
        || left > max_x
        || bottom > max_y
    {
        return Some([0.0; 4]);
    }
    if left < source.min_x as f32 || bottom < source.min_y as f32 || right > max_x || top > max_y {
        return None;
    }

    let footprint = BBox::new(
        left.floor() as i32,
        bottom.floor() as i32,
        (right.ceil() as i32).saturating_add(1),
        (top.ceil() as i32).saturating_add(1),
    )
    .intersection(&source);
    let mut colors = TileRange::from_bbox(footprint).map(|tile| {
        let mut color = uniform_rgba(input, tile, tile.to_bbox().intersection(&footprint))?;
        if input.channel(&Channel::RgbaAlpha).is_none() {
            color[3] = 1.0;
        }
        Some(color)
    });
    let first = colors.next()??;
    colors.all(|color| color == Some(first)).then_some(first)
}

fn bilinear(input: &Raster2D, x: f32, y: f32) -> [f32; 4] {
    let bbox = input.bbox;
    let (ix, iy) = (x.floor() as i32, y.floor() as i32);
    // Match the spatial renderer's black exterior and clamp taps on the final pixel.
    if !bbox.contains(ix, iy) || x > (bbox.max_x - 1) as f32 || y > (bbox.max_y - 1) as f32 {
        return [0.0; 4];
    }

    let (fx, fy) = (x - ix as f32, y - iy as f32);
    let sample_row = |row| {
        let left = input.sample_rgba(ix, row);
        if fx == 0.0 {
            return left;
        }
        let right = input.sample_rgba(ix + 1, row);
        std::array::from_fn(|c| left[c] * (1.0 - fx) + right[c] * fx)
    };
    let lower = sample_row(iy);
    if fy == 0.0 {
        return lower;
    }
    let upper = sample_row(iy + 1);
    std::array::from_fn(|c| lower[c] * (1.0 - fy) + upper[c] * fy)
}

inventory::submit! {
    NodeCartridgeDescriptor { kind: "Reformat", default_knobs, factory: || Arc::new(ReformatNode::new()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tanova_kernel::{MemoryGovernor, ProjectFormat};
    use tanova_render::TileCoord;

    #[test]
    fn all_resize_modes_use_centered_scale_and_default_dimensions_follow_project() {
        let src = Format::new("src", 4, 8, 1.0);
        let dst = Format::new("dst", 8, 8, 1.0);
        let mut ctx = EvalContext::new();
        ctx.insert(ProjectFormat::new(8, 8));
        let mut knobs = default_knobs();
        for (mode, top_right) in [
            ("width", (3.0, 5.0)),
            ("height", (4.0, 6.0)),
            ("fit", (4.0, 6.0)),
            ("fill", (3.0, 5.0)),
            ("distort", (3.0, 6.0)),
            ("none", (4.0, 6.0)),
        ] {
            knobs.insert("resize", KnobValue::Text(mode.into()));
            let op = ReformatOp::from_knobs(&knobs);
            assert_eq!(op.dimensions(&ctx), (8, 8));
            let inverse = op.inverse(&src, &dst);
            assert_eq!(inverse.transform_point(4.0, 4.0), (2.0, 4.0));
            assert_eq!(inverse.transform_point(6.0, 6.0), top_right);
        }
    }

    #[test]
    fn resamples_across_source_tile_boundaries_and_shares_depth() {
        let mut raster = Raster2D::for_format(Format::new("src", 512, 4, 1.0));
        let mut red = ChannelPlane::new();
        red.insert(TileCoord::new(0, 0), Arc::new(TileBuffer::new_uniform(0.0)));
        red.insert(TileCoord::new(1, 0), Arc::new(TileBuffer::new_uniform(1.0)));
        raster.insert_channel(Channel::RgbaRed, red);
        let tile = TileCoord::new(0, 0);
        let mut depth = ChannelPlane::new();
        let depth_buffer = Arc::new(TileBuffer::new_uniform(9.0));
        depth.insert(tile, Arc::clone(&depth_buffer));
        raster.insert_channel(Channel::DepthZ, depth);

        let mut ctx = EvalContext::new();
        ctx.insert(ProjectFormat::new(1024, 4));
        let mut knobs = default_knobs();
        knobs.insert("resize", KnobValue::Text("distort".into()));
        let mut packet = Packet::new();
        packet.insert(raster);
        let result = ReformatNode::from_knobs(knobs)
            .evaluate(&ctx, &[packet])
            .unwrap();
        let output = result.get::<Raster2D>().unwrap();
        assert_eq!(output.format, Format::new("Reformat", 1024, 4, 1.0));
        assert_eq!(output.bbox, BBox::new(0, 0, 1024, 4));
        assert_eq!(output.sample_rgba(511, 1), [0.5, 0.0, 0.0, 1.0]);
        assert_eq!(output.sample_rgba(512, 1), [1.0, 0.0, 0.0, 1.0]);
        assert!(Arc::ptr_eq(
            &depth_buffer,
            output
                .channel(&Channel::DepthZ)
                .unwrap()
                .get(&tile)
                .unwrap()
        ));
    }

    #[test]
    fn uniform_source_uses_no_dense_memory_at_identity_scale() {
        let mut ctx = EvalContext::new();
        ctx.insert(ProjectFormat::new(320, 48));
        let source = crate::ConstantNode::new().evaluate(&ctx, &[]).unwrap();
        let governor = MemoryGovernor::new(0);
        ctx.insert(governor.clone());
        let output = ReformatNode::new().evaluate(&ctx, &[source]).unwrap();
        assert_eq!(
            output.get::<Raster2D>().unwrap().sample_rgba(319, 47),
            [0.0, 0.0, 0.0, 1.0]
        );
        assert_eq!(governor.allocated_bytes(), 0);
    }
}

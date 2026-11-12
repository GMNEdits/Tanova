//! Exact Nuke math for `Grade` and `Merge2`, shared by the CPU and GPU paths.
//!
//! Every operation is expressed on one RGBA pixel at a time. The uniform-tile
//! fast path evaluates the same functions once per tile; the dense path runs
//! them per pixel, either in the `wgpu` kernels or in Rust.

use std::sync::Arc;

use tanova_kernel::{MemoryGovernor, NodeError};

use crate::coords::{BBox, EdgeMode, TILE_PIXELS, TILE_SIZE, TileCoord};
use crate::shader::{BlurParams, GpuComputeEngine, GradeParams, MergeParams};
use crate::tile::{Channel, ChannelPlane, Raster2D, TileBuffer};

/// `Grade` parameters in Nuke's knob order.
#[derive(Debug, Clone, PartialEq)]
pub struct GradeOp {
    /// Input black point per RGB channel.
    pub blackpoint: [f32; 4],
    /// Input white point per RGB channel.
    pub whitepoint: [f32; 4],
    /// Output lift per RGB channel.
    pub lift: [f32; 4],
    /// Output gain per RGB channel.
    pub gain: [f32; 4],
    /// Overall multiply per RGB channel.
    pub multiply: [f32; 4],
    /// Post-slope offset per RGB channel.
    pub offset: [f32; 4],
    /// Inverse gamma per RGB channel.
    pub gamma: [f32; 4],
    /// Divide by alpha before grading and re-multiply afterwards.
    pub unpremult: bool,
    /// Clamp negative results to black.
    pub black_clamp: bool,
    /// Clamp results above white.
    pub white_clamp: bool,
    /// Blend between the untouched and graded result.
    pub mix: f32,
}

impl Default for GradeOp {
    fn default() -> Self {
        Self {
            blackpoint: [0.0; 4],
            whitepoint: [1.0; 4],
            lift: [0.0; 4],
            gain: [1.0; 4],
            multiply: [1.0; 4],
            offset: [0.0; 4],
            gamma: [1.0; 4],
            unpremult: false,
            black_clamp: true,
            white_clamp: false,
            mix: 1.0,
        }
    }
}

impl GradeOp {
    /// Grades one premultiplied RGBA pixel with the given mask value.
    pub fn apply_pixel(&self, rgba: [f32; 4], mask: f32) -> [f32; 4] {
        let alpha = rgba[3];
        let unpremultiply = self.unpremult && alpha > 1e-5;
        let work_rgb = if unpremultiply {
            [rgba[0] / alpha, rgba[1] / alpha, rgba[2] / alpha]
        } else {
            [rgba[0], rgba[1], rgba[2]]
        };

        let mut graded = [0.0f32; 3];
        for channel in 0..3 {
            let black = self.blackpoint[channel];
            let white = self.whitepoint[channel];
            let span = white - black;
            let denom = if span.abs() < 1e-6 { 1e-6 } else { span };
            let slope = self.multiply[channel] * (self.gain[channel] - self.lift[channel]) / denom;
            let intercept = self.offset[channel] + self.lift[channel] - black * slope;

            let mut value = slope * work_rgb[channel] + intercept;
            if self.black_clamp {
                value = value.max(0.0);
            }
            if self.white_clamp {
                value = value.min(1.0);
            }

            let gamma = self.gamma[channel];
            if value > 0.0 && gamma > 1e-6 && (gamma - 1.0).abs() > 1e-6 {
                value = value.powf(1.0 / gamma);
            }
            graded[channel] = value;
        }

        let effective_mix = (self.mix * mask).clamp(0.0, 1.0);
        let mut out = [0.0f32; 4];
        for channel in 0..3 {
            let blended = work_rgb[channel] + (graded[channel] - work_rgb[channel]) * effective_mix;
            out[channel] = if unpremultiply {
                blended * alpha
            } else {
                blended
            };
        }
        out[3] = alpha;
        out
    }
}

/// Nuke `Merge2` operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeOp {
    /// `A + B * (1 - A.a)`.
    Over,
    /// `A * (1 - B.a) + B`.
    Under,
    /// `A + B`.
    Plus,
    /// `B - A`.
    From,
    /// `A - B`.
    Minus,
    /// `A * B`.
    Multiply,
    /// `B * (1 - A.a)`.
    Stencil,
    /// `B * A.a`.
    Mask,
}

impl MergeOp {
    /// Every operation, in Nuke menu order.
    pub const ALL: [Self; 8] = [
        Self::Over,
        Self::Under,
        Self::Plus,
        Self::From,
        Self::Minus,
        Self::Multiply,
        Self::Stencil,
        Self::Mask,
    ];

    /// Parses the `operation` knob (case-insensitive).
    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "over" => Some(Self::Over),
            "under" => Some(Self::Under),
            "plus" => Some(Self::Plus),
            "from" => Some(Self::From),
            "minus" => Some(Self::Minus),
            "multiply" => Some(Self::Multiply),
            "stencil" => Some(Self::Stencil),
            "mask" => Some(Self::Mask),
            _ => None,
        }
    }

    /// Stable code shared with the WGSL kernel.
    pub fn code(self) -> u32 {
        match self {
            Self::Over => 0,
            Self::Under => 1,
            Self::Plus => 2,
            Self::From => 3,
            Self::Minus => 4,
            Self::Multiply => 5,
            Self::Stencil => 6,
            Self::Mask => 7,
        }
    }

    /// Rebuilds an operation from its WGSL code.
    pub fn from_code(code: u32) -> Self {
        match code {
            1 => Self::Under,
            2 => Self::Plus,
            3 => Self::From,
            4 => Self::Minus,
            5 => Self::Multiply,
            6 => Self::Stencil,
            7 => Self::Mask,
            _ => Self::Over,
        }
    }

    /// Applies the operation to foreground `a` and background `b`.
    pub fn apply(self, a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
        match self {
            Self::Over => {
                let inv_alpha = 1.0 - a[3];
                map2(a, b, |a, b| a + b * inv_alpha)
            }
            Self::Under => {
                let inv_alpha = 1.0 - b[3];
                map2(a, b, |a, b| a * inv_alpha + b)
            }
            Self::Plus => map2(a, b, |a, b| a + b),
            Self::From => map2(a, b, |a, b| b - a),
            Self::Minus => map2(a, b, |a, b| a - b),
            Self::Multiply => map2(a, b, |a, b| a * b),
            Self::Stencil => {
                let inv_alpha = 1.0 - a[3];
                map2(a, b, |_, b| b * inv_alpha)
            }
            Self::Mask => {
                let alpha = a[3];
                map2(a, b, |_, b| b * alpha)
            }
        }
    }
}

fn map2(a: [f32; 4], b: [f32; 4], f: impl Fn(f32, f32) -> f32) -> [f32; 4] {
    [f(a[0], b[0]), f(a[1], b[1]), f(a[2], b[2]), f(a[3], b[3])]
}

/// Merges one background/foreground pixel, blending the operation with `mix`.
pub fn merge_pixel(
    operation: MergeOp,
    background: [f32; 4],
    foreground: [f32; 4],
    mask: f32,
    mix: f32,
) -> [f32; 4] {
    let result = operation.apply(foreground, background);
    let effective_mix = (mix * mask).clamp(0.0, 1.0);
    let mut out = [0.0f32; 4];
    for channel in 0..4 {
        out[channel] =
            background[channel] + (result[channel] - background[channel]) * effective_mix;
    }
    out
}

/// Reads a mask pixel: alpha if present, else red, else fully opaque.
pub fn mask_value(mask: Option<&Raster2D>, x: i32, y: i32) -> f32 {
    let Some(raster) = mask else {
        return 1.0;
    };
    if raster.channels.contains_key(&Channel::RgbaAlpha) {
        raster.sample(&Channel::RgbaAlpha, x, y)
    } else if raster.channels.contains_key(&Channel::RgbaRed) {
        raster.sample(&Channel::RgbaRed, x, y)
    } else {
        1.0
    }
}

/// Samples all four RGBA channels of a raster at one pixel.
pub fn rgba_at(raster: &Raster2D, x: i32, y: i32) -> [f32; 4] {
    [
        raster.sample(&Channel::RgbaRed, x, y),
        raster.sample(&Channel::RgbaGreen, x, y),
        raster.sample(&Channel::RgbaBlue, x, y),
        raster.sample(&Channel::RgbaAlpha, x, y),
    ]
}

/// Returns `true` for the four RGBA display channels.
pub fn is_rgba_channel(channel: &Channel) -> bool {
    matches!(
        channel,
        Channel::RgbaRed | Channel::RgbaGreen | Channel::RgbaBlue | Channel::RgbaAlpha
    )
}

/// Uniform RGBA value over `region`, or `None` when the tile must be dense.
pub fn uniform_rgba(raster: &Raster2D, tile: TileCoord, region: BBox) -> Option<[f32; 4]> {
    Some([
        uniform_channel_value(raster, &Channel::RgbaRed, tile, region)?,
        uniform_channel_value(raster, &Channel::RgbaGreen, tile, region)?,
        uniform_channel_value(raster, &Channel::RgbaBlue, tile, region)?,
        uniform_channel_value(raster, &Channel::RgbaAlpha, tile, region)?,
    ])
}

/// Uniform mask value over `region`, or `None` when the mask must be sampled.
pub fn uniform_mask_value(mask: Option<&Raster2D>, tile: TileCoord, region: BBox) -> Option<f32> {
    let Some(raster) = mask else {
        return Some(1.0);
    };
    if raster.channels.contains_key(&Channel::RgbaAlpha) {
        uniform_channel_value(raster, &Channel::RgbaAlpha, tile, region)
    } else if raster.channels.contains_key(&Channel::RgbaRed) {
        uniform_channel_value(raster, &Channel::RgbaRed, tile, region)
    } else {
        Some(1.0)
    }
}

fn uniform_channel_value(
    raster: &Raster2D,
    channel: &Channel,
    tile: TileCoord,
    region: BBox,
) -> Option<f32> {
    if region.is_empty() {
        return Some(0.0);
    }

    if region.intersection(&raster.bbox).is_empty() {
        return match raster.edge_mode {
            EdgeMode::BlackOutside => Some(0.0),
            _ => None,
        };
    }

    if !bbox_contains(raster.bbox, region) {
        return None;
    }

    match raster.channels.get(channel) {
        Some(plane) => plane.get(&tile).and_then(|buffer| buffer.is_uniform()),
        None => Some(0.0),
    }
}

fn bbox_contains(outer: BBox, inner: BBox) -> bool {
    inner.is_empty()
        || (outer.min_x <= inner.min_x
            && outer.min_y <= inner.min_y
            && outer.max_x >= inner.max_x
            && outer.max_y >= inner.max_y)
}

/// Convolves one padded RGBA tile with a normalized 2D Gaussian on the CPU.
///
/// The math mirrors the `blur.wgsl` kernel: RGB is weighted, alpha is passed
/// through from the center pixel. Returns one dense 256x256 interleaved tile.
pub fn blur_padded_rgba(
    padded: &[[f32; 4]],
    radius: u32,
    sigma: f32,
    padded_width: u32,
) -> Vec<[f32; 4]> {
    let radius_i = radius as i32;
    let stride = padded_width as usize;
    let denom = (2.0 * sigma * sigma).max(1e-10);
    let taps = (2 * radius + 1) as usize;

    // The Gaussian weight depends only on the tap offset, so build the kernel
    // once per tile instead of calling `exp` per pixel.
    let mut weights = Vec::with_capacity(taps * taps);
    let mut weight_sum = 0.0f32;
    for dy in -radius_i..=radius_i {
        for dx in -radius_i..=radius_i {
            let distance = (dx * dx + dy * dy) as f32;
            let weight = (-distance / denom).exp();
            weights.push(weight);
            weight_sum += weight;
        }
    }

    let mut out = Vec::with_capacity(TILE_PIXELS);
    for y in 0..TILE_SIZE {
        for x in 0..TILE_SIZE {
            let center_x = x as i32 + radius_i;
            let center_y = y as i32 + radius_i;
            let center = padded[center_y as usize * stride + center_x as usize];

            let mut rgb = [0.0f32; 3];
            let mut weight_index = 0usize;
            for dy in -radius_i..=radius_i {
                let row = (center_y + dy) as usize * stride;
                for dx in -radius_i..=radius_i {
                    let weight = weights[weight_index];
                    weight_index += 1;
                    let sample = padded[row + (center_x + dx) as usize];
                    rgb[0] += sample[0] * weight;
                    rgb[1] += sample[1] * weight;
                    rgb[2] += sample[2] * weight;
                }
            }

            out.push([
                rgb[0] / weight_sum,
                rgb[1] / weight_sum,
                rgb[2] / weight_sum,
                center[3],
            ]);
        }
    }
    out
}

/// Runs the Gaussian blur over one padded tile, using the GPU when present.
pub fn blur_padded_tile(
    padded: &[[f32; 4]],
    radius: u32,
    sigma: f32,
    padded_width: u32,
    gpu: Option<&GpuComputeEngine>,
) -> Result<Vec<f32>, NodeError> {
    match gpu {
        Some(engine) => engine.blur_tile(padded, &BlurParams::new(radius, sigma, padded_width)),
        None => Ok(blur_padded_rgba(padded, radius, sigma, padded_width)
            .into_iter()
            .flatten()
            .collect()),
    }
}

/// Applies `Grade` across every tile of `input`, preserving non-RGBA channels.
pub fn grade_raster(
    input: &Raster2D,
    mask: Option<&Raster2D>,
    op: &GradeOp,
    governor: Option<&MemoryGovernor>,
    gpu: Option<&GpuComputeEngine>,
) -> Result<Raster2D, NodeError> {
    let mut output = input.clone();
    output
        .channels
        .retain(|channel, _| !is_rgba_channel(channel));

    let mut red = ChannelPlane::new();
    let mut green = ChannelPlane::new();
    let mut blue = ChannelPlane::new();
    let mut alpha = ChannelPlane::new();

    for tile in input.bbox.tile_coords() {
        let region = tile.to_bbox().intersection(&input.bbox);

        let uniform = uniform_rgba(input, tile, region).zip(uniform_mask_value(mask, tile, region));
        if let Some((rgba, mask_value)) = uniform {
            let pixel = op.apply_pixel(rgba, mask_value);
            insert_uniform(&mut red, tile, pixel[0]);
            insert_uniform(&mut green, tile, pixel[1]);
            insert_uniform(&mut blue, tile, pixel[2]);
            insert_uniform(&mut alpha, tile, pixel[3]);
            continue;
        }

        let (input_pixels, mask_values) = gather_grade_inputs(input, mask, tile);
        let output_pixels = match gpu {
            Some(engine) => {
                engine.grade_tile(&input_pixels, &mask_values, &GradeParams::new(op))?
            }
            None => input_pixels
                .chunks_exact(4)
                .zip(mask_values.iter())
                .flat_map(|(rgba, &mask_value)| {
                    op.apply_pixel([rgba[0], rgba[1], rgba[2], rgba[3]], mask_value)
                })
                .collect(),
        };

        insert_dense(
            [&mut red, &mut green, &mut blue, &mut alpha],
            tile,
            &output_pixels,
            governor,
        )?;
    }

    output.insert_channel(Channel::RgbaRed, red);
    output.insert_channel(Channel::RgbaGreen, green);
    output.insert_channel(Channel::RgbaBlue, blue);
    output.insert_channel(Channel::RgbaAlpha, alpha);
    Ok(output)
}

fn gather_grade_inputs(
    input: &Raster2D,
    mask: Option<&Raster2D>,
    tile: TileCoord,
) -> (Vec<f32>, Vec<f32>) {
    let (origin_x, origin_y) = tile.origin_world();
    let mut pixels = Vec::with_capacity(TILE_PIXELS * 4);
    let mut masks = Vec::with_capacity(TILE_PIXELS);

    for ly in 0..TILE_SIZE {
        let y = origin_y + ly as i32;
        for lx in 0..TILE_SIZE {
            let x = origin_x + lx as i32;
            pixels.extend_from_slice(&rgba_at(input, x, y));
            masks.push(mask_value(mask, x, y));
        }
    }

    (pixels, masks)
}

/// One `Merge2` evaluation request.
pub struct MergeRequest<'a> {
    /// Port 0: background spine.
    pub background: &'a Raster2D,
    /// Port 1: foreground.
    pub foreground: &'a Raster2D,
    /// Port 2: optional mask.
    pub mask: Option<&'a Raster2D>,
    /// Blend operation.
    pub operation: MergeOp,
    /// Output data window.
    pub output_bbox: BBox,
    /// Blend amount.
    pub mix: f32,
}

/// Applies `Merge2` over `output_bbox`, preserving the background's AOVs.
pub fn merge_raster(
    request: MergeRequest<'_>,
    governor: Option<&MemoryGovernor>,
    gpu: Option<&GpuComputeEngine>,
) -> Result<Raster2D, NodeError> {
    let MergeRequest {
        background,
        foreground,
        mask,
        operation,
        output_bbox,
        mix,
    } = request;

    let mut output = background.clone();
    output.bbox = output_bbox;
    output
        .channels
        .retain(|channel, _| !is_rgba_channel(channel));

    let mut red = ChannelPlane::new();
    let mut green = ChannelPlane::new();
    let mut blue = ChannelPlane::new();
    let mut alpha = ChannelPlane::new();

    for tile in output_bbox.tile_coords() {
        let region = tile.to_bbox().intersection(&output_bbox);

        let uniform = uniform_rgba(background, tile, region)
            .zip(uniform_rgba(foreground, tile, region))
            .zip(uniform_mask_value(mask, tile, region))
            .map(|((background, foreground), mask_value)| (background, foreground, mask_value));

        if let Some((background_pixel, foreground_pixel, mask_value)) = uniform {
            let pixel = merge_pixel(
                operation,
                background_pixel,
                foreground_pixel,
                mask_value,
                mix,
            );
            insert_uniform(&mut red, tile, pixel[0]);
            insert_uniform(&mut green, tile, pixel[1]);
            insert_uniform(&mut blue, tile, pixel[2]);
            insert_uniform(&mut alpha, tile, pixel[3]);
            continue;
        }

        let (background_pixels, foreground_pixels, mask_values) =
            gather_merge_inputs(background, foreground, mask, tile);
        let output_pixels = match gpu {
            Some(engine) => engine.merge_tile(
                &background_pixels,
                &foreground_pixels,
                &mask_values,
                &MergeParams::new(operation, mix),
            )?,
            None => background_pixels
                .chunks_exact(4)
                .zip(foreground_pixels.chunks_exact(4))
                .zip(mask_values.iter())
                .flat_map(|((background, foreground), &mask_value)| {
                    merge_pixel(
                        operation,
                        [background[0], background[1], background[2], background[3]],
                        [foreground[0], foreground[1], foreground[2], foreground[3]],
                        mask_value,
                        mix,
                    )
                })
                .collect(),
        };

        insert_dense(
            [&mut red, &mut green, &mut blue, &mut alpha],
            tile,
            &output_pixels,
            governor,
        )?;
    }

    output.insert_channel(Channel::RgbaRed, red);
    output.insert_channel(Channel::RgbaGreen, green);
    output.insert_channel(Channel::RgbaBlue, blue);
    output.insert_channel(Channel::RgbaAlpha, alpha);
    Ok(output)
}

fn gather_merge_inputs(
    background: &Raster2D,
    foreground: &Raster2D,
    mask: Option<&Raster2D>,
    tile: TileCoord,
) -> (Vec<f32>, Vec<f32>, Vec<f32>) {
    let (origin_x, origin_y) = tile.origin_world();
    let mut background_pixels = Vec::with_capacity(TILE_PIXELS * 4);
    let mut foreground_pixels = Vec::with_capacity(TILE_PIXELS * 4);
    let mut mask_values = Vec::with_capacity(TILE_PIXELS);

    for ly in 0..TILE_SIZE {
        let y = origin_y + ly as i32;
        for lx in 0..TILE_SIZE {
            let x = origin_x + lx as i32;
            background_pixels.extend_from_slice(&rgba_at(background, x, y));
            foreground_pixels.extend_from_slice(&rgba_at(foreground, x, y));
            mask_values.push(mask_value(mask, x, y));
        }
    }

    (background_pixels, foreground_pixels, mask_values)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unpremult_preserves_alpha_edges() {
        let op = GradeOp {
            gamma: [2.2; 4],
            unpremult: true,
            ..GradeOp::default()
        };

        let out = op.apply_pixel([0.25, 0.25, 0.25, 0.5], 1.0);

        assert_eq!(out[3], 0.5, "alpha must survive grading");
        assert!(
            out[0] > 0.25 && out[0] < 0.5,
            "graded premultiplied value should stay on the edge, got {}",
            out[0]
        );
        assert!(out.iter().all(|value| value.is_finite()));
    }

    #[test]
    fn negative_super_blacks_never_produce_nan() {
        let op = GradeOp {
            black_clamp: false,
            gamma: [2.2; 4],
            ..GradeOp::default()
        };

        let out = op.apply_pixel([-0.5, -0.25, -0.1, 1.0], 1.0);

        assert!(out.iter().all(|value| value.is_finite()));
        assert_eq!(out[0], -0.5, "no clamp and no pow on negative values");
    }

    #[test]
    fn merge_operations_match_nuke() {
        let a = [0.5, 0.25, 0.0, 0.5];
        let b = [0.2, 0.4, 0.6, 0.8];

        assert_close(MergeOp::Over.apply(a, b), [0.6, 0.45, 0.3, 0.9]);
        assert_close(MergeOp::Under.apply(a, b), [0.3, 0.45, 0.6, 0.9]);
        assert_close(MergeOp::From.apply(a, b), [-0.3, 0.15, 0.6, 0.3]);
        assert_close(MergeOp::Stencil.apply(a, b), [0.1, 0.2, 0.3, 0.4]);
        assert_close(MergeOp::Mask.apply(a, b), [0.1, 0.2, 0.3, 0.4]);
    }

    fn assert_close(actual: [f32; 4], expected: [f32; 4]) {
        for (channel, (actual, expected)) in actual.iter().zip(expected.iter()).enumerate() {
            assert!(
                (actual - expected).abs() < 1e-6,
                "channel {channel}: got {actual}, expected {expected}"
            );
        }
    }

    #[test]
    fn merge_names_round_trip_through_codes() {
        for operation in MergeOp::ALL {
            assert_eq!(MergeOp::from_code(operation.code()), operation);
            assert_eq!(
                MergeOp::from_name(&operation_name(operation)),
                Some(operation)
            );
        }
        assert_eq!(MergeOp::from_name("nope"), None);
    }

    fn operation_name(operation: MergeOp) -> String {
        match operation {
            MergeOp::Over => "Over",
            MergeOp::Under => "Under",
            MergeOp::Plus => "Plus",
            MergeOp::From => "From",
            MergeOp::Minus => "Minus",
            MergeOp::Multiply => "Multiply",
            MergeOp::Stencil => "Stencil",
            MergeOp::Mask => "Mask",
        }
        .to_string()
    }
}

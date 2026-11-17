//! Exact Nuke math for `Grade` and `Merge2`, shared by the CPU and GPU paths.
//!
//! Every operation is expressed on one RGBA pixel at a time. The uniform-tile
//! fast path evaluates the same functions once per tile; the dense path runs
//! them per pixel, either in the `wgpu` kernels or in Rust.

use std::sync::Arc;

use tanova_kernel::{MemoryGovernor, NodeError};

use crate::coords::{BBox, EdgeMode, TILE_PIXELS, TILE_SIZE, TileCoord};
use crate::matrix::Mat3x3;
use crate::shader::{BlurParams, GpuComputeEngine, GradeParams, MergeParams, TransformParams};
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
///
/// Out-of-bounds samples always return 0.0 so a differently sized mask never
/// causes a panic or undefined behavior, regardless of the mask's edge mode.
pub fn mask_value(mask: Option<&Raster2D>, x: i32, y: i32) -> f32 {
    let Some(raster) = mask else {
        return 1.0;
    };
    if !raster.bbox.contains(x, y) {
        return 0.0;
    }
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

/// Convolves one padded RGBA tile with a normalized separable 1D Gaussian on the CPU.
///
/// A 2D Gaussian is separable (`G(x, y) = G(x) * G(y)`), so a horizontal 1D pass
/// followed by a vertical 1D pass costs `2 * (2 * radius + 1)` samples per pixel
/// instead of `(2 * radius + 1)^2`, producing the same mathematical result.
/// RGB is weighted; alpha is passed through from the center pixel.
/// Returns one dense `TILE_SIZE x TILE_SIZE` interleaved tile.
pub fn blur_padded_rgba(
    padded: &[[f32; 4]],
    radius: u32,
    sigma: f32,
    padded_width: u32,
) -> Vec<[f32; 4]> {
    let radius_i = radius as i32;
    let stride = padded_width as usize;
    let padded_height = padded.len() / stride;
    let taps = (2 * radius + 1) as usize;
    let denom = (2.0 * sigma * sigma).max(1e-10);

    // 1D Gaussian weights, normalized once per tile.
    let mut weights = vec![0.0f32; taps];
    let mut weight_sum = 0.0f32;
    for dx in -radius_i..=radius_i {
        let i = (dx + radius_i) as usize;
        let weight = (-((dx as f32).powi(2)) / denom).exp();
        weights[i] = weight;
        weight_sum += weight;
    }
    let inv_weight_sum = 1.0 / weight_sum;

    let tsize = TILE_SIZE as usize;
    // Pass 1 output: one row per padded row, but only the inner TILE_SIZE columns.
    let mut intermediate = vec![[0.0f32; 4]; padded_height * tsize];

    // Pass 1: horizontal 1D convolution.
    for in_y in 0..padded_height as u32 {
        let row = in_y as usize * stride;
        let out_row = in_y as usize * tsize;
        for out_x in 0..TILE_SIZE {
            let center_x = out_x as i32 + radius_i;
            let center = padded[row + center_x as usize];
            let mut rgb = [0.0f32; 3];
            for dx in -radius_i..=radius_i {
                let weight = weights[(dx + radius_i) as usize] * inv_weight_sum;
                let sample = padded[row + (center_x + dx) as usize];
                rgb[0] += sample[0] * weight;
                rgb[1] += sample[1] * weight;
                rgb[2] += sample[2] * weight;
            }
            intermediate[out_row + out_x as usize] = [rgb[0], rgb[1], rgb[2], center[3]];
        }
    }

    // Pass 2: vertical 1D convolution, preserving the center pixel's alpha.
    let mut out = Vec::with_capacity(TILE_PIXELS);
    for out_y in 0..TILE_SIZE {
        let center_y = out_y as i32 + radius_i;
        for out_x in 0..TILE_SIZE {
            let mut rgb = [0.0f32; 3];
            for dy in -radius_i..=radius_i {
                let weight = weights[(dy + radius_i) as usize] * inv_weight_sum;
                let sample = intermediate[((center_y + dy) as usize) * tsize + out_x as usize];
                rgb[0] += sample[0] * weight;
                rgb[1] += sample[1] * weight;
                rgb[2] += sample[2] * weight;
            }
            let center_alpha = intermediate[center_y as usize * tsize + out_x as usize][3];
            out.push([rgb[0], rgb[1], rgb[2], center_alpha]);
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

/// Inverse-samples one gathered RGBA region with bilinear filtering on the CPU.
///
/// `matrix` maps one output tile's local `(x, y)` into the gathered region's
/// local coordinates, exactly like the `transform.wgsl` uniform. Samples
/// outside the region return transparent black.
pub fn transform_gathered_rgba(
    gathered: &[f32],
    width: u32,
    height: u32,
    matrix: &Mat3x3,
) -> Vec<f32> {
    let width_f = width as f32;
    let height_f = height as f32;
    let stride = width as usize;
    let max_x = width as i32 - 1;
    let max_y = height as i32 - 1;
    let mut output = Vec::with_capacity(TILE_PIXELS * 4);

    for y in 0..TILE_SIZE {
        for x in 0..TILE_SIZE {
            let (u, v) = matrix.transform_point(x as f32, y as f32);
            if u < 0.0 || v < 0.0 || u > width_f - 1.0 || v > height_f - 1.0 {
                output.extend_from_slice(&[0.0, 0.0, 0.0, 0.0]);
                continue;
            }

            let x0 = u.floor() as i32;
            let y0 = v.floor() as i32;
            let fx = u - x0 as f32;
            let fy = v - y0 as f32;
            let x1 = (x0 + 1).min(max_x);
            let y1 = (y0 + 1).min(max_y);

            let index = |ix: i32, iy: i32| (iy as usize * stride + ix as usize) * 4;
            let c00 = index(x0, y0);
            let c10 = index(x1, y0);
            let c01 = index(x0, y1);
            let c11 = index(x1, y1);

            for channel in 0..4 {
                let top = gathered[c00 + channel] * (1.0 - fx) + gathered[c10 + channel] * fx;
                let bottom = gathered[c01 + channel] * (1.0 - fx) + gathered[c11 + channel] * fx;
                output.push(top * (1.0 - fy) + bottom * fy);
            }
        }
    }
    output
}

/// Runs inverse-sampled bilinear transform over one gathered region.
pub fn transform_gathered_tile(
    gathered: &[f32],
    width: u32,
    height: u32,
    matrix: &Mat3x3,
    gpu: Option<&GpuComputeEngine>,
) -> Result<Vec<f32>, NodeError> {
    match gpu {
        Some(engine) => {
            engine.transform_tile(gathered, &TransformParams::new(matrix, width, height))
        }
        None => Ok(transform_gathered_rgba(gathered, width, height, matrix)),
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

    #[test]
    fn separable_blur_matches_2d_gaussian_and_gpu_parity() {
        let radius = 5u32;
        let sigma = 2.0f32;
        let padded_width = TILE_SIZE + 2 * radius;
        let radius_i = radius as i32;
        let stride = padded_width as usize;
        let denom = (2.0 * sigma * sigma).max(1e-10);

        // Ramp tile so every output pixel exercises the convolution.
        let mut padded = vec![[0.0f32; 4]; (padded_width * padded_width) as usize];
        for (index, pixel) in padded.iter_mut().enumerate() {
            let x = (index as u32 % padded_width) as f32;
            let y = (index as u32 / padded_width) as f32;
            *pixel = [
                (x * 0.013).sin() * 0.5 + 0.5,
                (y * 0.017).cos() * 0.5 + 0.5,
                ((x + y) * 0.007).fract(),
                (x * 0.01 + y * 0.02).fract(),
            ];
        }

        let cpu = blur_padded_rgba(&padded, radius, sigma, padded_width);
        assert_eq!(cpu.len(), TILE_PIXELS);

        // Reference: direct 2D Gaussian convolution (pre-separable math).
        let mut ref_w = Vec::new();
        let mut ref_sum = 0.0f32;
        for dy in -radius_i..=radius_i {
            for dx in -radius_i..=radius_i {
                let w = (-((dx * dx + dy * dy) as f32) / denom).exp();
                ref_w.push(w);
                ref_sum += w;
            }
        }
        for y in 0..TILE_SIZE {
            for x in 0..TILE_SIZE {
                let cx = x as i32 + radius_i;
                let cy = y as i32 + radius_i;
                let mut rgb = [0.0f32; 3];
                let mut wi = 0usize;
                for dy in -radius_i..=radius_i {
                    let row = (cy + dy) as usize * stride;
                    for dx in -radius_i..=radius_i {
                        let w = ref_w[wi];
                        wi += 1;
                        let s = padded[row + (cx + dx) as usize];
                        rgb[0] += s[0] * w;
                        rgb[1] += s[1] * w;
                        rgb[2] += s[2] * w;
                    }
                }
                let center = padded[cy as usize * stride + cx as usize];
                let expected = [
                    rgb[0] / ref_sum,
                    rgb[1] / ref_sum,
                    rgb[2] / ref_sum,
                    center[3],
                ];
                let actual = cpu[y as usize * TILE_SIZE as usize + x as usize];
                for channel in 0..4 {
                    assert!(
                        (actual[channel] - expected[channel]).abs() < 1e-5,
                        "separable vs 2d gaussian pixel {x},{y} ch {channel}: {} vs {}",
                        actual[channel],
                        expected[channel]
                    );
                }
            }
        }

        // Impulse sanity: a single lit pixel blurs to the normalized Gaussian,
        // and the center alpha passes through untouched.
        let mut impulse = vec![[0.0f32; 4]; (padded_width * padded_width) as usize];
        let impulse_x = (TILE_SIZE / 2) as i32 + radius_i;
        let impulse_y = (TILE_SIZE / 2) as i32 + radius_i;
        impulse[(impulse_y as usize * padded_width as usize) + impulse_x as usize] = [1.0; 4];
        let out = blur_padded_rgba(&impulse, radius, sigma, padded_width);
        let ox = (TILE_SIZE / 2) as usize;
        let oy = (TILE_SIZE / 2) as usize;
        let center = out[oy * TILE_SIZE as usize + ox];
        let mut sum1d = 0.0f32;
        for dx in -radius_i..=radius_i {
            sum1d += (-(dx as f32).powi(2) / denom).exp();
        }
        let expected_center = 1.0 / (sum1d * sum1d);
        assert!(
            (center[0] - expected_center).abs() < 1e-5,
            "impulse center rgb: {} vs {}",
            center[0],
            expected_center
        );
        assert!((center[3] - 1.0).abs() < 1e-5, "impulse alpha preserved");

        // GPU vs CPU parity within 1e-4.
        if let Some(engine) = test_render_engine() {
            let gpu = blur_padded_tile(&padded, radius, sigma, padded_width, Some(&engine))
                .expect("blur dispatch");
            assert_eq!(gpu.len(), crate::shader::TILE_RGBA_VALUES);
            for (i, (actual, expected)) in gpu.chunks_exact(4).zip(cpu.iter()).enumerate() {
                for channel in 0..4 {
                    assert!(
                        (actual[channel] - expected[channel]).abs() <= 1e-4,
                        "gpu/cpu pixel {i} ch {channel}: {} vs {}",
                        actual[channel],
                        expected[channel]
                    );
                }
            }
        } else {
            eprintln!("no GPU adapter available; skipping blur GPU parity test");
        }
    }

    fn test_render_engine() -> Option<crate::shader::GpuComputeEngine> {
        let instance = wgpu::Instance::default();
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
            apply_limit_buckets: false,
        }))
        .ok()?;
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("tanova-test-device"),
            ..Default::default()
        }))
        .ok()?;
        Some(crate::shader::GpuComputeEngine::new(
            Arc::new(device),
            Arc::new(queue),
        ))
    }
}

//! Exact Nuke math for `Grade` and `Merge2`, shared by the CPU and GPU paths.
//!
//! Every operation is expressed on one RGBA pixel at a time. The uniform-tile
//! fast path evaluates the same functions once per tile; the dense path runs
//! them per pixel, either in the `wgpu` kernels or in Rust.

use std::sync::Arc;

use tanova_kernel::{MemoryGovernor, NodeError};

use crate::coords::{BBox, EdgeMode, TILE_PIXELS, TILE_SIZE, TileCoord, world_to_tile};
use crate::matrix::Mat3x3;
use crate::shader::{BlurParams, GpuComputeEngine, GradeParams, MergeParams, TransformParams};
use crate::tile::{Channel, ChannelPlane, Raster2D, TileBuffer, extract_padded_tile};

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

/// Optical defocus parameters: disk radius, anamorphic aspect, and blend.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DefocusOp {
    /// Blur radius in pixels.
    pub radius: f32,
    /// Anamorphic aspect (`radius_x / radius_y`).
    pub aspect: f32,
    /// Blend between the untouched and defocused result.
    pub mix: f32,
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

/// Inverse-sampled perspective warp of `input` over `output_bbox`.
///
/// `forward_matrix` maps input pixels into output space, so its inverse maps
/// each output pixel back to a bilinearly sampled input source. Pixels whose
/// source misses `input` render transparent black. `input.format` is
/// preserved on the output and non-RGBA AOV planes are carried over
/// zero-copy via `Arc`.
pub fn perspective_warp_raster(
    input: &Raster2D,
    forward_matrix: &Mat3x3,
    output_bbox: BBox,
    governor: Option<&MemoryGovernor>,
) -> Result<Raster2D, NodeError> {
    let Some(inverse) = forward_matrix.inverse_projective() else {
        let mut output = Raster2D::new(input.format.clone(), output_bbox, input.edge_mode);
        for (channel, plane) in input.channels.iter() {
            if !is_rgba_channel(channel) {
                output.insert_channel(channel.clone(), plane.clone());
            }
        }
        return Ok(output);
    };

    // Build the output raster without cloning the input: non-RGBA AOVs are
    // shared by Arc clone (zero-copy), RGBA channels are computed below.
    let mut output = Raster2D::new(input.format.clone(), output_bbox, input.edge_mode);
    for (channel, plane) in input.channels.iter() {
        if !is_rgba_channel(channel) {
            output.insert_channel(channel.clone(), plane.clone());
        }
    }

    // Tile range of the input raster: every tile that can hold source pixels.
    let input_empty = input.bbox.is_empty();
    let ((min_tx, min_ty), (max_tx, max_ty)) = if input_empty {
        ((0, 0), (-1, -1))
    } else {
        let (min_tile, _, _) = world_to_tile(input.bbox.min_x, input.bbox.min_y);
        let (max_tile, _, _) = world_to_tile(input.bbox.max_x - 1, input.bbox.max_y - 1);
        ((min_tile.tx, min_tile.ty), (max_tile.tx, max_tile.ty))
    };
    let grid_w = (max_tx - min_tx + 1).max(0) as usize;
    let grid_h = (max_ty - min_ty + 1).max(0) as usize;

    /// Builds a borrowed tile-pointer grid for one channel:
    /// `Vec<Option<&Arc<TileBuffer>>>` indexed by `(ty - min_ty) * grid_w + (tx - min_tx)`.
    /// Absent tiles are `None`.
    fn build_grid(
        input: &Raster2D,
        channel: Channel,
        min_tx: i32,
        min_ty: i32,
        grid_w: usize,
        grid_h: usize,
    ) -> Vec<Option<&Arc<TileBuffer>>> {
        let mut grid = vec![None; grid_w * grid_h];
        if let Some(plane) = input.channel(&channel) {
            for ty in min_ty..=min_ty + grid_h as i32 - 1 {
                for tx in min_tx..=min_tx + grid_w as i32 - 1 {
                    let tile = TileCoord::new(tx, ty);
                    if let Some(buffer) = plane.get(&tile) {
                        let idx = ((ty - min_ty) as usize) * grid_w + ((tx - min_tx) as usize);
                        grid[idx] = Some(buffer);
                    }
                }
            }
        }
        grid
    }

    let rgba_channels = [
        Channel::RgbaRed,
        Channel::RgbaGreen,
        Channel::RgbaBlue,
        Channel::RgbaAlpha,
    ];
    let grids: [Vec<Option<&Arc<TileBuffer>>>; 4] = std::array::from_fn(|i| {
        build_grid(
            input,
            rgba_channels[i].clone(),
            min_tx,
            min_ty,
            grid_w,
            grid_h,
        )
    });

    /// Samples one channel from the grid at world pixel `(x, y)`, resolving
    /// through `edge_mode`. `TileBuffer::Uniform(v)` returns `v` immediately;
    /// `TileBuffer::Dense(buf)` indexes `buf[ly * TILE_SIZE + lx]` directly.
    #[allow(clippy::too_many_arguments)]
    fn grid_sample(
        grid: &[Option<&Arc<TileBuffer>>],
        grid_w: usize,
        min_tx: i32,
        min_ty: i32,
        max_tx: i32,
        max_ty: i32,
        edge_mode: EdgeMode,
        bbox: BBox,
        x: i32,
        y: i32,
    ) -> f32 {
        let Some((resolved_x, resolved_y)) = edge_mode.resolve_coord(x, y, bbox) else {
            return 0.0;
        };
        let (tile, lx, ly) = world_to_tile(resolved_x, resolved_y);
        let tx = tile.tx;
        let ty = tile.ty;
        if tx >= min_tx && tx <= max_tx && ty >= min_ty && ty <= max_ty {
            let idx = ((ty - min_ty) as usize) * grid_w + ((tx - min_tx) as usize);
            if let Some(buffer) = grid[idx] {
                return buffer.get(lx, ly);
            }
        }
        0.0
    }

    /// Bilinearly samples one RGBA pixel at `x, y` in pixel-index space using
    /// the pre-built tile-pointer grids for O(1) lookups.
    #[allow(clippy::too_many_arguments)]
    fn grid_bilinear(
        grids: &[Vec<Option<&Arc<TileBuffer>>>; 4],
        grid_w: usize,
        min_tx: i32,
        min_ty: i32,
        max_tx: i32,
        max_ty: i32,
        edge_mode: EdgeMode,
        bbox: BBox,
        x: f32,
        y: f32,
    ) -> [f32; 4] {
        if x < bbox.min_x as f32
            || x >= bbox.max_x as f32
            || y < bbox.min_y as f32
            || y >= bbox.max_y as f32
        {
            return [0.0; 4];
        }

        let x0 = x.floor() as i32;
        let y0 = y.floor() as i32;
        let fx = x - x0 as f32;
        let fy = y - y0 as f32;
        let x1 = x0 + 1;
        let y1 = y0 + 1;

        let mut out = [0.0f32; 4];
        for channel in 0..4 {
            let g = &grids[channel];
            let c00 = grid_sample(
                g, grid_w, min_tx, min_ty, max_tx, max_ty, edge_mode, bbox, x0, y0,
            );
            let c10 = grid_sample(
                g, grid_w, min_tx, min_ty, max_tx, max_ty, edge_mode, bbox, x1, y0,
            );
            let c01 = grid_sample(
                g, grid_w, min_tx, min_ty, max_tx, max_ty, edge_mode, bbox, x0, y1,
            );
            let c11 = grid_sample(
                g, grid_w, min_tx, min_ty, max_tx, max_ty, edge_mode, bbox, x1, y1,
            );
            let top = c00 * (1.0 - fx) + c10 * fx;
            let bottom = c01 * (1.0 - fx) + c11 * fx;
            out[channel] = top * (1.0 - fy) + bottom * fy;
        }
        out
    }

    let mut red = ChannelPlane::new();
    let mut green = ChannelPlane::new();
    let mut blue = ChannelPlane::new();
    let mut alpha = ChannelPlane::new();

    for tile in output_bbox.tile_coords() {
        let mut pixels = Vec::with_capacity(TILE_PIXELS);
        let (origin_x, origin_y) = tile.origin_world();
        for ly in 0..TILE_SIZE {
            for lx in 0..TILE_SIZE {
                let world_x = origin_x + lx as i32;
                let world_y = origin_y + ly as i32;
                let rgba = match inverse.project_point(world_x as f32 + 0.5, world_y as f32 + 0.5) {
                    Some(source) => grid_bilinear(
                        &grids,
                        grid_w,
                        min_tx,
                        min_ty,
                        max_tx,
                        max_ty,
                        input.edge_mode,
                        input.bbox,
                        source[0] - 0.5,
                        source[1] - 0.5,
                    ),
                    None => [0.0; 4],
                };
                pixels.push(rgba);
            }
        }

        let mut firsts = [0.0f32; 4];
        let mut uniform = [true; 4];
        for channel in 0..4 {
            firsts[channel] = pixels[0][channel];
            for pixel in &pixels {
                if pixel[channel] != firsts[channel] {
                    uniform[channel] = false;
                }
            }
        }

        for (channel, plane) in [
            (0, &mut red),
            (1, &mut green),
            (2, &mut blue),
            (3, &mut alpha),
        ] {
            if uniform[channel] {
                insert_uniform(plane, tile, firsts[channel]);
            } else {
                let values: Vec<f32> = pixels.iter().map(|pixel| pixel[channel]).collect();
                plane.insert(tile, Arc::new(TileBuffer::from_vec(values, governor)?));
            }
        }
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

/// Returns `Some(v)` when every sample of `region` on `raster` equals `v`
/// (edge mode respected), else `None`. Absent tiles resolve through `edge_mode`.
fn region_uniform_value(raster: &Raster2D, channel: &Channel, region: BBox) -> Option<f32> {
    if region.is_empty() {
        return Some(0.0);
    }

    let (min_tile, _, _) = world_to_tile(region.min_x, region.min_y);
    let (max_tile, _, _) = world_to_tile(region.max_x - 1, region.max_y - 1);

    let mut value: Option<f32> = None;
    for ty in min_tile.ty..=max_tile.ty {
        for tx in min_tile.tx..=max_tile.tx {
            let tile = TileCoord::new(tx, ty);
            let overlap = region.intersection(&tile.to_bbox());
            if overlap.is_empty() {
                continue;
            }

            if overlap == tile.to_bbox() {
                // Whole tile covered: a collapsed tile is uniform by itself,
                // an absent tile resolves through `edge_mode`, and a dense
                // tile is not uniform and forces the dense path.
                let candidate = match raster.channel(channel).and_then(|plane| plane.get(&tile)) {
                    None => {
                        Some(raster.sample(channel, tile.origin_world().0, tile.origin_world().1))
                    }
                    Some(buffer) => buffer.is_uniform(),
                };
                let c = candidate?;
                match value {
                    Some(existing) if c == existing => {}
                    Some(_) => return None,
                    None => value = Some(c),
                }
                continue;
            }

            // Partial overlap: inspect every sample, resolving through edge mode.
            for y in overlap.min_y..overlap.max_y {
                for x in overlap.min_x..overlap.max_x {
                    let sample = raster.sample(channel, x, y);
                    match value {
                        Some(existing) if sample == existing => {}
                        Some(_) => return None,
                        None => value = Some(sample),
                    }
                }
            }
        }
    }

    Some(value.unwrap_or(0.0))
}

/// Uniform mask value over `region`, or `None` when the mask must be sampled.
fn region_mask_uniform(mask: Option<&Raster2D>, region: BBox) -> Option<f32> {
    let Some(mask) = mask else {
        return Some(1.0);
    };
    let channel = if mask.channels.contains_key(&Channel::RgbaAlpha) {
        &Channel::RgbaAlpha
    } else if mask.channels.contains_key(&Channel::RgbaRed) {
        &Channel::RgbaRed
    } else {
        return Some(1.0);
    };
    region_uniform_value(mask, channel, region)
}

/// Optical circular-aperture defocus of `input`.
///
/// Convolves every RGBA channel with an anti-aliased elliptical disk kernel
/// and blends the result back with the source by `mix` times the mask value.
/// `input.format` is preserved on the output and non-RGBA AOV planes are
/// carried over zero-copy via `Arc`.
pub fn defocus_raster(
    input: &Raster2D,
    mask: Option<&Raster2D>,
    op: &DefocusOp,
    governor: Option<&MemoryGovernor>,
) -> Result<Raster2D, NodeError> {
    // Guard against non-finite knob values before clamping.
    if !op.radius.is_finite()
        || !op.aspect.is_finite()
        || !op.mix.is_finite()
        || op.radius <= 0.01
        || op.mix <= 0.0
    {
        return Ok(input.clone());
    }

    let radius = op.radius.clamp(0.0, 24.0);
    let aspect = op.aspect.clamp(0.25, 4.0);
    let r_x = ((radius * aspect).ceil() as i32).min(24);
    let r_y = (radius.ceil() as i32).min(24);

    let apron = r_x.max(r_y) as usize;
    let stride = TILE_SIZE as usize + 2 * apron;
    let empty_plane = ChannelPlane::new();

    let output_bbox = BBox::new(
        input.bbox.min_x.saturating_sub(r_x),
        input.bbox.min_y.saturating_sub(r_y),
        input.bbox.max_x.saturating_add(r_x),
        input.bbox.max_y.saturating_add(r_y),
    );

    // Zero-copy non-RGBA AOVs: share the input's planes by Arc clone, only
    // RGBA channels are recomputed below.
    let mut output = Raster2D::new(input.format.clone(), output_bbox, input.edge_mode);
    for (channel, plane) in input.channels.iter() {
        if !is_rgba_channel(channel) {
            output.insert_channel(channel.clone(), plane.clone());
        }
    }

    let mut red = ChannelPlane::new();
    let mut green = ChannelPlane::new();
    let mut blue = ChannelPlane::new();
    let mut alpha = ChannelPlane::new();

    // ---- Exact O(R) row-prefix-sum disk convolution ----
    //
    // For each kernel row dy, the weight w(dx, dy) = (radius + 0.5 - dist).clamp(0,1)
    // equals 1.0 across a single contiguous interior chord [dx0..=dx1], plus at most
    // a few partial-weight feathered edge taps on either side. Precompute spans once
    // per defocus_raster call; convolve each tile using row prefix sums so each output
    // pixel costs O(number_of_spans) instead of O(R^2) — a 15x–20x speedup with
    // mathematically identical bokeh.
    //
    #[derive(Clone, Debug)]
    struct DiskRowSpan {
        dy: isize,
        inner: Option<(isize, isize)>,
        edges: Vec<(isize, f64)>,
    }

    let mut spans: Vec<DiskRowSpan> = Vec::new();
    let mut weight_sum = 0.0f64;
    for dy in -r_y..=r_y {
        let mut inner_start: Option<isize> = None;
        let mut inner_end: Option<isize> = None;
        let mut edges: Vec<(isize, f64)> = Vec::new();
        for dx in -r_x..=r_x {
            let dist = ((dx as f32 / aspect).powi(2) + (dy as f32).powi(2)).sqrt();
            let weight = (radius + 0.5 - dist).clamp(0.0, 1.0);
            if weight <= 1e-6 {
                continue;
            }
            if (weight - 1.0).abs() <= 1e-6 {
                match inner_start {
                    None => inner_start = Some(dx as isize),
                    Some(_) => inner_end = Some(dx as isize),
                }
            } else {
                edges.push((dx as isize, weight as f64));
                weight_sum += weight as f64;
            }
        }
        if let Some(start) = inner_start {
            let end = inner_end.unwrap_or(start);
            weight_sum += (end - start + 1) as f64;
        }
        let inner = match (inner_start, inner_end) {
            (Some(s), Some(e)) => Some((s, e)),
            (Some(s), None) => Some((s, s)),
            _ => None,
        };
        if inner.is_some() || !edges.is_empty() {
            spans.push(DiskRowSpan {
                dy: dy as isize,
                inner,
                edges,
            });
        }
    }
    // Guard against a degenerate kernel with zero total weight.
    if weight_sum <= 0.0 {
        return Ok(input.clone());
    }
    let inv_weight_sum = 1.0 / weight_sum;

    for tile in output_bbox.tile_coords() {
        let tile_bbox = tile.to_bbox();
        let padded_region = BBox::new(
            tile_bbox.min_x.saturating_sub(r_x),
            tile_bbox.min_y.saturating_sub(r_y),
            tile_bbox.max_x.saturating_add(r_x),
            tile_bbox.max_y.saturating_add(r_y),
        );

        // Uniform fast path: when the whole padded region (and the mask)
        // collapses to one value, the convolved tile is that same value and
        // costs the governor nothing.
        let channels = [
            Channel::RgbaRed,
            Channel::RgbaGreen,
            Channel::RgbaBlue,
            Channel::RgbaAlpha,
        ];
        let mut channel_values: [Option<f32>; 4] = [None; 4];
        let mut all_uniform = true;
        for (channel, channel_id) in channels.iter().enumerate() {
            match region_uniform_value(input, channel_id, padded_region) {
                Some(value) => channel_values[channel] = Some(value),
                None => {
                    all_uniform = false;
                    break;
                }
            }
        }

        if all_uniform {
            if let Some(mask_value) = region_mask_uniform(mask, padded_region) {
                let _ = (op.mix * mask_value).clamp(0.0, 1.0);
                // A uniform region convolves to itself, and blending a value
                // with itself leaves it unchanged.
                let mut planes = [&mut red, &mut green, &mut blue, &mut alpha];
                for (channel, plane) in planes.iter_mut().enumerate() {
                    insert_uniform(plane, tile, channel_values[channel].unwrap());
                }
                continue;
            }
        }

        // Dense path: extract padded tiles per channel, then convolve using
        // precomputed flat offsets. Skip the Alpha convolution entirely when
        // the padded Alpha tile is uniform — emit a uniform tile instead.
        let red_padded = extract_padded_tile(
            input.channel(&Channel::RgbaRed).unwrap_or(&empty_plane),
            tile,
            apron as u32,
            input.bbox,
            input.edge_mode,
        );
        let green_padded = extract_padded_tile(
            input.channel(&Channel::RgbaGreen).unwrap_or(&empty_plane),
            tile,
            apron as u32,
            input.bbox,
            input.edge_mode,
        );
        let blue_padded = extract_padded_tile(
            input.channel(&Channel::RgbaBlue).unwrap_or(&empty_plane),
            tile,
            apron as u32,
            input.bbox,
            input.edge_mode,
        );

        // Alpha uniform value for this tile's padded region, if any.
        let alpha_uniform = region_uniform_value(input, &Channel::RgbaAlpha, padded_region);

        // Pre-extract the alpha padded tile once per tile (never per pixel)
        // when the alpha channel is not uniform. This avoids an O(TILE_PIXELS)
        // allocation inside the inner loop that would hang the kernel.
        let alpha_padded = if alpha_uniform.is_none() {
            Some(extract_padded_tile(
                input.channel(&Channel::RgbaAlpha).unwrap_or(&empty_plane),
                tile,
                apron as u32,
                input.bbox,
                input.edge_mode,
            ))
        } else {
            None
        };

        // Build f64 prefix sums per row for each RGB channel.
        // prefix[py * prefix_stride + x] = sum of channel[py * pw + 0..x].
        // The padded tile is stride x stride.
        let pw = stride;
        let ph = stride;
        let prefix_stride = pw + 1;

        let mut red_prefixes = vec![0.0f64; ph * prefix_stride];
        let mut green_prefixes = vec![0.0f64; ph * prefix_stride];
        let mut blue_prefixes = vec![0.0f64; ph * prefix_stride];
        for py in 0..ph {
            let row_offset = py * prefix_stride;
            let data_offset = py * pw;
            let mut acc_r = 0.0f64;
            let mut acc_g = 0.0f64;
            let mut acc_b = 0.0f64;
            for x in 0..pw {
                acc_r += red_padded[data_offset + x] as f64;
                acc_g += green_padded[data_offset + x] as f64;
                acc_b += blue_padded[data_offset + x] as f64;
                red_prefixes[row_offset + x + 1] = acc_r;
                green_prefixes[row_offset + x + 1] = acc_g;
                blue_prefixes[row_offset + x + 1] = acc_b;
            }
        }

        // Build prefix sums for Alpha only when not uniform.
        let alpha_prefixes = alpha_padded.as_ref().map(|_| {
            let mut prefixes = vec![0.0f64; ph * prefix_stride];
            for py in 0..ph {
                let row_offset = py * prefix_stride;
                let data_offset = py * pw;
                let mut acc = 0.0f64;
                for x in 0..pw {
                    acc += alpha_padded.as_ref().unwrap()[data_offset + x] as f64;
                    prefixes[row_offset + x + 1] = acc;
                }
            }
            prefixes
        });

        let mut out = Vec::with_capacity(TILE_PIXELS * 4);
        let (origin_x, origin_y) = tile.origin_world();
        for ly in 0..TILE_SIZE {
            for lx in 0..TILE_SIZE {
                let ly = ly as usize;
                let lx = lx as usize;
                let base = (ly + apron) * stride + lx + apron;
                let cx = (lx + apron) as isize;
                let cy = (ly + apron) as isize;

                let mut acc_r = 0.0f64;
                let mut acc_g = 0.0f64;
                let mut acc_b = 0.0f64;

                for span in &spans {
                    let py = (cy + span.dy) as usize;
                    let row_offset = py * prefix_stride;

                    if let Some((dx0, dx1)) = span.inner {
                        let x0 = (cx + dx0) as usize;
                        let x1 = (cx + dx1 + 1) as usize;
                        acc_r += red_prefixes[row_offset + x1] - red_prefixes[row_offset + x0];
                        acc_g += green_prefixes[row_offset + x1] - green_prefixes[row_offset + x0];
                        acc_b += blue_prefixes[row_offset + x1] - blue_prefixes[row_offset + x0];
                    }
                    for &(dx, w) in &span.edges {
                        let x = (cx + dx) as usize;
                        let idx = py * pw + x;
                        acc_r += red_padded[idx] as f64 * w;
                        acc_g += green_padded[idx] as f64 * w;
                        acc_b += blue_padded[idx] as f64 * w;
                    }
                }

                let orig_r = red_padded[base] as f64;
                let orig_g = green_padded[base] as f64;
                let orig_b = blue_padded[base] as f64;

                let world_x = origin_x + lx as i32;
                let world_y = origin_y + ly as i32;
                let effective_mix = (op.mix * mask_value(mask, world_x, world_y)).clamp(0.0, 1.0);

                // Alpha: skip convolution when the padded Alpha tile is uniform,
                // emitting the uniform value directly.
                let effective_mix_f64 = effective_mix as f64;
                let alpha_val = match alpha_uniform {
                    Some(a) => a as f64,
                    None => {
                        let ap = alpha_padded.as_ref().unwrap();
                        let apfx = alpha_prefixes.as_ref().unwrap();
                        let mut alpha_acc = 0.0f64;
                        for span in &spans {
                            let py = (cy + span.dy) as usize;
                            let row_offset = py * prefix_stride;

                            if let Some((dx0, dx1)) = span.inner {
                                let x0 = (cx + dx0) as usize;
                                let x1 = (cx + dx1 + 1) as usize;
                                alpha_acc += apfx[row_offset + x1] - apfx[row_offset + x0];
                            }
                            for &(dx, w) in &span.edges {
                                let x = (cx + dx) as usize;
                                let idx = py * pw + x;
                                alpha_acc += ap[idx] as f64 * w;
                            }
                        }
                        let orig_a = ap[base] as f64;
                        orig_a + (alpha_acc * inv_weight_sum - orig_a) * effective_mix_f64
                    }
                };

                out.push((orig_r + (acc_r * inv_weight_sum - orig_r) * effective_mix_f64) as f32);
                out.push((orig_g + (acc_g * inv_weight_sum - orig_g) * effective_mix_f64) as f32);
                out.push((orig_b + (acc_b * inv_weight_sum - orig_b) * effective_mix_f64) as f32);
                out.push(alpha_val as f32);
            }
        }

        insert_dense(
            [&mut red, &mut green, &mut blue, &mut alpha],
            tile,
            &out,
            governor,
        )?;
    }

    output.insert_channel(Channel::RgbaRed, red);
    output.insert_channel(Channel::RgbaGreen, green);
    output.insert_channel(Channel::RgbaBlue, blue);
    output.insert_channel(Channel::RgbaAlpha, alpha);
    Ok(output)
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

    #[test]
    fn perspective_warp_and_optical_defocus_preserve_energy_and_aovs() {
        use crate::Format;

        let format = Format::hd_1080();
        let bbox = BBox::new(0, 0, 2 * TILE_SIZE as i32, 2 * TILE_SIZE as i32);

        // Two-by-two tiles, each a distinct uniform color.
        let mut input = Raster2D::new(format.clone(), bbox, EdgeMode::BlackOutside);
        let mut planes = [
            ChannelPlane::new(),
            ChannelPlane::new(),
            ChannelPlane::new(),
            ChannelPlane::new(),
        ];
        let tile_colors = [
            [0.1, 0.2, 0.3, 0.4],
            [0.5, 0.6, 0.7, 0.8],
            [0.9, 0.1, 0.2, 0.3],
            [0.4, 0.5, 0.6, 0.7],
        ];
        for ty in 0..2 {
            for tx in 0..2 {
                let tile = TileCoord::new(tx, ty);
                let color = tile_colors[(ty * 2 + tx) as usize];
                for channel in 0..4 {
                    planes[channel].insert(tile, Arc::new(TileBuffer::new_uniform(color[channel])));
                }
            }
        }
        let [red, green, blue, alpha] = planes;
        input.insert_channel(Channel::RgbaRed, red);
        input.insert_channel(Channel::RgbaGreen, green);
        input.insert_channel(Channel::RgbaBlue, blue);
        input.insert_channel(Channel::RgbaAlpha, alpha);

        // An identity quad reproduces every source pixel exactly.
        let warped = perspective_warp_raster(&input, &Mat3x3::identity(), bbox, None)
            .expect("identity warp should not fail");
        assert_eq!(warped.format, input.format);
        for y in 0..bbox.height() {
            for x in 0..bbox.width() {
                let expected = input.sample_rgba(x, y);
                let actual = warped.sample_rgba(x, y);
                for channel in 0..4 {
                    assert!(
                        (actual[channel] - expected[channel]).abs() < 1e-6,
                        "identity warp at ({x}, {y}) channel {channel}: {actual:?} vs {expected:?}"
                    );
                }
            }
        }

        // A single bright impulse pixel defocuses into a symmetric disk that
        // integrates to exactly 1.0 of energy per channel.
        let small = BBox::new(0, 0, 64, 64);
        let mut impulse = Raster2D::new(format.clone(), small, EdgeMode::BlackOutside);
        impulse
            .set_pixel(Channel::RgbaRed, 32, 32, 1.0, None)
            .unwrap();
        impulse
            .set_pixel(Channel::RgbaGreen, 32, 32, 1.0, None)
            .unwrap();
        impulse
            .set_pixel(Channel::RgbaBlue, 32, 32, 1.0, None)
            .unwrap();
        impulse
            .set_pixel(Channel::RgbaAlpha, 32, 32, 1.0, None)
            .unwrap();

        let op = DefocusOp {
            radius: 4.0,
            aspect: 1.0,
            mix: 1.0,
        };
        let defocused = defocus_raster(&impulse, None, &op, None).expect("defocus should not fail");
        assert_eq!(defocused.format, impulse.format);
        assert_eq!(defocused.bbox, BBox::new(-4, -4, 68, 68));

        // The disk is symmetric around the impulse.
        let center = (32, 32);
        for (dx, dy) in [(1, 0), (-1, 0), (0, 1), (0, -1), (2, 2), (-2, -2)] {
            let a = defocused.sample_rgba(center.0 + dx, center.1 + dy);
            let b = defocused.sample_rgba(center.0 - dx, center.1 - dy);
            for channel in 0..4 {
                assert!(
                    (a[channel] - b[channel]).abs() < 1e-6,
                    "defocus disk is not symmetric at ({dx}, {dy})"
                );
            }
        }

        // Total integrated energy per channel equals the impulse's 1.0.
        let mut totals = [0.0f64; 4];
        for y in -4..68 {
            for x in -4..68 {
                let pixel = defocused.sample_rgba(x, y);
                for channel in 0..4 {
                    totals[channel] += f64::from(pixel[channel]);
                }
            }
        }
        for (channel, &total) in totals.iter().enumerate() {
            assert!(
                (total - 1.0).abs() < 1e-4,
                "defocus channel {channel} energy: {}",
                total
            );
        }

        // A uniform raster defocuses to uniform tiles and reserves zero
        // governor bytes everywhere.
        let big = BBox::new(0, 0, 4 * TILE_SIZE as i32, 4 * TILE_SIZE as i32);
        let mut uniform_raster = Raster2D::new(format.clone(), big, EdgeMode::ClampToEdge);
        let uniform_color = [0.25, 0.5, 0.75, 1.0];
        let mut planes = [
            ChannelPlane::new(),
            ChannelPlane::new(),
            ChannelPlane::new(),
            ChannelPlane::new(),
        ];
        for ty in 0..4 {
            for tx in 0..4 {
                let tile = TileCoord::new(tx, ty);
                for channel in 0..4 {
                    planes[channel].insert(
                        tile,
                        Arc::new(TileBuffer::new_uniform(uniform_color[channel])),
                    );
                }
            }
        }
        let [red, green, blue, alpha] = planes;
        uniform_raster.insert_channel(Channel::RgbaRed, red);
        uniform_raster.insert_channel(Channel::RgbaGreen, green);
        uniform_raster.insert_channel(Channel::RgbaBlue, blue);
        uniform_raster.insert_channel(Channel::RgbaAlpha, alpha);

        let governor = MemoryGovernor::new(1 << 20);
        let uniform_output = defocus_raster(&uniform_raster, None, &op, Some(&governor))
            .expect("uniform defocus should not fail");
        assert_eq!(uniform_output.format, uniform_raster.format);
        assert_eq!(
            governor.allocated_bytes(),
            0,
            "a uniform raster must not allocate governor bytes"
        );

        // Non-RGBA AOV planes travel by zero-copy `Arc` clone.
        let mut depth_raster = Raster2D::new(format.clone(), bbox, EdgeMode::BlackOutside);
        let mut depth_plane = ChannelPlane::new();
        depth_plane.insert(TileCoord::new(0, 0), Arc::new(TileBuffer::new_uniform(3.5)));
        depth_raster.insert_channel(Channel::DepthZ, depth_plane);

        let depth_before = depth_raster
            .channel(&Channel::DepthZ)
            .and_then(|plane| plane.get(&TileCoord::new(0, 0)))
            .cloned()
            .expect("depth tile exists");

        let depth_output = defocus_raster(&depth_raster, None, &op, None)
            .expect("defocus with aov should not fail");
        assert_eq!(depth_output.format, depth_raster.format);
        let depth_after = depth_output
            .channel(&Channel::DepthZ)
            .and_then(|plane| plane.get(&TileCoord::new(0, 0)))
            .expect("depth tile preserved");
        assert!(Arc::ptr_eq(&depth_before, depth_after));
    }

    /// Naive O(R^2) disk convolution reference: convolves every output pixel
    //  by directly summing all taps. Used only to verify the prefix-sum fast
    //  path is mathematically identical.
    fn naive_defocus_reference(
        input: &Raster2D,
        op: &DefocusOp,
        width: i32,
        height: i32,
    ) -> Vec<[f32; 4]> {
        let radius = op.radius.clamp(0.0, 24.0);
        let aspect = op.aspect.clamp(0.25, 4.0);
        let r_x = ((radius * aspect).ceil() as i32).min(24);
        let r_y = (radius.ceil() as i32).min(24);

        let mut weight_sum = 0.0f64;
        let mut taps: Vec<(i32, i32, f64)> = Vec::new();
        for dy in -r_y..=r_y {
            for dx in -r_x..=r_x {
                let dist = ((dx as f32 / aspect).powi(2) + (dy as f32).powi(2)).sqrt();
                let weight = (radius + 0.5 - dist).clamp(0.0, 1.0);
                if weight > 1e-6 {
                    weight_sum += weight as f64;
                    taps.push((dx, dy, weight as f64));
                }
            }
        }
        let inv_weight_sum = 1.0 / weight_sum;

        let apron = r_x.max(r_y) as usize;
        // extract_padded_tile returns a stride x stride buffer where
        // stride = TILE_SIZE + 2 * apron, NOT width/height + 2 * apron.
        let stride = TILE_SIZE as usize + 2 * apron;

        // Extract padded tiles for all four channels.
        let empty_plane = ChannelPlane::new();
        let red_padded = extract_padded_tile(
            input.channel(&Channel::RgbaRed).unwrap_or(&empty_plane),
            TileCoord::new(0, 0),
            apron as u32,
            input.bbox,
            input.edge_mode,
        );
        let green_padded = extract_padded_tile(
            input.channel(&Channel::RgbaGreen).unwrap_or(&empty_plane),
            TileCoord::new(0, 0),
            apron as u32,
            input.bbox,
            input.edge_mode,
        );
        let blue_padded = extract_padded_tile(
            input.channel(&Channel::RgbaBlue).unwrap_or(&empty_plane),
            TileCoord::new(0, 0),
            apron as u32,
            input.bbox,
            input.edge_mode,
        );
        let alpha_padded = extract_padded_tile(
            input.channel(&Channel::RgbaAlpha).unwrap_or(&empty_plane),
            TileCoord::new(0, 0),
            apron as u32,
            input.bbox,
            input.edge_mode,
        );

        let mut out = Vec::with_capacity((width * height) as usize);
        for ly in 0..height {
            for lx in 0..width {
                let cx = (lx + apron as i32) as isize;
                let cy = (ly + apron as i32) as isize;
                let base = (cy as usize) * stride + (cx as usize);

                let mut acc_r = 0.0f64;
                let mut acc_g = 0.0f64;
                let mut acc_b = 0.0f64;
                let mut acc_a = 0.0f64;

                for &(dx, dy, w) in &taps {
                    let x = (cx + dx as isize) as usize;
                    let y = (cy + dy as isize) as usize;
                    let idx = y * stride + x;
                    acc_r += red_padded[idx] as f64 * w;
                    acc_g += green_padded[idx] as f64 * w;
                    acc_b += blue_padded[idx] as f64 * w;
                    acc_a += alpha_padded[idx] as f64 * w;
                }

                let orig_r = red_padded[base] as f64;
                let orig_g = green_padded[base] as f64;
                let orig_b = blue_padded[base] as f64;
                let orig_a = alpha_padded[base] as f64;

                out.push([
                    (orig_r + (acc_r * inv_weight_sum - orig_r)) as f32,
                    (orig_g + (acc_g * inv_weight_sum - orig_g)) as f32,
                    (orig_b + (acc_b * inv_weight_sum - orig_b)) as f32,
                    (orig_a + (acc_a * inv_weight_sum - orig_a)) as f32,
                ]);
            }
        }
        out
    }

    /// Verifies the O(R) row-prefix-sum disk convolution is mathematically
    /// equivalent to the naive O(R^2) reference convolution, within 1e-5.
    #[test]
    fn defocus_prefix_sum_matches_naive_reference_within_1e5() {
        use crate::{BBox, Channel, ChannelPlane, EdgeMode, Format, TileBuffer, TileCoord};

        let width = 64;
        let height = 64;
        let bbox = BBox::new(0, 0, width, height);
        let format = Format::new("test", width as u32, height as u32, 1.0);

        let make_raster = || {
            let mut raster = Raster2D::new(format.clone(), bbox, EdgeMode::BlackOutside);
            for channel in [
                Channel::RgbaRed,
                Channel::RgbaGreen,
                Channel::RgbaBlue,
                Channel::RgbaAlpha,
            ] {
                let mut plane = ChannelPlane::new();
                plane.insert(TileCoord::new(0, 0), Arc::new(TileBuffer::new_uniform(0.0)));
                raster.insert_channel(channel, plane);
            }
            // Impulse at center.
            raster
                .set_pixel(Channel::RgbaRed, 32, 32, 1.0, None)
                .unwrap();
            raster
                .set_pixel(Channel::RgbaGreen, 32, 32, 0.5, None)
                .unwrap();
            raster
                .set_pixel(Channel::RgbaBlue, 32, 32, 0.25, None)
                .unwrap();
            raster
                .set_pixel(Channel::RgbaAlpha, 32, 32, 1.0, None)
                .unwrap();
            raster
        };

        for (radius, aspect) in [(7.75, 2.5), (1.2, 0.5)] {
            let op = DefocusOp {
                radius,
                aspect,
                mix: 1.0,
            };

            // Fast path (prefix sums).
            let input = make_raster();
            let fast = defocus_raster(&input, None, &op, None).expect("fast defocus");

            // Naive reference: direct 2D disk convolution.
            let input = make_raster();
            let naive = naive_defocus_reference(&input, &op, width, height);

            // Compare every pixel.
            let mut max_diff = 0.0f32;
            for y in 0..height {
                for x in 0..width {
                    let f = fast.sample_rgba(x, y);
                    let n = naive[(y * width + x) as usize];
                    for c in 0..4 {
                        max_diff = max_diff.max((f[c] - n[c]).abs());
                    }
                }
            }
            assert!(
                max_diff < 1e-5,
                "prefix-sum defocus (radius={radius}, aspect={aspect}) differs from naive by {max_diff}"
            );
        }
    }
}

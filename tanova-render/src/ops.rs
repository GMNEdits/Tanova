//! Exact Nuke math for `Grade` and `Merge2`, shared by the CPU and GPU paths.
//!
//! Every operation is expressed on one RGBA pixel at a time. The uniform-tile
//! fast path evaluates the same functions once per tile; the dense path runs
//! them per pixel, either in the `wgpu` kernels or in Rust.

use std::sync::Arc;

use tanova_kernel::{MemoryGovernor, NodeError, ProxyScale};

use crate::coords::{BBox, EdgeMode, TILE_PIXELS, TILE_SIZE, TileCoord, world_to_tile};
use crate::matrix::Mat3x3;
use crate::pool::{parallel_map_ordered, worker_thread_count};
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
    ///
    /// Exact endpoints: `effective_mix <= 0.0` returns the original pixel
    /// (no round-trip drift at mask `0.0`); `effective_mix >= 1.0` returns the
    /// processed result directly (no `orig + (res - orig)` precision loss).
    /// When `unpremult` is set, the sequence is `unpremult -> process ->
    /// premult`, and transparent (`alpha == 0` or non-finite) pixels always
    /// grade to `[0, 0, 0, alpha]` so `offset`/`lift` can never leak RGB onto
    /// transparent pixels.
    pub fn apply_pixel(&self, rgba: [f32; 4], mask: f32) -> [f32; 4] {
        let alpha = rgba[3];
        let effective_mix = (self.mix * mask).clamp(0.0, 1.0);
        if effective_mix <= 0.0 {
            return rgba;
        }

        let unpremultiply = self.unpremult;
        let alpha_invalid = alpha == 0.0 || !alpha.is_finite();
        let mut work_rgb = [rgba[0], rgba[1], rgba[2]];
        if unpremultiply {
            if alpha_invalid {
                work_rgb = [0.0, 0.0, 0.0];
            } else {
                work_rgb = std::array::from_fn(|c| {
                    let q = rgba[c] / alpha;
                    if q.is_finite() { q } else { rgba[c] }
                });
            }
        }

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

        if unpremultiply {
            // Always repremultiply: `offset`/`lift` can never leak RGB onto
            // transparent (`alpha == 0`) or non-finite alpha pixels because the
            // processed RGB is forced to `[0, 0, 0]` there.
            graded = if alpha_invalid {
                [0.0, 0.0, 0.0]
            } else {
                [graded[0] * alpha, graded[1] * alpha, graded[2] * alpha]
            };
        }

        if effective_mix >= 1.0 {
            return [graded[0], graded[1], graded[2], alpha];
        }
        let mut out = [0.0f32; 4];
        for channel in 0..3 {
            out[channel] = rgba[channel] + (graded[channel] - rgba[channel]) * effective_mix;
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
            Self::Multiply => map2(a, b, |a, b| {
                // Nuke's `multiply`: two super-blacks stay negative instead of
                // flipping positive, per component.
                if a < 0.0 && b < 0.0 { a } else { a * b }
            }),
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
    let effective_mix = (mix * mask).clamp(0.0, 1.0);
    // Exact endpoints: mix 0.0 returns the background, mix 1.0 returns the
    // operation result (no `b + (result - b)` precision loss).
    if effective_mix <= 0.0 {
        return background;
    }
    let result = operation.apply(foreground, background);
    if effective_mix >= 1.0 {
        return result;
    }
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
///
/// Uses [`Raster2D::sample_rgba`] so a missing alpha plane reads as implicit
/// `1.0` inside `bbox` and `0.0` outside, matching the rest of the pipeline.
pub fn rgba_at(raster: &Raster2D, x: i32, y: i32) -> [f32; 4] {
    raster.sample_rgba(x, y)
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
///
/// Matches [`mask_value`]: a missing mask is `1.0`, an empty region or one
/// entirely outside the mask's `bbox` is `0.0`, and a region crossing the bbox
/// boundary must be sampled per pixel (`None`).
pub fn uniform_mask_value(mask: Option<&Raster2D>, tile: TileCoord, region: BBox) -> Option<f32> {
    let Some(raster) = mask else {
        return Some(1.0);
    };
    if region.is_empty() {
        return Some(0.0);
    }
    if region.intersection(&raster.bbox).is_empty() {
        return Some(0.0);
    }
    if !bbox_contains(raster.bbox, region) {
        return None;
    }
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
        // A missing alpha plane is implicit opaque inside `bbox`; any other
        // missing plane is transparent black.
        None => {
            if *channel == Channel::RgbaAlpha {
                Some(1.0)
            } else {
                Some(0.0)
            }
        }
    }
}

fn bbox_contains(outer: BBox, inner: BBox) -> bool {
    inner.is_empty()
        || (outer.min_x <= inner.min_x
            && outer.min_y <= inner.min_y
            && outer.max_x >= inner.max_x
            && outer.max_y >= inner.max_y)
}

/// Extracts an apron-padded single-channel tile for a missing alpha plane:
/// implicit opaque (`1.0`) at samples inside `input.bbox`, transparent black
/// (`0.0`) at exterior samples (`BlackOutside`).
fn extract_implicit_alpha_padded(input: &Raster2D, tile: TileCoord, apron: u32) -> Vec<f32> {
    let apron = apron as usize;
    let stride = TILE_SIZE as usize + 2 * apron;
    let (origin_x, origin_y) = tile.origin_world();
    let start_x = origin_x.saturating_sub(apron as i32);
    let start_y = origin_y.saturating_sub(apron as i32);
    let bbox = input.bbox;
    let mut out = vec![0.0f32; stride * stride];
    for py in 0..stride {
        let y = start_y + py as i32;
        for px in 0..stride {
            let x = start_x + px as i32;
            if bbox.contains(x, y) {
                out[py * stride + px] = 1.0;
            }
        }
    }
    out
}

/// Convolves one padded RGBA tile with a normalized separable 1D Gaussian on the CPU.
///
/// A 2D Gaussian is separable (`G(x, y) = G(x) * G(y)`), so a horizontal 1D pass
/// followed by a vertical 1D pass costs `2 * (2 * radius + 1)` samples per pixel
/// instead of `(2 * radius + 1)^2`, producing the same mathematical result.
/// When `blur_alpha` is true (`Blur`) all four premultiplied channels are
/// convolved; when false (`Glow`) only RGB is weighted and alpha passes
/// through from the center pixel.
/// Returns one dense `TILE_SIZE x TILE_SIZE` interleaved tile.
pub fn blur_padded_rgba(
    padded: &[[f32; 4]],
    radius: u32,
    sigma: f32,
    padded_width: u32,
    blur_alpha: bool,
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

    // Compute one Pass-1 (horizontal) row of the intermediate buffer.
    let compute_pass1_row = |in_y: usize| -> Vec<[f32; 4]> {
        let mut row = Vec::with_capacity(tsize);
        let row_offset = in_y * stride;
        for out_x in 0..TILE_SIZE {
            let center_x = out_x as i32 + radius_i;
            let center = padded[row_offset + center_x as usize];
            let mut out = [0.0f32; 4];
            for dx in -radius_i..=radius_i {
                let weight = weights[(dx + radius_i) as usize] * inv_weight_sum;
                let sample = padded[row_offset + (center_x + dx) as usize];
                if blur_alpha {
                    out[0] += sample[0] * weight;
                    out[1] += sample[1] * weight;
                    out[2] += sample[2] * weight;
                    out[3] += sample[3] * weight;
                } else {
                    out[0] += sample[0] * weight;
                    out[1] += sample[1] * weight;
                    out[2] += sample[2] * weight;
                    out[3] = center[3];
                }
            }
            row.push(out);
        }
        row
    };

    let use_parallel = radius >= 4 && worker_thread_count() > 1;
    if use_parallel {
        let row_indices: Vec<usize> = (0..padded_height).collect();
        let rows: Vec<Vec<[f32; 4]>> =
            parallel_map_ordered(&row_indices, |&in_y| -> Result<Vec<[f32; 4]>, NodeError> {
                Ok(compute_pass1_row(in_y))
            })
            .expect("pass-1 row closure never errors");
        for (in_y, row_data) in row_indices.iter().zip(rows.iter()) {
            let out_row = *in_y * tsize;
            intermediate[out_row..out_row + tsize].copy_from_slice(row_data);
        }
    } else {
        for in_y in 0..padded_height {
            let row_data = compute_pass1_row(in_y);
            let out_row = in_y * tsize;
            intermediate[out_row..out_row + tsize].copy_from_slice(&row_data);
        }
    }

    // Pass 2: vertical 1D convolution.
    let mut out = Vec::with_capacity(TILE_PIXELS);
    out.resize(TILE_PIXELS, [0.0f32; 4]);

    // Compute one Pass-2 (vertical) output row.
    let compute_pass2_row = |out_y: usize| -> Vec<[f32; 4]> {
        let center_y = out_y as i32 + radius_i;
        let mut row = Vec::with_capacity(tsize);
        for out_x in 0..TILE_SIZE {
            let center_index = center_y as usize * tsize + out_x as usize;
            let center_alpha = intermediate[center_index][3];
            let mut out = [0.0f32; 4];
            for dy in -radius_i..=radius_i {
                let weight = weights[(dy + radius_i) as usize] * inv_weight_sum;
                let sample = intermediate[((center_y + dy) as usize) * tsize + out_x as usize];
                if blur_alpha {
                    out[0] += sample[0] * weight;
                    out[1] += sample[1] * weight;
                    out[2] += sample[2] * weight;
                    out[3] += sample[3] * weight;
                } else {
                    out[0] += sample[0] * weight;
                    out[1] += sample[1] * weight;
                    out[2] += sample[2] * weight;
                    out[3] = center_alpha;
                }
            }
            row.push(out);
        }
        row
    };

    if use_parallel {
        let row_indices: Vec<usize> = (0..TILE_SIZE as usize).collect();
        let rows: Vec<Vec<[f32; 4]>> =
            parallel_map_ordered(&row_indices, |&out_y| -> Result<Vec<[f32; 4]>, NodeError> {
                Ok(compute_pass2_row(out_y))
            })
            .expect("pass-2 row closure never errors");
        for (out_y, row_data) in row_indices.iter().zip(rows.iter()) {
            let out_row = *out_y * tsize;
            out[out_row..out_row + tsize].copy_from_slice(row_data);
        }
    } else {
        for out_y in 0..TILE_SIZE as usize {
            let row_data = compute_pass2_row(out_y);
            let out_row = out_y * tsize;
            out[out_row..out_row + tsize].copy_from_slice(&row_data);
        }
    }
    out
}

/// Runs the Gaussian blur over one padded tile, using the GPU when present.
///
/// `blur_alpha` (`true` = `Blur`, all four channels; `false` = `Glow`, RGB
/// only) selects whether the alpha channel is convolved too.
pub fn blur_padded_tile(
    padded: &[[f32; 4]],
    radius: u32,
    sigma: f32,
    padded_width: u32,
    blur_alpha: bool,
    gpu: Option<&GpuComputeEngine>,
) -> Result<Vec<f32>, NodeError> {
    match gpu {
        Some(engine) => engine.blur_tile(
            padded,
            &BlurParams::new(radius, sigma, padded_width, blur_alpha),
        ),
        None => Ok(
            blur_padded_rgba(padded, radius, sigma, padded_width, blur_alpha)
                .into_iter()
                .flatten()
                .collect(),
        ),
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
    let stride = width as usize;
    let mut output = Vec::with_capacity(TILE_PIXELS * 4);

    for y in 0..TILE_SIZE {
        for x in 0..TILE_SIZE {
            let (u, v) = matrix.transform_point(x as f32, y as f32);
            let x0 = u.floor() as i32;
            let y0 = v.floor() as i32;
            let fx = u - x0 as f32;
            let fy = v - y0 as f32;

            // Resolve each of the four bilinear taps independently: an
            // exterior tap (BlackOutside) reads transparent black, so the
            // half-pixel strip before the first / after the last pixel center
            // blends toward zero instead of being rejected outright.
            let sample = |tx: i32, ty: i32| -> [f32; 4] {
                if tx < 0 || ty < 0 || tx >= width as i32 || ty >= height as i32 {
                    return [0.0; 4];
                }
                let idx = (ty as usize * stride + tx as usize) * 4;
                [
                    gathered[idx],
                    gathered[idx + 1],
                    gathered[idx + 2],
                    gathered[idx + 3],
                ]
            };
            let c00 = sample(x0, y0);
            let c10 = sample(x0 + 1, y0);
            let c01 = sample(x0, y0 + 1);
            let c11 = sample(x0 + 1, y0 + 1);

            for channel in 0..4 {
                let top = c00[channel] * (1.0 - fx) + c10[channel] * fx;
                let bottom = c01[channel] * (1.0 - fx) + c11[channel] * fx;
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

    let tiles: Vec<TileCoord> = input.bbox.tile_coords();
    let use_parallel = gpu.is_none() && tiles.len() >= 2 && worker_thread_count() > 1;

    enum TileResult {
        Uniform([f32; 4]),
        Dense(Vec<f32>),
    }

    let compute_grade_tile = |tile: TileCoord| -> Result<(TileCoord, TileResult), NodeError> {
        let region = tile.to_bbox().intersection(&input.bbox);
        let uniform = uniform_rgba(input, tile, region).zip(uniform_mask_value(mask, tile, region));
        if let Some((rgba, mask_value)) = uniform {
            let pixel = op.apply_pixel(rgba, mask_value);
            return Ok((
                tile,
                TileResult::Uniform([pixel[0], pixel[1], pixel[2], pixel[3]]),
            ));
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
        Ok((tile, TileResult::Dense(output_pixels)))
    };

    let tile_results: Result<Vec<(TileCoord, TileResult)>, NodeError> = if use_parallel {
        parallel_map_ordered(&tiles, |tile| compute_grade_tile(*tile))
    } else {
        let mut results = Vec::with_capacity(tiles.len());
        for tile in tiles {
            results.push(compute_grade_tile(tile)?);
        }
        Ok(results)
    };
    let tile_results = tile_results?;

    for (tile, result) in tile_results {
        match result {
            TileResult::Uniform(pixel) => {
                insert_uniform(&mut red, tile, pixel[0]);
                insert_uniform(&mut green, tile, pixel[1]);
                insert_uniform(&mut blue, tile, pixel[2]);
                insert_uniform(&mut alpha, tile, pixel[3]);
            }
            TileResult::Dense(output_pixels) => {
                insert_dense(
                    [&mut red, &mut green, &mut blue, &mut alpha],
                    tile,
                    &output_pixels,
                    governor,
                )?;
            }
        }
    }

    output.insert_channel(Channel::RgbaRed, red);
    output.insert_channel(Channel::RgbaGreen, green);
    output.insert_channel(Channel::RgbaBlue, blue);
    output.insert_channel(Channel::RgbaAlpha, alpha);
    Ok(output)
}

/// Walks one `256x256` tile in Bottom-Left order, resolving every column and
/// row through `bbox`/`edge_mode` exactly once, and yields contiguous runs that
/// stay inside a single source tile.
///
/// Edge resolution is separable per axis, so resolving `x` with an in-bounds
/// `y` (and vice versa) reproduces [`ChannelPlane::sample`] exactly while
/// avoiding a `HashMap` lookup per pixel.
fn for_each_resolved_span(
    bbox: BBox,
    edge_mode: EdgeMode,
    tile: TileCoord,
    mut visit: impl FnMut(usize, usize, usize, TileCoord, usize, usize),
) {
    let stride = TILE_SIZE as usize;
    let (origin_x, origin_y) = tile.origin_world();

    let inside_x = bbox.min_x <= origin_x && origin_x + stride as i32 <= bbox.max_x;
    let inside_y = bbox.min_y <= origin_y && origin_y + stride as i32 <= bbox.max_y;

    let xs: Vec<Option<i32>> = if inside_x {
        (0..stride).map(|col| Some(origin_x + col as i32)).collect()
    } else {
        (0..stride)
            .map(|col| {
                edge_mode
                    .resolve_coord(origin_x + col as i32, bbox.min_y, bbox)
                    .map(|(resolved, _)| resolved)
            })
            .collect()
    };
    let ys: Vec<Option<i32>> = if inside_y {
        (0..stride).map(|row| Some(origin_y + row as i32)).collect()
    } else {
        (0..stride)
            .map(|row| {
                edge_mode
                    .resolve_coord(bbox.min_x, origin_y + row as i32, bbox)
                    .map(|(_, resolved)| resolved)
            })
            .collect()
    };

    for (row, resolved_y) in ys.iter().enumerate() {
        let Some(resolved_y) = *resolved_y else {
            continue;
        };
        let mut col = 0;
        while col < stride {
            let Some(resolved_x) = xs[col] else {
                col += 1;
                continue;
            };
            let (source_tile, lx, ly) = world_to_tile(resolved_x, resolved_y);
            let mut run = 1usize;
            while col + run < stride && lx as usize + run < stride {
                match xs[col + run] {
                    Some(next_x) if next_x == resolved_x + run as i32 => run += 1,
                    _ => break,
                }
            }
            visit(col, row, run, source_tile, lx as usize, ly as usize);
            col += run;
        }
    }
}

/// Gathers one tile's four RGBA planes directly from their dense slices.
///
/// Every plane lookup happens once per run instead of once per pixel, which
/// removes the `IndexMap`/`HashMap` traffic that dominated CPU tile gathering.
/// Output is interleaved `R,G,B,A` in Bottom-Left row-major order and matches
/// [`rgba_at`] pixel for pixel.
fn gather_rgba_tile(raster: &Raster2D, tile: TileCoord) -> Vec<f32> {
    let planes: [Option<&ChannelPlane>; 4] = [
        raster.channel(&Channel::RgbaRed),
        raster.channel(&Channel::RgbaGreen),
        raster.channel(&Channel::RgbaBlue),
        raster.channel(&Channel::RgbaAlpha),
    ];
    let stride = TILE_SIZE as usize;
    let mut out = vec![0.0f32; TILE_PIXELS * 4];
    let (origin_x, origin_y) = tile.origin_world();
    let bbox = raster.bbox;
    let alpha_missing = planes[3].is_none();

    for_each_resolved_span(
        raster.bbox,
        raster.edge_mode,
        tile,
        |col, row, run, source_tile, lx, ly| {
            let base = (row * stride + col) * 4;
            // Every present plane (including a present alpha plane) reads its
            // tile; missing planes keep the transparent-black default.
            for index in 0..4 {
                if index == 3 && alpha_missing {
                    continue;
                }
                let Some(plane) = planes[index] else {
                    continue;
                };
                let Some(buffer) = plane.get(&source_tile) else {
                    continue;
                };
                match buffer.is_uniform() {
                    Some(value) => {
                        for k in 0..run {
                            out[base + k * 4 + index] = value;
                        }
                    }
                    None => {
                        let src = buffer.as_dense_slice().expect("dense tile slice");
                        let src_off = ly * stride + lx;
                        for k in 0..run {
                            out[base + k * 4 + index] = src[src_off + k];
                        }
                    }
                }
            }
            // A missing alpha plane is implicit opaque inside `bbox` and
            // transparent black outside it (matching `sample_rgba`).
            if alpha_missing {
                for k in 0..run {
                    let x = origin_x + col as i32 + k as i32;
                    let y = origin_y + row as i32;
                    if bbox.contains(x, y) {
                        out[base + k * 4 + 3] = 1.0;
                    }
                }
            }
        },
    );

    out
}

/// Gathers one tile of mask values, matching [`mask_value`] exactly.
///
/// A missing mask is fully opaque (`1.0`). Pixels outside the mask's `bbox` are
/// always `0.0`, while pixels inside read the alpha plane when present, then
/// the red plane, then fall back to `1.0`.
fn gather_mask_tile(mask: Option<&Raster2D>, tile: TileCoord) -> Vec<f32> {
    let Some(mask) = mask else {
        return vec![1.0f32; TILE_PIXELS];
    };
    let plane = mask
        .channel(&Channel::RgbaAlpha)
        .or_else(|| mask.channel(&Channel::RgbaRed));
    let stride = TILE_SIZE as usize;
    let mut out = vec![0.0f32; TILE_PIXELS];
    let (origin_x, origin_y) = tile.origin_world();

    let x_start = origin_x.max(mask.bbox.min_x);
    let x_end = (origin_x + TILE_SIZE as i32).min(mask.bbox.max_x);
    let y_start = origin_y.max(mask.bbox.min_y);
    let y_end = (origin_y + TILE_SIZE as i32).min(mask.bbox.max_y);
    if x_start >= x_end || y_start >= y_end {
        return out;
    }

    let col_start = (x_start - origin_x) as usize;
    let col_end = (x_end - origin_x) as usize;

    for y in y_start..y_end {
        let row = (y - origin_y) as usize;
        let row_offset = row * stride;
        let mut col = col_start;
        while col < col_end {
            let Some(plane) = plane else {
                out[row_offset + col..row_offset + col_end].fill(1.0);
                break;
            };
            let x = origin_x + col as i32;
            let (source_tile, lx, ly) = world_to_tile(x, y);
            let mut run = 1usize;
            while col + run < col_end && lx as usize + run < stride {
                run += 1;
            }
            if let Some(buffer) = plane.get(&source_tile) {
                let base = row_offset + col;
                match buffer.is_uniform() {
                    Some(value) => out[base..base + run].fill(value),
                    None => {
                        let src = buffer.as_dense_slice().expect("dense tile slice");
                        let src_off = ly as usize * stride + lx as usize;
                        out[base..base + run].copy_from_slice(&src[src_off..src_off + run]);
                    }
                }
            }
            col += run;
        }
    }

    out
}

fn gather_grade_inputs(
    input: &Raster2D,
    mask: Option<&Raster2D>,
    tile: TileCoord,
) -> (Vec<f32>, Vec<f32>) {
    (gather_rgba_tile(input, tile), gather_mask_tile(mask, tile))
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

    let tiles: Vec<TileCoord> = output_bbox.tile_coords();
    let use_parallel = gpu.is_none() && tiles.len() >= 2 && worker_thread_count() > 1;

    enum MergeTileResult {
        Uniform([f32; 4]),
        Dense(Vec<f32>),
    }

    let compute_merge_tile = |tile: TileCoord| -> Result<(TileCoord, MergeTileResult), NodeError> {
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
            return Ok((
                tile,
                MergeTileResult::Uniform([pixel[0], pixel[1], pixel[2], pixel[3]]),
            ));
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
        Ok((tile, MergeTileResult::Dense(output_pixels)))
    };

    let tile_results: Result<Vec<(TileCoord, MergeTileResult)>, NodeError> = if use_parallel {
        parallel_map_ordered(&tiles, |tile| compute_merge_tile(*tile))
    } else {
        let mut results = Vec::with_capacity(tiles.len());
        for tile in tiles {
            results.push(compute_merge_tile(tile)?);
        }
        Ok(results)
    };
    let tile_results = tile_results?;

    for (tile, result) in tile_results {
        match result {
            MergeTileResult::Uniform(pixel) => {
                insert_uniform(&mut red, tile, pixel[0]);
                insert_uniform(&mut green, tile, pixel[1]);
                insert_uniform(&mut blue, tile, pixel[2]);
                insert_uniform(&mut alpha, tile, pixel[3]);
            }
            MergeTileResult::Dense(output_pixels) => {
                insert_dense(
                    [&mut red, &mut green, &mut blue, &mut alpha],
                    tile,
                    &output_pixels,
                    governor,
                )?;
            }
        }
    }

    output.insert_channel(Channel::RgbaRed, red);
    output.insert_channel(Channel::RgbaGreen, green);
    output.insert_channel(Channel::RgbaBlue, blue);
    output.insert_channel(Channel::RgbaAlpha, alpha);
    Ok(output)
}

/// Builds a proxy raster by bilinearly sampling source pixel centers.
///
/// Partial edge pixels clamp the source taps to its data window. Each plane
/// (including AOVs) keeps its own tiles and the output retains the edge mode.
pub fn downsample_raster(
    input: &Raster2D,
    scale: f32,
    governor: Option<&MemoryGovernor>,
) -> Result<Raster2D, NodeError> {
    let scale = ProxyScale::sanitized(scale);
    if (scale - 1.0).abs() <= 1e-4 {
        return Ok(input.clone());
    }
    let source = input.bbox;
    let scale_coord = |value: i32| f64::from(value) * f64::from(scale);
    let dst_bbox = if source.is_empty() {
        BBox::new(0, 0, 0, 0)
    } else {
        BBox::new(
            scale_coord(source.min_x).floor() as i32,
            scale_coord(source.min_y).floor() as i32,
            scale_coord(source.max_x).ceil() as i32,
            scale_coord(source.max_y).ceil() as i32,
        )
    };
    let mut output = Raster2D::new(
        input.format.clone().scaled(scale),
        dst_bbox,
        input.edge_mode,
    );
    let tiles = dst_bbox.tile_coords();
    let inv = f64::from(scale).recip();
    let taps = |range: std::ops::Range<i32>| {
        range
            .map(|dst| {
                // Do not hard-clamp the source coordinate to the bbox: an
                // exterior tap (BlackOutside) reads transparent black instead
                // of the edge pixel, so a sample 0.25 px before the first
                // pixel center blends 0.25 toward zero.
                let sample = (f64::from(dst) + 0.5) * inv - 0.5;
                let a = sample.floor() as i32;
                let b = a + 1;
                (a, b, (sample - f64::from(a)) as f32)
            })
            .collect::<Vec<_>>()
    };
    for (channel, plane) in &input.channels {
        let render_tile = |tile: &TileCoord| -> Result<Arc<TileBuffer>, NodeError> {
            let region = tile.to_bbox().intersection(&dst_bbox);
            let xs = taps(region.min_x..region.max_x);
            let ys = taps(region.min_y..region.max_y);
            let (first, _, _) = world_to_tile(xs[0].0, ys[0].0);
            let (last, _, _) = world_to_tile(xs[xs.len() - 1].1, ys[ys.len() - 1].1);
            // Only look up the small source footprint this output tile touches.
            let columns = (last.tx - first.tx + 1) as usize;
            let grid: Vec<_> = (first.ty..=last.ty)
                .flat_map(|ty| {
                    (first.tx..=last.tx).map(move |tx| plane.get(&TileCoord::new(tx, ty)))
                })
                .collect();
            let uniform = |buffer: Option<&Arc<TileBuffer>>| {
                buffer.map_or(Some(0.0), |buffer| buffer.is_uniform())
            };
            if let Some(value) = uniform(grid[0])
                && grid.iter().all(|&buffer| {
                    uniform(buffer).is_some_and(|other| other.to_bits() == value.to_bits())
                })
            {
                return Ok(Arc::new(TileBuffer::new_uniform(value)));
            }
            let sample = |x, y| {
                // Resolve each tap independently through the bbox/edge mode:
                // exterior taps read transparent black in `BlackOutside` and
                // clamp/mirror/bounce for the other modes.
                let Some((rx, ry)) = input.edge_mode.resolve_coord(x, y, source) else {
                    return 0.0;
                };
                let (coord, lx, ly) = world_to_tile(rx, ry);
                let index =
                    (coord.ty - first.ty) as usize * columns + (coord.tx - first.tx) as usize;
                grid[index].map_or(0.0, |buf| buf.get(lx, ly))
            };
            let blend = |a: f32, b: f32, weight: f32| {
                if a.to_bits() == b.to_bits() || weight == 0.0 {
                    a
                } else {
                    a * (1.0 - weight) + b * weight
                }
            };
            // Reserve the dense tile before allocating it, including in workers.
            let mut buffer = TileBuffer::new_dense(0.0, governor)?;
            let pixels = buffer.as_dense_slice_mut().expect("new dense tile");
            let (ox, oy) = tile.origin_world();
            let left = (region.min_x - ox) as usize;
            let bottom = (region.min_y - oy) as usize;
            for (row, &(y0, y1, fy)) in ys.iter().enumerate() {
                let offset = (row + bottom) * TILE_SIZE as usize + left;
                for (pixel, &(x0, x1, fx)) in pixels[offset..offset + xs.len()].iter_mut().zip(&xs)
                {
                    let lower = blend(sample(x0, y0), sample(x1, y0), fx);
                    *pixel = if y0 == y1 {
                        lower
                    } else {
                        blend(lower, blend(sample(x0, y1), sample(x1, y1), fx), fy)
                    };
                }
            }
            Ok(Arc::new(buffer))
        };
        let buffers = parallel_map_ordered(&tiles, render_tile)?;
        let mut dst = ChannelPlane::new();
        for (&tile, buffer) in tiles.iter().zip(buffers) {
            dst.insert(tile, buffer);
        }
        output.insert_channel(channel.clone(), dst);
    }
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
    let alpha_missing = input.channel(&Channel::RgbaAlpha).is_none();

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
        alpha_missing: bool,
        x: f32,
        y: f32,
    ) -> [f32; 4] {
        // Each bilinear tap is resolved independently against `edge_mode`:
        // exterior taps read transparent black instead of the whole sample
        // being rejected, so the half-pixel strip before the first / after the
        // last pixel center blends toward zero.
        let x0 = x.floor() as i32;
        let y0 = y.floor() as i32;
        let fx = x - x0 as f32;
        let fy = y - y0 as f32;
        let x1 = x0 + 1;
        let y1 = y0 + 1;

        let mut out = [0.0f32; 4];
        for channel in 0..4 {
            // A missing alpha plane is implicit opaque (`1.0`) inside `bbox`
            // and transparent (`0.0`) outside, matching `sample_rgba`; resolve
            // it per tap so the exterior half-pixel strip fades out.
            if channel == 3 && alpha_missing {
                let alpha_at = |tx: i32, ty: i32| {
                    if bbox.contains(tx, ty) { 1.0f32 } else { 0.0 }
                };
                let c00 = alpha_at(x0, y0);
                let c10 = alpha_at(x1, y0);
                let c01 = alpha_at(x0, y1);
                let c11 = alpha_at(x1, y1);
                let top = c00 * (1.0 - fx) + c10 * fx;
                let bottom = c01 * (1.0 - fx) + c11 * fx;
                out[channel] = top * (1.0 - fy) + bottom * fy;
                continue;
            }
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

    let tiles: Vec<TileCoord> = output_bbox.tile_coords();
    let use_parallel = tiles.len() >= 2 && worker_thread_count() > 1;

    let compute_warp_tile = |tile: TileCoord| -> Result<(TileCoord, Vec<[f32; 4]>), NodeError> {
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
                        alpha_missing,
                        source[0] - 0.5,
                        source[1] - 0.5,
                    ),
                    None => [0.0; 4],
                };
                pixels.push(rgba);
            }
        }
        Ok((tile, pixels))
    };

    let tile_pixel_results = if use_parallel {
        parallel_map_ordered(&tiles, |tile| compute_warp_tile(*tile))
    } else {
        let mut results = Vec::with_capacity(tiles.len());
        for tile in tiles {
            results.push(compute_warp_tile(tile)?);
        }
        Ok(results)
    };
    let tile_pixel_results = tile_pixel_results?;

    for (tile, pixels) in tile_pixel_results {
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
    (
        gather_rgba_tile(background, tile),
        gather_rgba_tile(foreground, tile),
        gather_mask_tile(mask, tile),
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

    enum DefocusTileResult {
        Uniform([f32; 4]),
        Dense(Vec<f32>),
    }

    let compute_defocus_tile =
        |tile: TileCoord| -> Result<(TileCoord, DefocusTileResult), NodeError> {
            let tile_bbox = tile.to_bbox();
            let padded_region = BBox::new(
                tile_bbox.min_x.saturating_sub(r_x),
                tile_bbox.min_y.saturating_sub(r_y),
                tile_bbox.max_x.saturating_add(r_x),
                tile_bbox.max_y.saturating_add(r_y),
            );

            // Uniform fast path: when the whole padded region (and the mask)
            // collapses to one value, the convolved tile is that same value.
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
                    // A uniform region convolves to itself; blending with itself is a no-op.
                    let rgba = [
                        channel_values[0].unwrap(),
                        channel_values[1].unwrap(),
                        channel_values[2].unwrap(),
                        channel_values[3].unwrap(),
                    ];
                    return Ok((tile, DefocusTileResult::Uniform(rgba)));
                }
            }

            // Dense path.
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

            let alpha_plane = input.channel(&Channel::RgbaAlpha);
            let alpha_missing = alpha_plane.is_none();

            // Missing alpha plane: implicit opaque (`1.0`) inside `input.bbox`,
            // transparent black (`0.0`) outside — matching `sample_rgba`.
            let alpha_uniform = if alpha_missing {
                if padded_region.is_empty() {
                    Some(0.0)
                } else if bbox_contains(input.bbox, padded_region) {
                    Some(1.0)
                } else if padded_region.intersection(&input.bbox).is_empty() {
                    Some(0.0)
                } else {
                    None
                }
            } else {
                region_uniform_value(input, &Channel::RgbaAlpha, padded_region)
            };

            let alpha_padded = if alpha_uniform.is_none() {
                if alpha_missing {
                    Some(extract_implicit_alpha_padded(input, tile, apron as u32))
                } else {
                    Some(extract_padded_tile(
                        alpha_plane.unwrap(),
                        tile,
                        apron as u32,
                        input.bbox,
                        input.edge_mode,
                    ))
                }
            } else {
                None
            };

            // Only pixels inside `output_bbox` are reachable downstream, so
            // clip the convolution to this tile's active intersection. On a
            // 256x256 frame the eight border tiles only own a 4px strip, and
            // this skips the other 99% of their 256x256 work.
            let active = tile_bbox.intersection(&output_bbox);
            if active.is_empty() {
                return Ok((
                    tile,
                    DefocusTileResult::Dense(vec![0.0f32; TILE_PIXELS * 4]),
                ));
            }
            let lx_start = (active.min_x - tile_bbox.min_x) as usize;
            let lx_end = (active.max_x - tile_bbox.min_x) as usize;
            let ly_start = (active.min_y - tile_bbox.min_y) as usize;
            let ly_end = (active.max_y - tile_bbox.min_y) as usize;

            let pw = stride;
            let ph = stride;
            let prefix_stride = pw + 1;

            // Prefix rows the active band needs: `cy = ly + apron` plus the
            // kernel's `dy` range. Building only these rows is exact because
            // each row's prefix still accumulates from x = 0, so every
            // subtraction is bit-for-bit identical to the unclipped pass.
            let py_start = (ly_start + apron).saturating_sub(r_y as usize).min(ph);
            let py_end = (ly_end.saturating_sub(1) + apron + r_y as usize + 1).min(ph);
            let py_end = py_end.max(py_start);
            let prefix_rows = py_end - py_start;

            let mut red_prefixes = vec![0.0f64; prefix_rows * prefix_stride];
            let mut green_prefixes = vec![0.0f64; prefix_rows * prefix_stride];
            let mut blue_prefixes = vec![0.0f64; prefix_rows * prefix_stride];
            for py in py_start..py_end {
                let row_offset = (py - py_start) * prefix_stride;
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

            let alpha_prefixes = alpha_padded.as_ref().map(|_| {
                let mut prefixes = vec![0.0f64; prefix_rows * prefix_stride];
                for py in py_start..py_end {
                    let row_offset = (py - py_start) * prefix_stride;
                    let data_offset = py * pw;
                    let mut acc = 0.0f64;
                    for x in 0..pw {
                        acc += alpha_padded.as_ref().unwrap()[data_offset + x] as f64;
                        prefixes[row_offset + x + 1] = acc;
                    }
                }
                prefixes
            });

            let (origin_x, origin_y) = tile.origin_world();
            let mut out = vec![0.0f32; TILE_PIXELS * 4];
            for ly in ly_start..ly_end {
                for lx in lx_start..lx_end {
                    let base = (ly + apron) * stride + lx + apron;
                    let cx = (lx + apron) as isize;
                    let cy = (ly + apron) as isize;

                    let mut acc_r = 0.0f64;
                    let mut acc_g = 0.0f64;
                    let mut acc_b = 0.0f64;

                    for span in &spans {
                        let py = (cy + span.dy) as usize;
                        let row_offset = (py - py_start) * prefix_stride;

                        if let Some((dx0, dx1)) = span.inner {
                            let x0 = (cx + dx0) as usize;
                            let x1 = (cx + dx1 + 1) as usize;
                            acc_r += red_prefixes[row_offset + x1] - red_prefixes[row_offset + x0];
                            acc_g +=
                                green_prefixes[row_offset + x1] - green_prefixes[row_offset + x0];
                            acc_b +=
                                blue_prefixes[row_offset + x1] - blue_prefixes[row_offset + x0];
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
                    let effective_mix =
                        (op.mix * mask_value(mask, world_x, world_y)).clamp(0.0, 1.0);

                    let effective_mix_f64 = effective_mix as f64;
                    let alpha_val = match alpha_uniform {
                        Some(a) => a as f64,
                        None => {
                            let ap = alpha_padded.as_ref().unwrap();
                            let apfx = alpha_prefixes.as_ref().unwrap();
                            let mut alpha_acc = 0.0f64;
                            for span in &spans {
                                let py = (cy + span.dy) as usize;
                                let row_offset = (py - py_start) * prefix_stride;

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

                    let pixel = (ly * TILE_SIZE as usize + lx) * 4;
                    out[pixel] =
                        (orig_r + (acc_r * inv_weight_sum - orig_r) * effective_mix_f64) as f32;
                    out[pixel + 1] =
                        (orig_g + (acc_g * inv_weight_sum - orig_g) * effective_mix_f64) as f32;
                    out[pixel + 2] =
                        (orig_b + (acc_b * inv_weight_sum - orig_b) * effective_mix_f64) as f32;
                    out[pixel + 3] = alpha_val as f32;
                }
            }

            Ok((tile, DefocusTileResult::Dense(out)))
        };

    let tiles: Vec<TileCoord> = output_bbox.tile_coords();
    let use_parallel_tiles = tiles.len() >= 2 && worker_thread_count() > 1;

    // Parallelize across tiles when there are enough tiles and worker
    // threads. Single-tile / single-worker falls back to sequential.
    let tile_results: Result<Vec<(TileCoord, DefocusTileResult)>, NodeError> = if use_parallel_tiles
    {
        parallel_map_ordered(&tiles, |tile| compute_defocus_tile(*tile))
    } else {
        let mut results = Vec::with_capacity(tiles.len());
        for tile in tiles {
            results.push(compute_defocus_tile(tile)?);
        }
        Ok(results)
    };
    let tile_results = tile_results?;

    for (tile, result) in tile_results {
        match result {
            DefocusTileResult::Uniform(pixel) => {
                insert_uniform(&mut red, tile, pixel[0]);
                insert_uniform(&mut green, tile, pixel[1]);
                insert_uniform(&mut blue, tile, pixel[2]);
                insert_uniform(&mut alpha, tile, pixel[3]);
            }
            DefocusTileResult::Dense(out) => {
                insert_dense(
                    [&mut red, &mut green, &mut blue, &mut alpha],
                    tile,
                    &out,
                    governor,
                )?;
            }
        }
    }

    output.insert_channel(Channel::RgbaRed, red);
    output.insert_channel(Channel::RgbaGreen, green);
    output.insert_channel(Channel::RgbaBlue, blue);
    output.insert_channel(Channel::RgbaAlpha, alpha);
    Ok(output)
}

#[cfg(test)]
mod tests {
    #[test]
    fn downsample_preserves_planes_bounds_uniform_tiles_and_governor() {
        use super::*;
        use crate::Format;

        let bbox = BBox::new(-3, -1, 513, 9);
        let mut input = Raster2D::new(
            Format::new("odd", 513, 9, 1.5),
            bbox,
            EdgeMode::BlackOutside,
        );
        let mut depth = ChannelPlane::new();
        let uniform = Arc::new(TileBuffer::new_uniform(9.0));
        for tile in bbox.tile_coords() {
            depth.insert(tile, Arc::clone(&uniform));
        }
        input.insert_channel(Channel::DepthZ, depth);
        let governor = MemoryGovernor::new(0);
        let cloned = downsample_raster(&input, 1.0, Some(&governor)).unwrap();
        assert!(Arc::ptr_eq(
            cloned
                .channel(&Channel::DepthZ)
                .unwrap()
                .get(&TileCoord::new(0, 0))
                .unwrap(),
            &uniform
        ));
        let half = downsample_raster(&input, 0.5, Some(&governor)).unwrap();
        assert_eq!((half.format.width, half.format.height), (257, 5));
        assert_eq!(half.format.logical_size(0.5), (513, 9));
        assert_eq!(half.format.pixel_aspect, 1.5);
        assert_eq!(half.bbox, BBox::new(-2, -1, 257, 5));
        assert_eq!(half.sample(&Channel::DepthZ, -2, -1), 9.0);
        assert_eq!(half.sample(&Channel::DepthZ, 256, 4), 9.0);
        assert_eq!(governor.allocated_bytes(), 0);

        input.set_pixel(Channel::RgbaRed, 0, 0, 4.0, None).unwrap();
        assert!(matches!(
            downsample_raster(&input, 0.5, Some(&governor)),
            Err(NodeError::MemoryExceeded { .. })
        ));
        let governor = MemoryGovernor::new(crate::TILE_BYTES);
        let half = downsample_raster(&input, 0.5, Some(&governor)).unwrap();
        assert_eq!(half.sample(&Channel::RgbaRed, 0, 0), 1.0);
        assert_eq!(half.sample(&Channel::RgbaRed, -1, 0), 0.0);
        assert_eq!(governor.allocated_bytes(), crate::TILE_BYTES);
        drop(half);
        assert_eq!(governor.allocated_bytes(), 0);
    }

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

        let cpu = blur_padded_rgba(&padded, radius, sigma, padded_width, true);
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
                let mut rgba = [0.0f32; 4];
                let mut wi = 0usize;
                for dy in -radius_i..=radius_i {
                    let row = (cy + dy) as usize * stride;
                    for dx in -radius_i..=radius_i {
                        let w = ref_w[wi];
                        wi += 1;
                        let s = padded[row + (cx + dx) as usize];
                        for channel in 0..4 {
                            rgba[channel] += s[channel] * w;
                        }
                    }
                }
                let expected = [
                    rgba[0] / ref_sum,
                    rgba[1] / ref_sum,
                    rgba[2] / ref_sum,
                    rgba[3] / ref_sum,
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

        // Impulse sanity: a single lit pixel blurs to the normalized Gaussian in
        // all four premultiplied channels, so alpha matches RGB exactly.
        let mut impulse = vec![[0.0f32; 4]; (padded_width * padded_width) as usize];
        let impulse_x = (TILE_SIZE / 2) as i32 + radius_i;
        let impulse_y = (TILE_SIZE / 2) as i32 + radius_i;
        impulse[(impulse_y as usize * padded_width as usize) + impulse_x as usize] = [1.0; 4];
        let out = blur_padded_rgba(&impulse, radius, sigma, padded_width, true);
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
        assert!(
            (center[3] - expected_center).abs() < 1e-5,
            "impulse alpha blurs like RGB: {} vs {}",
            center[3],
            expected_center
        );
        let neighbor = out[oy * TILE_SIZE as usize + ox + 1];
        assert!(
            (neighbor[3] - neighbor[0]).abs() < 1e-6,
            "blurred alpha matches blurred RGB off-center"
        );

        // `Glow`-style blur keeps the center alpha untouched.
        let rgb_only = blur_padded_rgba(&padded, radius, sigma, padded_width, false);
        for y in 0..TILE_SIZE {
            for x in 0..TILE_SIZE {
                let cx = x as i32 + radius_i;
                let cy = y as i32 + radius_i;
                let center_alpha = padded[cy as usize * stride + cx as usize][3];
                let actual = rgb_only[y as usize * TILE_SIZE as usize + x as usize];
                assert!(
                    (actual[3] - center_alpha).abs() < 1e-6,
                    "rgb-only blur must preserve center alpha"
                );
            }
        }

        // GPU vs CPU parity within 1e-4.
        if let Some(engine) = test_render_engine() {
            let gpu = blur_padded_tile(&padded, radius, sigma, padded_width, true, Some(&engine))
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

    #[test]
    fn parallel_ops_match_sequential_pixels_on_multi_tile_frame() {
        use crate::Format;
        // 512x512 = 4 tiles (2x2), non-uniform so every tile takes the dense path.
        let width = 512;
        let height = 512;
        let format = Format::new("test", width as u32, height as u32, 1.0);
        let bbox = format.to_bbox();
        let mut input = Raster2D::for_format(format.clone());
        for y in 0..height {
            for x in 0..width {
                let v = ((x as f32) / width as f32 * 2.0 - 1.0).clamp(0.0, 1.0);
                input.set_pixel(Channel::RgbaRed, x, y, v, None).unwrap();
                input
                    .set_pixel(Channel::RgbaGreen, x, y, v * 0.5, None)
                    .unwrap();
                input
                    .set_pixel(Channel::RgbaBlue, x, y, 1.0 - v, None)
                    .unwrap();
                input
                    .set_pixel(Channel::RgbaAlpha, x, y, 1.0, None)
                    .unwrap();
            }
        }

        let governor = MemoryGovernor::new(2 * 1024 * 1024 * 1024);

        // --- Defocus: parallel output is deterministic and releases permits ---
        let op = DefocusOp {
            radius: 8.0,
            aspect: 1.0,
            mix: 1.0,
        };
        let parallel = defocus_raster(&input, None, &op, Some(&governor)).expect("defocus");
        let parallel2 = defocus_raster(&input, None, &op, Some(&governor)).expect("defocus 2");
        assert_eq!(parallel.format, format);
        // Determinism: two parallel runs produce identical pixels.
        for y in 0..height {
            for x in 0..width {
                assert_eq!(
                    parallel.sample_rgba(x, y),
                    parallel2.sample_rgba(x, y),
                    "defocus must be deterministic across runs at ({x}, {y})"
                );
            }
        }
        drop(parallel);
        drop(parallel2);
        assert_eq!(
            governor.allocated_bytes(),
            0,
            "defocus permits must be released"
        );

        // --- Grade: parallel CPU path, permit cleanup ---
        let grade_op = GradeOp::default();
        let graded = grade_raster(&input, None, &grade_op, Some(&governor), None).expect("grade");
        assert_eq!(graded.format, format);
        assert!(
            !graded.channels.is_empty(),
            "grade must produce RGBA channels"
        );
        drop(graded);
        assert_eq!(
            governor.allocated_bytes(),
            0,
            "grade permits must be released"
        );

        // --- Merge: parallel CPU path (self-over) ---
        let merge_req = MergeRequest {
            background: &input,
            foreground: &input,
            mask: None,
            operation: MergeOp::Over,
            output_bbox: bbox,
            mix: 1.0,
        };
        let merged = merge_raster(merge_req, Some(&governor), None).expect("merge");
        assert_eq!(merged.format, format);
        drop(merged);
        assert_eq!(
            governor.allocated_bytes(),
            0,
            "merge permits must be released"
        );

        // --- Perspective warp: parallel tiles ---
        let forward = Mat3x3::identity();
        let warped =
            perspective_warp_raster(&input, &forward, bbox, Some(&governor)).expect("warp");
        assert_eq!(warped.format, format);
        drop(warped);
        assert_eq!(
            governor.allocated_bytes(),
            0,
            "warp permits must be released"
        );
    }

    /// The direct-slice tile gatherers must match `rgba_at`/`mask_value`
    /// pixel-for-pixel, including partial 1080p edge tiles and a mask whose
    /// bbox is smaller than the frame.
    #[test]
    fn gather_inputs_match_reference_sampling_on_partial_and_masked_tiles() {
        use crate::{BBox, Channel, ChannelPlane, EdgeMode, Format, TileBuffer, TileCoord};

        let format = Format::hd_1080();
        let bbox = format.to_bbox();

        let coords = [(0, 0), (3, 2), (6, 3), (7, 3), (7, 4), (8, 4), (-1, -1)];
        let filled_plane = |seed: i32| {
            let mut plane = ChannelPlane::new();
            for &(tx, ty) in &coords {
                let mut data = vec![0.0f32; TILE_PIXELS];
                for (index, value) in data.iter_mut().enumerate() {
                    *value = ((tx * 97 + ty * 53 + seed * 29 + index as i32).rem_euclid(211))
                        as f32
                        / 211.0;
                }
                plane.insert(
                    TileCoord::new(tx, ty),
                    Arc::new(TileBuffer::from_vec(data, None).unwrap()),
                );
            }
            plane
        };

        let mut input = Raster2D::new(format.clone(), bbox, EdgeMode::ClampToEdge);
        input.insert_channel(Channel::RgbaRed, filled_plane(1));
        input.insert_channel(Channel::RgbaGreen, filled_plane(2));
        input.insert_channel(Channel::RgbaBlue, filled_plane(3));
        input.insert_channel(Channel::RgbaAlpha, filled_plane(4));

        let mut mask = Raster2D::new(
            format.clone(),
            BBox::new(100, 100, 1800, 1000),
            EdgeMode::ClampToEdge,
        );
        mask.insert_channel(Channel::RgbaAlpha, filled_plane(5));

        for &(tx, ty) in &coords {
            let tile = TileCoord::new(tx, ty);
            let (origin_x, origin_y) = tile.origin_world();

            let mut reference_pixels = Vec::with_capacity(TILE_PIXELS * 4);
            let mut reference_masks = Vec::with_capacity(TILE_PIXELS);
            for ly in 0..TILE_SIZE {
                for lx in 0..TILE_SIZE {
                    let x = origin_x + lx as i32;
                    let y = origin_y + ly as i32;
                    reference_pixels.extend_from_slice(&rgba_at(&input, x, y));
                    reference_masks.push(mask_value(Some(&mask), x, y));
                }
            }

            let (grade_pixels, grade_masks) = gather_grade_inputs(&input, Some(&mask), tile);
            assert_eq!(
                grade_pixels, reference_pixels,
                "grade gather mismatch at ({tx}, {ty})"
            );
            assert_eq!(
                grade_masks, reference_masks,
                "grade mask mismatch at ({tx}, {ty})"
            );

            let (background, foreground, merge_masks) =
                gather_merge_inputs(&input, &input, Some(&mask), tile);
            assert_eq!(
                background, reference_pixels,
                "merge background mismatch at ({tx}, {ty})"
            );
            assert_eq!(
                foreground, reference_pixels,
                "merge foreground mismatch at ({tx}, {ty})"
            );
            assert_eq!(
                merge_masks, reference_masks,
                "merge mask mismatch at ({tx}, {ty})"
            );
        }
    }
}

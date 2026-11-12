//! Cached `wgpu` compute engine for the core VFX cartridges.
//!
//! Pipelines are compiled exactly once, when [`GpuComputeEngine::new`] runs, and
//! then reused for every tile. Dense 256x256 tiles are uploaded, processed by a
//! `16x16` workgroup kernel, and read back into planar `f32` data.
//!
//! The engine knows nothing about the DAG. Cartridges find it through
//! [`EvalContextGpu::gpu`] (or `ctx.get::<GpuComputeEngine>()`) and fall back to
//! the identical Rust math in [`crate::ops`] when no GPU is available.

use std::sync::Arc;

use bytemuck::{Pod, Zeroable};
use tanova_kernel::{EvalContext, NodeError};
use wgpu::util::DeviceExt;

use crate::coords::{TILE_PIXELS, TILE_SIZE};
use crate::matrix::Mat3x3;
use crate::ops::{GradeOp, MergeOp};

/// Threads per workgroup axis. A 256-pixel tile needs `256 / 16 = 16` groups.
pub const WORKGROUP_SIZE: u32 = 16;

/// Workgroups per axis for one 256x256 tile.
pub const DISPATCH_SIZE: u32 = TILE_SIZE / WORKGROUP_SIZE;

/// RGBA floats in one dense tile.
pub const TILE_RGBA_VALUES: usize = TILE_PIXELS * 4;

/// Bytes in one dense RGBA tile.
pub const TILE_RGBA_BYTES: u64 = TILE_RGBA_VALUES as u64 * std::mem::size_of::<f32>() as u64;

/// Bytes in one dense single-channel tile.
pub const TILE_MASK_BYTES: u64 = TILE_PIXELS as u64 * std::mem::size_of::<f32>() as u64;

/// `Grade` uniforms, laid out as nine 16-byte-aligned `vec4` values.
///
/// WGSL's uniform layout rules require each field to start on a 16-byte
/// boundary, so every member is a `vec4` and the struct size is a multiple of
/// 16. The Rust struct uses the same layout.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
pub struct GradeParams {
    /// Input black point, per RGB channel.
    pub blackpoint: [f32; 4],
    /// Input white point, per RGB channel.
    pub whitepoint: [f32; 4],
    /// Output lift, per RGB channel.
    pub lift: [f32; 4],
    /// Output gain, per RGB channel.
    pub gain: [f32; 4],
    /// Overall multiply, per RGB channel.
    pub multiply: [f32; 4],
    /// Post-slope offset, per RGB channel.
    pub offset: [f32; 4],
    /// Inverse gamma, per RGB channel.
    pub gamma: [f32; 4],
    /// Flags: `[unpremult, black_clamp, white_clamp, 0]`.
    pub flags: [u32; 4],
    /// Blend: `[mix, 0, 0, 0]`.
    pub mix: [f32; 4],
}

impl GradeParams {
    /// Packs a [`GradeOp`] into GPU-ready uniforms.
    pub fn new(op: &GradeOp) -> Self {
        Self {
            blackpoint: op.blackpoint,
            whitepoint: op.whitepoint,
            lift: op.lift,
            gain: op.gain,
            multiply: op.multiply,
            offset: op.offset,
            gamma: op.gamma,
            flags: [
                u32::from(op.unpremult),
                u32::from(op.black_clamp),
                u32::from(op.white_clamp),
                0,
            ],
            mix: [op.mix, 0.0, 0.0, 0.0],
        }
    }
}

/// `Merge2` uniforms: an operation code and a blend amount.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
pub struct MergeParams {
    /// `[operation_code, 0, 0, 0]`, matching [`MergeOp::code`].
    pub operation: [u32; 4],
    /// `[mix, 0, 0, 0]`.
    pub mix: [f32; 4],
}

impl MergeParams {
    /// Packs a [`MergeOp`] and its blend amount into GPU-ready uniforms.
    pub fn new(operation: MergeOp, mix: f32) -> Self {
        Self {
            operation: [operation.code(), 0, 0, 0],
            mix: [mix, 0.0, 0.0, 0.0],
        }
    }
}

/// `Blur` uniforms: radius, Gaussian sigma, apron, and padded row stride.
///
/// Four `u32`/`f32` scalars occupy one 16-byte uniform slot, so the struct
/// matches WGSL's uniform layout without padding.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
pub struct BlurParams {
    /// Convolution radius in pixels (`size.ceil()`).
    pub radius: u32,
    /// Gaussian standard deviation.
    pub sigma: f32,
    /// Apron halo on every side of the output tile.
    pub apron: u32,
    /// Row stride of the padded input buffer, in pixels.
    pub padded_width: u32,
}

impl BlurParams {
    /// Packs the blur geometry into GPU-ready uniforms.
    pub fn new(radius: u32, sigma: f32, padded_width: u32) -> Self {
        Self {
            radius,
            sigma,
            apron: radius,
            padded_width,
        }
    }
}

/// `Transform` uniforms: inverse affine rows plus gathered input dimensions.
///
/// The inverse matrix maps one output tile's local pixel coordinates into the
/// gathered input region's local coordinates. Three `vec4` rows keep the struct
/// at 64 bytes with WGSL's 16-byte uniform alignment.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
pub struct TransformParams {
    /// First row of the inverse matrix, padded to `vec4`.
    pub inv_row0: [f32; 4],
    /// Second row of the inverse matrix, padded to `vec4`.
    pub inv_row1: [f32; 4],
    /// Third row of the inverse matrix, padded to `vec4`.
    pub inv_row2: [f32; 4],
    /// `[gathered_width, gathered_height, 0, 0]`.
    pub input_size: [u32; 4],
}

impl TransformParams {
    /// Packs the inverse matrix and gathered region size into GPU uniforms.
    pub fn new(matrix: &Mat3x3, width: u32, height: u32) -> Self {
        Self {
            inv_row0: matrix.row(0),
            inv_row1: matrix.row(1),
            inv_row2: matrix.row(2),
            input_size: [width, height, 0, 0],
        }
    }
}

/// One compiled compute pass: its bind-group layout and pipeline.
struct ComputePass {
    layout: wgpu::BindGroupLayout,
    pipeline: wgpu::ComputePipeline,
}

/// Shared, immutable GPU state. `Arc` makes the engine cheap to clone and safe
/// to share between the UI and render threads.
struct EngineInner {
    device: Arc<wgpu::Device>,
    queue: Arc<wgpu::Queue>,
    grade: ComputePass,
    merge: ComputePass,
    blur: ComputePass,
    transform: ComputePass,
}

/// A cached `wgpu` compute engine for `Grade` and `Merge2`.
#[derive(Clone)]
pub struct GpuComputeEngine {
    inner: Arc<EngineInner>,
}

impl std::fmt::Debug for GpuComputeEngine {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GpuComputeEngine")
            .finish_non_exhaustive()
    }
}

impl GpuComputeEngine {
    /// Compiles both kernels once and caches their pipelines.
    pub fn new(device: Arc<wgpu::Device>, queue: Arc<wgpu::Queue>) -> Self {
        let grade = build_pass(
            &device,
            "tanova-grade",
            GRADE_WGSL,
            &[
                storage_read(),
                storage_read(),
                wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                storage_read_write(),
            ],
        );
        let merge = build_pass(
            &device,
            "tanova-merge2",
            MERGE_WGSL,
            &[
                storage_read(),
                storage_read(),
                storage_read(),
                wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                storage_read_write(),
            ],
        );
        let blur = build_pass(
            &device,
            "tanova-blur",
            BLUR_WGSL,
            &[
                storage_read(),
                wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                storage_read_write(),
            ],
        );
        let transform = build_pass(
            &device,
            "tanova-transform",
            TRANSFORM_WGSL,
            &[
                storage_read(),
                wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                storage_read_write(),
            ],
        );

        Self {
            inner: Arc::new(EngineInner {
                device,
                queue,
                grade,
                merge,
                blur,
                transform,
            }),
        }
    }

    /// Borrows the engine carried by an evaluation context, if any.
    pub fn from_context(ctx: &EvalContext) -> Option<&Self> {
        ctx.get::<Self>()
    }

    /// The device this engine dispatches on.
    pub fn device(&self) -> &wgpu::Device {
        &self.inner.device
    }

    /// Runs `Grade` over one dense RGBA tile plus its mask tile.
    pub fn grade_tile(
        &self,
        input_rgba: &[f32],
        mask: &[f32],
        params: &GradeParams,
    ) -> Result<Vec<f32>, NodeError> {
        if input_rgba.len() != TILE_RGBA_VALUES {
            return Err(NodeError::Other(format!(
                "grade input needs {TILE_RGBA_VALUES} floats, got {}",
                input_rgba.len()
            )));
        }
        if mask.len() != TILE_PIXELS {
            return Err(NodeError::Other(format!(
                "grade mask needs {TILE_PIXELS} floats, got {}",
                mask.len()
            )));
        }

        let device = &self.inner.device;
        let input = create_buffer(device, "tanova-grade-input", input_rgba, true);
        let mask_buffer = create_buffer(device, "tanova-grade-mask", mask, true);
        let uniform = create_uniform_buffer(device, "tanova-grade-params", &[*params]);
        let output = create_output_buffer(device, TILE_RGBA_BYTES);

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("tanova-grade-bind-group"),
            layout: &self.inner.grade.layout,
            entries: &[
                binding(0, &input),
                binding(1, &mask_buffer),
                binding(2, &uniform),
                binding(3, &output),
            ],
        });

        self.run(&self.inner.grade, &bind_group, &output, TILE_RGBA_BYTES)
    }

    /// Runs `Merge2` over one dense background tile, foreground tile, and mask.
    pub fn merge_tile(
        &self,
        background: &[f32],
        foreground: &[f32],
        mask: &[f32],
        params: &MergeParams,
    ) -> Result<Vec<f32>, NodeError> {
        for (name, values) in [("background", background), ("foreground", foreground)] {
            if values.len() != TILE_RGBA_VALUES {
                return Err(NodeError::Other(format!(
                    "merge {name} needs {TILE_RGBA_VALUES} floats, got {}",
                    values.len()
                )));
            }
        }
        if mask.len() != TILE_PIXELS {
            return Err(NodeError::Other(format!(
                "merge mask needs {TILE_PIXELS} floats, got {}",
                mask.len()
            )));
        }

        let device = &self.inner.device;
        let background_buffer = create_buffer(device, "tanova-merge-b", background, true);
        let foreground_buffer = create_buffer(device, "tanova-merge-a", foreground, true);
        let mask_buffer = create_buffer(device, "tanova-merge-mask", mask, true);
        let uniform = create_uniform_buffer(device, "tanova-merge-params", &[*params]);
        let output = create_output_buffer(device, TILE_RGBA_BYTES);

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("tanova-merge-bind-group"),
            layout: &self.inner.merge.layout,
            entries: &[
                binding(0, &background_buffer),
                binding(1, &foreground_buffer),
                binding(2, &mask_buffer),
                binding(3, &uniform),
                binding(4, &output),
            ],
        });

        self.run(&self.inner.merge, &bind_group, &output, TILE_RGBA_BYTES)
    }

    /// Runs the Gaussian `Blur` over one padded RGBA tile.
    ///
    /// `padded` is `padded_width^2` interleaved pixels (the 256x256 tile plus
    /// an `apron` halo). The output is one dense 256x256 interleaved RGBA tile.
    pub fn blur_tile(
        &self,
        padded: &[[f32; 4]],
        params: &BlurParams,
    ) -> Result<Vec<f32>, NodeError> {
        let expected = params.padded_width as usize * params.padded_width as usize;
        if padded.len() != expected {
            return Err(NodeError::Other(format!(
                "blur input needs {expected} pixels, got {}",
                padded.len()
            )));
        }

        let device = &self.inner.device;
        let input = create_buffer(device, "tanova-blur-input", padded, true);
        let uniform = create_uniform_buffer(device, "tanova-blur-params", &[*params]);
        let output = create_output_buffer(device, TILE_RGBA_BYTES);

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("tanova-blur-bind-group"),
            layout: &self.inner.blur.layout,
            entries: &[
                binding(0, &input),
                binding(1, &uniform),
                binding(2, &output),
            ],
        });

        self.run(&self.inner.blur, &bind_group, &output, TILE_RGBA_BYTES)
    }

    /// Runs inverse-sampled bilinear `Transform` over one gathered input tile.
    ///
    /// `gathered` is `width * height` interleaved RGBA pixels covering the
    /// inverse-mapped footprint of one output tile. `params` maps output-local
    /// coordinates into that gathered region. The output is one dense 256x256
    /// interleaved RGBA tile; samples outside the gathered region read as zero.
    pub fn transform_tile(
        &self,
        gathered: &[f32],
        params: &TransformParams,
    ) -> Result<Vec<f32>, NodeError> {
        let width = params.input_size[0] as usize;
        let height = params.input_size[1] as usize;
        let expected = width * height * 4;
        if gathered.len() != expected {
            return Err(NodeError::Other(format!(
                "transform input needs {expected} floats, got {}",
                gathered.len()
            )));
        }

        let device = &self.inner.device;
        let input = create_buffer(device, "tanova-transform-input", gathered, true);
        let uniform = create_uniform_buffer(device, "tanova-transform-params", &[*params]);
        let output = create_output_buffer(device, TILE_RGBA_BYTES);

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("tanova-transform-bind-group"),
            layout: &self.inner.transform.layout,
            entries: &[
                binding(0, &input),
                binding(1, &uniform),
                binding(2, &output),
            ],
        });

        self.run(&self.inner.transform, &bind_group, &output, TILE_RGBA_BYTES)
    }

    /// Records one dispatch, waits for it, and reads the output back.
    fn run(
        &self,
        pass: &ComputePass,
        bind_group: &wgpu::BindGroup,
        output: &wgpu::Buffer,
        output_bytes: u64,
    ) -> Result<Vec<f32>, NodeError> {
        let device = &self.inner.device;
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("tanova-readback"),
            size: output_bytes,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("tanova-compute-encoder"),
        });
        {
            let mut compute = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("tanova-compute-pass"),
                timestamp_writes: None,
            });
            compute.set_pipeline(&pass.pipeline);
            compute.set_bind_group(0, bind_group, &[]);
            compute.dispatch_workgroups(DISPATCH_SIZE, DISPATCH_SIZE, 1);
        }
        encoder.copy_buffer_to_buffer(output, 0, &readback, 0, output_bytes);
        self.inner.queue.submit(Some(encoder.finish()));

        let (sender, receiver) = std::sync::mpsc::channel();
        readback
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                let _ = sender.send(result);
            });
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|error| NodeError::Other(format!("wgpu poll failed: {error}")))?;
        receiver
            .recv()
            .map_err(|error| NodeError::Other(format!("wgpu map callback lost: {error}")))?
            .map_err(|error| NodeError::Other(format!("wgpu buffer map failed: {error}")))?;

        let pixels = {
            let view = readback
                .slice(..)
                .get_mapped_range()
                .map_err(|error| NodeError::Other(format!("wgpu mapped range failed: {error}")))?;
            bytemuck::cast_slice::<u8, f32>(&view[..]).to_vec()
        };
        readback.unmap();
        Ok(pixels)
    }
}

/// Convenience accessor so cartridges can ask for the engine without naming
/// any kernel internals.
pub trait EvalContextGpu {
    /// The cached GPU compute engine, if one was injected.
    fn gpu(&self) -> Option<&GpuComputeEngine>;
}

impl EvalContextGpu for EvalContext {
    fn gpu(&self) -> Option<&GpuComputeEngine> {
        GpuComputeEngine::from_context(self)
    }
}

fn storage_read() -> wgpu::BindingType {
    wgpu::BindingType::Buffer {
        ty: wgpu::BufferBindingType::Storage { read_only: true },
        has_dynamic_offset: false,
        min_binding_size: None,
    }
}

fn storage_read_write() -> wgpu::BindingType {
    wgpu::BindingType::Buffer {
        ty: wgpu::BufferBindingType::Storage { read_only: false },
        has_dynamic_offset: false,
        min_binding_size: None,
    }
}

fn build_pass(
    device: &wgpu::Device,
    label: &str,
    source: &str,
    bindings: &[wgpu::BindingType],
) -> ComputePass {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(label),
        source: wgpu::ShaderSource::Wgsl(source.into()),
    });

    let entries: Vec<wgpu::BindGroupLayoutEntry> = bindings
        .iter()
        .enumerate()
        .map(|(index, ty)| wgpu::BindGroupLayoutEntry {
            binding: index as u32,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: *ty,
            count: None,
        })
        .collect();

    let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some(label),
        entries: &entries,
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some(label),
        bind_group_layouts: &[Some(&layout)],
        immediate_size: 0,
    });
    let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some(label),
        layout: Some(&pipeline_layout),
        module: &shader,
        entry_point: Some("main"),
        compilation_options: wgpu::PipelineCompilationOptions::default(),
        cache: None,
    });

    ComputePass { layout, pipeline }
}

fn create_buffer<T: Pod>(
    device: &wgpu::Device,
    label: &str,
    data: &[T],
    read_only: bool,
) -> wgpu::Buffer {
    let usage = if read_only {
        wgpu::BufferUsages::STORAGE
    } else {
        wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC
    };
    device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some(label),
        contents: bytemuck::cast_slice(data),
        usage,
    })
}

fn create_uniform_buffer<T: Pod>(device: &wgpu::Device, label: &str, data: &[T]) -> wgpu::Buffer {
    device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some(label),
        contents: bytemuck::cast_slice(data),
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
    })
}

fn create_output_buffer(device: &wgpu::Device, size: u64) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("tanova-compute-output"),
        size,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    })
}

fn binding<'a>(index: u32, buffer: &'a wgpu::Buffer) -> wgpu::BindGroupEntry<'a> {
    wgpu::BindGroupEntry {
        binding: index,
        resource: buffer.as_entire_binding(),
    }
}

const GRADE_WGSL: &str = r#"
struct GradeParams {
    blackpoint: vec4<f32>,
    whitepoint: vec4<f32>,
    lift: vec4<f32>,
    gain: vec4<f32>,
    multiply: vec4<f32>,
    offset: vec4<f32>,
    gamma: vec4<f32>,
    flags: vec4<u32>,
    mix: vec4<f32>,
};

@group(0) @binding(0) var<storage, read> input_pixels: array<vec4<f32>>;
@group(0) @binding(1) var<storage, read> mask_values: array<f32>;
@group(0) @binding(2) var<uniform> params: GradeParams;
@group(0) @binding(3) var<storage, read_write> output_pixels: array<vec4<f32>>;

@compute @workgroup_size(16, 16, 1)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    if (id.x >= 256u || id.y >= 256u) {
        return;
    }
    let index = id.y * 256u + id.x;
    let rgba = input_pixels[index];
    let alpha = rgba.a;
    let unpremultiply = params.flags.x != 0u && alpha > 1e-5;

    var work_rgb = rgba.rgb;
    if (unpremultiply) {
        work_rgb = rgba.rgb / alpha;
    }

    var graded = vec3<f32>(0.0);
    for (var channel = 0u; channel < 3u; channel = channel + 1u) {
        let black = params.blackpoint[channel];
        let white = params.whitepoint[channel];
        var denom = white - black;
        if (abs(denom) < 1e-6) {
            denom = 1e-6;
        }
        let slope = params.multiply[channel] * (params.gain[channel] - params.lift[channel]) / denom;
        let intercept = params.offset[channel] + params.lift[channel] - black * slope;
        var value = slope * work_rgb[channel] + intercept;

        if (params.flags.y != 0u) {
            value = max(value, 0.0);
        }
        if (params.flags.z != 0u) {
            value = min(value, 1.0);
        }

        let gamma = params.gamma[channel];
        if (value > 0.0 && gamma > 1e-6 && abs(gamma - 1.0) > 1e-6) {
            value = pow(value, 1.0 / gamma);
        }
        graded[channel] = value;
    }

    let effective_mix = clamp(params.mix.x * mask_values[index], 0.0, 1.0);
    var out_rgb = work_rgb + (graded - work_rgb) * effective_mix;
    if (unpremultiply) {
        out_rgb = out_rgb * alpha;
    }
    output_pixels[index] = vec4<f32>(out_rgb, alpha);
}
"#;

const MERGE_WGSL: &str = r#"
struct MergeParams {
    operation: vec4<u32>,
    mix: vec4<f32>,
};

@group(0) @binding(0) var<storage, read> background_pixels: array<vec4<f32>>;
@group(0) @binding(1) var<storage, read> foreground_pixels: array<vec4<f32>>;
@group(0) @binding(2) var<storage, read> mask_values: array<f32>;
@group(0) @binding(3) var<uniform> params: MergeParams;
@group(0) @binding(4) var<storage, read_write> output_pixels: array<vec4<f32>>;

fn apply(op: u32, a: vec4<f32>, b: vec4<f32>) -> vec4<f32> {
    switch op {
        case 1u: {
            return a * (1.0 - b.a) + b;
        }
        case 2u: {
            return a + b;
        }
        case 3u: {
            return b - a;
        }
        case 4u: {
            return a - b;
        }
        case 5u: {
            return a * b;
        }
        case 6u: {
            return b * (1.0 - a.a);
        }
        case 7u: {
            return b * a.a;
        }
        default: {
            return a + b * (1.0 - a.a);
        }
    }
}

@compute @workgroup_size(16, 16, 1)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    if (id.x >= 256u || id.y >= 256u) {
        return;
    }
    let index = id.y * 256u + id.x;
    let a = foreground_pixels[index];
    let b = background_pixels[index];
    let result = apply(params.operation.x, a, b);
    let effective_mix = clamp(params.mix.x * mask_values[index], 0.0, 1.0);
    output_pixels[index] = b + (result - b) * effective_mix;
}
"#;

const BLUR_WGSL: &str = r#"
struct BlurParams {
    radius: u32,
    sigma: f32,
    apron: u32,
    padded_width: u32,
};

// Padded apron tiles can exceed the 64 KiB uniform limit, so pixel data must
// live in storage buffers rather than `var<uniform>`.
@group(0) @binding(0) var<storage, read> input_pixels: array<vec4<f32>>;
@group(0) @binding(1) var<uniform> params: BlurParams;
@group(0) @binding(2) var<storage, read_write> output_pixels: array<vec4<f32>>;

@compute @workgroup_size(16, 16, 1)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    if (id.x >= 256u || id.y >= 256u) {
        return;
    }

    let radius = i32(params.radius);
    let stride = i32(params.padded_width);
    let center_x = i32(id.x) + i32(params.apron);
    let center_y = i32(id.y) + i32(params.apron);
    let center = input_pixels[center_y * stride + center_x];

    let sigma = max(params.sigma, 1e-5);
    let denom = max(2.0 * sigma * sigma, 1e-10);

    var sum = vec3<f32>(0.0);
    var weight_sum = 0.0;
    for (var dy = -radius; dy <= radius; dy = dy + 1) {
        for (var dx = -radius; dx <= radius; dx = dx + 1) {
            let weight = exp(-f32(dx * dx + dy * dy) / denom);
            let sample = input_pixels[(center_y + dy) * stride + (center_x + dx)];
            sum = sum + sample.rgb * weight;
            weight_sum = weight_sum + weight;
        }
    }

    // Alpha is intentionally untouched: the pipeline is premultiplied, so the
    // center coverage must survive the RGB convolution.
    output_pixels[id.y * 256u + id.x] = vec4<f32>(sum / weight_sum, center.a);
}
"#;

const TRANSFORM_WGSL: &str = r#"
struct TransformParams {
    inv_row0: vec4<f32>,
    inv_row1: vec4<f32>,
    inv_row2: vec4<f32>,
    input_size: vec4<u32>,
};

// Gathered input footprints can exceed the 64 KiB uniform limit, so pixels live
// in storage buffers.
@group(0) @binding(0) var<storage, read> input_pixels: array<vec4<f32>>;
@group(0) @binding(1) var<uniform> params: TransformParams;
@group(0) @binding(2) var<storage, read_write> output_pixels: array<vec4<f32>>;

@compute @workgroup_size(16, 16, 1)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    if (id.x >= 256u || id.y >= 256u) {
        return;
    }
    let index = id.y * 256u + id.x;

    // Inverse sampling: map the output pixel back into the gathered input.
    let local = vec3<f32>(f32(id.x), f32(id.y), 1.0);
    let w = dot(params.inv_row2.xyz, local);
    if (abs(w) < 1e-12) {
        output_pixels[index] = vec4<f32>(0.0);
        return;
    }
    let uv = vec2<f32>(
        dot(params.inv_row0.xyz, local),
        dot(params.inv_row1.xyz, local),
    ) / w;

    let width = f32(params.input_size.x);
    let height = f32(params.input_size.y);

    // Outside the gathered footprint is transparent black. The +2 px gather
    // margin keeps tile interiors away from this branch.
    if (uv.x < 0.0 || uv.y < 0.0 || uv.x > width - 1.0 || uv.y > height - 1.0) {
        output_pixels[index] = vec4<f32>(0.0);
        return;
    }

    let x0 = i32(floor(uv.x));
    let y0 = i32(floor(uv.y));
    let fx = uv.x - f32(x0);
    let fy = uv.y - f32(y0);
    let stride = i32(params.input_size.x);
    let x1 = min(x0 + 1, i32(params.input_size.x) - 1);
    let y1 = min(y0 + 1, i32(params.input_size.y) - 1);

    let c00 = input_pixels[y0 * stride + x0];
    let c10 = input_pixels[y0 * stride + x1];
    let c01 = input_pixels[y1 * stride + x0];
    let c11 = input_pixels[y1 * stride + x1];

    let top = mix(c00, c10, fx);
    let bottom = mix(c01, c11, fx);
    output_pixels[index] = mix(top, bottom, fy);
}
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::merge_pixel;

    fn test_engine() -> Option<GpuComputeEngine> {
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
        Some(GpuComputeEngine::new(Arc::new(device), Arc::new(queue)))
    }

    fn ramp_rgba() -> Vec<f32> {
        (0..TILE_PIXELS)
            .flat_map(|index| {
                let value = (index as f32 / TILE_PIXELS as f32).clamp(0.0, 1.0);
                [
                    value,
                    (value * 0.5).fract(),
                    1.0 - value,
                    0.25 + 0.75 * value,
                ]
            })
            .collect()
    }

    fn ramp_mask() -> Vec<f32> {
        (0..TILE_PIXELS)
            .map(|index| (index as f32 / TILE_PIXELS as f32).clamp(0.0, 1.0))
            .collect()
    }

    #[test]
    fn uniform_structs_match_wgsl_alignment() {
        assert_eq!(std::mem::size_of::<GradeParams>(), 144);
        assert_eq!(std::mem::size_of::<MergeParams>(), 32);
        assert_eq!(std::mem::size_of::<BlurParams>(), 16);
        assert_eq!(std::mem::size_of::<TransformParams>(), 64);
        assert_eq!(std::mem::align_of::<GradeParams>(), 4);
    }

    #[test]
    fn gpu_grade_matches_cpu_fallback() {
        let Some(engine) = test_engine() else {
            eprintln!("no GPU adapter available; skipping Grade parity test");
            return;
        };

        let op = GradeOp {
            blackpoint: [0.05, 0.02, 0.0, 0.0],
            whitepoint: [0.95, 0.9, 1.0, 1.0],
            lift: [0.01, 0.0, 0.02, 0.0],
            gain: [1.1, 0.9, 1.0, 1.0],
            multiply: [1.0, 1.05, 0.95, 1.0],
            offset: [0.0, 0.01, -0.01, 0.0],
            gamma: [2.2, 1.8, 1.0, 1.0],
            unpremult: true,
            black_clamp: true,
            white_clamp: false,
            mix: 0.85,
        };
        let input = ramp_rgba();
        let mask = ramp_mask();
        let gpu = engine
            .grade_tile(&input, &mask, &GradeParams::new(&op))
            .expect("grade dispatch");

        for (index, (actual, source)) in gpu.chunks_exact(4).zip(input.chunks_exact(4)).enumerate()
        {
            let expected =
                op.apply_pixel([source[0], source[1], source[2], source[3]], mask[index]);
            for channel in 0..4 {
                assert!(
                    (actual[channel] - expected[channel]).abs() <= 1e-5,
                    "pixel {index} channel {channel}: gpu {} vs cpu {}",
                    actual[channel],
                    expected[channel]
                );
            }
        }
    }

    #[test]
    fn gpu_blur_matches_cpu_fallback() {
        let Some(engine) = test_engine() else {
            eprintln!("no GPU adapter available; skipping Blur parity test");
            return;
        };

        let radius = 5u32;
        let sigma = 2.0f32;
        let padded_width = TILE_SIZE + 2 * radius;
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

        let gpu = engine
            .blur_tile(&padded, &BlurParams::new(radius, sigma, padded_width))
            .expect("blur dispatch");
        let cpu = crate::ops::blur_padded_rgba(&padded, radius, sigma, padded_width);

        assert_eq!(gpu.len(), TILE_RGBA_VALUES);
        for (index, (actual, expected)) in gpu.chunks_exact(4).zip(cpu.iter()).enumerate() {
            for channel in 0..4 {
                assert!(
                    (actual[channel] - expected[channel]).abs() <= 1e-4,
                    "pixel {index} channel {channel}: gpu {} vs cpu {}",
                    actual[channel],
                    expected[channel]
                );
            }
        }
    }

    #[test]
    fn gpu_transform_matches_cpu_bilinear_fallback() {
        let Some(engine) = test_engine() else {
            eprintln!("no GPU adapter available; skipping Transform parity test");
            return;
        };

        let width = 600u32;
        let height = 600u32;
        let mut gathered = Vec::with_capacity((width * height * 4) as usize);
        for y in 0..height {
            for x in 0..width {
                let fx = x as f32;
                let fy = y as f32;
                gathered.extend_from_slice(&[
                    (fx * 0.021).sin() * 0.5 + 0.5,
                    (fy * 0.017).cos() * 0.5 + 0.5,
                    ((fx + fy) * 0.009).fract(),
                    (fx * 0.005 + fy * 0.011).fract(),
                ]);
            }
        }

        // Map output-local pixels into the gathered region with a scale,
        // rotation, and translation so every branch of the sampler runs.
        let matrix = Mat3x3::translate(260.0, 240.0)
            .mul(&Mat3x3::rotate(0.3))
            .mul(&Mat3x3::scale(0.8, 0.9));
        let gpu = engine
            .transform_tile(&gathered, &TransformParams::new(&matrix, width, height))
            .expect("transform dispatch");
        let cpu = crate::ops::transform_gathered_rgba(&gathered, width, height, &matrix);

        assert_eq!(gpu.len(), TILE_RGBA_VALUES);
        for (index, (actual, expected)) in gpu.chunks_exact(4).zip(cpu.chunks_exact(4)).enumerate()
        {
            for channel in 0..4 {
                assert!(
                    (actual[channel] - expected[channel]).abs() <= 1e-4,
                    "pixel {index} channel {channel}: gpu {} vs cpu {}",
                    actual[channel],
                    expected[channel]
                );
            }
        }
    }

    #[test]
    fn gpu_merge_matches_cpu_fallback() {
        let Some(engine) = test_engine() else {
            eprintln!("no GPU adapter available; skipping Merge2 parity test");
            return;
        };

        let background = ramp_rgba();
        let foreground: Vec<f32> = ramp_rgba()
            .iter()
            .enumerate()
            .map(|(index, value)| {
                if index % 4 == 3 {
                    (value * 0.5).clamp(0.0, 1.0)
                } else {
                    (value * 0.75).clamp(0.0, 1.0)
                }
            })
            .collect();
        let mask = ramp_mask();

        for operation in MergeOp::ALL {
            let gpu = engine
                .merge_tile(
                    &background,
                    &foreground,
                    &mask,
                    &MergeParams::new(operation, 0.9),
                )
                .expect("merge dispatch");

            for (index, ((actual, b), a)) in gpu
                .chunks_exact(4)
                .zip(background.chunks_exact(4))
                .zip(foreground.chunks_exact(4))
                .enumerate()
            {
                let expected = merge_pixel(
                    operation,
                    [b[0], b[1], b[2], b[3]],
                    [a[0], a[1], a[2], a[3]],
                    mask[index],
                    0.9,
                );
                // `expected` is a single pixel; compare below.
                for channel in 0..4 {
                    assert!(
                        (actual[channel] - expected[channel]).abs() <= 1e-5,
                        "{operation:?} pixel {index} channel {channel}: gpu {} vs cpu {}",
                        actual[channel],
                        expected[channel]
                    );
                }
            }
        }
    }
}

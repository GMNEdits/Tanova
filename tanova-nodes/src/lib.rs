//! Tanova node cartridges.
//!
//! Each node lives in exactly one file (e.g. `src/blur.rs`) and registers
//! itself with the kernel's global registry via `inventory`.

pub mod backdrop;
pub mod blur;
pub mod clamp;
pub mod color_correct;
pub mod constant;
pub mod crop;
pub mod dot;
pub mod dummy;
pub mod grade;
pub mod invert;
pub mod merge;
pub mod premult;
pub mod read;
pub mod saturation;
pub mod transform;
pub mod unpremult;
pub mod util;
pub mod viewer;
pub mod write;

pub use backdrop::BackdropNode;
pub use blur::BlurNode;
pub use clamp::ClampNode;
pub use color_correct::ColorCorrectNode;
pub use constant::{ConstantNode, DEFAULT_COLOR};
pub use crop::CropNode;
pub use dot::DotNode;
pub use dummy::{DEFAULT_MESSAGE, DummyNode, DummyPayload};
pub use grade::{GradeNode, grade_op};
pub use invert::InvertNode;
pub use merge::Merge2Node;
pub use premult::PremultNode;
pub use read::ReadNode;
pub use saturation::SaturationNode;
pub use transform::TransformNode;
pub use unpremult::UnpremultNode;
pub use viewer::ViewerNode;
pub use write::{WriteNode, default_knobs as write_default_knobs, export_packet_with_write_knobs};

/// Forces this crate, and therefore its `inventory` constructors, to be linked.
///
/// Binaries and integration tests call this before using the registry so the
/// linker cannot dead-strip the cartridge submissions.
pub fn ensure_linked() {}

/// Evaluates one masked color operation per pixel while sharing untouched planes.
fn map_color_tiles<const N: usize>(
    ctx: &tanova_kernel::EvalContext,
    input: &tanova_render::Raster2D,
    mask: Option<&tanova_render::Raster2D>,
    channels: [tanova_render::Channel; N],
    apply: impl Fn([f32; 4], f32) -> [f32; N],
) -> Result<tanova_render::Raster2D, tanova_kernel::NodeError> {
    use std::sync::Arc;
    use tanova_render::ops::{uniform_mask_value, uniform_rgba};
    use tanova_render::{Channel, ChannelPlane, TILE_PIXELS, TILE_SIZE, TileBuffer, mask_value};

    let mut output = input.clone();
    let mut planes: [ChannelPlane; N] = std::array::from_fn(|_| ChannelPlane::new());
    let missing_alpha = input.channel(&Channel::RgbaAlpha).is_none();
    for tile in input.bbox.tile_coords() {
        let bounds = tile.to_bbox();
        let region = bounds.intersection(&input.bbox);
        let uniform_mask = if let Some(mask) = mask {
            let overlap = region.intersection(&mask.bbox);
            if overlap.is_empty() {
                Some(0.0)
            } else if overlap == region {
                uniform_mask_value(Some(mask), tile, region)
            } else {
                None
            }
        } else {
            Some(1.0)
        };
        if let Some((mut rgba, factor)) = uniform_rgba(input, tile, region).zip(uniform_mask) {
            if missing_alpha {
                rgba[3] = 1.0;
            }
            for (plane, value) in planes.iter_mut().zip(apply(rgba, factor)) {
                plane.insert(tile, Arc::new(TileBuffer::new_uniform(value)));
            }
            continue;
        }

        let mut values: [Vec<f32>; N] = std::array::from_fn(|_| Vec::with_capacity(TILE_PIXELS));
        for y in 0..TILE_SIZE as i32 {
            for x in 0..TILE_SIZE as i32 {
                let wx = bounds.min_x + x;
                let wy = bounds.min_y + y;
                let rgba = input.sample_rgba(wx, wy);
                for (values, value) in values.iter_mut().zip(apply(rgba, mask_value(mask, wx, wy)))
                {
                    values.push(value);
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
    for (channel, plane) in channels.into_iter().zip(planes) {
        output.insert_channel(channel, plane);
    }
    Ok(output)
}

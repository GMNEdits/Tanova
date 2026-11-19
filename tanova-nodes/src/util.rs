//! Small helpers shared by the core cartridges.
//!
//! Every helper reads from the live [`KnobTable`] injected into `EvalContext`,
//! falling back to a caller-supplied default when the knob is missing or has the
//! wrong type.

use tanova_kernel::{EvalContext, KnobTable, KnobValue, NodeError, Packet};
use tanova_render::Raster2D;

/// Reads an RGBA color knob.
pub fn color_knob(knobs: &KnobTable, name: &str, default: [f32; 4]) -> [f32; 4] {
    match knobs.get(name) {
        Some(KnobValue::Color(color)) => *color,
        _ => default,
    }
}

/// Reads a boolean knob.
pub fn bool_knob(knobs: &KnobTable, name: &str, default: bool) -> bool {
    match knobs.get(name) {
        Some(KnobValue::Bool(value)) => *value,
        _ => default,
    }
}

/// Reads a float knob.
pub fn float_knob(knobs: &KnobTable, name: &str, default: f32) -> f32 {
    match knobs.get(name) {
        Some(KnobValue::Float(value)) => *value as f32,
        Some(KnobValue::Int(value)) => *value as f32,
        _ => default,
    }
}

/// Reads an integer knob.
pub fn int_knob(knobs: &KnobTable, name: &str, default: i64) -> i64 {
    match knobs.get(name) {
        Some(KnobValue::Int(value)) => *value,
        Some(KnobValue::Float(value)) => *value as i64,
        _ => default,
    }
}

/// Reads a text knob.
pub fn text_knob(knobs: &KnobTable, name: &str, default: &str) -> String {
    match knobs.get(name) {
        Some(KnobValue::Text(value)) => value.clone(),
        _ => default.to_string(),
    }
}

/// Borrows the raster on `port`, if the upstream packet carried one.
pub fn raster_input(inputs: &[Packet], port: usize) -> Option<&Raster2D> {
    inputs.get(port).and_then(|packet| packet.get::<Raster2D>())
}

/// Formats an RGBA color for TCL serialization.
pub fn format_color(color: [f32; 4]) -> String {
    format!("{} {} {} {}", color[0], color[1], color[2], color[3])
}

/// Formats a float knob for TCL serialization.
pub fn format_float(value: f32) -> String {
    if value.fract() == 0.0 {
        format!("{value:.1}")
    } else {
        value.to_string()
    }
}

/// Evaluates one masked color operation per pixel while sharing untouched
/// planes.
///
/// `channels` names the planes `apply` writes; every other channel (AOVs such
/// as `depth.Z`) is cloned through untouched so a color edit never copies an
/// entire auxiliary plane.
pub fn map_color_tiles<const N: usize>(
    ctx: &EvalContext,
    input: &Raster2D,
    mask: Option<&Raster2D>,
    channels: [tanova_render::Channel; N],
    apply: impl Fn([f32; 4], f32) -> [f32; N],
) -> Result<Raster2D, NodeError> {
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

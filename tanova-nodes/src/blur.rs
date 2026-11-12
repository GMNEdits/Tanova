//! `Blur`: Nuke-style Gaussian blur with BBox expansion and apron sampling.
//!
//! The pixel convolution lives in `tanova-render` (`blur.wgsl` with a matching
//! CPU fallback); this file only maps the `size` knob to a radius, grows the
//! data window, and streams padded RGBA tiles through the engine. Non-RGBA AOV
//! planes pass through by `Arc` clone.

use std::sync::Arc;

use tanova_kernel::{
    EvalContext, KnobTable, KnobValue, MemoryGovernor, NodeCartridgeDescriptor, NodeError,
    NodeTrait, Packet,
};
use tanova_render::{
    Channel, ChannelPlane, EvalContextGpu, Raster2D, TILE_SIZE, TileBuffer, TileCoord,
    blur_padded_tile, extract_padded_tile, is_rgba_channel,
};

use crate::util::{float_knob, format_float, raster_input};

/// Default blur size; below [`MIN_BLUR_SIZE`] the node bypasses untouched.
pub const DEFAULT_SIZE: f32 = 0.0;

/// Sizes under this threshold are a no-op, matching Nuke's disabled blur.
pub const MIN_BLUR_SIZE: f32 = 0.1;

/// Largest radius whose padded storage buffer stays inside a 128 MiB binding.
pub const MAX_BLUR_RADIUS: u32 = 1024;

/// Default knob values for a fresh [`BlurNode`].
pub fn default_knobs() -> KnobTable {
    let mut knobs = KnobTable::with_capacity(1);
    knobs.insert("size", KnobValue::Float(DEFAULT_SIZE as f64));
    knobs
}

/// A Nuke-style Gaussian blur cartridge.
#[derive(Debug, Clone)]
pub struct BlurNode {
    knobs: KnobTable,
}

impl Default for BlurNode {
    fn default() -> Self {
        Self {
            knobs: default_knobs(),
        }
    }
}

impl BlurNode {
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

impl NodeTrait for BlurNode {
    fn evaluate(&self, ctx: &EvalContext, inputs: &[Packet]) -> Result<Packet, NodeError> {
        let input = raster_input(inputs, 0).ok_or(NodeError::MissingInput("main"))?;
        let knobs = ctx.knobs().unwrap_or(&self.knobs);
        let size = float_knob(knobs, "size", DEFAULT_SIZE);

        // Zero-radius bypass: hand the upstream packet back untouched so the
        // cached `Arc<TileBuffer>` handles stay shared.
        if size < MIN_BLUR_SIZE {
            return Ok(inputs[0].clone());
        }

        let radius = size.ceil() as u32;
        if radius > MAX_BLUR_RADIUS {
            return Err(NodeError::InvalidKnob {
                knob: "size".to_string(),
                reason: format!("blur radius {radius} exceeds the {MAX_BLUR_RADIUS}-pixel maximum"),
            });
        }
        let sigma = (size / 3.0).max(1e-5);
        let output_bbox = input.bbox.pad(radius as i32);

        let governor = ctx.governor();
        let gpu = ctx.gpu();

        let mut output = Raster2D::new(input.format.clone(), output_bbox, input.edge_mode);
        // Non-RGBA AOVs (DepthZ, custom planes) never enter the convolution.
        for (channel, plane) in input.channels.iter() {
            if !is_rgba_channel(channel) {
                output.insert_channel(channel.clone(), plane.clone());
            }
        }

        let padded_width = TILE_SIZE + 2 * radius;
        let padded_pixels = (padded_width * padded_width) as usize;

        let mut red = ChannelPlane::new();
        let mut green = ChannelPlane::new();
        let mut blue = ChannelPlane::new();
        let mut alpha = ChannelPlane::new();

        for tile in output_bbox.tile_coords() {
            let red_values = padded_channel(input, &Channel::RgbaRed, tile, radius, padded_pixels);
            let green_values =
                padded_channel(input, &Channel::RgbaGreen, tile, radius, padded_pixels);
            let blue_values =
                padded_channel(input, &Channel::RgbaBlue, tile, radius, padded_pixels);
            let alpha_values =
                padded_channel(input, &Channel::RgbaAlpha, tile, radius, padded_pixels);

            // A constant apron stays constant under a normalized Gaussian, so
            // uniform tiles skip the convolution entirely.
            if let [
                Some(red_value),
                Some(green_value),
                Some(blue_value),
                Some(alpha_value),
            ] = [
                uniform_value(&red_values),
                uniform_value(&green_values),
                uniform_value(&blue_values),
                uniform_value(&alpha_values),
            ] {
                insert_uniform(&mut red, tile, red_value);
                insert_uniform(&mut green, tile, green_value);
                insert_uniform(&mut blue, tile, blue_value);
                insert_uniform(&mut alpha, tile, alpha_value);
                continue;
            }

            let mut padded = Vec::with_capacity(padded_pixels * 4);
            for index in 0..padded_pixels {
                padded.push([
                    red_values[index],
                    green_values[index],
                    blue_values[index],
                    alpha_values[index],
                ]);
            }

            let pixels = blur_padded_tile(&padded, radius, sigma, padded_width, gpu)?;
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
        let size = float_knob(&self.knobs, "size", DEFAULT_SIZE);
        format!("Blur {{\n size {}\n}}", format_float(size))
    }
}

/// Extracts one channel's apron-padded tile, filling missing planes with the
/// same defaults the display path uses (black RGB, opaque alpha).
fn padded_channel(
    input: &Raster2D,
    channel: &Channel,
    tile: TileCoord,
    apron: u32,
    padded_pixels: usize,
) -> Vec<f32> {
    match input.channel(channel) {
        Some(plane) => extract_padded_tile(plane, tile, apron, input.bbox, input.edge_mode),
        None => {
            let value = if *channel == Channel::RgbaAlpha {
                1.0
            } else {
                0.0
            };
            vec![value; padded_pixels]
        }
    }
}

fn uniform_value(values: &[f32]) -> Option<f32> {
    let first = *values.first()?;
    values.iter().all(|value| *value == first).then_some(first)
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
    Arc::new(BlurNode::new())
}

inventory::submit! {
    NodeCartridgeDescriptor {
        kind: "Blur",
        default_knobs,
        factory,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tanova_kernel::KnobTable;
    use tanova_render::{BBox, EdgeMode, Format, TILE_PIXELS};

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
                TileCoord::new(0, 0),
                Arc::new(TileBuffer::new_uniform(value)),
            );
            raster.insert_channel(channel, plane);
        }
        raster
    }

    fn context_with_size(size: f32) -> EvalContext {
        let mut knobs = KnobTable::with_capacity(1);
        knobs.insert("size", KnobValue::Float(size as f64));
        let mut ctx = EvalContext::new();
        ctx.insert(knobs);
        ctx
    }

    #[test]
    fn zero_radius_bypass_is_a_pointer_clone() {
        let tile = TileCoord::new(0, 0);
        let data: Vec<f32> = (0..TILE_PIXELS)
            .map(|index| index as f32 / TILE_PIXELS as f32)
            .collect();
        let buffer = Arc::new(TileBuffer::from_vec(data, None).expect("dense tile"));

        let mut plane = ChannelPlane::new();
        plane.insert(tile, Arc::clone(&buffer));
        let bbox = BBox::new(0, 0, TILE_SIZE as i32, TILE_SIZE as i32);
        let mut raster = Raster2D::new(
            Format::new("test", TILE_SIZE, TILE_SIZE, 1.0),
            bbox,
            EdgeMode::BlackOutside,
        );
        raster.insert_channel(Channel::RgbaRed, plane);

        let mut packet = Packet::new();
        packet.insert(raster);

        let output = BlurNode::new()
            .evaluate(&EvalContext::new(), &[packet])
            .expect("zero-radius blur should bypass");
        let output_raster = output.get::<Raster2D>().expect("raster");
        let output_buffer = output_raster
            .channel(&Channel::RgbaRed)
            .and_then(|plane| plane.get(&tile))
            .expect("red tile");

        assert!(
            Arc::ptr_eq(&buffer, output_buffer),
            "zero-radius blur must reuse the upstream tile instead of copying it"
        );
    }

    #[test]
    fn blur_expands_the_data_window_by_the_radius() {
        let input = uniform_raster(100, 100, 0.75, EdgeMode::ClampToEdge);
        let mut packet = Packet::new();
        packet.insert(input);

        let output = BlurNode::new()
            .evaluate(&context_with_size(10.0), &[packet])
            .expect("blur should evaluate");
        let raster = output.get::<Raster2D>().expect("raster");

        assert_eq!(
            raster.bbox,
            BBox::new(-10, -10, 110, 110),
            "size 10 must pad the data window by ten pixels on every side"
        );
        // A constant image stays constant under a normalized Gaussian.
        let pixel = raster.sample_rgba(50, 50);
        for (channel, value) in pixel.iter().enumerate() {
            assert!((value - 0.75).abs() < 1e-4, "channel {channel} was {value}");
        }
    }

    #[test]
    fn non_rgba_planes_pass_through_by_arc() {
        let mut input = uniform_raster(8, 8, 0.5, EdgeMode::BlackOutside);
        let mut depth = ChannelPlane::new();
        let depth_buffer = Arc::new(TileBuffer::new_uniform(9.0));
        depth.insert(TileCoord::new(0, 0), Arc::clone(&depth_buffer));
        input.insert_channel(Channel::DepthZ, depth);

        let mut packet = Packet::new();
        packet.insert(input);
        let output = BlurNode::new()
            .evaluate(&context_with_size(1.0), &[packet])
            .expect("blur should evaluate");
        let raster = output.get::<Raster2D>().expect("raster");
        let output_depth = raster
            .channel(&Channel::DepthZ)
            .and_then(|plane| plane.get(&TileCoord::new(0, 0)))
            .expect("depth tile");

        assert!(Arc::ptr_eq(&depth_buffer, output_depth));
    }

    #[test]
    fn blur_spreads_a_bright_pixel_into_its_neighbours() {
        let bbox = BBox::new(0, 0, 8, 8);
        let mut raster =
            Raster2D::new(Format::new("test", 8, 8, 1.0), bbox, EdgeMode::BlackOutside);
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
        raster
            .set_pixel(Channel::RgbaRed, 4, 4, 1.0, None)
            .expect("red impulse");
        raster
            .set_pixel(Channel::RgbaAlpha, 4, 4, 1.0, None)
            .expect("alpha impulse");

        let mut packet = Packet::new();
        packet.insert(raster);
        let output = BlurNode::new()
            .evaluate(&context_with_size(1.0), &[packet])
            .expect("blur should evaluate");
        let blurred = output.get::<Raster2D>().expect("raster");

        let center = blurred.sample_rgba(4, 4);
        let neighbour = blurred.sample_rgba(5, 4);
        assert!(center[0] < 1.0, "the impulse must lose energy: {center:?}");
        assert!(
            center[0] > 0.9,
            "most energy stays at the center: {center:?}"
        );
        assert!(
            neighbour[0] > 0.0 && neighbour[0] < center[0],
            "the impulse must bleed into the neighbour: {neighbour:?}"
        );
        assert_eq!(center[3], 1.0, "alpha must pass through untouched");
        assert_eq!(neighbour[3], 0.0, "alpha must not blur");
    }

    #[test]
    fn serialization_writes_the_size_knob() {
        let text = BlurNode::new().serialize_to_tcl();
        assert!(text.starts_with("Blur {"), "got {text}");
        assert!(text.contains("size"), "got {text}");
    }
}

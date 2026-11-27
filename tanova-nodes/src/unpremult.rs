//! Nuke-style Unpremult cartridge with governed tiles and zero-copy AOVs.
use crate::util::{float_knob, raster_input};
use std::sync::Arc;
use tanova_kernel::{
    EvalContext, KnobTable, KnobValue, NodeCartridgeDescriptor, NodeError, NodeTrait, Packet,
};
use tanova_render::ops::uniform_rgba;
use tanova_render::{Channel, ChannelPlane, TILE_PIXELS, TILE_SIZE, TileBuffer};
/// Default knob values for a fresh cartridge.
pub fn default_knobs() -> KnobTable {
    let mut knobs = KnobTable::new();
    knobs.insert("alpha", KnobValue::Text("rgba.alpha".into()));
    knobs.insert("mix", KnobValue::Float(1.0));
    knobs
}
/// Nuke-style `Unpremult` image operation.
#[derive(Debug, Clone)]
pub struct UnpremultNode {
    knobs: KnobTable,
}
impl Default for UnpremultNode {
    fn default() -> Self {
        Self {
            knobs: default_knobs(),
        }
    }
}
impl UnpremultNode {
    /// Creates a node with default knobs.
    pub fn new() -> Self {
        Self::default()
    }
    /// Creates a node over an existing knob table.
    pub fn from_knobs(knobs: KnobTable) -> Self {
        Self { knobs }
    }
    /// Borrows the stored knobs; evaluation honors live context overrides.
    pub fn knobs(&self) -> &KnobTable {
        &self.knobs
    }
}
/// The plane used as the premultiplication factor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlphaSource {
    RgbaAlpha,
    DepthZ,
    None,
}
impl AlphaSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::RgbaAlpha => "rgba.alpha",
            Self::DepthZ => "depth.Z",
            Self::None => "none",
        }
    }
}
/// Parsed parameters; pixel evaluation never accesses a knob table.
#[derive(Debug, Clone, Copy)]
pub struct UnpremultOp {
    pub alpha: AlphaSource,
    pub mix: f32,
}
impl UnpremultOp {
    pub fn from_knobs(knobs: &KnobTable) -> Self {
        let alpha = match crate::util::text_knob(knobs, "alpha", "rgba.alpha").as_str() {
            "depth.Z" | "Z" => AlphaSource::DepthZ,
            "none" | "off" => AlphaSource::None,
            // Includes rgba.alpha, alpha, a, and the fallback for unknown names.
            _ => AlphaSource::RgbaAlpha,
        };
        Self {
            alpha,
            mix: float_knob(knobs, "mix", 1.0).clamp(0.0, 1.0),
        }
    }
    /// Applies the operation with an already sampled factor from the selected plane.
    pub fn apply_pixel(&self, rgba: [f32; 4], alpha: f32) -> [f32; 4] {
        if self.alpha == AlphaSource::None || self.mix == 0.0 {
            return rgba;
        }
        // Guard cleanly: zero or non-finite alpha is a passthrough; tiny
        // signed factors (e.g. `±1e-6`) round-trip cleanly with `Premult`.
        if alpha == 0.0 || !alpha.is_finite() {
            return rgba;
        }
        let mut out = rgba;
        for c in 0..3 {
            let q = rgba[c] / alpha;
            let corrected = if q.is_finite() { q } else { rgba[c] };
            out[c] = if self.mix == 1.0 {
                corrected
            } else {
                rgba[c] + (corrected - rgba[c]) * self.mix
            };
        }
        out
    }
}
impl NodeTrait for UnpremultNode {
    fn evaluate(&self, ctx: &EvalContext, inputs: &[Packet]) -> Result<Packet, NodeError> {
        let input = raster_input(inputs, 0).ok_or(NodeError::MissingInput("main"))?;
        let op = UnpremultOp::from_knobs(ctx.knobs().unwrap_or(&self.knobs));
        // Untouched alpha and AOV tiles retain their original Arcs.
        let mut output = input.clone();
        let active = [true, true, true, false];
        let mut planes: [ChannelPlane; 4] = std::array::from_fn(|_| ChannelPlane::new());
        if op.mix == 0.0 || op.alpha == AlphaSource::None {
            let mut packet = Packet::new();
            packet.insert(output);
            return Ok(packet);
        }
        for tile in input.bbox.tile_coords() {
            let region = tile.to_bbox().intersection(&input.bbox);
            let uniform = uniform_rgba(input, tile, region).and_then(|rgba| {
                let factor = match op.alpha {
                    AlphaSource::RgbaAlpha => Some(rgba[3]),
                    AlphaSource::None => Some(1.0),
                    AlphaSource::DepthZ => match input.channel(&Channel::DepthZ) {
                        None => Some(0.0),
                        Some(plane) => plane.get(&tile).and_then(|buffer| buffer.is_uniform()),
                    },
                };
                factor.map(|factor| (rgba, factor))
            });
            if let Some((rgba, factor)) = uniform {
                let out = op.apply_pixel(rgba, factor);
                for c in 0..4 {
                    if active[c] {
                        planes[c].insert(tile, Arc::new(TileBuffer::new_uniform(out[c])));
                    }
                }
                continue;
            }
            // Gather all active planes in one pass: sample and evaluate each pixel once.
            let mut values: [Vec<f32>; 4] = std::array::from_fn(|c| {
                if active[c] {
                    Vec::with_capacity(TILE_PIXELS)
                } else {
                    Vec::new()
                }
            });
            let bounds = tile.to_bbox();
            for y in 0..TILE_SIZE {
                for x in 0..TILE_SIZE {
                    let wx = bounds.min_x + x as i32;
                    let wy = bounds.min_y + y as i32;
                    let rgba = input.sample_rgba(wx, wy);
                    let factor = match op.alpha {
                        AlphaSource::RgbaAlpha => rgba[3],
                        AlphaSource::DepthZ => input.sample(&Channel::DepthZ, wx, wy),
                        AlphaSource::None => 1.0,
                    };
                    let out = op.apply_pixel(rgba, factor);
                    for c in 0..4 {
                        if active[c] {
                            values[c].push(out[c]);
                        }
                    }
                }
            }
            for c in 0..4 {
                if active[c] {
                    let buffer =
                        TileBuffer::from_vec(std::mem::take(&mut values[c]), ctx.governor())?;
                    planes[c].insert(tile, Arc::new(buffer));
                }
            }
        }
        for (c, channel) in [
            Channel::RgbaRed,
            Channel::RgbaGreen,
            Channel::RgbaBlue,
            Channel::RgbaAlpha,
        ]
        .into_iter()
        .enumerate()
        {
            if active[c] {
                output.insert_channel(channel, std::mem::take(&mut planes[c]));
            }
        }
        let mut packet = Packet::new();
        packet.insert(output);
        Ok(packet)
    }
    fn serialize_to_tcl(&self) -> String {
        let op = UnpremultOp::from_knobs(&self.knobs);
        format!(
            "Unpremult {{\n alpha {}\n mix {}\n}}",
            op.alpha.as_str(),
            crate::util::format_float(op.mix)
        )
    }
}
fn factory() -> Arc<dyn NodeTrait> {
    Arc::new(UnpremultNode::new())
}
inventory::submit! { NodeCartridgeDescriptor { kind: "Unpremult", default_knobs, factory } }

#[cfg(test)]
mod tests {
    use super::*;
    use tanova_kernel::MemoryGovernor;
    use tanova_render::{BBox, Format, Raster2D, TileCoord};
    fn input(color: [f32; 4]) -> Packet {
        let mut raster = Raster2D::new(
            Format::new("test", 256, 256, 1.0),
            BBox::new(0, 0, 256, 256),
            Default::default(),
        );
        for (c, v) in [
            Channel::RgbaRed,
            Channel::RgbaGreen,
            Channel::RgbaBlue,
            Channel::RgbaAlpha,
            Channel::DepthZ,
        ]
        .into_iter()
        .zip(color.into_iter().chain([42.0]))
        {
            let mut plane = ChannelPlane::new();
            plane.insert(TileCoord::new(0, 0), Arc::new(TileBuffer::new_uniform(v)));
            raster.insert_channel(c, plane);
        }
        let mut p = Packet::new();
        p.insert(raster);
        p
    }
    #[test]
    fn tiny_alpha_is_safe() {
        // Alpha exactly zero is a passthrough (nothing to divide by).
        let p = UnpremultNode::new()
            .evaluate(&EvalContext::new(), &[input([0.2, 0.4, 0.8, 0.0])])
            .unwrap();
        assert_eq!(
            p.get::<Raster2D>().unwrap().sample_rgba(0, 0),
            [0.2, 0.4, 0.8, 0.0]
        );

        // Tiny signed alphas are no longer swallowed by an absolute cutoff:
        // they divide through and stay finite.
        for alpha in [1e-5f32, 1e-6, -1e-6] {
            let p = UnpremultNode::new()
                .evaluate(&EvalContext::new(), &[input([0.2, 0.4, 0.8, alpha])])
                .unwrap();
            let rgba = p.get::<Raster2D>().unwrap().sample_rgba(0, 0);
            assert!(rgba[..3].iter().all(|value| value.is_finite()));
            for (actual, original) in rgba[..3].iter().zip([0.2f32, 0.4, 0.8]) {
                let expected = original / alpha;
                assert!(
                    (actual - expected).abs() <= expected.abs() * 1e-6,
                    "unpremult must divide through tiny alpha {alpha}: {actual} vs {expected}"
                );
            }
            assert_eq!(rgba[3], alpha);
        }
    }
    #[test]
    fn uniform_costs_zero() {
        let governor = MemoryGovernor::new(0);
        let mut ctx = EvalContext::new();
        ctx.insert(governor.clone());
        UnpremultNode::new()
            .evaluate(&ctx, &[input([0.2, 0.4, 0.8, 0.5])])
            .unwrap();
        assert_eq!(governor.allocated_bytes(), 0);
    }

    #[test]
    fn disabled_alpha_is_a_zero_copy_passthrough_even_for_dense_tiles() {
        let mut p = input([0.25, 0.5, 1.0, 0.0]);
        let mut raster = p.remove::<Raster2D>().unwrap();
        raster
            .set_pixel(Channel::RgbaRed, 3, 4, 0.75, None)
            .unwrap();
        p.insert(raster.clone());
        for source in ["none", "off"] {
            let mut knobs = default_knobs();
            knobs.insert("alpha", KnobValue::Text(source.into()));
            let mut ctx = EvalContext::new();
            let governor = MemoryGovernor::new(0);
            ctx.insert(governor.clone());
            let output = UnpremultNode::from_knobs(knobs)
                .evaluate(&ctx, &[p.clone()])
                .unwrap();
            let output = output.get::<Raster2D>().unwrap();
            assert_eq!(output.sample_rgba(3, 4), raster.sample_rgba(3, 4));
            for channel in raster.channels.keys() {
                assert!(Arc::ptr_eq(
                    raster
                        .channel(channel)
                        .unwrap()
                        .get(&TileCoord::new(0, 0))
                        .unwrap(),
                    output
                        .channel(channel)
                        .unwrap()
                        .get(&TileCoord::new(0, 0))
                        .unwrap()
                ));
            }
            assert_eq!(governor.allocated_bytes(), 0);
        }
    }
    #[test]
    fn mix_is_clamped_and_live_knobs_override_stored_values() {
        for source in ["rgba.alpha", "alpha", "a"] {
            for (mix, expected) in [(-2.0, [0.25, 0.5, 1.0, 0.5]), (2.0, [0.5, 1.0, 2.0, 0.5])] {
                let mut knobs = default_knobs();
                knobs.insert("alpha", KnobValue::Text(source.into()));
                knobs.insert("mix", KnobValue::Float(mix));
                let node = UnpremultNode::from_knobs(knobs.clone());
                let output = node
                    .evaluate(&EvalContext::new(), &[input([0.25, 0.5, 1.0, 0.5])])
                    .unwrap();
                assert_eq!(
                    output.get::<Raster2D>().unwrap().sample_rgba(0, 0),
                    expected
                );
                let text = node.serialize_to_tcl();
                assert!(text.contains(if mix < 0.0 { "mix 0.0" } else { "mix 1.0" }));
                let mut ctx = EvalContext::new();
                ctx.insert(knobs);
                let live = UnpremultNode::new()
                    .evaluate(&ctx, &[input([0.25, 0.5, 1.0, 0.5])])
                    .unwrap();
                assert_eq!(live.get::<Raster2D>().unwrap().sample_rgba(0, 0), expected);
            }
        }
    }
    #[test]
    fn depth_selector_handles_uniform_and_dense_depth_without_modifying_aovs() {
        for source in ["depth.Z", "Z"] {
            for dense in [false, true] {
                let mut p = input([0.25, 0.5, 1.0, 0.5]);
                let mut raster = p.remove::<Raster2D>().unwrap();
                let governor = MemoryGovernor::new(if dense { 8 * 1024 * 1024 } else { 0 });
                if dense {
                    raster
                        .set_pixel(Channel::DepthZ, 3, 4, 2.0, Some(&governor))
                        .unwrap();
                }
                p.insert(raster.clone());
                let mut knobs = default_knobs();
                knobs.insert("alpha", KnobValue::Text(source.into()));
                let mut ctx = EvalContext::new();
                ctx.insert(governor.clone());
                let output = UnpremultNode::from_knobs(knobs)
                    .evaluate(&ctx, &[p])
                    .unwrap();
                let output = output.get::<Raster2D>().unwrap();
                let factor = if dense { 2.0 } else { 42.0 };
                let expected = [0.25 / factor, 0.5 / factor, 1.0 / factor, 0.5];
                assert_eq!(output.sample_rgba(3, 4), expected);
                for c in [Channel::DepthZ, Channel::RgbaAlpha] {
                    assert!(Arc::ptr_eq(
                        raster
                            .channel(&c)
                            .unwrap()
                            .get(&TileCoord::new(0, 0))
                            .unwrap(),
                        output
                            .channel(&c)
                            .unwrap()
                            .get(&TileCoord::new(0, 0))
                            .unwrap()
                    ));
                }
                if !dense {
                    assert_eq!(governor.allocated_bytes(), 0);
                }
            }
        }
    }
}

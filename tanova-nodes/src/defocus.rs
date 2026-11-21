//! `Defocus`: optical circular-disk blur with an optional mask and mix.
//!
//! The pixel convolution lives in `tanova-render` (`defocus_raster` with an
//! anti-aliased elliptical disk kernel); this file only maps the `defocus`,
//! `aspect`, and `mix` knobs to a `DefocusOp`, pulls the optional mask on
//! `PortId(1)`, and streams padded RGBA tiles through the engine. Non-RGBA AOV
//! planes pass through by `Arc` clone.

use std::sync::Arc;

use tanova_kernel::{
    EvalContext, KnobTable, KnobValue, NodeCartridgeDescriptor, NodeError, NodeTrait, Packet,
};
use tanova_render::{DefocusOp, Raster2D, defocus_raster};

use crate::util::{float_knob, format_float, raster_input};

/// Default knob values for a fresh [`DefocusNode`].
pub fn default_knobs() -> KnobTable {
    let mut knobs = KnobTable::with_capacity(3);
    knobs.insert("defocus", KnobValue::Float(4.0));
    knobs.insert("aspect", KnobValue::Float(1.0));
    knobs.insert("mix", KnobValue::Float(1.0));
    knobs
}

/// A Nuke-style `Defocus` cartridge: circular disk blur with optional mask.
#[derive(Debug, Clone)]
pub struct DefocusNode {
    knobs: KnobTable,
}

impl Default for DefocusNode {
    fn default() -> Self {
        Self {
            knobs: default_knobs(),
        }
    }
}

impl DefocusNode {
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

impl NodeTrait for DefocusNode {
    fn evaluate(&self, ctx: &EvalContext, inputs: &[Packet]) -> Result<Packet, NodeError> {
        let input = raster_input(inputs, 0).ok_or(NodeError::MissingInput("main"))?;
        let mask = inputs.get(1).and_then(|packet| packet.get::<Raster2D>());
        let knobs = ctx.knobs().unwrap_or(&self.knobs);

        let defocus = float_knob(knobs, "defocus", 4.0);
        let aspect = float_knob(knobs, "aspect", 1.0);
        let mix = float_knob(knobs, "mix", 1.0);

        // Guard against non-finite knob values before passing to the raster.
        if !defocus.is_finite() || !aspect.is_finite() || !mix.is_finite() {
            let mut packet = Packet::new();
            packet.insert(input.clone());
            return Ok(packet);
        }

        let op = DefocusOp {
            radius: defocus,
            aspect,
            mix,
        };

        let governor = ctx.governor();
        let output = defocus_raster(input, mask, &op, governor)?;

        let mut packet = Packet::new();
        packet.insert(output);
        Ok(packet)
    }

    fn serialize_to_tcl(&self) -> String {
        let defocus = float_knob(&self.knobs, "defocus", 4.0);
        let aspect = float_knob(&self.knobs, "aspect", 1.0);
        let mix = float_knob(&self.knobs, "mix", 1.0);
        format!(
            "Defocus {{\n defocus {}\n aspect {}\n mix {}\n}}",
            format_float(defocus),
            format_float(aspect),
            format_float(mix)
        )
    }
}

fn factory() -> Arc<dyn NodeTrait> {
    Arc::new(DefocusNode::new())
}

inventory::submit! {
    NodeCartridgeDescriptor {
        kind: "Defocus",
        default_knobs,
        factory,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tanova_kernel::KnobTable;
    use tanova_render::{BBox, Channel, ChannelPlane, EdgeMode, Format, TileBuffer, TileCoord};

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

    #[test]
    fn defocus_cartridge_blurs_and_respects_mask_and_aovs() {
        // Build a sharp impulse pixel at the center of a 16x16 image.
        let mut input = Raster2D::new(
            Format::new("test", 16, 16, 1.0),
            BBox::new(0, 0, 16, 16),
            EdgeMode::BlackOutside,
        );
        for channel in [
            Channel::RgbaRed,
            Channel::RgbaGreen,
            Channel::RgbaBlue,
            Channel::RgbaAlpha,
        ] {
            let mut plane = ChannelPlane::new();
            plane.insert(TileCoord::new(0, 0), Arc::new(TileBuffer::new_uniform(0.0)));
            input.insert_channel(channel, plane);
        }
        input.set_pixel(Channel::RgbaRed, 8, 8, 1.0, None).unwrap();
        input
            .set_pixel(Channel::RgbaAlpha, 8, 8, 1.0, None)
            .unwrap();

        // DepthZ AOV must pass through untouched.
        let mut depth = ChannelPlane::new();
        let depth_tile = Arc::new(TileBuffer::new_uniform(42.0));
        depth.insert(TileCoord::new(0, 0), depth_tile.clone());
        input.insert_channel(Channel::DepthZ, depth);

        let mut packet = Packet::new();
        packet.insert(input.clone());

        let mut knobs = KnobTable::with_capacity(3);
        knobs.insert("defocus", KnobValue::Float(3.0));
        knobs.insert("aspect", KnobValue::Float(1.0));
        knobs.insert("mix", KnobValue::Float(1.0));
        let mut ctx = EvalContext::new();
        ctx.insert(knobs);

        let node = DefocusNode::new();
        let output = node
            .evaluate(&ctx, &[packet.clone()])
            .expect("defocus should evaluate");
        let raster = output.get::<Raster2D>().expect("raster");

        // The center pixel loses energy (blur spreads it out).
        let center = raster.sample_rgba(8, 8);
        assert!(
            center[0] > 0.0,
            "the impulse must keep some energy at center: {center:?}"
        );
        assert!(
            center[0] < 1.0,
            "the impulse must lose energy to the blur: {center:?}"
        );

        // Energy bleeds into neighbours (circular disk).
        let neighbour = raster.sample_rgba(9, 8);
        assert!(
            neighbour[0] > 0.0 && neighbour[0] <= center[0] + 1e-6,
            "the impulse must bleed into the neighbour: {neighbour:?}"
        );
        assert!(
            raster.sample_rgba(11, 8)[0] > 0.0,
            "disk reaches along the axis"
        );
        assert!(raster.sample_rgba(8, 11)[0] > 0.0);
        assert_eq!(
            raster.sample_rgba(11, 11)[0],
            0.0,
            "disk excludes square-kernel corners"
        );
        assert!((raster.sample_rgba(10, 8)[0] - raster.sample_rgba(8, 10)[0]).abs() < 1e-6);

        // DepthZ AOV passes through untouched.
        let depth_after = raster
            .channel(&Channel::DepthZ)
            .and_then(|plane| plane.get(&TileCoord::new(0, 0)))
            .expect("depth tile");
        assert_eq!(depth_after.is_uniform().unwrap_or(0.0), 42.0);
        assert!(Arc::ptr_eq(&depth_tile, depth_after));

        // Alpha is zero on the left half and one on the right half. Red
        // stays one everywhere to verify alpha takes precedence for masking.
        let mut mask = uniform_raster(16, 16, 1.0, EdgeMode::BlackOutside);
        for y in 0..16 {
            for x in 0..8 {
                mask.set_pixel(Channel::RgbaAlpha, x, y, 0.0, None).unwrap();
            }
        }
        let mut mask_packet = Packet::new();
        mask_packet.insert(mask);
        let masked = node.evaluate(&ctx, &[packet, mask_packet]).unwrap();
        let masked = masked.get::<Raster2D>().unwrap();
        assert!(raster.sample_rgba(7, 8)[0] > 0.0);
        assert_eq!(
            masked.sample_rgba(7, 8),
            input.sample_rgba(7, 8),
            "zero mask preserves input"
        );
        assert_eq!(
            masked.sample_rgba(8, 8),
            center,
            "opaque mask applies disk blur"
        );
        assert!(Arc::ptr_eq(
            &depth_tile,
            masked
                .channel(&Channel::DepthZ)
                .unwrap()
                .get(&TileCoord::new(0, 0))
                .unwrap()
        ));

        // Output bbox expands by the radius.
        assert!(
            raster.bbox.width() >= 8,
            "output bbox must expand for blur: {:?}",
            raster.bbox
        );
    }

    #[test]
    fn defocus_zero_radius_is_bypass() {
        let input = uniform_raster(8, 8, 0.5, EdgeMode::BlackOutside);
        let mut packet = Packet::new();
        packet.insert(input.clone());

        let mut knobs = KnobTable::with_capacity(3);
        knobs.insert("defocus", KnobValue::Float(0.0));
        knobs.insert("aspect", KnobValue::Float(1.0));
        knobs.insert("mix", KnobValue::Float(1.0));
        let mut ctx = EvalContext::new();
        ctx.insert(knobs);

        let node = DefocusNode::new();
        let output = node
            .evaluate(&ctx, &[packet])
            .expect("zero-radius should bypass");
        let output_raster = output.get::<Raster2D>().expect("raster");
        assert_eq!(
            output_raster.sample_rgba(4, 4),
            [0.5, 0.5, 0.5, 0.5],
            "zero-radius defocus must pass through unchanged"
        );
    }

    #[test]
    fn serialization_writes_the_knobs() {
        let text = DefocusNode::new().serialize_to_tcl();
        assert!(text.starts_with("Defocus {"), "got {text}");
        assert!(text.contains("defocus"), "got {text}");
        assert!(text.contains("aspect"), "got {text}");
        assert!(text.contains("mix"), "got {text}");
    }
}

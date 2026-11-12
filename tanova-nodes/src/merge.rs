//! `Merge2`: Nuke's B/A compositing node.
//!
//! Port order is strict: port 0 is `B` (background spine), port 1 is `A`
//! (foreground), port 2 is an optional mask. Non-RGBA AOV planes are inherited
//! from `B` by `Arc` clone.

use std::sync::Arc;

use tanova_kernel::{
    EvalContext, KnobTable, KnobValue, NodeCartridgeDescriptor, NodeError, NodeTrait, Packet,
};
use tanova_render::{EvalContextGpu, MergeOp, MergeRequest, merge_raster};

use crate::util::{float_knob, format_float, raster_input, text_knob};

/// Default operation name.
pub const DEFAULT_OPERATION: &str = "over";

/// Default data-window rule.
pub const DEFAULT_BBOX: &str = "union";

/// Default blend amount.
pub const DEFAULT_MIX: f32 = 1.0;

/// Default knob values for a fresh [`Merge2Node`].
pub fn default_knobs() -> KnobTable {
    let mut knobs = KnobTable::with_capacity(3);
    knobs.insert("operation", KnobValue::Text(DEFAULT_OPERATION.to_string()));
    knobs.insert("bbox", KnobValue::Text(DEFAULT_BBOX.to_string()));
    knobs.insert("mix", KnobValue::Float(DEFAULT_MIX as f64));
    knobs
}

/// A Nuke-style B/A merge cartridge.
#[derive(Debug, Clone)]
pub struct Merge2Node {
    knobs: KnobTable,
}

impl Default for Merge2Node {
    fn default() -> Self {
        Self {
            knobs: default_knobs(),
        }
    }
}

impl Merge2Node {
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

impl NodeTrait for Merge2Node {
    fn evaluate(&self, ctx: &EvalContext, inputs: &[Packet]) -> Result<Packet, NodeError> {
        let background = raster_input(inputs, 0).ok_or(NodeError::MissingInput("B"))?;
        let foreground = raster_input(inputs, 1).ok_or(NodeError::MissingInput("A"))?;
        let mask = raster_input(inputs, 2);
        let knobs = ctx.knobs().unwrap_or(&self.knobs);

        let operation = MergeOp::from_name(&text_knob(knobs, "operation", DEFAULT_OPERATION))
            .unwrap_or(MergeOp::Over);
        let output_bbox = match text_knob(knobs, "bbox", DEFAULT_BBOX)
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "b" => background.bbox,
            "a" => foreground.bbox,
            "intersection" => background.bbox.intersection(&foreground.bbox),
            _ => background.bbox.union(&foreground.bbox),
        };
        let mix = float_knob(knobs, "mix", DEFAULT_MIX);

        let raster = merge_raster(
            MergeRequest {
                background,
                foreground,
                mask,
                operation,
                output_bbox,
                mix,
            },
            ctx.governor(),
            ctx.gpu(),
        )?;

        let mut packet = Packet::new();
        packet.insert(raster);
        Ok(packet)
    }

    fn serialize_to_tcl(&self) -> String {
        let operation = text_knob(&self.knobs, "operation", DEFAULT_OPERATION);
        let bbox = text_knob(&self.knobs, "bbox", DEFAULT_BBOX);
        format!(
            "Merge2 {{\n operation \"{}\"\n bbox \"{}\"\n mix {}\n}}",
            operation,
            bbox,
            format_float(float_knob(&self.knobs, "mix", DEFAULT_MIX))
        )
    }
}

fn factory() -> Arc<dyn NodeTrait> {
    Arc::new(Merge2Node::new())
}

inventory::submit! {
    NodeCartridgeDescriptor {
        kind: "Merge2",
        default_knobs,
        factory,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tanova_render::{
        BBox, Channel, ChannelPlane, Format, Raster2D, TILE_SIZE, TileBuffer, TileCoord,
    };

    fn raster(color: [f32; 4], depth: Option<f32>) -> Raster2D {
        let bbox = BBox::new(0, 0, TILE_SIZE as i32, TILE_SIZE as i32);
        let mut raster =
            Raster2D::new(Format::new("tile", 256, 256, 1.0), bbox, Default::default());
        for (channel, value) in [
            (Channel::RgbaRed, color[0]),
            (Channel::RgbaGreen, color[1]),
            (Channel::RgbaBlue, color[2]),
            (Channel::RgbaAlpha, color[3]),
        ] {
            let mut plane = ChannelPlane::new();
            plane.insert(
                TileCoord::new(0, 0),
                Arc::new(TileBuffer::new_uniform(value)),
            );
            raster.insert_channel(channel, plane);
        }
        if let Some(depth) = depth {
            let mut plane = ChannelPlane::new();
            plane.insert(
                TileCoord::new(0, 0),
                Arc::new(TileBuffer::new_uniform(depth)),
            );
            raster.insert_channel(Channel::DepthZ, plane);
        }
        raster
    }

    fn packet(raster: Raster2D) -> Packet {
        let mut packet = Packet::new();
        packet.insert(raster);
        packet
    }

    #[test]
    fn over_merges_a_onto_b_and_keeps_b_depth() {
        let background = raster([0.0, 0.0, 0.0, 1.0], Some(12.0));
        let foreground = raster([0.5, 0.25, 0.0, 1.0], None);

        let output = Merge2Node::new()
            .evaluate(
                &EvalContext::new(),
                &[packet(background), packet(foreground)],
            )
            .expect("merge should evaluate");
        let raster = output.get::<Raster2D>().expect("raster");

        assert_eq!(raster.sample(&Channel::RgbaRed, 0, 0), 0.5);
        assert_eq!(raster.sample(&Channel::RgbaAlpha, 0, 0), 1.0);
        assert_eq!(raster.sample(&Channel::DepthZ, 0, 0), 12.0);
    }

    #[test]
    fn missing_b_or_a_is_reported() {
        assert!(matches!(
            Merge2Node::new().evaluate(&EvalContext::new(), &[]),
            Err(NodeError::MissingInput("B"))
        ));

        let background = raster([0.0; 4], None);
        assert!(matches!(
            Merge2Node::new().evaluate(&EvalContext::new(), &[packet(background)]),
            Err(NodeError::MissingInput("A"))
        ));
    }

    #[test]
    fn serialization_lists_the_nuke_knobs() {
        let text = Merge2Node::new().serialize_to_tcl();
        assert_eq!(
            text,
            "Merge2 {\n operation \"over\"\n bbox \"union\"\n mix 1.0\n}"
        );
    }
}

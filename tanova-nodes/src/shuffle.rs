//! Routes image channels by sharing the input tile buffers.

use std::sync::Arc;

use crate::util::raster_input;
use tanova_kernel::{
    EvalContext, KnobTable, KnobValue, NodeCartridgeDescriptor, NodeError, NodeTrait, Packet,
};
use tanova_render::{Channel, ChannelPlane, Raster2D, TileBuffer};

const OUTPUTS: [(&str, Channel); 4] = [
    ("red", Channel::RgbaRed),
    ("green", Channel::RgbaGreen),
    ("blue", Channel::RgbaBlue),
    ("alpha", Channel::RgbaAlpha),
];

pub fn default_knobs() -> KnobTable {
    let mut knobs = KnobTable::with_capacity(4);
    for (name, _) in OUTPUTS {
        knobs.insert(name, KnobValue::Text(name.into()));
    }
    knobs
}

#[derive(Debug, Clone)]
pub struct ShuffleNode {
    knobs: KnobTable,
}

impl Default for ShuffleNode {
    fn default() -> Self {
        Self::from_knobs(default_knobs())
    }
}

impl ShuffleNode {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_knobs(knobs: KnobTable) -> Self {
        Self { knobs }
    }
}

enum Selector {
    Channel(Channel),
    Black,
    White,
}

impl Selector {
    fn parse(text: &str) -> Self {
        Self::Channel(match text {
            "red" | "r" => Channel::RgbaRed,
            "green" | "g" => Channel::RgbaGreen,
            "blue" | "b" => Channel::RgbaBlue,
            "alpha" | "a" => Channel::RgbaAlpha,
            "depth.Z" | "Z" => Channel::DepthZ,
            "white" | "1" => return Self::White,
            _ => return Self::Black,
        })
    }

    fn plane(&self, input: &Raster2D) -> ChannelPlane {
        if let Self::Channel(channel) = self
            && let Some(plane) = input.channel(channel)
        {
            return plane.clone();
        }

        let value = if matches!(self, Self::White) {
            1.0
        } else {
            0.0
        };
        let buffer = Arc::new(TileBuffer::new_uniform(value));
        let mut plane = ChannelPlane::new();
        for tile in input.bbox.tile_coords() {
            plane.insert(tile, Arc::clone(&buffer));
        }
        plane
    }
}

fn selector_text<'a>(knobs: &'a KnobTable, name: &'static str) -> &'a str {
    match knobs.get(name) {
        Some(KnobValue::Text(value)) => value,
        _ => name,
    }
}

struct ShuffleOp {
    selectors: [Selector; 4],
}

impl ShuffleOp {
    fn from_knobs(knobs: &KnobTable) -> Self {
        Self {
            selectors: OUTPUTS.map(|(name, _)| Selector::parse(selector_text(knobs, name))),
        }
    }
}

impl NodeTrait for ShuffleNode {
    fn evaluate(&self, ctx: &EvalContext, inputs: &[Packet]) -> Result<Packet, NodeError> {
        let input = raster_input(inputs, 0).ok_or(NodeError::MissingInput("main"))?;
        let op = ShuffleOp::from_knobs(ctx.knobs().unwrap_or(&self.knobs));
        let mut output = input.clone();
        for ((_, channel), selector) in OUTPUTS.into_iter().zip(op.selectors) {
            output.insert_channel(channel, selector.plane(input));
        }
        let mut packet = Packet::new();
        packet.insert(output);
        Ok(packet)
    }

    fn serialize_to_tcl(&self) -> String {
        format!(
            "Shuffle {{\n red {}\n green {}\n blue {}\n alpha {}\n}}",
            selector_text(&self.knobs, "red"),
            selector_text(&self.knobs, "green"),
            selector_text(&self.knobs, "blue"),
            selector_text(&self.knobs, "alpha"),
        )
    }
}

inventory::submit! {
    NodeCartridgeDescriptor { kind: "Shuffle", default_knobs, factory: || Arc::new(ShuffleNode::new()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tanova_kernel::MemoryGovernor;
    use tanova_render::{Format, TILE_PIXELS, TileCoord};

    #[test]
    fn routes_dense_depth_to_rgb_without_allocating_dense_tiles() {
        let tile = TileCoord::new(0, 0);
        let depth = Arc::new(TileBuffer::from_vec(vec![42.0; TILE_PIXELS], None).unwrap());
        let mut plane = ChannelPlane::new();
        plane.insert(tile, Arc::clone(&depth));
        let mut raster = Raster2D::for_format(Format::new("test", 256, 256, 1.0));
        let custom = Channel::Custom("motion.x".into());
        raster.insert_channel(Channel::DepthZ, plane.clone());
        raster.insert_channel(custom.clone(), plane);
        let mut input = Packet::new();
        input.insert(raster);

        let governor = MemoryGovernor::new(0);
        let mut ctx = EvalContext::new();
        ctx.insert(governor.clone());
        let mut knobs = default_knobs();
        for (name, selector) in [
            ("red", "Z"),
            ("green", "depth.Z"),
            ("blue", "Z"),
            ("alpha", "1"),
        ] {
            knobs.insert(name, KnobValue::Text(selector.into()));
        }
        let output = ShuffleNode::from_knobs(knobs)
            .evaluate(&ctx, &[input])
            .unwrap();
        let raster = output.get::<Raster2D>().unwrap();
        for channel in [
            Channel::RgbaRed,
            Channel::RgbaGreen,
            Channel::RgbaBlue,
            Channel::DepthZ,
            custom,
        ] {
            assert!(Arc::ptr_eq(
                &depth,
                raster.channel(&channel).unwrap().get(&tile).unwrap()
            ));
        }
        assert_eq!(raster.sample_rgba(0, 0), [42.0, 42.0, 42.0, 1.0]);
        assert_eq!(governor.allocated_bytes(), 0);
    }

    #[test]
    fn missing_and_black_selectors_create_uniform_zero() {
        let input = Raster2D::for_format(tanova_render::Format::new("test", 257, 1, 1.0));
        for selector in [
            Selector::parse("a"),
            Selector::parse("0"),
            Selector::parse("black"),
        ] {
            let plane = selector.plane(&input);
            assert_eq!(plane.len(), 2);
            assert!(
                plane
                    .iter()
                    .all(|(_, buffer)| buffer.is_uniform() == Some(0.0))
            );
        }
    }
}

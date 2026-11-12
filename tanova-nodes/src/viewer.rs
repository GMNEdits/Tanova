//! `Viewer`: Nuke's physical viewer node.
//!
//! The viewer cartridge owns no knobs and never changes pixels. It is a pure
//! passthrough so a `.nk` script's `Viewer` block survives a load/save round
//! trip and can be used as the render target. The transient UI viewer remains
//! a separate display concept; this node is the graph-visible anchor.

use std::sync::Arc;

use tanova_kernel::{
    EvalContext, KnobTable, NodeCartridgeDescriptor, NodeError, NodeTrait, Packet,
};

/// Default knobs for a fresh [`ViewerNode`] (none).
pub fn default_knobs() -> KnobTable {
    KnobTable::new()
}

/// A zero-copy passthrough cartridge.
#[derive(Debug, Clone, Copy, Default)]
pub struct ViewerNode;

impl ViewerNode {
    /// Creates the viewer cartridge.
    pub fn new() -> Self {
        Self
    }
}

impl NodeTrait for ViewerNode {
    fn evaluate(&self, _ctx: &EvalContext, inputs: &[Packet]) -> Result<Packet, NodeError> {
        // `Packet` is a cheap type map: cloning it copies the map, while pixel
        // payloads stay behind their `Arc`s.
        Ok(inputs.first().cloned().unwrap_or_default())
    }

    fn serialize_to_tcl(&self) -> String {
        "Viewer {}".to_string()
    }
}

fn factory() -> Arc<dyn NodeTrait> {
    Arc::new(ViewerNode)
}

inventory::submit! {
    NodeCartridgeDescriptor {
        kind: "Viewer",
        default_knobs,
        factory,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tanova_render::{BBox, Format, Raster2D};

    #[test]
    fn passes_the_first_input_through() {
        let raster = Raster2D::new(
            Format::new("viewer", 4, 4, 1.0),
            BBox::new(0, 0, 4, 4),
            Default::default(),
        );
        let mut input = Packet::new();
        input.insert(raster.clone());

        let output = ViewerNode::new()
            .evaluate(&EvalContext::new(), &[input])
            .expect("viewer should evaluate");
        let forwarded = output.get::<Raster2D>().expect("raster passthrough");
        assert_eq!(forwarded.format, raster.format);
        assert_eq!(forwarded.bbox, raster.bbox);
    }

    #[test]
    fn empty_input_yields_an_empty_packet() {
        let output = ViewerNode::new()
            .evaluate(&EvalContext::new(), &[])
            .expect("viewer should evaluate");
        assert!(output.is_empty());
    }

    #[test]
    fn has_no_knobs() {
        assert!(default_knobs().is_empty());
        assert_eq!(ViewerNode.serialize_to_tcl(), "Viewer {}");
    }

    #[test]
    fn viewer_is_registered() {
        let descriptor =
            tanova_kernel::find_cartridge("Viewer").expect("Viewer must be registered");
        assert!((descriptor.default_knobs)().is_empty());
        let implementation = (descriptor.factory)();
        let output = implementation
            .evaluate(&EvalContext::new(), &[])
            .expect("viewer factory should evaluate");
        assert!(output.is_empty());
    }
}

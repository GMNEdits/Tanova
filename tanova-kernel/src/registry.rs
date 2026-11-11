//! Compile-time cartridge registry.
//!
//! Cartridges register themselves with `inventory::submit!`; the kernel only
//! ever looks descriptors up by `kind`, so adding a node never touches kernel
//! code.

use std::sync::Arc;

use crate::knobs::KnobTable;
use crate::types::NodeTrait;

/// Static description of one cartridge, submitted at compile time.
pub struct NodeCartridgeDescriptor {
    /// Registry name the DAG uses (e.g. `"Dummy"`).
    pub kind: &'static str,
    /// Builds the node's initial [`KnobTable`].
    pub default_knobs: fn() -> KnobTable,
    /// Creates the cartridge.
    pub factory: fn() -> Arc<dyn NodeTrait>,
}

inventory::collect!(NodeCartridgeDescriptor);

/// Looks up a registered cartridge by kind.
pub fn find_cartridge(kind: &str) -> Option<&'static NodeCartridgeDescriptor> {
    inventory::iter::<NodeCartridgeDescriptor>
        .into_iter()
        .find(|descriptor| descriptor.kind == kind)
}

/// Lists every registered cartridge kind, sorted for stable UI ordering.
pub fn registered_cartridge_kinds() -> Vec<&'static str> {
    let mut kinds: Vec<&'static str> = inventory::iter::<NodeCartridgeDescriptor>
        .into_iter()
        .map(|descriptor| descriptor.kind)
        .collect();
    kinds.sort_unstable();
    kinds
}

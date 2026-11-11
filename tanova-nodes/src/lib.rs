//! Tanova node cartridges.
//!
//! Each node lives in exactly one file (e.g. `src/blur.rs`) and registers
//! itself with the kernel's global registry via `inventory`.

pub mod dummy;

pub use dummy::{DEFAULT_MESSAGE, DummyNode, DummyPayload};

/// Forces this crate, and therefore its `inventory` constructors, to be linked.
///
/// Binaries and integration tests call this before using the registry so the
/// linker cannot dead-strip the cartridge submissions.
pub fn ensure_linked() {}

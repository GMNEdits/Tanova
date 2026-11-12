//! Tanova node cartridges.
//!
//! Each node lives in exactly one file (e.g. `src/blur.rs`) and registers
//! itself with the kernel's global registry via `inventory`.

pub mod constant;
pub mod dummy;
pub mod grade;
pub mod merge;
pub mod util;

pub use constant::{ConstantNode, DEFAULT_COLOR};
pub use dummy::{DEFAULT_MESSAGE, DummyNode, DummyPayload};
pub use grade::{GradeNode, grade_op};
pub use merge::Merge2Node;

/// Forces this crate, and therefore its `inventory` constructors, to be linked.
///
/// Binaries and integration tests call this before using the registry so the
/// linker cannot dead-strip the cartridge submissions.
pub fn ensure_linked() {}

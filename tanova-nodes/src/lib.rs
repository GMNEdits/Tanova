//! Tanova node cartridges.
//!
//! Each node lives in exactly one file (e.g. `src/blur.rs`) and registers
//! itself with the kernel's global registry via `inventory`.

pub mod blur;
pub mod constant;
pub mod dummy;
pub mod grade;
pub mod merge;
pub mod read;
pub mod transform;
pub mod util;
pub mod viewer;
pub mod write;

pub use blur::BlurNode;
pub use constant::{ConstantNode, DEFAULT_COLOR};
pub use dummy::{DEFAULT_MESSAGE, DummyNode, DummyPayload};
pub use grade::{GradeNode, grade_op};
pub use merge::Merge2Node;
pub use read::ReadNode;
pub use transform::TransformNode;
pub use viewer::ViewerNode;
pub use write::{WriteNode, default_knobs as write_default_knobs, export_packet_with_write_knobs};

/// Forces this crate, and therefore its `inventory` constructors, to be linked.
///
/// Binaries and integration tests call this before using the registry so the
/// linker cannot dead-strip the cartridge submissions.
pub fn ensure_linked() {}

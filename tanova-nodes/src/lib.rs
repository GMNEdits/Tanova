//! Tanova node cartridges.
//!
//! Each node lives in exactly one file (e.g. `src/blur.rs`) and registers
//! itself with the kernel's global registry via `inventory`.

pub mod backdrop;
pub mod blur;
pub mod checkerboard;
pub mod clamp;
pub mod color_correct;
pub mod constant;
pub mod crop;
pub mod dot;
pub mod dummy;
pub mod frame_hold;
pub mod grade;
pub mod invert;
pub mod merge;
pub mod premult;
pub mod read;
pub mod reformat;
pub mod saturation;
pub mod shuffle;
pub mod switch;
pub mod time_offset;
pub mod transform;
pub mod unpremult;
pub mod util;
pub mod viewer;
pub mod write;

pub use backdrop::BackdropNode;
pub use blur::BlurNode;
pub use checkerboard::CheckerBoardNode;
pub use clamp::ClampNode;
pub use color_correct::ColorCorrectNode;
pub use constant::{ConstantNode, DEFAULT_COLOR};
pub use crop::CropNode;
pub use dot::DotNode;
pub use dummy::{DEFAULT_MESSAGE, DummyNode, DummyPayload};
pub use frame_hold::FrameHoldNode;
pub use grade::{GradeNode, grade_op};
pub use invert::InvertNode;
pub use merge::Merge2Node;
pub use premult::PremultNode;
pub use read::ReadNode;
pub use reformat::ReformatNode;
pub use saturation::SaturationNode;
pub use shuffle::ShuffleNode;
pub use switch::SwitchNode;
pub use time_offset::TimeOffsetNode;
pub use transform::TransformNode;
pub use unpremult::UnpremultNode;
pub use viewer::ViewerNode;
pub use write::{WriteNode, default_knobs as write_default_knobs, export_packet_with_write_knobs};

/// Forces this crate, and therefore its `inventory` constructors, to be linked.
///
/// Binaries and integration tests call this before using the registry so the
/// linker cannot dead-strip the cartridge submissions.
pub fn ensure_linked() {}

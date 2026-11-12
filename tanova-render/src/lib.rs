//! Tanova GPU rendering.
//!
//! Renders to off-screen textures on the kernel thread, using tile aprons to
//! avoid grid seams and GPU-side color transforms (ACES / Rec.709).
//!
//! Spatial data is Bottom-Left `(0, 0)` with `+Y` up, 32-bit linear float, and
//! a premultiplied-alpha pipeline. The only conversion to Top-Left bytes lives
//! in [`Raster2D::to_top_left_rgba8`].

pub mod coords;
pub mod ops;
pub mod shader;
pub mod tile;

pub use coords::{
    BBox, EdgeMode, Format, RegionOfInterest, TILE_PIXELS, TILE_SIZE, TileCoord, TileRange,
    world_to_tile,
};
pub use ops::{
    GradeOp, MergeOp, MergeRequest, grade_raster, mask_value, merge_pixel, merge_raster, rgba_at,
};
pub use shader::{
    DISPATCH_SIZE, EvalContextGpu, GpuComputeEngine, GradeParams, MergeParams, WORKGROUP_SIZE,
};
pub use tile::{Channel, ChannelPlane, Raster2D, TILE_BYTES, TileBuffer, extract_padded_tile};

//! Governed planar tiles, channel planes, and apron stitching.
//!
//! A [`TileBuffer`] is either a collapsed constant (cheap solid/empty tiles) or
//! a dense 256x256 `f32` allocation that holds a live [`MemoryPermit`]. The
//! permit travels with the buffer into the RAM cache and returns its bytes the
//! moment the last `Arc<TileBuffer>` is dropped.

use std::collections::HashMap;
use std::sync::Arc;

use indexmap::IndexMap;
use tanova_kernel::{MemoryGovernor, MemoryPermit, NodeError};

use crate::coords::{BBox, EdgeMode, Format, TILE_PIXELS, TILE_SIZE, TileCoord, world_to_tile};

/// Bytes one dense tile reserves.
pub const TILE_BYTES: usize = TILE_PIXELS * std::mem::size_of::<f32>();

/// Pixels per tile row, as a `usize` index stride.
const TILE_STRIDE: usize = TILE_SIZE as usize;

/// A named data plane carried by a [`Raster2D`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Channel {
    /// Red.
    RgbaRed,
    /// Green.
    RgbaGreen,
    /// Blue.
    RgbaBlue,
    /// Alpha (premultiplied pipeline).
    RgbaAlpha,
    /// Depth.
    DepthZ,
    /// Any cartridge-defined plane.
    Custom(String),
}

/// The backing storage of one tile.
#[derive(Debug)]
enum TileStorage {
    /// One constant value for every pixel; no allocation.
    Uniform(f32),
    /// A dense tile plus the reservation keeping it inside the governor's cap.
    Dense {
        data: Vec<f32>,
        permit: Option<MemoryPermit>,
    },
}

/// One 256x256 tile of a single channel.
#[derive(Debug)]
pub struct TileBuffer {
    storage: TileStorage,
}

impl TileBuffer {
    /// Creates a collapsed constant tile. Reserves no memory.
    pub fn new_uniform(value: f32) -> Self {
        Self {
            storage: TileStorage::Uniform(value),
        }
    }

    /// Creates a dense tile filled with `fill`, reserving [`TILE_BYTES`].
    pub fn new_dense(fill: f32, governor: Option<&MemoryGovernor>) -> Result<Self, NodeError> {
        let permit = request_tile_permit(governor)?;
        Ok(Self {
            storage: TileStorage::Dense {
                data: vec![fill; TILE_PIXELS],
                permit,
            },
        })
    }

    /// Wraps an existing dense tile, reserving [`TILE_BYTES`].
    pub fn from_vec(data: Vec<f32>, governor: Option<&MemoryGovernor>) -> Result<Self, NodeError> {
        if data.len() != TILE_PIXELS {
            return Err(NodeError::Other(format!(
                "tile buffer needs {TILE_PIXELS} pixels, got {}",
                data.len()
            )));
        }
        let permit = request_tile_permit(governor)?;
        Ok(Self {
            storage: TileStorage::Dense { data, permit },
        })
    }

    /// Returns the constant value if the tile is collapsed.
    pub fn is_uniform(&self) -> Option<f32> {
        match &self.storage {
            TileStorage::Uniform(value) => Some(*value),
            TileStorage::Dense { .. } => None,
        }
    }

    /// Returns `true` if the tile owns a dense allocation.
    pub fn is_dense(&self) -> bool {
        matches!(self.storage, TileStorage::Dense { .. })
    }

    /// Bytes this tile currently reserves from the governor.
    ///
    /// Uniform tiles and dense tiles built without a governor reserve nothing.
    pub fn governed_bytes(&self) -> usize {
        match &self.storage {
            TileStorage::Uniform(_) => 0,
            TileStorage::Dense { permit, .. } => permit.as_ref().map_or(0, MemoryPermit::bytes),
        }
    }

    /// Reads one local pixel (`lx` right, `ly` up).
    ///
    /// Out-of-range reads return `0.0`.
    pub fn get(&self, lx: u32, ly: u32) -> f32 {
        match &self.storage {
            TileStorage::Uniform(value) => *value,
            TileStorage::Dense { data, .. } => {
                data.get(local_index(lx, ly)).copied().unwrap_or(0.0)
            }
        }
    }

    /// Writes one local pixel, promoting a uniform tile to dense when needed.
    pub fn set(
        &mut self,
        lx: u32,
        ly: u32,
        value: f32,
        governor: Option<&MemoryGovernor>,
    ) -> Result<(), NodeError> {
        if lx >= TILE_SIZE || ly >= TILE_SIZE {
            return Err(NodeError::Other(format!(
                "tile local coordinate ({lx}, {ly}) is outside {TILE_SIZE}x{TILE_SIZE}"
            )));
        }

        let uniform = match &self.storage {
            TileStorage::Uniform(current) => Some(*current),
            TileStorage::Dense { .. } => None,
        };

        if let Some(current) = uniform {
            if current == value {
                return Ok(());
            }
            let permit = request_tile_permit(governor)?;
            let mut data = vec![current; TILE_PIXELS];
            data[local_index(lx, ly)] = value;
            self.storage = TileStorage::Dense { data, permit };
            return Ok(());
        }

        if let TileStorage::Dense { data, .. } = &mut self.storage {
            data[local_index(lx, ly)] = value;
        }
        Ok(())
    }

    /// Dense pixel data, if the tile is not collapsed.
    fn dense_slice(&self) -> Option<&[f32]> {
        match &self.storage {
            TileStorage::Dense { data, .. } => Some(data),
            TileStorage::Uniform(_) => None,
        }
    }

    /// Read-only dense pixel data, if the tile is not collapsed.
    ///
    /// The slice is `TILE_SIZE * TILE_SIZE` values in Bottom-Left row-major
    /// order. Callers that walk a whole tile use this to avoid a `HashMap`
    /// lookup per pixel; uniform tiles keep returning `None` so they can be
    /// filled without allocating.
    pub fn as_dense_slice(&self) -> Option<&[f32]> {
        self.dense_slice()
    }
}

fn local_index(lx: u32, ly: u32) -> usize {
    ly as usize * TILE_STRIDE + lx as usize
}

fn request_tile_permit(
    governor: Option<&MemoryGovernor>,
) -> Result<Option<MemoryPermit>, NodeError> {
    match governor {
        Some(governor) => Ok(Some(governor.request_permit(TILE_BYTES)?)),
        None => Ok(None),
    }
}

/// One channel's tiles, keyed by tile coordinate.
#[derive(Debug, Clone, Default)]
pub struct ChannelPlane {
    tiles: HashMap<TileCoord, Arc<TileBuffer>>,
}

impl ChannelPlane {
    /// Creates an empty plane; absent tiles sample as `0.0`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts or replaces a tile.
    pub fn insert(&mut self, tile: TileCoord, buffer: Arc<TileBuffer>) -> Option<Arc<TileBuffer>> {
        self.tiles.insert(tile, buffer)
    }

    /// Borrows a tile.
    pub fn get(&self, tile: &TileCoord) -> Option<&Arc<TileBuffer>> {
        self.tiles.get(tile)
    }

    /// Iterates tiles in unspecified order.
    pub fn iter(&self) -> impl Iterator<Item = (&TileCoord, &Arc<TileBuffer>)> {
        self.tiles.iter()
    }

    /// Number of tiles.
    pub fn len(&self) -> usize {
        self.tiles.len()
    }

    /// Returns `true` if the plane holds no tiles.
    pub fn is_empty(&self) -> bool {
        self.tiles.is_empty()
    }

    /// Samples a world-space pixel, resolving edge behaviour through `bbox`.
    pub fn sample(&self, x: i32, y: i32, bbox: BBox, edge_mode: EdgeMode) -> f32 {
        let Some((resolved_x, resolved_y)) = edge_mode.resolve_coord(x, y, bbox) else {
            return 0.0;
        };
        let (tile, lx, ly) = world_to_tile(resolved_x, resolved_y);
        self.tiles
            .get(&tile)
            .map_or(0.0, |buffer| buffer.get(lx, ly))
    }

    /// Writes a world-space pixel, creating or promoting the owning tile.
    pub fn set(
        &mut self,
        x: i32,
        y: i32,
        value: f32,
        governor: Option<&MemoryGovernor>,
    ) -> Result<(), NodeError> {
        let (tile, lx, ly) = world_to_tile(x, y);

        if let Some(buffer) = self.tiles.get_mut(&tile) {
            if let Some(owned) = Arc::get_mut(buffer) {
                return owned.set(lx, ly, value, governor);
            }
            let mut copy = copy_buffer(buffer, governor)?;
            copy.set(lx, ly, value, governor)?;
            *buffer = Arc::new(copy);
            return Ok(());
        }

        let mut buffer = TileBuffer::new_uniform(0.0);
        buffer.set(lx, ly, value, governor)?;
        self.tiles.insert(tile, Arc::new(buffer));
        Ok(())
    }
}

fn copy_buffer(
    buffer: &TileBuffer,
    governor: Option<&MemoryGovernor>,
) -> Result<TileBuffer, NodeError> {
    match buffer.dense_slice() {
        Some(data) => TileBuffer::from_vec(data.to_vec(), governor),
        None => Ok(TileBuffer::new_uniform(buffer.get(0, 0))),
    }
}

/// A planar image: format, bounds, edge behaviour, and one plane per channel.
///
/// Cloning is cheap: planes clone their `Arc<TileBuffer>` handles, so an
/// untouched channel is shared, never copied.
#[derive(Debug, Clone)]
pub struct Raster2D {
    /// Output format.
    pub format: Format,
    /// Valid pixel bounds in Bottom-Left world space.
    pub bbox: BBox,
    /// How samples outside `bbox` behave.
    pub edge_mode: EdgeMode,
    /// Planes in insertion order.
    pub channels: IndexMap<Channel, ChannelPlane>,
}

/// Viewer display transform applied to linear RGB before byte conversion.
///
/// `Linear` is the default and passes linear RGB through untouched, which
/// keeps `to_top_left_rgba8` and `to_top_left_rgba8_with_display`
/// byte-identical to their historical output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DisplayTransform {
    /// Pass linear RGB through directly.
    #[default]
    Linear,
    /// IEC 61966-2-1 sRGB opto-electronic transfer function.
    Srgb,
    /// ITU-R BT.709 opto-electronic transfer function.
    Rec709,
    /// ACES filmic tone-mapping curve followed by sRGB encoding.
    AcesTonemap,
}

/// Viewer channel soloing applied after the display transform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ViewerChannel {
    /// Full `[r, g, b, a]` with raw alpha.
    #[default]
    Rgba,
    /// Display-transformed red channel as luminance, opaque alpha.
    Red,
    /// Display-transformed green channel as luminance, opaque alpha.
    Green,
    /// Display-transformed blue channel as luminance, opaque alpha.
    Blue,
    /// Raw alpha as luminance, opaque alpha (no gamma or curve on mattes).
    Alpha,
}

/// Applies `DisplayTransform` to one linear channel value.
fn apply_display_transform(value: f32, transform: DisplayTransform) -> f32 {
    match transform {
        DisplayTransform::Linear => value,
        DisplayTransform::Srgb => srgb_oetf(value),
        DisplayTransform::Rec709 => rec709_oetf(value),
        DisplayTransform::AcesTonemap => srgb_oetf(aces_filmic(value)),
    }
}

/// IEC 61966-2-1 sRGB opto-electronic transfer function (linear -> display).
fn srgb_oetf(c: f32) -> f32 {
    if c <= 0.0031308 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

/// ITU-R BT.709 opto-electronic transfer function (linear -> display).
fn rec709_oetf(c: f32) -> f32 {
    if c < 0.018 {
        4.5 * c
    } else {
        1.099 * c.powf(0.45) - 0.099
    }
}

/// ACES filmic tone-mapping curve followed by sRGB encoding.
fn aces_filmic(c: f32) -> f32 {
    (c * (2.51 * c + 0.03)) / (c * (2.43 * c + 0.59) + 0.14)
}

impl Raster2D {
    /// Creates an empty raster.
    pub fn new(format: Format, bbox: BBox, edge_mode: EdgeMode) -> Self {
        Self {
            format,
            bbox,
            edge_mode,
            channels: IndexMap::new(),
        }
    }

    /// Creates an empty raster covering the format's full frame.
    pub fn for_format(format: Format) -> Self {
        let bbox = format.to_bbox();
        Self::new(format, bbox, EdgeMode::BlackOutside)
    }

    /// Borrows a plane.
    pub fn channel(&self, channel: &Channel) -> Option<&ChannelPlane> {
        self.channels.get(channel)
    }

    /// Mutably borrows a plane. Mutating one channel never touches the others.
    pub fn channel_mut(&mut self, channel: &Channel) -> Option<&mut ChannelPlane> {
        self.channels.get_mut(channel)
    }

    /// Inserts or replaces a plane.
    pub fn insert_channel(
        &mut self,
        channel: Channel,
        plane: ChannelPlane,
    ) -> Option<ChannelPlane> {
        self.channels.insert(channel, plane)
    }

    /// Samples one channel at a world-space pixel.
    pub fn sample(&self, channel: &Channel, x: i32, y: i32) -> f32 {
        self.channels
            .get(channel)
            .map_or(0.0, |plane| plane.sample(x, y, self.bbox, self.edge_mode))
    }

    /// Samples all four RGBA channels at one Bottom-Left `(x, y)` pixel.
    ///
    /// A missing alpha plane reads as fully opaque, matching the display path.
    pub fn sample_rgba(&self, x: i32, y: i32) -> [f32; 4] {
        let alpha = if self.channels.contains_key(&Channel::RgbaAlpha) {
            self.sample(&Channel::RgbaAlpha, x, y)
        } else {
            1.0
        };
        [
            self.sample(&Channel::RgbaRed, x, y),
            self.sample(&Channel::RgbaGreen, x, y),
            self.sample(&Channel::RgbaBlue, x, y),
            alpha,
        ]
    }

    /// Writes one channel pixel, creating the plane on demand.
    pub fn set_pixel(
        &mut self,
        channel: Channel,
        x: i32,
        y: i32,
        value: f32,
        governor: Option<&MemoryGovernor>,
    ) -> Result<(), NodeError> {
        self.channels
            .entry(channel)
            .or_default()
            .set(x, y, value, governor)
    }

    /// Converts linear float `Rgba*` planes into Top-Left `RGBA8` bytes.
    ///
    /// This is the single canonical boundary between Tanova's Bottom-Left
    /// (`+Y` up) linear pipeline and the Top-Left (`+Y` down) byte buffers
    /// consumed by `egui` and `wgpu` uploads. A missing alpha channel is
    /// treated as fully opaque.
    pub fn to_top_left_rgba8(&self, display_bbox: BBox) -> Vec<u8> {
        self.to_top_left_rgba8_with_view(
            display_bbox,
            1.0,
            1.0,
            DisplayTransform::Linear,
            ViewerChannel::Rgba,
        )
    }

    /// Converts to Top-Left `RGBA8` through the viewer's exposure and gamma.
    ///
    /// `gain` multiplies RGB before `gamma` applies the inverse power. Alpha is
    /// left untouched so transparent edges keep their coverage. Non-positive
    /// RGB values skip `pow`, which keeps super-blacks finite.
    pub fn to_top_left_rgba8_with_display(
        &self,
        display_bbox: BBox,
        gain: f32,
        gamma: f32,
    ) -> Vec<u8> {
        self.to_top_left_rgba8_with_view(
            display_bbox,
            gain,
            gamma,
            DisplayTransform::Linear,
            ViewerChannel::Rgba,
        )
    }

    /// Converts to Top-Left `RGBA8` through gain, gamma, a display transform
    /// and channel soloing.
    ///
    /// `gain` and viewer `gamma` (`val.powf(1.0 / gamma)` for `val > 0.0`)
    /// apply to `R`, `G`, `B` before `transform` encodes the display color
    /// space. `channel` solos one channel: `R`/`G`/`B` render the
    /// display-transformed channel as luminance with an opaque alpha byte,
    /// while `Alpha` renders raw alpha as luminance (`[a_u8, a_u8, a_u8,
    /// 255]`) with no curve applied to alpha mattes.
    #[allow(clippy::too_many_arguments)]
    pub fn to_top_left_rgba8_with_view(
        &self,
        display_bbox: BBox,
        gain: f32,
        gamma: f32,
        transform: DisplayTransform,
        channel: ViewerChannel,
    ) -> Vec<u8> {
        let width = display_bbox.width().max(0) as usize;
        let height = display_bbox.height().max(0) as usize;
        let mut out = vec![0u8; width * height * 4];
        if width == 0 || height == 0 {
            return out;
        }

        let has_alpha = self.channels.contains_key(&Channel::RgbaAlpha);

        for row in 0..height {
            let y = display_bbox.max_y - 1 - row as i32;
            for col in 0..width {
                let x = display_bbox.min_x + col as i32;
                let index = (row * width + col) * 4;

                let mut r = self.sample(&Channel::RgbaRed, x, y) * gain;
                let mut g = self.sample(&Channel::RgbaGreen, x, y) * gain;
                let mut b = self.sample(&Channel::RgbaBlue, x, y) * gain;
                if gamma > 1e-6 {
                    if r > 0.0 {
                        r = r.powf(1.0 / gamma);
                    }
                    if g > 0.0 {
                        g = g.powf(1.0 / gamma);
                    }
                    if b > 0.0 {
                        b = b.powf(1.0 / gamma);
                    }
                }
                r = apply_display_transform(r, transform);
                g = apply_display_transform(g, transform);
                b = apply_display_transform(b, transform);

                let a = if has_alpha {
                    self.sample(&Channel::RgbaAlpha, x, y)
                } else {
                    1.0
                };
                let a_u8 = to_unorm8(a);

                let bytes = match channel {
                    ViewerChannel::Rgba => [to_unorm8(r), to_unorm8(g), to_unorm8(b), a_u8],
                    ViewerChannel::Red => {
                        let v = to_unorm8(r);
                        [v, v, v, 255]
                    }
                    ViewerChannel::Green => {
                        let v = to_unorm8(g);
                        [v, v, v, 255]
                    }
                    ViewerChannel::Blue => {
                        let v = to_unorm8(b);
                        [v, v, v, 255]
                    }
                    ViewerChannel::Alpha => [a_u8, a_u8, a_u8, 255],
                };
                out[index] = bytes[0];
                out[index + 1] = bytes[1];
                out[index + 2] = bytes[2];
                out[index + 3] = bytes[3];
            }
        }

        out
    }
}

fn to_unorm8(value: f32) -> u8 {
    if value.is_finite() {
        (value.clamp(0.0, 1.0) * 255.0).round() as u8
    } else {
        0
    }
}

/// Extracts one tile plus an `apron`-pixel halo on every side.
///
/// The result is `(TILE_SIZE + 2 * apron)^2` contiguous `f32` values in
/// Bottom-Left row-major order. Samples crossing into neighbouring tiles (or
/// outside `bbox`) are resolved through `edge_mode`, so shader kernels never
/// see a seam at tile borders.
pub fn extract_padded_tile(
    plane: &ChannelPlane,
    tile: TileCoord,
    apron: u32,
    bbox: BBox,
    edge_mode: EdgeMode,
) -> Vec<f32> {
    let apron = apron as usize;
    let stride = TILE_STRIDE + 2 * apron;
    let (origin_x, origin_y) = tile.origin_world();
    let start_x = origin_x.saturating_sub(apron as i32);
    let start_y = origin_y.saturating_sub(apron as i32);

    // Pre-zeroed output: `BlackOutside` pixels outside `bbox` are simply never
    // written, so they keep the required `0.0` without any extra work.
    let mut out = vec![0.0f32; stride * stride];

    // Resolve every output column/row through `edge_mode` exactly once. Edge
    // resolution is separable per axis, so resolving x with an in-bounds y (and
    // vice versa) reproduces `ChannelPlane::sample` bit-for-bit while avoiding
    // a `HashMap` lookup per pixel.
    let inside_x = bbox.min_x <= start_x && start_x.saturating_add(stride as i32) <= bbox.max_x;
    let inside_y = bbox.min_y <= start_y && start_y.saturating_add(stride as i32) <= bbox.max_y;

    let xs: Vec<Option<i32>> = if inside_x {
        (0..stride)
            .map(|col| Some(start_x.saturating_add(col as i32)))
            .collect()
    } else {
        (0..stride)
            .map(|col| {
                let x = start_x.saturating_add(col as i32);
                edge_mode
                    .resolve_coord(x, bbox.min_y, bbox)
                    .map(|(resolved, _)| resolved)
            })
            .collect()
    };
    let ys: Vec<Option<i32>> = if inside_y {
        (0..stride)
            .map(|row| Some(start_y.saturating_add(row as i32)))
            .collect()
    } else {
        (0..stride)
            .map(|row| {
                let y = start_y.saturating_add(row as i32);
                edge_mode
                    .resolve_coord(bbox.min_x, y, bbox)
                    .map(|(_, resolved)| resolved)
            })
            .collect()
    };

    for (row, resolved_y) in ys.iter().enumerate() {
        let Some(resolved_y) = *resolved_y else {
            continue;
        };
        let out_row = row * stride;
        let mut col = 0;
        while col < stride {
            let Some(resolved_x) = xs[col] else {
                col += 1;
                continue;
            };

            let (start_tile, lx, ly) = world_to_tile(resolved_x, resolved_y);
            // Extend the run while the next column stays inside the same tile
            // and resolves to the next contiguous world x. This copies one
            // straight span per tile crossing instead of one pixel at a time.
            let mut run = 1usize;
            while col + run < stride && lx as usize + run < TILE_STRIDE {
                match xs[col + run] {
                    Some(next_x) if next_x == resolved_x.saturating_add(run as i32) => run += 1,
                    _ => break,
                }
            }

            if let Some(buffer) = plane.get(&start_tile) {
                let dst = &mut out[out_row + col..out_row + col + run];
                match buffer.is_uniform() {
                    Some(value) => dst.fill(value),
                    None => {
                        let src = buffer
                            .as_dense_slice()
                            .expect("dense tile must expose its slice");
                        let src_off = ly as usize * TILE_STRIDE + lx as usize;
                        dst.copy_from_slice(&src[src_off..src_off + run]);
                    }
                }
            }
            // Absent tile: leave the pre-zeroed span untouched.

            col += run;
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tile_at(tx: i32, ty: i32) -> TileCoord {
        TileCoord::new(tx, ty)
    }

    #[test]
    fn dense_tiles_are_governed_by_live_permits() {
        let governor = MemoryGovernor::new(1 << 20);
        assert_eq!(governor.allocated_bytes(), 0);

        let buffer = Arc::new(TileBuffer::new_dense(0.25, Some(&governor)).unwrap());
        assert_eq!(governor.allocated_bytes(), TILE_BYTES);
        assert_eq!(buffer.governed_bytes(), TILE_BYTES);
        assert_eq!(buffer.get(0, 0), 0.25);
        assert_eq!(buffer.get(255, 255), 0.25);

        let shared = Arc::clone(&buffer);
        assert_eq!(
            governor.allocated_bytes(),
            TILE_BYTES,
            "cloning the Arc must not reserve again"
        );
        drop(shared);
        assert_eq!(governor.allocated_bytes(), TILE_BYTES);

        drop(buffer);
        assert_eq!(governor.allocated_bytes(), 0);
    }

    #[test]
    fn dense_tiles_respect_the_cap() {
        let tight = MemoryGovernor::new(TILE_BYTES - 1);
        assert!(matches!(
            TileBuffer::new_dense(0.0, Some(&tight)),
            Err(NodeError::MemoryExceeded { .. })
        ));
        assert_eq!(tight.allocated_bytes(), 0);

        assert!(matches!(
            TileBuffer::from_vec(vec![0.0; 4], None),
            Err(NodeError::Other(_))
        ));
    }

    #[test]
    fn uniform_tiles_cost_nothing_until_promoted() {
        let governor = MemoryGovernor::new(1 << 20);
        let mut buffer = TileBuffer::new_uniform(0.5);

        assert_eq!(buffer.is_uniform(), Some(0.5));
        assert_eq!(governor.allocated_bytes(), 0);
        assert_eq!(buffer.governed_bytes(), 0);

        // Writing the same value keeps the constant representation.
        buffer.set(1, 1, 0.5, Some(&governor)).unwrap();
        assert_eq!(buffer.is_uniform(), Some(0.5));
        assert_eq!(governor.allocated_bytes(), 0);

        buffer.set(1, 1, 0.25, Some(&governor)).unwrap();
        assert_eq!(buffer.is_uniform(), None);
        assert!(buffer.is_dense());
        assert_eq!(governor.allocated_bytes(), TILE_BYTES);
        assert_eq!(buffer.governed_bytes(), TILE_BYTES);
        assert_eq!(buffer.get(1, 1), 0.25);
        assert_eq!(buffer.get(2, 2), 0.5);

        drop(buffer);
        assert_eq!(governor.allocated_bytes(), 0);
    }

    #[test]
    fn mutating_one_channel_keeps_other_planes_zero_copy() {
        let mut raster = Raster2D::new(
            Format::hd_1080(),
            BBox::new(0, 0, TILE_SIZE as i32, TILE_SIZE as i32),
            EdgeMode::BlackOutside,
        );

        let mut red = ChannelPlane::new();
        red.insert(tile_at(0, 0), Arc::new(TileBuffer::new_uniform(0.0)));
        let mut depth = ChannelPlane::new();
        depth.insert(tile_at(0, 0), Arc::new(TileBuffer::new_uniform(5.0)));

        raster.insert_channel(Channel::RgbaRed, red);
        raster.insert_channel(Channel::DepthZ, depth);

        let depth_before = raster
            .channel(&Channel::DepthZ)
            .and_then(|plane| plane.get(&tile_at(0, 0)))
            .expect("depth tile")
            .clone();

        raster
            .channel_mut(&Channel::RgbaRed)
            .expect("red plane")
            .set(3, 4, 0.75, None)
            .unwrap();

        let depth_after = raster
            .channel(&Channel::DepthZ)
            .and_then(|plane| plane.get(&tile_at(0, 0)))
            .expect("depth tile");

        assert!(
            Arc::ptr_eq(&depth_before, depth_after),
            "touching RgbaRed must not copy the DepthZ plane"
        );
        assert_eq!(depth_after.get(3, 4), 5.0);
        assert_eq!(raster.sample(&Channel::RgbaRed, 3, 4), 0.75);
    }

    #[test]
    fn apron_stitches_across_negative_and_positive_tile_boundaries() {
        let mut plane = ChannelPlane::new();
        plane.insert(tile_at(-1, 0), Arc::new(TileBuffer::new_uniform(3.0)));
        plane.insert(tile_at(0, 0), Arc::new(TileBuffer::new_uniform(7.0)));

        let bbox = BBox::new(-4096, -4096, 4096, 4096);
        let stride = TILE_STRIDE + 2;

        let center = extract_padded_tile(&plane, tile_at(0, 0), 1, bbox, EdgeMode::BlackOutside);
        assert_eq!(center.len(), stride * stride);
        assert_eq!(center[0], 0.0, "row y = -1 is outside both tiles");
        assert_eq!(center[stride], 3.0, "world x = -1 lives in tile -1");
        assert_eq!(center[stride + 1], 7.0, "world x = 0 lives in tile 0");
        assert_eq!(
            center[stride + stride - 2],
            7.0,
            "world x = 255 is the last pixel of tile 0"
        );
        assert_eq!(
            center[stride + stride - 1],
            0.0,
            "world x = 256 belongs to the empty tile 1"
        );

        let left = extract_padded_tile(&plane, tile_at(-1, 0), 1, bbox, EdgeMode::BlackOutside);
        assert_eq!(left[0], 0.0, "world (-257, -1) is empty");
        assert_eq!(left[stride + 256], 3.0, "world x = -1");
        assert_eq!(left[stride + 257], 7.0, "world x = 0 crosses the tile seam");

        let bounds = BBox::new(0, 0, TILE_SIZE as i32, TILE_SIZE as i32);
        let clamped = extract_padded_tile(&plane, tile_at(0, 0), 1, bounds, EdgeMode::ClampToEdge);
        assert_eq!(clamped[0], 7.0, "the apron clamps onto the edge pixel");
        assert_eq!(clamped[stride * stride - 1], 7.0);
    }

    #[test]
    fn display_conversion_flips_bottom_left_to_top_left() {
        let bbox = BBox::new(0, 0, 2, 2);
        let mut raster = Raster2D::new(Format::new("2x2", 2, 2, 1.0), bbox, EdgeMode::BlackOutside);

        // Nuke's bottom row (y = 0) is red.
        raster.set_pixel(Channel::RgbaRed, 0, 0, 1.0, None).unwrap();
        raster.set_pixel(Channel::RgbaRed, 1, 0, 1.0, None).unwrap();

        let bytes = raster.to_top_left_rgba8(bbox);
        assert_eq!(bytes.len(), 2 * 2 * 4);

        // Display row 0 is the top of the frame (Nuke y = 1): black, opaque.
        assert_eq!(&bytes[0..4], &[0, 0, 0, 255]);

        // The last display row is Nuke y = 0: red, opaque.
        let height = 2;
        let last_row = (height - 1) * 2 * 4;
        assert_eq!(&bytes[last_row..last_row + 4], &[255, 0, 0, 255]);
        assert_eq!(&bytes[last_row + 4..last_row + 8], &[255, 0, 0, 255]);
    }

    #[test]
    fn premultiplied_alpha_is_converted_when_present() {
        let bbox = BBox::new(0, 0, 1, 1);
        let mut raster = Raster2D::new(Format::new("1x1", 1, 1, 1.0), bbox, EdgeMode::BlackOutside);
        raster
            .set_pixel(Channel::RgbaAlpha, 0, 0, 0.5, None)
            .unwrap();

        let bytes = raster.to_top_left_rgba8(bbox);
        assert_eq!(&bytes[0..4], &[0, 0, 0, 128]);
    }

    #[test]
    fn display_gain_and_gamma_affect_rgb_but_not_alpha() {
        let bbox = BBox::new(0, 0, 1, 1);
        let mut raster = Raster2D::new(Format::new("1x1", 1, 1, 1.0), bbox, EdgeMode::BlackOutside);
        raster
            .set_pixel(Channel::RgbaRed, 0, 0, 0.25, None)
            .unwrap();
        raster
            .set_pixel(Channel::RgbaAlpha, 0, 0, 0.5, None)
            .unwrap();

        // 0.25 * 2 = 0.5, then pow(0.5, 1 / 2.0) = 0.7071.
        let bytes = raster.to_top_left_rgba8_with_display(bbox, 2.0, 2.0);
        assert_eq!(bytes[0], 180);
        assert_eq!(bytes[3], 128, "viewer display must not touch alpha");

        // Super-blacks skip pow and clamp to zero instead of going NaN.
        let mut negative =
            Raster2D::new(Format::new("1x1", 1, 1, 1.0), bbox, EdgeMode::BlackOutside);
        negative
            .set_pixel(Channel::RgbaRed, 0, 0, -0.5, None)
            .unwrap();
        let bytes = negative.to_top_left_rgba8_with_display(bbox, 1.0, 2.2);
        assert_eq!(bytes[0], 0);
    }

    #[test]
    fn sample_rgba_reads_bottom_left_pixels() {
        let bbox = BBox::new(0, 0, 2, 2);
        let mut raster = Raster2D::new(Format::new("2x2", 2, 2, 1.0), bbox, EdgeMode::BlackOutside);
        raster.set_pixel(Channel::RgbaRed, 0, 0, 0.1, None).unwrap();
        raster
            .set_pixel(Channel::RgbaGreen, 0, 0, 0.2, None)
            .unwrap();
        raster
            .set_pixel(Channel::RgbaBlue, 0, 0, 0.3, None)
            .unwrap();
        raster
            .set_pixel(Channel::RgbaAlpha, 0, 0, 0.4, None)
            .unwrap();

        let pixel = raster.sample_rgba(0, 0);
        assert!((pixel[0] - 0.1).abs() < 1e-6);
        assert!((pixel[1] - 0.2).abs() < 1e-6);
        assert!((pixel[2] - 0.3).abs() < 1e-6);
        assert!((pixel[3] - 0.4).abs() < 1e-6);

        // Missing alpha reads as opaque.
        let mut no_alpha =
            Raster2D::new(Format::new("2x2", 2, 2, 1.0), bbox, EdgeMode::BlackOutside);
        no_alpha
            .set_pixel(Channel::RgbaRed, 1, 1, 0.7, None)
            .unwrap();
        assert_eq!(no_alpha.sample_rgba(1, 1)[3], 1.0);
    }

    #[test]
    fn display_transforms_and_channel_soloing_produce_expected_rgba8() {
        let bbox = BBox::new(0, 0, 1, 1);
        let mut raster = Raster2D::new(Format::new("1x1", 1, 1, 1.0), bbox, EdgeMode::BlackOutside);
        raster
            .set_pixel(Channel::RgbaRed, 0, 0, 0.214, None)
            .unwrap();
        raster
            .set_pixel(Channel::RgbaAlpha, 0, 0, 0.5, None)
            .unwrap();

        // The legacy entry points delegate to the view form with
        // `Linear` / `Rgba`, so output stays byte-identical.
        let legacy = raster.to_top_left_rgba8(bbox);
        let direct = raster.to_top_left_rgba8_with_view(
            bbox,
            1.0,
            1.0,
            DisplayTransform::Linear,
            ViewerChannel::Rgba,
        );
        let exposed = raster.to_top_left_rgba8_with_display(bbox, 1.0, 1.0);
        assert_eq!(legacy, direct);
        assert_eq!(legacy, exposed);
        assert_eq!(&legacy[0..4], &[55, 0, 0, 128]);

        // sRGB OETF maps 0.214 linear to mid-gray (~128).
        let srgb = raster.to_top_left_rgba8_with_view(
            bbox,
            1.0,
            1.0,
            DisplayTransform::Srgb,
            ViewerChannel::Rgba,
        );
        assert!(
            (srgb[0] as i32 - 128).abs() <= 2,
            "srgb(0.214) = {}",
            srgb[0]
        );

        // ACES tonemap compresses HDR 5.0 into display range without clipping.
        let mut hdr = Raster2D::new(Format::new("1x1", 1, 1, 1.0), bbox, EdgeMode::BlackOutside);
        hdr.set_pixel(Channel::RgbaRed, 0, 0, 5.0, None).unwrap();
        let aces = hdr.to_top_left_rgba8_with_view(
            bbox,
            1.0,
            1.0,
            DisplayTransform::AcesTonemap,
            ViewerChannel::Rgba,
        );
        assert!(aces[0] < 255, "ACES must not clip HDR 5.0 to white");
        assert!(aces[0] > 240, "ACES should map HDR 5.0 near display white");

        // Channel soloing: red renders luminance with opaque alpha.
        let red = raster.to_top_left_rgba8_with_view(
            bbox,
            1.0,
            1.0,
            DisplayTransform::Linear,
            ViewerChannel::Red,
        );
        assert_eq!(&red[0..4], &[55, 55, 55, 255]);

        // Alpha solo renders raw alpha as luminance; no curve is applied.
        let alpha = raster.to_top_left_rgba8_with_view(
            bbox,
            1.0,
            1.0,
            DisplayTransform::Srgb,
            ViewerChannel::Alpha,
        );
        assert_eq!(&alpha[0..4], &[128, 128, 128, 255]);
    }

    /// Reference implementation of the original per-pixel sampling loop.
    fn naive_extract_padded_tile(
        plane: &ChannelPlane,
        tile: TileCoord,
        apron: u32,
        bbox: BBox,
        edge_mode: EdgeMode,
    ) -> Vec<f32> {
        let apron = apron as usize;
        let stride = TILE_STRIDE + 2 * apron;
        let (origin_x, origin_y) = tile.origin_world();
        let start_x = origin_x.saturating_sub(apron as i32);
        let start_y = origin_y.saturating_sub(apron as i32);

        let mut out = Vec::with_capacity(stride * stride);
        for row in 0..stride {
            let y = start_y.saturating_add(row as i32);
            for col in 0..stride {
                let x = start_x.saturating_add(col as i32);
                out.push(plane.sample(x, y, bbox, edge_mode));
            }
        }
        out
    }

    /// The direct-slice apron extractor must be bit-identical to the historical
    /// per-pixel `sample` loop on interior, negative, and partial 1080p edges,
    /// for every edge mode.
    #[test]
    fn fast_extract_padded_tile_matches_reference_across_partial_1080p_and_negative_edges() {
        let format = Format::hd_1080();
        let bbox = format.to_bbox();

        // Mixed uniform/dense tiles covering the frame plus a ring outside it.
        let mut plane = ChannelPlane::new();
        for ty in -1..=4i32 {
            for tx in -1..=7i32 {
                if (tx + ty).rem_euclid(3) == 0 {
                    let value = (tx * 3 + ty) as f32 * 0.25;
                    plane.insert(tile_at(tx, ty), Arc::new(TileBuffer::new_uniform(value)));
                } else {
                    let mut data = vec![0.0f32; TILE_PIXELS];
                    for (index, value) in data.iter_mut().enumerate() {
                        *value =
                            ((tx * 131 + ty * 71 + index as i32).rem_euclid(251)) as f32 / 251.0;
                    }
                    plane.insert(
                        tile_at(tx, ty),
                        Arc::new(TileBuffer::from_vec(data, None).unwrap()),
                    );
                }
            }
        }

        // Interior, negative, right edge (partial), top edge (partial), corner.
        let cases = [(3, 2), (-1, 0), (7, 4), (0, 4), (-1, -1), (7, 0), (0, -1)];
        let modes = [
            EdgeMode::BlackOutside,
            EdgeMode::ClampToEdge,
            EdgeMode::Mirror,
            EdgeMode::Bounce,
        ];

        for &(tx, ty) in &cases {
            for apron in [0u32, 4, 12] {
                for edge_mode in modes {
                    let fast = extract_padded_tile(&plane, tile_at(tx, ty), apron, bbox, edge_mode);
                    let naive =
                        naive_extract_padded_tile(&plane, tile_at(tx, ty), apron, bbox, edge_mode);
                    assert_eq!(
                        fast.len(),
                        naive.len(),
                        "length mismatch at ({tx}, {ty}) apron {apron}"
                    );
                    for (index, (fast_value, naive_value)) in
                        fast.iter().zip(naive.iter()).enumerate()
                    {
                        assert_eq!(
                            fast_value, naive_value,
                            "tile ({tx}, {ty}) apron {apron} {edge_mode:?} at index {index}"
                        );
                    }
                }
            }
        }

        // An empty bbox must resolve every sample to 0.0 in both paths.
        let empty = BBox::new(10, 10, 10, 10);
        for edge_mode in modes {
            let fast = extract_padded_tile(&plane, tile_at(3, 2), 4, empty, edge_mode);
            let naive = naive_extract_padded_tile(&plane, tile_at(3, 2), 4, empty, edge_mode);
            assert_eq!(fast, naive, "empty bbox mismatch under {edge_mode:?}");
        }
    }
}

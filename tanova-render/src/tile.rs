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
        self.to_top_left_rgba8_with_display(display_bbox, 1.0, 1.0)
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
        let width = display_bbox.width().max(0) as usize;
        let height = display_bbox.height().max(0) as usize;
        let mut out = vec![0u8; width * height * 4];
        if width == 0 || height == 0 {
            return out;
        }

        let has_alpha = self.channels.contains_key(&Channel::RgbaAlpha);
        let display = |value: f32| {
            let mut value = value * gain;
            if value > 0.0 && gamma > 1e-6 {
                value = value.powf(1.0 / gamma);
            }
            to_unorm8(value)
        };

        for row in 0..height {
            let y = display_bbox.max_y - 1 - row as i32;
            for col in 0..width {
                let x = display_bbox.min_x + col as i32;
                let index = (row * width + col) * 4;

                out[index] = display(self.sample(&Channel::RgbaRed, x, y));
                out[index + 1] = display(self.sample(&Channel::RgbaGreen, x, y));
                out[index + 2] = display(self.sample(&Channel::RgbaBlue, x, y));
                out[index + 3] = if has_alpha {
                    to_unorm8(self.sample(&Channel::RgbaAlpha, x, y))
                } else {
                    255
                };
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
}

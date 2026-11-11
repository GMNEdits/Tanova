//! Nuke-style spatial types.
//!
//! Tanova is Bottom-Left `(0, 0)`: `+X` points right and `+Y` points up. Tile
//! math is Euclidean, so a pixel at `x = -1` belongs to tile `-1` at local
//! coordinate `255`, never tile `0` at `-1`.

/// Edge length of one render tile, in pixels.
pub const TILE_SIZE: u32 = 256;

/// Pixels in one dense tile.
pub const TILE_PIXELS: usize = (TILE_SIZE * TILE_SIZE) as usize;

/// Identifies one tile on the infinite canvas.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TileCoord {
    /// Tile column (`+X`).
    pub tx: i32,
    /// Tile row (`+Y`, upward).
    pub ty: i32,
}

impl TileCoord {
    /// Builds a tile coordinate.
    pub const fn new(tx: i32, ty: i32) -> Self {
        Self { tx, ty }
    }

    /// World-space position of this tile's Bottom-Left corner.
    pub fn origin_world(&self) -> (i32, i32) {
        (
            self.tx.saturating_mul(TILE_SIZE as i32),
            self.ty.saturating_mul(TILE_SIZE as i32),
        )
    }

    /// Half-open world-space bounds of this tile.
    pub fn to_bbox(&self) -> BBox {
        let (x, y) = self.origin_world();
        let size = TILE_SIZE as i32;
        BBox::new(x, y, x.saturating_add(size), y.saturating_add(size))
    }
}

/// Splits a world-space pixel into its tile and local `(lx, ly)` coordinates.
///
/// Uses Euclidean division and remainder so negative pixels map to negative
/// tiles with a non-negative local offset.
pub fn world_to_tile(x: i32, y: i32) -> (TileCoord, u32, u32) {
    let size = TILE_SIZE as i32;
    (
        TileCoord::new(x.div_euclid(size), y.div_euclid(size)),
        x.rem_euclid(size) as u32,
        y.rem_euclid(size) as u32,
    )
}

/// Iterator over every tile inside an inclusive tile rectangle.
#[derive(Debug, Clone)]
pub struct TileRange {
    min: TileCoord,
    max: TileCoord,
    index: i64,
    total: i64,
}

impl TileRange {
    /// Iterates tiles from `min` to `max`, inclusive on both corners.
    pub fn new(min: TileCoord, max: TileCoord) -> Self {
        let width = i64::from(max.tx) - i64::from(min.tx) + 1;
        let height = i64::from(max.ty) - i64::from(min.ty) + 1;
        let total = if width <= 0 || height <= 0 {
            0
        } else {
            width * height
        };
        Self {
            min,
            max,
            index: 0,
            total,
        }
    }

    /// Iterates every tile overlapping `bbox`.
    pub fn from_bbox(bbox: BBox) -> Self {
        if bbox.is_empty() {
            return Self::new(TileCoord::new(0, 0), TileCoord::new(-1, -1));
        }
        let (min, _, _) = world_to_tile(bbox.min_x, bbox.min_y);
        let (max, _, _) = world_to_tile(bbox.max_x - 1, bbox.max_y - 1);
        Self::new(min, max)
    }

    /// Returns `true` if the range covers no tiles.
    pub fn is_empty(&self) -> bool {
        self.total == 0
    }
}

impl Iterator for TileRange {
    type Item = TileCoord;

    fn next(&mut self) -> Option<Self::Item> {
        if self.index >= self.total {
            return None;
        }
        let height = i64::from(self.max.ty) - i64::from(self.min.ty) + 1;
        let index = self.index;
        self.index += 1;
        let tx = i64::from(self.min.tx) + index / height;
        let ty = i64::from(self.min.ty) + index % height;
        Some(TileCoord::new(tx as i32, ty as i32))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = (self.total - self.index).max(0) as usize;
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for TileRange {}

/// Half-open world-space rectangle: `[min_x, max_x) x [min_y, max_y)`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct BBox {
    /// Left edge (inclusive).
    pub min_x: i32,
    /// Bottom edge (inclusive).
    pub min_y: i32,
    /// Right edge (exclusive).
    pub max_x: i32,
    /// Top edge (exclusive).
    pub max_y: i32,
}

impl BBox {
    /// Builds a half-open rectangle.
    pub const fn new(min_x: i32, min_y: i32, max_x: i32, max_y: i32) -> Self {
        Self {
            min_x,
            min_y,
            max_x,
            max_y,
        }
    }

    /// Width in pixels, never negative.
    pub fn width(&self) -> i32 {
        self.max_x.saturating_sub(self.min_x).max(0)
    }

    /// Height in pixels, never negative.
    pub fn height(&self) -> i32 {
        self.max_y.saturating_sub(self.min_y).max(0)
    }

    /// Returns `true` if the rectangle covers no pixels.
    pub fn is_empty(&self) -> bool {
        self.max_x <= self.min_x || self.max_y <= self.min_y
    }

    /// Returns `true` if `(x, y)` is inside the rectangle.
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.min_x && x < self.max_x && y >= self.min_y && y < self.max_y
    }

    /// Smallest rectangle covering both inputs.
    pub fn union(&self, other: &Self) -> Self {
        if self.is_empty() {
            return *other;
        }
        if other.is_empty() {
            return *self;
        }
        Self {
            min_x: self.min_x.min(other.min_x),
            min_y: self.min_y.min(other.min_y),
            max_x: self.max_x.max(other.max_x),
            max_y: self.max_y.max(other.max_y),
        }
    }

    /// Overlap of both rectangles; may be empty.
    pub fn intersection(&self, other: &Self) -> Self {
        Self {
            min_x: self.min_x.max(other.min_x),
            min_y: self.min_y.max(other.min_y),
            max_x: self.max_x.min(other.max_x),
            max_y: self.max_y.min(other.max_y),
        }
    }

    /// Grows the rectangle by `radius` pixels on every side.
    ///
    /// A negative radius shrinks it.
    pub fn pad(&self, radius: i32) -> Self {
        Self {
            min_x: self.min_x.saturating_sub(radius),
            min_y: self.min_y.saturating_sub(radius),
            max_x: self.max_x.saturating_add(radius),
            max_y: self.max_y.saturating_add(radius),
        }
    }

    /// Every tile overlapping this rectangle.
    pub fn tile_coords(&self) -> Vec<TileCoord> {
        TileRange::from_bbox(*self).collect()
    }
}

/// Image format description, in pixels and pixel aspect ratio.
#[derive(Debug, Clone, PartialEq)]
pub struct Format {
    /// Human-readable format name (e.g. `HD 1080`).
    pub name: String,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Pixel aspect ratio (`1.0` is square).
    pub pixel_aspect: f32,
}

impl Format {
    /// Builds a format.
    pub fn new(name: impl Into<String>, width: u32, height: u32, pixel_aspect: f32) -> Self {
        Self {
            name: name.into(),
            width,
            height,
            pixel_aspect,
        }
    }

    /// 1920x1080 square-pixel HD.
    pub fn hd_1080() -> Self {
        Self::new("HD 1080", 1920, 1080, 1.0)
    }

    /// World-space bounds of the format, anchored at the origin.
    pub fn to_bbox(&self) -> BBox {
        BBox::new(
            0,
            0,
            i32::try_from(self.width).unwrap_or(i32::MAX),
            i32::try_from(self.height).unwrap_or(i32::MAX),
        )
    }
}

/// The part of the frame a viewer actually asked for.
///
/// Travels top-down inside `EvalContext` so cartridges can skip tiles that will
/// never be displayed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegionOfInterest(pub BBox);

/// How sampling behaves outside a bounding box.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum EdgeMode {
    /// Outside the box is transparent black.
    #[default]
    BlackOutside,
    /// Outside pixels repeat the nearest edge pixel.
    ClampToEdge,
    /// Outside pixels mirror, duplicating the edge pixel.
    Mirror,
    /// Outside pixels bounce without duplicating the edge pixel.
    Bounce,
}

impl EdgeMode {
    /// Resolves an out-of-bounds sample to a real pixel, or `None` for
    /// transparent black.
    pub fn resolve_coord(self, x: i32, y: i32, bounds: BBox) -> Option<(i32, i32)> {
        if bounds.is_empty() {
            return None;
        }

        match self {
            Self::BlackOutside => bounds.contains(x, y).then_some((x, y)),
            Self::ClampToEdge => Some((
                x.clamp(bounds.min_x, bounds.max_x - 1),
                y.clamp(bounds.min_y, bounds.max_y - 1),
            )),
            Self::Mirror => Some((
                mirror(x, bounds.min_x, bounds.max_x),
                mirror(y, bounds.min_y, bounds.max_y),
            )),
            Self::Bounce => Some((
                bounce(x, bounds.min_x, bounds.max_x),
                bounce(y, bounds.min_y, bounds.max_y),
            )),
        }
    }
}

/// Mirrors with edge duplication: `size * 2` period.
fn mirror(value: i32, min: i32, max: i32) -> i32 {
    let size = i64::from(max) - i64::from(min);
    if size <= 0 {
        return min;
    }
    let period = size * 2;
    let offset = (i64::from(value) - i64::from(min)).rem_euclid(period);
    let reflected = if offset >= size {
        period - 1 - offset
    } else {
        offset
    };
    (i64::from(min) + reflected) as i32
}

/// Bounces without edge duplication: `(size - 1) * 2` period.
fn bounce(value: i32, min: i32, max: i32) -> i32 {
    let size = i64::from(max) - i64::from(min);
    if size <= 1 {
        return min;
    }
    let period = (size - 1) * 2;
    let offset = (i64::from(value) - i64::from(min)).rem_euclid(period);
    let reflected = if offset >= size {
        period - offset
    } else {
        offset
    };
    (i64::from(min) + reflected) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negative_pixels_use_euclidean_tiles() {
        assert_eq!(world_to_tile(-1, -50), (TileCoord::new(-1, -1), 255, 206));
        assert_eq!(world_to_tile(-256, -256), (TileCoord::new(-1, -1), 0, 0));
        assert_eq!(world_to_tile(-257, 255), (TileCoord::new(-2, 0), 255, 255));
        assert_eq!(world_to_tile(256, 512), (TileCoord::new(1, 2), 0, 0));
        assert_eq!(world_to_tile(0, 0), (TileCoord::new(0, 0), 0, 0));
    }

    #[test]
    fn tile_origin_and_bbox_stay_bottom_left() {
        let tile = TileCoord::new(-1, -1);
        assert_eq!(tile.origin_world(), (-256, -256));
        assert_eq!(tile.to_bbox(), BBox::new(-256, -256, 0, 0));

        let origin = TileCoord::new(0, 0).to_bbox();
        assert_eq!(origin, BBox::new(0, 0, 256, 256));
        assert!(origin.contains(0, 0));
        assert!(origin.contains(255, 255));
        assert!(!origin.contains(256, 255));
    }

    #[test]
    fn bbox_tile_coords_handle_negative_boundaries() {
        let tiles = BBox::new(-257, -257, -255, -255).tile_coords();
        assert_eq!(tiles.len(), 4);
        for tile in [
            TileCoord::new(-2, -2),
            TileCoord::new(-2, -1),
            TileCoord::new(-1, -2),
            TileCoord::new(-1, -1),
        ] {
            assert!(tiles.contains(&tile), "missing {tile:?} in {tiles:?}");
        }

        assert_eq!(
            BBox::new(0, 0, 1, 1).tile_coords(),
            vec![TileCoord::new(0, 0)]
        );
        assert_eq!(
            BBox::new(-1, -1, 1, 1).tile_coords().len(),
            4,
            "pixels -1 and 0 straddle the tile boundary"
        );
        assert!(BBox::new(5, 5, 5, 5).tile_coords().is_empty());
    }

    #[test]
    fn bbox_set_operations_are_half_open() {
        let a = BBox::new(0, 0, 10, 10);
        let b = BBox::new(5, -5, 15, 5);

        assert_eq!(a.union(&b), BBox::new(0, -5, 15, 10));
        assert_eq!(a.intersection(&b), BBox::new(5, 0, 10, 5));
        assert_eq!(a.pad(2), BBox::new(-2, -2, 12, 12));
        assert!(a.intersection(&BBox::new(20, 20, 30, 30)).is_empty());
        assert_eq!(BBox::default().width(), 0);
    }

    #[test]
    fn edge_modes_resolve_outside_pixels() {
        let bounds = BBox::new(0, 0, 4, 4);

        assert_eq!(EdgeMode::BlackOutside.resolve_coord(-1, 0, bounds), None);
        assert_eq!(
            EdgeMode::ClampToEdge.resolve_coord(-1, 9, bounds),
            Some((0, 3))
        );
        assert_eq!(EdgeMode::Mirror.resolve_coord(-1, 4, bounds), Some((0, 3)));
        assert_eq!(EdgeMode::Mirror.resolve_coord(5, 0, bounds), Some((2, 0)));
        assert_eq!(EdgeMode::Bounce.resolve_coord(4, 0, bounds), Some((2, 0)));
        assert_eq!(EdgeMode::Bounce.resolve_coord(5, 0, bounds), Some((1, 0)));
        assert_eq!(
            EdgeMode::ClampToEdge.resolve_coord(0, 0, BBox::default()),
            None
        );
    }

    #[test]
    fn hd_1080_format_bounds_start_at_the_origin() {
        let format = Format::hd_1080();
        assert_eq!(format.width, 1920);
        assert_eq!(format.height, 1080);
        assert_eq!(format.pixel_aspect, 1.0);
        assert_eq!(format.to_bbox(), BBox::new(0, 0, 1920, 1080));
    }
}

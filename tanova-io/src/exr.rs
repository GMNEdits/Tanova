//! Safe OpenEXR ingestion.
//!
//! EXR files are hostile input: they may be missing channels, carry f16 data,
//! use a data window larger than the display window, or be truncated. Nothing
//! in this module indexes a channel or a pixel without a bounds check, and
//! every failure is reported as a [`NodeError`] so the kernel can keep running.

use std::path::Path;
use std::sync::Arc;

use exr::prelude::{AnyChannels, FlatSamples, Image, Layer, read_first_flat_layer_from_file};
use tanova_kernel::{MemoryGovernor, NodeError};
use tanova_render::{
    BBox, Channel, ChannelPlane, EdgeMode, Format, Raster2D, TileBuffer, world_to_tile,
};

/// Loads the first flat layer of an EXR file into a governed [`Raster2D`].
///
/// RGB channels are read directly. A missing RGB channel falls back to the
/// luminance channel `Y`, and then to `0.0`; a missing alpha channel defaults
/// to `1.0`. Pixels are converted from EXR's Top-Left origin to Tanova's
/// Bottom-Left origin, and any overscan outside the display window is dropped.
pub fn load_exr(path: &str, governor: Option<&MemoryGovernor>) -> Result<Raster2D, NodeError> {
    if !Path::new(path).exists() {
        return Err(NodeError::Other(format!("EXR file not found: {path}")));
    }

    let image = read_first_flat_layer_from_file(path)
        .map_err(|error| NodeError::Other(format!("failed to read EXR `{path}`: {error}")))?;

    raster_from_image(image, governor)
}

fn raster_from_image(
    image: Image<Layer<AnyChannels<FlatSamples>>>,
    governor: Option<&MemoryGovernor>,
) -> Result<Raster2D, NodeError> {
    let display = image.attributes.display_window;
    let display_min_x = display.position.x();
    let display_min_y = display.position.y();
    let display_max_x = display_min_x.saturating_add(i32_from_usize(display.size.width()));
    let display_max_y = display_min_y.saturating_add(i32_from_usize(display.size.height()));

    let layer = image.layer_data;
    let data = layer.absolute_bounds();
    let data_min_x = data.position.x();
    let data_min_y = data.position.y();
    let data_max_x = data_min_x.saturating_add(i32_from_usize(data.size.width()));
    let data_max_y = data_min_y.saturating_add(i32_from_usize(data.size.height()));

    let width = if display.size.width() == 0 {
        data.size.width()
    } else {
        display.size.width()
    };
    let height = if display.size.height() == 0 {
        data.size.height()
    } else {
        display.size.height()
    };

    let pixel_aspect =
        if image.attributes.pixel_aspect.is_finite() && image.attributes.pixel_aspect > 0.0 {
            image.attributes.pixel_aspect
        } else {
            1.0
        };

    let format = Format::new(
        "EXR",
        u32::try_from(width).unwrap_or(u32::MAX),
        u32::try_from(height).unwrap_or(u32::MAX),
        pixel_aspect,
    );

    // Only the part of the data window the display window actually shows is
    // kept; everything else is overscan and would never reach the viewer.
    let min_x = data_min_x.max(display_min_x);
    let min_y = data_min_y.max(display_min_y);
    let max_x = data_max_x.min(display_max_x);
    let max_y = data_max_y.min(display_max_y);

    if max_x <= min_x || max_y <= min_y {
        return Ok(Raster2D::new(
            format,
            BBox::new(0, 0, 0, 0),
            EdgeMode::BlackOutside,
        ));
    }

    // EXR is Top-Left with +Y down; Tanova is Bottom-Left with +Y up. The
    // display window anchors the output frame, so both axes become 0-based.
    let nuke_min_x = min_x.saturating_sub(display_min_x);
    let nuke_max_x = max_x.saturating_sub(display_min_x);
    let nuke_min_y = display_max_y.saturating_sub(max_y);
    let nuke_max_y = display_max_y.saturating_sub(min_y);
    let bbox = BBox::new(nuke_min_x, nuke_min_y, nuke_max_x, nuke_max_y);

    let red = find_channel(&layer, "R");
    let green = find_channel(&layer, "G");
    let blue = find_channel(&layer, "B");
    let luma = find_channel(&layer, "Y");
    let alpha = find_channel(&layer, "A");

    let data_width = data.size.width();

    let mut raster = Raster2D::new(format, bbox, EdgeMode::BlackOutside);
    let mut red_plane = ChannelPlane::new();
    let mut green_plane = ChannelPlane::new();
    let mut blue_plane = ChannelPlane::new();
    let mut alpha_plane = ChannelPlane::new();

    for tile in bbox.tile_coords() {
        let tile_bbox = tile.to_bbox();
        let x0 = tile_bbox.min_x.max(bbox.min_x);
        let y0 = tile_bbox.min_y.max(bbox.min_y);
        let x1 = tile_bbox.max_x.min(bbox.max_x);
        let y1 = tile_bbox.max_y.min(bbox.max_y);

        let mut red_tile = TileBuffer::new_dense(0.0, governor)?;
        let mut green_tile = TileBuffer::new_dense(0.0, governor)?;
        let mut blue_tile = TileBuffer::new_dense(0.0, governor)?;
        let mut alpha_tile = TileBuffer::new_dense(1.0, governor)?;

        for nuke_y in y0..y1 {
            for nuke_x in x0..x1 {
                let exr_x = nuke_x.saturating_add(display_min_x);
                let exr_y = display_max_y.saturating_sub(1).saturating_sub(nuke_y);

                let index = flat_index(exr_x, exr_y, data_min_x, data_min_y, data_width);

                let red_value = sample_channel(red.or(luma), index).unwrap_or(0.0);
                let green_value = sample_channel(green.or(luma), index).unwrap_or(0.0);
                let blue_value = sample_channel(blue.or(luma), index).unwrap_or(0.0);
                let alpha_value = sample_channel(alpha, index).unwrap_or(1.0);

                let (_, lx, ly) = world_to_tile(nuke_x, nuke_y);
                red_tile.set(lx, ly, red_value, governor)?;
                green_tile.set(lx, ly, green_value, governor)?;
                blue_tile.set(lx, ly, blue_value, governor)?;
                alpha_tile.set(lx, ly, alpha_value, governor)?;
            }
        }

        red_plane.insert(tile, Arc::new(red_tile));
        green_plane.insert(tile, Arc::new(green_tile));
        blue_plane.insert(tile, Arc::new(blue_tile));
        alpha_plane.insert(tile, Arc::new(alpha_tile));
    }

    raster.insert_channel(Channel::RgbaRed, red_plane);
    raster.insert_channel(Channel::RgbaGreen, green_plane);
    raster.insert_channel(Channel::RgbaBlue, blue_plane);
    raster.insert_channel(Channel::RgbaAlpha, alpha_plane);

    Ok(raster)
}

fn find_channel<'a>(
    layer: &'a Layer<AnyChannels<FlatSamples>>,
    name: &str,
) -> Option<&'a FlatSamples> {
    layer.channel_data.list.iter().find_map(|channel| {
        channel_matches(channel.name.as_slice(), name).then_some(&channel.sample_data)
    })
}

/// Matches both a bare channel name and a layered one (`beauty.R`).
fn channel_matches(channel: &[u8], name: &str) -> bool {
    let name = name.as_bytes();
    channel == name
        || (channel.len() > name.len()
            && channel.ends_with(name)
            && channel[channel.len() - name.len() - 1] == b'.')
}

fn flat_index(
    exr_x: i32,
    exr_y: i32,
    data_min_x: i32,
    data_min_y: i32,
    data_width: usize,
) -> Option<usize> {
    let x = exr_x.checked_sub(data_min_x)?;
    let y = exr_y.checked_sub(data_min_y)?;
    if x < 0 || y < 0 {
        return None;
    }
    let x = usize::try_from(x).ok()?;
    let y = usize::try_from(y).ok()?;
    if x >= data_width {
        return None;
    }
    y.checked_mul(data_width)?.checked_add(x)
}

fn sample_channel(samples: Option<&FlatSamples>, index: Option<usize>) -> Option<f32> {
    let index = index?;
    match samples? {
        FlatSamples::F16(values) => values.get(index).map(|value| value.to_f32()),
        FlatSamples::F32(values) => values.get(index).copied(),
        FlatSamples::U32(values) => values.get(index).map(|value| *value as f32),
    }
}

fn i32_from_usize(value: usize) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use exr::prelude::{write_rgb_file, write_rgba_file};

    use super::*;

    static NEXT_FILE: AtomicU64 = AtomicU64::new(0);

    fn temp_exr(tag: &str) -> PathBuf {
        let id = NEXT_FILE.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("tanova-exr-{}-{tag}-{id}.exr", std::process::id()))
    }

    #[test]
    fn loads_rgba_and_inverts_y() {
        let path = temp_exr("rgba");
        write_rgba_file(&path, 2, 2, |x, y| match (x, y) {
            (0, 0) => (1.0_f32, 0.0, 0.0, 1.0),
            (1, 0) => (0.0, 1.0, 0.0, 1.0),
            (0, 1) => (0.0, 0.0, 1.0, 0.5),
            _ => (0.25, 0.25, 0.25, 1.0),
        })
        .expect("write test EXR");

        let raster = load_exr(path.to_str().expect("utf-8 path"), None).expect("load EXR");
        let _ = std::fs::remove_file(&path);

        assert_eq!(raster.format.width, 2);
        assert_eq!(raster.format.height, 2);
        assert_eq!(raster.bbox, BBox::new(0, 0, 2, 2));

        // EXR (0, 0) is the top-left pixel; in Bottom-Left space that is y = 1.
        assert_eq!(raster.sample_rgba(0, 1), [1.0, 0.0, 0.0, 1.0]);
        // EXR (0, 1) is the bottom-left pixel; in Bottom-Left space that is y = 0.
        assert_eq!(raster.sample_rgba(0, 0), [0.0, 0.0, 1.0, 0.5]);
        assert_eq!(raster.sample_rgba(1, 1), [0.0, 1.0, 0.0, 1.0]);
    }

    #[test]
    fn missing_alpha_defaults_to_opaque() {
        let path = temp_exr("rgb");
        write_rgb_file(&path, 2, 2, |x, y| {
            (x as f32 / 2.0, y as f32 / 2.0, 0.5_f32)
        })
        .expect("write test EXR");

        let raster = load_exr(path.to_str().expect("utf-8 path"), None).expect("load EXR");
        let _ = std::fs::remove_file(&path);

        for y in 0..2 {
            for x in 0..2 {
                assert_eq!(raster.sample_rgba(x, y)[3], 1.0);
            }
        }
    }

    #[test]
    fn missing_file_is_an_error_not_a_panic() {
        let path = temp_exr("missing");
        let result = load_exr(path.to_str().expect("utf-8 path"), None);
        assert!(result.is_err());
    }

    #[test]
    fn governor_denial_propagates() {
        let path = temp_exr("governor");
        write_rgba_file(&path, 256, 256, |_, _| (1.0_f32, 1.0, 1.0, 1.0)).expect("write test EXR");

        let governor = MemoryGovernor::new(0);
        let result = load_exr(path.to_str().expect("utf-8 path"), Some(&governor));
        let _ = std::fs::remove_file(&path);

        assert!(matches!(result, Err(NodeError::MemoryExceeded { .. })));
    }
}

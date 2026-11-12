//! Safe frame-sequence path expansion.
//!
//! Timelines hand us fractional times; file names need integers. The resolver
//! rounds first, then expands the common VFX padding tokens so a missing token
//! never turns into a panic or a malformed path.

/// Expands sequence tokens in `path` for `frame`.
///
/// `frame` is rounded to the nearest integer, so a sub-frame request such as
/// `12.8` resolves to `13`. Supported tokens are `%04d`, `%03d`, `####`, and
/// `###`. Tokens without a match are left untouched.
///
/// ```
/// use tanova_io::resolve_sequence_path;
///
/// assert_eq!(
///     resolve_sequence_path("plate.%04d.exr", 12.0),
///     "plate.0012.exr"
/// );
/// assert_eq!(
///     resolve_sequence_path("plate.####.exr", 12.8),
///     "plate.0013.exr"
/// );
/// ```
pub fn resolve_sequence_path(path: &str, frame: f64) -> String {
    let frame = frame.round() as i64;
    let mut resolved = path.to_string();

    for (token, width) in [("%04d", 4), ("%03d", 3), ("####", 4), ("###", 3)] {
        if resolved.contains(token) {
            let padded = format!("{frame:0width$}");
            resolved = resolved.replace(token, &padded);
        }
    }

    resolved
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounds_sub_frames_before_padding() {
        assert_eq!(
            resolve_sequence_path("plate.%04d.exr", 12.8),
            "plate.0013.exr"
        );
        assert_eq!(
            resolve_sequence_path("plate.%04d.exr", 12.2),
            "plate.0012.exr"
        );
    }

    #[test]
    fn expands_hash_tokens() {
        assert_eq!(
            resolve_sequence_path("beauty.####.exr", 7.0),
            "beauty.0007.exr"
        );
        assert_eq!(
            resolve_sequence_path("beauty.###.exr", 7.0),
            "beauty.007.exr"
        );
    }

    #[test]
    fn leaves_plain_paths_untouched() {
        assert_eq!(
            resolve_sequence_path("single_plate.exr", 42.0),
            "single_plate.exr"
        );
    }

    #[test]
    fn expands_every_token_in_a_path() {
        assert_eq!(
            resolve_sequence_path("shot_%03d/plate.####.exr", 5.0),
            "shot_005/plate.0005.exr"
        );
    }

    #[test]
    fn keeps_negative_frames_parseable() {
        assert_eq!(
            resolve_sequence_path("plate.%04d.exr", -1.0),
            "plate.-001.exr"
        );
    }
}

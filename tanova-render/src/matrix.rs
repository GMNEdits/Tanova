//! 3x3 affine matrices for 2D transforms.
//!
//! Tanova's spatial math is Bottom-Left with `+Y` up, so [`Mat3x3::rotate`]
//! rotates counter-clockwise. Matrices are row-major and multiplied as
//! `self * other`, which means `other` is applied to a point first.

/// A row-major 3x3 matrix used for 2D affine transforms.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Mat3x3 {
    rows: [[f32; 3]; 3],
}

impl Default for Mat3x3 {
    fn default() -> Self {
        Self::identity()
    }
}

impl Mat3x3 {
    /// The identity transform.
    pub const fn identity() -> Self {
        Self {
            rows: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
        }
    }

    /// Translation by `(tx, ty)`.
    pub const fn translate(tx: f32, ty: f32) -> Self {
        Self {
            rows: [[1.0, 0.0, tx], [0.0, 1.0, ty], [0.0, 0.0, 1.0]],
        }
    }

    /// Non-uniform scale around the origin.
    pub const fn scale(sx: f32, sy: f32) -> Self {
        Self {
            rows: [[sx, 0.0, 0.0], [0.0, sy, 0.0], [0.0, 0.0, 1.0]],
        }
    }

    /// Counter-clockwise rotation by `radians`.
    pub fn rotate(radians: f32) -> Self {
        let (sin, cos) = radians.sin_cos();
        Self {
            rows: [[cos, -sin, 0.0], [sin, cos, 0.0], [0.0, 0.0, 1.0]],
        }
    }

    /// Matrix product `self * other`.
    ///
    /// `other` is applied to a point first, then `self`.
    pub fn mul(&self, other: &Self) -> Self {
        let mut rows = [[0.0f32; 3]; 3];
        for (row_index, row) in rows.iter_mut().enumerate() {
            for (column, value) in row.iter_mut().enumerate() {
                *value = (0..3)
                    .map(|k| self.rows[row_index][k] * other.rows[k][column])
                    .sum();
            }
        }
        Self { rows }
    }

    /// Inverts the matrix, or returns `None` when it is singular.
    pub fn inverse(&self) -> Option<Self> {
        let [[a, b, c], [d, e, f], [g, h, i]] = self.rows;
        let determinant = a * (e * i - f * h) - b * (d * i - f * g) + c * (d * h - e * g);
        if determinant.abs() < 1e-12 || !determinant.is_finite() {
            return None;
        }
        let inv_det = 1.0 / determinant;
        Some(Self {
            rows: [
                [
                    (e * i - f * h) * inv_det,
                    (c * h - b * i) * inv_det,
                    (b * f - c * e) * inv_det,
                ],
                [
                    (f * g - d * i) * inv_det,
                    (a * i - c * g) * inv_det,
                    (c * d - a * f) * inv_det,
                ],
                [
                    (d * h - e * g) * inv_det,
                    (b * g - a * h) * inv_det,
                    (a * e - b * d) * inv_det,
                ],
            ],
        })
    }

    /// Transforms one point, dividing by the homogeneous `w` when needed.
    pub fn transform_point(&self, x: f32, y: f32) -> (f32, f32) {
        let [[a, b, c], [d, e, f], [g, h, i]] = self.rows;
        let transformed_x = a * x + b * y + c;
        let transformed_y = d * x + e * y + f;
        let w = g * x + h * y + i;
        if w.abs() > 1e-12 {
            (transformed_x / w, transformed_y / w)
        } else {
            (transformed_x, transformed_y)
        }
    }

    /// One row padded to a `vec4` for WGSL uniform upload.
    pub fn row(&self, index: usize) -> [f32; 4] {
        let row = self.rows[index.min(2)];
        [row[0], row[1], row[2], 0.0]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_close(actual: Mat3x3, expected: Mat3x3) {
        for row in 0..3 {
            for column in 0..3 {
                assert!(
                    (actual.rows[row][column] - expected.rows[row][column]).abs() < 1e-5,
                    "row {row} column {column}: {} vs {}",
                    actual.rows[row][column],
                    expected.rows[row][column]
                );
            }
        }
    }

    #[test]
    fn scale_and_rotation_inverses_match_the_opposite_transform() {
        assert_close(
            Mat3x3::scale(2.0, 2.0).inverse().expect("invertible"),
            Mat3x3::scale(0.5, 0.5),
        );
        assert_close(
            Mat3x3::rotate(std::f32::consts::FRAC_PI_4)
                .inverse()
                .expect("invertible"),
            Mat3x3::rotate(-std::f32::consts::FRAC_PI_4),
        );
    }

    #[test]
    fn multiplication_applies_the_right_hand_side_first() {
        // Translate after scaling: (1, 1) -> (2, 2) -> (12, 7).
        let matrix = Mat3x3::translate(10.0, 5.0).mul(&Mat3x3::scale(2.0, 2.0));
        assert_eq!(matrix.transform_point(1.0, 1.0), (12.0, 7.0));
    }

    #[test]
    fn rotation_turns_counter_clockwise_in_bottom_left_space() {
        let (x, y) = Mat3x3::rotate(std::f32::consts::FRAC_PI_2).transform_point(1.0, 0.0);
        assert!(x.abs() < 1e-6 && (y - 1.0).abs() < 1e-6, "got ({x}, {y})");
    }

    #[test]
    fn singular_matrices_have_no_inverse() {
        assert!(Mat3x3::scale(0.0, 1.0).inverse().is_none());
    }
}

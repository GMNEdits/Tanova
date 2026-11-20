//! 3x3 affine matrices for 2D transforms.
//!
//! Tanova's spatial math is Bottom-Left with `+Y` up, so [`Mat3x3::rotate`]
//! rotates counter-clockwise. Matrices are row-major and multiplied as
//! `self * other`, which means `other` is applied to a point first.

use crate::Raster2D;

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

    /// Projects one point through the homogeneous divide.
    ///
    /// Computes `(wx, wy, w) = self * [x, y, 1]^T`. Returns `None` when `w`
    /// is zero or non-finite, else `Some([wx / w, wy / w])`.
    pub fn project_point(&self, x: f32, y: f32) -> Option<[f32; 2]> {
        let [[a, b, c], [d, e, f], [g, h, i]] = self.rows;
        let wx = a * x + b * y + c;
        let wy = d * x + e * y + f;
        let w = g * x + h * y + i;
        if !w.is_finite() || w.abs() <= 1e-6 {
            return None;
        }
        Some([wx / w, wy / w])
    }

    /// Full projective inverse via the cofactor/adjugate matrix divided by
    /// `det`, handling arbitrary non-affine bottom rows `[h20, h21, h22]`.
    ///
    /// Returns `None` when the determinant is zero or non-finite.
    pub fn inverse_projective(&self) -> Option<Self> {
        let [[a, b, c], [d, e, f], [g, h, i]] = self.rows;
        let determinant = a * (e * i - f * h) - b * (d * i - f * g) + c * (d * h - e * g);
        if !determinant.is_finite() || determinant.abs() <= 1e-9 {
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

    /// Solves for the projective matrix mapping `from[i]` to `to[i]` for all
    /// four corners, using the 8-point Direct Linear Transform in `f64`.
    ///
    /// `h22` is fixed to `1.0`. Returns `None` for collinear or duplicate
    /// corners (a singular 8x8 system) or when any entry is non-finite.
    #[allow(clippy::needless_range_loop)]
    pub fn from_quad_to_quad(from: [[f32; 2]; 4], to: [[f32; 2]; 4]) -> Option<Self> {
        let mut system = [[0.0f64; 9]; 8];
        for i in 0..4 {
            let x = f64::from(from[i][0]);
            let y = f64::from(from[i][1]);
            let u = f64::from(to[i][0]);
            let v = f64::from(to[i][1]);

            let row = &mut system[i * 2];
            row[0] = x;
            row[1] = y;
            row[2] = 1.0;
            row[3] = 0.0;
            row[4] = 0.0;
            row[5] = 0.0;
            row[6] = -u * x;
            row[7] = -u * y;
            row[8] = u;

            let row = &mut system[i * 2 + 1];
            row[0] = 0.0;
            row[1] = 0.0;
            row[2] = 0.0;
            row[3] = x;
            row[4] = y;
            row[5] = 1.0;
            row[6] = -v * x;
            row[7] = -v * y;
            row[8] = v;
        }

        // Gaussian elimination with partial pivoting.
        for column in 0..8 {
            let mut pivot_row = column;
            let mut pivot = system[column][column].abs();
            for row in (column + 1)..8 {
                let value = system[row][column].abs();
                if value > pivot {
                    pivot = value;
                    pivot_row = row;
                }
            }
            if pivot <= 1e-12 {
                return None;
            }
            system.swap(column, pivot_row);
            let pivot_value = system[column][column];
            for row in (column + 1)..8 {
                let factor = system[row][column] / pivot_value;
                for c in column..9 {
                    system[row][c] -= factor * system[column][c];
                }
            }
        }

        let mut h = [0.0f64; 8];
        for column in (0..8).rev() {
            let mut value = system[column][8];
            for c in (column + 1)..8 {
                value -= system[column][c] * h[c];
            }
            value /= system[column][column];
            if !value.is_finite() {
                return None;
            }
            h[column] = value;
        }

        Some(Self {
            rows: [
                [h[0] as f32, h[1] as f32, h[2] as f32],
                [h[3] as f32, h[4] as f32, h[5] as f32],
                [h[6] as f32, h[7] as f32, 1.0],
            ],
        })
    }

    /// One row padded to a `vec4` for WGSL uniform upload.
    pub fn row(&self, index: usize) -> [f32; 4] {
        let row = self.rows[index.min(2)];
        [row[0], row[1], row[2], 0.0]
    }
}

/// A concatenated 2D affine transform chain carried inside a [`tanova_kernel::Packet`].
#[derive(Debug, Clone)]
pub struct TransformChain {
    /// The upstream source raster before any spatial resampling in this chain.
    pub source_raster: std::sync::Arc<Raster2D>,
    /// The combined forward `Mat3x3` accumulated across contiguous `Transform` nodes.
    pub combined_matrix: Mat3x3,
}

impl TransformChain {
    /// Starts a new transform chain from `source_raster` and an initial `matrix`.
    pub fn new(source_raster: std::sync::Arc<Raster2D>, matrix: Mat3x3) -> Self {
        Self {
            source_raster,
            combined_matrix: matrix,
        }
    }

    /// Concatenates a downstream `next_matrix` onto the accumulated chain
    /// (`next_matrix * self.combined_matrix`).
    pub fn chain(&self, next_matrix: &Mat3x3) -> Self {
        Self {
            source_raster: std::sync::Arc::clone(&self.source_raster),
            combined_matrix: next_matrix.mul(&self.combined_matrix),
        }
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

    #[test]
    fn transform_chain_concatenates_scale_and_inverse_rotation_to_identity() {
        let raster = std::sync::Arc::new(crate::Raster2D::new(
            crate::coords::Format::hd_1080(),
            crate::coords::BBox::new(0, 0, 10, 10),
            crate::coords::EdgeMode::BlackOutside,
        ));
        let chain = TransformChain::new(raster.clone(), Mat3x3::scale(0.5, 0.5));
        let chained = chain.chain(&Mat3x3::scale(2.0, 2.0));
        assert!(std::sync::Arc::ptr_eq(
            &chained.source_raster,
            &chain.source_raster
        ));
        let mut max_diff = 0.0f32;
        for row in 0..3 {
            for column in 0..3 {
                let diff = (chained.combined_matrix.rows[row][column]
                    - Mat3x3::identity().rows[row][column])
                    .abs();
                max_diff = max_diff.max(diff);
            }
        }
        assert!(
            max_diff < 1e-6,
            "combined matrix not identity: {chained:?}, max diff {max_diff}"
        );
    }

    #[test]
    fn homography_maps_all_four_corners_and_inverts_cleanly() {
        let from = [[0.0, 0.0], [256.0, 0.0], [256.0, 256.0], [0.0, 256.0]];
        let to = [[20.0, 10.0], [230.0, 30.0], [250.0, 240.0], [10.0, 220.0]];

        let matrix = Mat3x3::from_quad_to_quad(from, to).expect("quad must solve");
        for i in 0..4 {
            let projected = matrix
                .project_point(from[i][0], from[i][1])
                .expect("corner must project");
            assert!(
                (projected[0] - to[i][0]).abs() < 1e-4,
                "corner {i} x: {} vs {}",
                projected[0],
                to[i][0]
            );
            assert!(
                (projected[1] - to[i][1]).abs() < 1e-4,
                "corner {i} y: {} vs {}",
                projected[1],
                to[i][1]
            );
        }

        let inverse = matrix.inverse_projective().expect("matrix must invert");
        for i in 0..4 {
            let back = inverse
                .project_point(to[i][0], to[i][1])
                .expect("corner must back-project");
            assert!(
                (back[0] - from[i][0]).abs() < 1e-4,
                "corner {i} x: {} vs {}",
                back[0],
                from[i][0]
            );
            assert!(
                (back[1] - from[i][1]).abs() < 1e-4,
                "corner {i} y: {} vs {}",
                back[1],
                from[i][1]
            );
        }

        // Collinear or duplicate corners are singular.
        let collinear = [[0.0, 0.0], [1.0, 1.0], [2.0, 2.0], [3.0, 3.0]];
        assert!(Mat3x3::from_quad_to_quad(collinear, to).is_none());
        assert!(Mat3x3::from_quad_to_quad(from, collinear).is_none());
        let duplicate = [[0.0, 0.0], [0.0, 0.0], [256.0, 256.0], [0.0, 256.0]];
        assert!(Mat3x3::from_quad_to_quad(duplicate, to).is_none());
    }
}

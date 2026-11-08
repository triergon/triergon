//! Projective transforms and their quantisation into bitmap keys.
//!
//! libass rasterises a glyph once per distinct quantised transform: the output
//! origin moves to the transformed bounding-box centre, which is rounded to 1/8
//! pixel, and the remaining matrix is rounded to steps that keep the position
//! error near 1/8 pixel. [`BitmapTransform`] is that quantised key;
//! [`BitmapTransform::restore`] rebuilds the matrix the bitmap is drawn with.

use crate::fp;
use crate::geometry::{Rect, Vector};

/// Transform error estimate, in 1/64 pixel.
const POSITION_PRECISION: f64 = 8.0;
/// The largest perspective magnification relative to the box centre.
pub(crate) const MAX_PERSP_SCALE: f64 = 16.0;
/// log2 of the sub-pixel steps per pixel: 1/8 pixel.
const SUBPIXEL_ORDER: u32 = 3;
/// Quantised magnitudes at or beyond this are rejected.
const MAX_QUANTIZED: f64 = 1_000_000.0;

/// A 3×3 projective transform of 26.6 coordinates:
/// `x' = (m₀₀x + m₀₁y + m₀₂) / z`, `y' = (m₁₀x + m₁₁y + m₁₂) / z`,
/// `z = m₂₀x + m₂₁y + m₂₂`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Matrix(pub [[f64; 3]; 3]);

impl Matrix {
    pub const IDENTITY: Self = Self([[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]);

    /// Row `i` applied to (x, y, 1): `mᵢ₀x + mᵢ₁y + mᵢ₂`, in that order.
    #[inline(always)]
    pub(crate) fn row(&self, i: usize, x: f64, y: f64) -> f64 {
        let r = &self.0[i];
        r[0] * x + r[1] * y + r[2]
    }
}

/// How [`Outline::transform_into`](crate::Outline::transform_into) applies a
/// [`Matrix`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Projection {
    /// The first two rows only: `x' = m₀₀x + m₀₁y + m₀₂`, `y' = m₁₀x + m₁₁y + m₁₂`.
    Affine,
    /// All three rows, the depth clamped to at least 0.1.
    Perspective,
}

/// The sub-pixel remainder of a run's first glyph, in 1/8 pixel; the run's later
/// glyphs and borders are positioned against it so the run keeps its spacing.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct RunOffset {
    pub(crate) x: f64,
    pub(crate) y: f64,
}

/// A quantised transform: the bitmap-cache key of one outline's rendering.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub(crate) struct BitmapTransform {
    /// Sub-pixel position of the box centre, in 1/8 pixel (0..8).
    offset: Vector,
    /// Quantised first row (x and y coefficients).
    x: Vector,
    /// Quantised second row.
    y: Vector,
    /// Quantised perspective row; zero for affine transforms.
    z: Vector,
}

/// The result of [`BitmapTransform::quantize`].
#[derive(Clone, Copy, Debug)]
pub(crate) struct Quantized {
    /// The whole-pixel position of the bitmap origin.
    pub(crate) position: Vector,
    pub(crate) key: BitmapTransform,
    /// The sub-pixel remainder of the box centre: the run's offset when this
    /// bitmap starts a run.
    pub(crate) remainder: RunOffset,
    /// The matrix with its input origin at the box centre and its output origin
    /// at the transformed centre.
    pub(crate) centred: Matrix,
}

/// The centre and half extents of a control box, widened by a pixel.
struct BoxFrame {
    centre: [f64; 2],
    half: [f64; 2],
}

impl BoxFrame {
    fn new(cbox: &Rect) -> Self {
        Self {
            centre: [
                f64::from(cbox.x_min + cbox.x_max) / 2.0,
                f64::from(cbox.y_min + cbox.y_max) / 2.0,
            ],
            half: [
                f64::from(cbox.x_max - cbox.x_min) / 2.0 + 64.0,
                f64::from(cbox.y_max - cbox.y_min) / 2.0 + 64.0,
            ],
        }
    }
}

impl BitmapTransform {
    /// Quantises `m` for an outline with control box `cbox`, placed against the
    /// remainder of the run it continues (`None` for a run's first bitmap, or a
    /// bitmap of its own). `None` when the transform is degenerate or too large.
    pub(crate) fn quantize(m: &Matrix, cbox: &Rect, run: Option<RunOffset>) -> Option<Quantized> {
        let frame = BoxFrame::new(cbox);
        let [x0, y0] = frame.centre;
        let [dx, dy] = frame.half;
        let mut centred = *m;
        let m = &mut centred.0;
        for row in m.iter_mut() {
            row[2] += row[0] * x0 + row[1] * y0;
        }
        if m[2][2] <= 0.0 {
            return None;
        }
        let w = 1.0 / m[2][2];
        let mut centre = [m[0][2] * w, m[1][2] * w];
        for i in 0..2 {
            for j in 0..2 {
                m[i][j] -= m[2][j] * centre[i];
            }
        }
        let delta = run.map_or([0.0, 0.0], |offset| [offset.x, offset.y]);
        let mut qr = [0i32; 2];
        for i in 0..2 {
            centre[i] /= f64::from(64 >> SUBPIXEL_ORDER);
            centre[i] -= delta[i];
            if !(centre[i].abs() < MAX_QUANTIZED) {
                return None;
            }
            qr[i] = fp::lrint32(centre[i]);
        }

        // The least depth over the box, limited to a 1/16 of the centre depth.
        let z0 = m[2][2] - m[2][0].abs() * dx - m[2][1].abs() * dy;
        let w = 1.0 / POSITION_PRECISION / fp::max(z0, m[2][2] / MAX_PERSP_SCALE);
        let mut mul = [dx * w, dy * w];
        let quantize = |value: f64| (value.abs() < MAX_QUANTIZED).then(|| fp::lrint32(value));
        let mut qm = [[0i32; 2]; 3];
        for i in 0..2 {
            for j in 0..2 {
                qm[i][j] = quantize(m[i][j] * mul[j])?;
            }
        }
        let qmx = qm[0][0].abs() + qm[0][1].abs();
        let qmy = qm[1][0].abs() + qm[1][1].abs();
        let w = POSITION_PRECISION * f64::from(qmx.max(qmy));
        mul[0] *= w;
        mul[1] *= w;
        for j in 0..2 {
            qm[2][j] = quantize(m[2][j] * mul[j])?;
        }

        let mask = (1 << SUBPIXEL_ORDER) - 1;
        Some(Quantized {
            position: Vector::new(qr[0] >> SUBPIXEL_ORDER, qr[1] >> SUBPIXEL_ORDER),
            key: Self {
                offset: Vector::new(qr[0] & mask, qr[1] & mask),
                x: Vector::new(qm[0][0], qm[0][1]),
                y: Vector::new(qm[1][0], qm[1][1]),
                z: Vector::new(qm[2][0], qm[2][1]),
            },
            remainder: RunOffset {
                x: centre[0] - f64::from(qr[0]),
                y: centre[1] - f64::from(qr[1]),
            },
            centred,
        })
    }

    /// How outlines are drawn under the restored matrix: in perspective when
    /// the transform has a perspective row.
    pub(crate) const fn projection(&self) -> Projection {
        if self.z.x != 0 || self.z.y != 0 {
            Projection::Perspective
        } else {
            Projection::Affine
        }
    }

    /// The matrix the bitmap is drawn with, for an outline with control box
    /// `cbox`: depth 1 at the nearest box corner, output origin at the bitmap
    /// origin plus the sub-pixel offset.
    pub(crate) fn restore(&self, cbox: &Rect) -> Matrix {
        let frame = BoxFrame::new(cbox);
        let [x0, y0] = frame.centre;
        let [dx, dy] = frame.half;
        let q_x = POSITION_PRECISION / dx;
        let q_y = POSITION_PRECISION / dy;
        let mut m = [[0.0; 3]; 3];
        m[0][0] = f64::from(self.x.x) * q_x;
        m[0][1] = f64::from(self.x.y) * q_y;
        m[1][0] = f64::from(self.y.x) * q_x;
        m[1][1] = f64::from(self.y.y) * q_y;
        let qmx = self.x.x.abs() + self.x.y.abs();
        let qmy = self.y.x.abs() + self.y.y.abs();
        let scale_z = 1.0 / POSITION_PRECISION / f64::from(qmx.max(qmy));
        m[2][0] = f64::from(self.z.x) * q_x * scale_z;
        m[2][1] = f64::from(self.z.y) * q_y * scale_z;
        m[2][2] = 1.0 + m[2][0].abs() * dx + m[2][1].abs() * dy;
        m[2][2] = fp::min(m[2][2], MAX_PERSP_SCALE);
        let step = 64 >> SUBPIXEL_ORDER;
        let centre = [
            f64::from(self.offset.x * step),
            f64::from(self.offset.y * step),
        ];
        for i in 0..2 {
            for j in 0..3 {
                m[i][j] += m[2][j] * centre[i];
            }
        }
        for row in &mut m {
            row[2] -= row[0] * x0 + row[1] * y0;
        }
        Matrix(m)
    }
}

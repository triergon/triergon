//! What the stroker offsets: the pen's tolerances, normals in normal space,
//! and the segments the pen draws along an outline's curves.

use super::Pen;
use super::border::BySide;
use super::cubic::Cubic;
use super::quadratic::Quadratic;
use super::spline::Spline;
use crate::geometry::{DVector, Vector};
use crate::outline::Curve;

/// The pen in the stroker's terms, fixed for one outline: its half widths,
/// the normal space they scale to, and the error bounds of the offset.
#[derive(Clone, Copy, Debug)]
pub(super) struct Tolerances {
    /// The half widths along x and y.
    width: DVector,
    /// The scales into normal space: 1 / max(tolerance, width) per axis.
    scale: DVector,
    /// Steps shorter than this along both axes have no direction.
    eps: i32,
    /// Cosine of the widest corner joined without a cap.
    pub(super) merge_cos: f64,
    /// Cosine of the widest arc one quadratic spline approximates.
    pub(super) split_cos: f64,
    /// Normal-space length below which a derivative has no direction.
    pub(super) min_len: f64,
    /// Radial error bound of the quadratic fit.
    pub(super) err_q: f64,
    /// Radial error bound of the cubic fit.
    pub(super) err_c: f64,
    /// Tangent of the angular error bound.
    pub(super) err_a: f64,
}

impl Tolerances {
    pub(super) fn new(pen: &Pen) -> Self {
        let (width, eps) = (pen.width, pen.tolerance);
        let rel_err = f64::from(eps) / f64::from(width.x.max(width.y));
        let e = (2.0 * rel_err).sqrt();
        Self {
            width: DVector::new(f64::from(width.x), f64::from(width.y)),
            scale: DVector::new(
                1.0 / f64::from(eps.max(width.x)),
                1.0 / f64::from(eps.max(width.y)),
            ),
            eps,
            merge_cos: 1.0 - rel_err,
            split_cos: 1.0 + 8.0 * rel_err - 4.0 * (1.0 + rel_err) * e,
            min_len: rel_err / 4.0,
            err_q: 8.0 * (1.0 + rel_err) * (1.0 + rel_err),
            err_c: 390.0 * rel_err * rel_err,
            err_a: e,
        }
    }

    /// The step from `from` to `to` in normal space, turned a quarter:
    /// (dy / y width, −dx / x width).
    pub(super) fn derivative(&self, from: Vector, to: Vector) -> DVector {
        let (dx, dy) = (to.x.wrapping_sub(from.x), to.y.wrapping_sub(from.y));
        DVector::new(f64::from(dy) * self.scale.y, f64::from(-dx) * self.scale.x)
    }

    /// The derivative from `from` to `to`, or `None` when the step is shorter
    /// than the tolerance along both axes and has no direction.
    pub(super) fn leg(&self, from: Vector, to: Vector) -> Option<DVector> {
        let (dx, dy) = (to.x.wrapping_sub(from.x), to.y.wrapping_sub(from.y));
        let eps = self.eps;
        let negligible = dx > -eps && dx < eps && dy > -eps && dy < eps;
        (!negligible).then(|| self.derivative(from, to))
    }

    /// `at` displaced by +`offset` and −`offset` scaled by the widths and
    /// truncated: the points of the outer and inner sides.
    pub(super) fn offsets(&self, at: Vector, offset: DVector) -> BySide<Vector> {
        let dx = (self.width.x * offset.x) as i32;
        let dy = (self.width.y * offset.y) as i32;
        BySide {
            outer: Vector::new(at.x.wrapping_add(dx), at.y.wrapping_add(dy)),
            inner: Vector::new(at.x.wrapping_sub(dx), at.y.wrapping_sub(dy)),
        }
    }
}

/// A unit normal and the length of the derivative it came from.
#[derive(Clone, Copy, Debug)]
pub(super) struct Normal {
    pub(super) unit: DVector,
    pub(super) len: f64,
}

impl Normal {
    pub(super) fn of(leg: DVector) -> Self {
        let len = leg.len();
        Self {
            unit: leg.scale(1.0 / len),
            len,
        }
    }

    /// The normal of `leg`, or `None` when it is shorter than `min_len` and
    /// gives no direction.
    pub(super) fn directed(leg: DVector, min_len: f64) -> Option<Self> {
        let normal = Self::of(leg);
        (normal.len >= min_len).then_some(normal)
    }

    /// The normal of a leg half as long.
    pub(super) fn halved(self) -> Self {
        Self {
            len: self.len / 2.0,
            ..self
        }
    }
}

/// A segment of the source as the pen draws it, from where the pen stands.
#[derive(Clone, Copy, Debug)]
pub(super) enum Segment {
    Line(Line),
    Quadratic(Quadratic),
    Cubic(Cubic),
}

/// A line with a direction.
#[derive(Clone, Copy, Debug)]
pub(super) struct Line {
    pub(super) from: Vector,
    pub(super) to: Vector,
    pub(super) normal: DVector,
}

impl Segment {
    /// The segment from `pen` along `curve`, whose own start the pen stands at
    /// or near. A spline with a leg too short to give a direction is drawn as
    /// a line; `None` when the line is too short as well.
    pub(super) fn new(pen: Vector, curve: Curve, tolerances: &Tolerances) -> Option<Self> {
        let (spline, end) = match curve {
            Curve::Line([_, end]) => (None, end),
            Curve::Quadratic([_, control, end]) => {
                let spline = Quadratic::new([pen, control, end], tolerances);
                (spline.map(Self::Quadratic), end)
            }
            Curve::Cubic([_, c0, c1, end]) => {
                let spline = Cubic::new([pen, c0, c1, end], tolerances);
                (spline.map(Self::Cubic), end)
            }
        };
        spline.or_else(|| Self::line(pen, end, tolerances))
    }

    /// The line from `from` to `to`, or `None` when it has no direction.
    pub(super) fn line(from: Vector, to: Vector, tolerances: &Tolerances) -> Option<Self> {
        let leg = tolerances.leg(from, to)?;
        Some(Self::Line(Line {
            from,
            to,
            normal: Normal::of(leg).unit,
        }))
    }

    pub(super) fn start(&self) -> Vector {
        match self {
            Self::Line(line) => line.from,
            Self::Quadratic(spline) => spline.start(),
            Self::Cubic(spline) => spline.start(),
        }
    }

    pub(super) fn end(&self) -> Vector {
        match self {
            Self::Line(line) => line.to,
            Self::Quadratic(spline) => spline.end(),
            Self::Cubic(spline) => spline.end(),
        }
    }

    pub(super) fn start_normal(&self) -> DVector {
        let [start, _] = match self {
            Self::Line(line) => return line.normal,
            Self::Quadratic(spline) => spline.normals(),
            Self::Cubic(spline) => spline.normals(),
        };
        start.unit
    }
}

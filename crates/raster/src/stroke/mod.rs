//! Border outlines: the ±1 offset curves of an outline in the space scaled by
//! (1 / x width, 1 / y width), as libass strokes `\bord`, `\xbord` and `\ybord`.
//!
//! Lines offset directly. Splines first offset their control points, then the
//! offset curve's radial and angular error is bounded through the control points
//! of products of B-splines; a spline whose bound fails is halved and retried
//! (`quadratic`, `cubic`). Joins wider than the merge angle get round caps of
//! quadratic arcs (`join`), points too close to give a direction are merged, and
//! where the offset exceeds the radius of curvature the self-intersecting part
//! is replaced by a polyline covering only the positive-winding area. Two
//! outlines come out (`output`) because a self-intersecting source needs both
//! offsets.
//!
//! The state is grouped by what owns it: the pen's tolerances, fixed for the
//! whole outline; the two outputs; and the join state of the contour being
//! stroked.

mod cubic;
mod join;
mod output;
mod quadratic;

use triergon_core::{Error, Result, WorkBudget};

use crate::geometry::{DVector, OUTLINE_MAX, Vector};
use crate::outline::{Curve, Outline, SegmentKind};
use crate::set::{Member, Set};
use crate::shape::Shape;
use join::Joint;
use output::Output;

/// How [`Raster::stroke`](crate::Raster::stroke) strokes: the half widths of
/// the border along x and y, and the tolerance the offset curves keep to, all
/// in outline units.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Pen {
    /// The border's half widths along x and y (libass's `xbord` and `ybord`);
    /// the larger must lie in `tolerance..=OUTLINE_MAX`.
    pub width: Vector,
    /// The largest error the offset curves may have (libass's `eps`), at
    /// least 1.
    pub tolerance: i32,
}

/// Strokes `source` with `pen` into `border`, which is cleared first; see
/// [`Raster::stroke`](crate::Raster::stroke).
pub(crate) fn stroke(
    source: &Outline,
    pen: &Pen,
    points_limit: usize,
    border: &mut Shape,
    work: &mut WorkBudget,
) -> Result<()> {
    let radius = pen.width.x.max(pen.width.y);
    if pen.tolerance < 1 || radius < pen.tolerance || radius > OUTLINE_MAX {
        return Err(Error::Invalid("border size out of range"));
    }
    let result = border.rebuild(|outlines| {
        let mut stroker = Stroker {
            tolerance: Tolerances::new(pen),
            out: Output::new(outlines, points_limit),
            joint: Joint::default(),
            last_point: Vector::default(),
            work,
        };
        stroker.outline(source)
    });
    if result.is_err() {
        border.clear();
    }
    result
}

/// One of the two border outlines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    /// The +offset outline.
    Outer,
    /// The −offset outline.
    Inner,
}

impl Member for Side {
    fn bit(self) -> u8 {
        1 << self.index()
    }
}

impl Side {
    const BOTH: [Self; 2] = [Self::Outer, Self::Inner];

    const fn index(self) -> usize {
        match self {
            Self::Outer => 0,
            Self::Inner => 1,
        }
    }

    /// The side a turn with cross product `s` folds over: the inner side of a
    /// counter-clockwise turn, the outer side of a clockwise one.
    fn folded(s: f64) -> Self {
        if s < 0.0 { Self::Outer } else { Self::Inner }
    }
}

type Sides = Set<Side>;

fn both() -> Sides {
    Sides::of(Side::BOTH)
}

/// The pen's tolerances, fixed for one outline.
#[derive(Clone, Copy, Debug)]
struct Tolerances {
    /// The half widths along x and y.
    width: [f64; 2],
    /// The scales into normal space: 1 / max(eps, width) per axis.
    scale: [f64; 2],
    eps: i32,
    /// Cosine of the widest join without a cap.
    merge_cos: f64,
    /// Cosine of the widest arc one quadratic spline approximates.
    split_cos: f64,
    /// Normal-space step below which points are merged.
    min_len: f64,
    /// Radial error bound of the quadratic estimate.
    err_q: f64,
    /// Radial error bound of the cubic estimate.
    err_c: f64,
    /// Tangent of the angular error bound.
    err_a: f64,
}

impl Tolerances {
    fn new(pen: &Pen) -> Self {
        let (border, eps) = (pen.width, pen.tolerance);
        let rel_err = f64::from(eps) / f64::from(border.x.max(border.y));
        let e = (2.0 * rel_err).sqrt();
        Self {
            width: [f64::from(border.x), f64::from(border.y)],
            scale: [
                1.0 / f64::from(eps.max(border.x)),
                1.0 / f64::from(eps.max(border.y)),
            ],
            eps,
            merge_cos: 1.0 - rel_err,
            split_cos: 1.0 + 8.0 * rel_err - 4.0 * (1.0 + rel_err) * e,
            min_len: rel_err / 4.0,
            err_q: 8.0 * (1.0 + rel_err) * (1.0 + rel_err),
            err_c: 390.0 * rel_err * rel_err,
            err_a: e,
        }
    }

    /// Whether `d` is shorter than the tolerance along both axes.
    fn tiny(&self, d: Vector) -> bool {
        let eps = self.eps;
        d.x > -eps && d.x < eps && d.y > -eps && d.y < eps
    }

    /// The normal-space derivative of the step `d`: (dy / ybord, −dx / xbord).
    fn derivative(&self, d: Vector) -> DVector {
        DVector::new(
            f64::from(d.y) * self.scale[1],
            f64::from(-d.x) * self.scale[0],
        )
    }

    /// `pt` displaced by +`offs` and −`offs`, scaled by the widths and
    /// truncated: the outer and inner border points.
    fn displaced(&self, pt: Vector, offs: DVector) -> [Vector; 2] {
        let dx = (self.width[0] * offs.x) as i32;
        let dy = (self.width[1] * offs.y) as i32;
        [
            Vector::new(pt.x.wrapping_add(dx), pt.y.wrapping_add(dy)),
            Vector::new(pt.x.wrapping_sub(dx), pt.y.wrapping_sub(dy)),
        ]
    }
}

/// A unit normal and the length of the derivative it came from.
#[derive(Clone, Copy, Debug)]
struct Normal {
    v: DVector,
    len: f64,
}

impl Normal {
    fn of(deriv: DVector) -> Self {
        let len = deriv.len();
        Self {
            v: deriv.scale(1.0 / len),
            len,
        }
    }

    /// The normal of `deriv`, or `None` when it is shorter than `min_len` and
    /// gives no direction.
    fn directed(deriv: DVector, min_len: f64) -> Option<Self> {
        let len = deriv.len();
        (len >= min_len).then(|| Self {
            v: deriv.scale(1.0 / len),
            len,
        })
    }

    fn halved(self) -> Self {
        Self {
            v: self.v,
            len: self.len / 2.0,
        }
    }
}

fn halve(d: DVector) -> DVector {
    DVector::new(d.x / 2.0, d.y / 2.0)
}

fn mean(a: DVector, b: DVector) -> DVector {
    DVector::new((a.x + b.x) / 2.0, (a.y + b.y) / 2.0)
}

fn sum_scaled(a: DVector, b: DVector, mul: f64) -> DVector {
    DVector::new((a.x + b.x) * mul, (a.y + b.y) * mul)
}

fn sub(a: Vector, b: Vector) -> Vector {
    Vector::new(a.x.wrapping_sub(b.x), a.y.wrapping_sub(b.y))
}

/// The stroker over one source outline.
struct Stroker<'a> {
    tolerance: Tolerances,
    out: Output<'a>,
    joint: Joint,
    /// The end of the last segment drawn.
    last_point: Vector,
    work: &'a mut WorkBudget,
}

impl Stroker<'_> {
    /// Strokes every contour of `source`.
    fn outline(&mut self, source: &Outline) -> Result<()> {
        for contour in source.contours() {
            self.last_point = contour.start;
            for curve in contour.curves {
                match curve {
                    Curve::Line([_, end]) => self.add_line(end, both())?,
                    Curve::Quadratic([_, p1, end]) => self.add_quadratic(p1, end, both())?,
                    Curve::Cubic([_, p1, p2, end]) => self.add_cubic(p1, p2, end, both())?,
                }
            }
            if contour.closed {
                self.close_contour(both())?;
            }
        }
        Ok(())
    }

    /// Adds `pt` displaced by ±`offs` to the outlines of `sides`, starting a
    /// segment of `segment` there when given.
    fn emit(
        &mut self,
        pt: Vector,
        offs: DVector,
        segment: Option<SegmentKind>,
        sides: Sides,
    ) -> Result<()> {
        let points = self.tolerance.displaced(pt, offs);
        self.out.push(points, segment, sides, self.work)
    }

    fn add_line(&mut self, pt1: Vector, sides: Sides) -> Result<()> {
        let d = sub(pt1, self.last_point);
        if self.tolerance.tiny(d) {
            return Ok(());
        }
        let normal = Normal::of(self.tolerance.derivative(d)).v;
        let start = self.last_point;
        self.start_segment(start, normal, sides)?;
        self.emit_first_point(start, SegmentKind::Line, sides)?;
        self.joint.last_normal = normal;
        self.last_point = pt1;
        Ok(())
    }

    fn close_contour(&mut self, mut sides: Sides) -> Result<()> {
        if self.joint.contour_start {
            // Nothing was drawn: the contour is a point, its border a circle.
            if sides == both() {
                sides = Sides::of([Side::Outer]);
            }
            let pt = self.last_point;
            self.draw_circle(pt, sides)?;
        } else {
            let (first_point, first_normal) = (self.joint.first_point, self.joint.first_normal);
            self.add_line(first_point, sides)?;
            self.start_segment(first_point, first_normal, sides)?;
            let open = sides.minus(self.joint.last_skip);
            let tail = open.intersection(self.joint.first_skip);
            let line = Some(SegmentKind::Line);
            self.emit(first_point, first_normal, line, tail)?;
            if self.joint.last_normal != first_normal {
                let head = open.minus(self.joint.first_skip);
                let points = self
                    .tolerance
                    .displaced(first_point, self.joint.last_normal);
                self.out.fix_first_point(points, head);
            }
            self.joint.contour_start = true;
        }
        self.out.close_contour(sides);
        Ok(())
    }
}

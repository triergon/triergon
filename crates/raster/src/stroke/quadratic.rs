//! Offsetting quadratic splines.

use triergon_core::Result;

use super::{Normal, Side, Sides, Stroker, halve, mean, sub, sum_scaled};
use crate::geometry::{DVector, Vector, split_quadratic};
use crate::limits::charge;
use crate::outline::SegmentKind;

/// A quadratic spline being offset: its points, the normal-space derivatives
/// of its two control legs and its end normals.
#[derive(Clone, Copy, Debug)]
struct Quadratic {
    pt: [Vector; 3],
    deriv: [DVector; 2],
    normal: [Normal; 2],
}

impl Stroker<'_> {
    pub(super) fn add_quadratic(&mut self, pt1: Vector, pt2: Vector, sides: Sides) -> Result<()> {
        let d0 = sub(pt1, self.last_point);
        let d1 = sub(pt2, pt1);
        if self.tolerance.tiny(d0) || self.tolerance.tiny(d1) {
            return self.add_line(pt2, sides);
        }
        let pt = [self.last_point, pt1, pt2];
        self.last_point = pt2;
        let deriv = [self.tolerance.derivative(d0), self.tolerance.derivative(d1)];
        let piece = Quadratic {
            pt,
            deriv,
            normal: deriv.map(Normal::of),
        };
        let first = self.joint.contour_start;
        self.start_segment(pt[0], piece.normal[0].v, sides)?;
        self.offset_quadratic(&piece, sides, first)
    }

    /// The offset control point of `q` when its error is within bounds; `c`
    /// and `s` are the dot and cross products of its end normals.
    fn estimate_quadratic(&self, q: &Quadratic, c: f64, s: f64) -> Option<DVector> {
        let tolerance = &self.tolerance;
        let normal = &q.normal;
        // Radial error.
        if !((3.0 + c) * (3.0 + c) < tolerance.err_q * (1.0 + c)) {
            return None;
        }
        let mul = 1.0 / (1.0 + c);
        let l0 = 2.0 * normal[0].len;
        let l1 = 2.0 * normal[1].len;
        let dot0 = l0 + normal[1].len * c;
        let crs0 = (l0 * mul - normal[1].len) * s;
        let dot1 = l1 + normal[0].len * c;
        let crs1 = (l1 * mul - normal[0].len) * s;
        // Angular error.
        if !(crs0.abs() < tolerance.err_a * dot0 && crs1.abs() < tolerance.err_a * dot1) {
            return None;
        }
        Some(sum_scaled(normal[0].v, normal[1].v, mul))
    }

    fn offset_quadratic(&mut self, q: &Quadratic, mut sides: Sides, first: bool) -> Result<()> {
        charge(self.work, 1)?;
        let (pt, normal) = (&q.pt, &q.normal);
        let c = normal[0].v.dot(normal[1].v);
        let s = normal[0].v.cross(normal[1].v);
        let mut check = sides;
        let fold = Side::folded(s);
        if sides.contains(fold) {
            let folded = Sides::of([fold]);
            let abs_s = s.abs();
            let f0 = normal[0].len * c + normal[1].len;
            let f1 = normal[1].len * c + normal[0].len;
            let g0 = normal[0].len * abs_s;
            let g1 = normal[1].len * abs_s;
            if f0 < abs_s && f1 < abs_s {
                // The offset curve may intersect itself.
                let d2 = (f0 * normal[1].len + f1 * normal[0].len) / 2.0;
                if d2 < g0 && d2 < g1 {
                    self.prepare_skip(pt[0], folded, first)?;
                    let line = Some(SegmentKind::Line);
                    if f0 < 0.0 || f1 < 0.0 {
                        self.emit(pt[0], DVector::ZERO, line, folded)?;
                        self.emit(pt[2], DVector::ZERO, line, folded)?;
                    } else {
                        let offs = normal[0].v.scale(f0 / abs_s);
                        self.emit(pt[0], offs, line, folded)?;
                    }
                    sides = sides.without(fold);
                    if sides.is_empty() {
                        self.joint.last_normal = normal[1].v;
                        return Ok(());
                    }
                }
                check = check.toggled(fold);
            } else if c + g0 < 1.0 && c + g1 < 1.0 {
                check = check.toggled(fold);
            }
        }

        if !check.is_empty()
            && let Some(control) = self.estimate_quadratic(q, c, s)
        {
            self.emit_first_point(pt[0], SegmentKind::Quadratic, check)?;
            self.emit(pt[1], control, None, check)?;
            sides = sides.minus(check);
            if sides.is_empty() {
                self.joint.last_normal = normal[1].v;
                return Ok(());
            }
        }
        self.split_quadratic(q, sides, first)
    }

    /// Halves a quadratic whose estimate failed and offsets both halves, or
    /// joins them with a cap where the middle has no direction.
    fn split_quadratic(&mut self, q: &Quadratic, sides: Sides, first: bool) -> Result<()> {
        let next = split_quadratic(&q.pt);
        let d0 = halve(q.deriv[0]);
        let d2 = halve(q.deriv[1]);
        let d1 = mean(d0, d2);
        let Some(middle) = Normal::directed(d1, self.tolerance.min_len) else {
            // Degenerate: the middle has no direction.
            let end = q.normal[1].v;
            self.emit_first_point(next[0], SegmentKind::Line, sides)?;
            self.start_segment(next[2], end, sides)?;
            self.joint.last_skip = self.joint.last_skip.minus(sides);
            return self.emit(next[2], end, Some(SegmentKind::Line), sides);
        };
        let low = Quadratic {
            pt: [next[0], next[1], next[2]],
            deriv: [d0, d1],
            normal: [q.normal[0].halved(), middle],
        };
        let high = Quadratic {
            pt: [next[2], next[3], next[4]],
            deriv: [d1, d2],
            normal: [middle, q.normal[1].halved()],
        };
        self.offset_quadratic(&low, sides, first)?;
        self.offset_quadratic(&high, sides, false)
    }
}

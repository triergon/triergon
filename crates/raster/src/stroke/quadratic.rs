//! Offsetting quadratic splines.

use triergon_core::Result;

use super::Stroker;
use super::border::{Side, Sides};
use super::segment::{Normal, Tolerances};
use super::spline::{Plan, Shortcut, Spline};
use crate::geometry::{DVector, Vector, split_quadratic};
use crate::outline::SegmentKind;

/// A quadratic spline: its points, the normal-space legs of its control
/// polygon and its end normals.
#[derive(Clone, Copy, Debug)]
pub(super) struct Quadratic {
    points: [Vector; 3],
    legs: [DVector; 2],
    normals: [Normal; 2],
}

impl Quadratic {
    /// The spline through `points`, or `None` when a leg has no direction.
    pub(super) fn new(points: [Vector; 3], tolerances: &Tolerances) -> Option<Self> {
        let [p0, p1, p2] = points;
        let legs = [tolerances.leg(p0, p1)?, tolerances.leg(p1, p2)?];
        Some(Self {
            points,
            legs,
            normals: legs.map(Normal::of),
        })
    }

    /// The offset of the control point when it is within the error bounds;
    /// `c` and `s` are the dot and cross products of the end normals.
    fn fit(&self, tolerances: &Tolerances, c: f64, s: f64) -> Option<DVector> {
        let [n0, n1] = self.normals;
        // Radial error.
        if !((3.0 + c) * (3.0 + c) < tolerances.err_q * (1.0 + c)) {
            return None;
        }
        let mul = 1.0 / (1.0 + c);
        let l0 = 2.0 * n0.len;
        let l1 = 2.0 * n1.len;
        let dot0 = l0 + n1.len * c;
        let crs0 = (l0 * mul - n1.len) * s;
        let dot1 = l1 + n0.len * c;
        let crs1 = (l1 * mul - n0.len) * s;
        // Angular error.
        if !(crs0.abs() < tolerances.err_a * dot0 && crs1.abs() < tolerances.err_a * dot1) {
            return None;
        }
        Some((n0.unit + n1.unit).scale(mul))
    }
}

impl Spline for Quadratic {
    const KIND: SegmentKind = SegmentKind::Quadratic;

    type Controls = [(Vector, DVector); 1];

    fn start(&self) -> Vector {
        self.points[0]
    }

    fn end(&self) -> Vector {
        self.points[2]
    }

    fn normals(&self) -> [Normal; 2] {
        self.normals
    }

    fn plan(&self, tolerances: &Tolerances, sides: Sides) -> Plan<Self::Controls> {
        let [n0, n1] = self.normals;
        let c = n0.unit.dot(n1.unit);
        let s = n0.unit.cross(n1.unit);
        let fold = Side::folded(s);
        let mut shortcut = None;
        let mut fitted = sides;
        if sides.contains(fold) {
            let abs_s = s.abs();
            let f0 = n0.len * c + n1.len;
            let f1 = n1.len * c + n0.len;
            let g0 = n0.len * abs_s;
            let g1 = n1.len * abs_s;
            if f0 < abs_s && f1 < abs_s {
                // The offset may intersect itself.
                let d2 = (f0 * n1.len + f1 * n0.len) / 2.0;
                if d2 < g0 && d2 < g1 {
                    shortcut = Some((fold, Shortcut::new([f0, f1], abs_s, n0.unit)));
                }
                fitted = fitted.without(fold);
            } else if c + g0 < 1.0 && c + g1 < 1.0 {
                fitted = fitted.without(fold);
            }
        }
        let fit = if fitted.is_empty() {
            None
        } else {
            self.fit(tolerances, c, s)
                .map(|control| (fitted, [(self.points[1], control)]))
        };
        Plan { shortcut, fit }
    }

    fn split(&self, stroker: &mut Stroker<'_>, sides: Sides) -> Result<()> {
        let [p0, p1, p2, p3, p4] = split_quadratic(&self.points);
        let [n0, n1] = self.normals;
        let first = self.legs[0].halved();
        let last = self.legs[1].halved();
        let middle = first.mean(last);
        let Some(normal) = Normal::directed(middle, stroker.tolerances.min_len) else {
            // The middle has no direction: two lines meeting there.
            stroker.resume(SegmentKind::Line, p0, sides)?;
            stroker.join(p2, n1.unit, sides)?;
            return stroker.resume_along(SegmentKind::Line, p2, n1.unit, sides);
        };
        let low = Self {
            points: [p0, p1, p2],
            legs: [first, middle],
            normals: [n0.halved(), normal],
        };
        let high = Self {
            points: [p2, p3, p4],
            legs: [middle, last],
            normals: [normal, n1.halved()],
        };
        stroker.offset_spline(&low, sides)?;
        stroker.offset_spline(&high, sides)
    }
}

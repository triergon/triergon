//! Offsetting cubic splines.

use triergon_core::Result;

use super::Stroker;
use super::border::{Side, Sides};
use super::segment::{Normal, Tolerances};
use super::spline::{Plan, Shortcut, Spline};
use crate::geometry::{DVector, Vector, split_cubic};
use crate::outline::SegmentKind;

/// A cubic spline: its points, the normal-space derivatives of its first
/// leg, middle and last leg, and its end normals.
#[derive(Clone, Copy, Debug)]
pub(super) struct Cubic {
    points: [Vector; 4],
    legs: [DVector; 3],
    normals: [Normal; 2],
}

/// A cubic's end normals against each other (`c` dot, `s` cross) and
/// against its middle leg (`dc`, `ds`).
#[derive(Clone, Copy, Debug)]
struct Frame {
    c: f64,
    s: f64,
    dc: [f64; 2],
    ds: [f64; 2],
}

/// The tests of a fitted offset on the side the spline folds over: whether
/// it may intersect itself, whether its derivative reaches zero at either
/// end, and whether it is clipped at either end.
#[derive(Clone, Copy, Debug)]
struct Outcome {
    intersection: bool,
    zero: [bool; 2],
    clip: [bool; 2],
}

/// The outcomes that let the folded side be fitted: for each test the
/// outcome required, or `None` when the test does not decide.
#[derive(Clone, Copy, Debug)]
struct Expectation {
    side: Side,
    intersection: Option<bool>,
    zero: [Option<bool>; 2],
    clip: [Option<bool>; 2],
}

impl Expectation {
    /// What a fit must show on `side`, the folded side of a spline whose
    /// offset does not intersect itself; `c` is the end normals' dot product,
    /// `f` and `g` the end terms against `abs_s` and `ds` the end normals'
    /// cross products with the middle leg. `None` when that side is split
    /// without a fit.
    fn new(
        side: Side,
        c: f64,
        [f0, f1]: [f64; 2],
        [g0, g1]: [f64; 2],
        abs_s: f64,
        ds: [f64; 2],
    ) -> Option<Self> {
        // libass's badness: one for a middle leg the end normals do not lie
        // on opposite sides of, one for each end with c + g < 1. Past two,
        // the folded side is split rather than fitted.
        let parallel = (ds[0] < 0.0) != (ds[1] < 0.0);
        let bad = [c + g0 < 1.0, c + g1 < 1.0];
        if !parallel && bad[0] && bad[1] {
            return None;
        }
        let end = |bad: bool, beyond: bool| match (bad, parallel) {
            (true, true) => (Some(!beyond), (c < 0.0).then_some(beyond)),
            (false, false) => (Some(false), (c > 0.0).then_some(false)),
            _ => (None, None),
        };
        let (zero0, clip0) = end(bad[0], f0 > abs_s);
        let (zero1, clip1) = end(bad[1], f1 > abs_s);
        // The intersection decides for an odd count of a parallel middle and
        // good ends; the outcome it requires flips with the inner side, the
        // last normal turning from the middle and a good first end.
        let decides = parallel ^ !bad[0] ^ !bad[1];
        let required = (side == Side::Inner) ^ (ds[1] < 0.0) ^ !bad[0];
        Some(Self {
            side,
            intersection: decides.then_some(required),
            zero: [zero0, zero1],
            clip: [clip0, clip1],
        })
    }

    fn admits(&self, outcome: &Outcome) -> bool {
        let agrees = |required: Option<bool>, actual: bool| required.is_none_or(|r| r == actual);
        let both_agree = |required: [Option<bool>; 2], actual: [bool; 2]| {
            required.into_iter().zip(actual).all(|(r, a)| agrees(r, a))
        };
        agrees(self.intersection, outcome.intersection)
            && both_agree(self.zero, outcome.zero)
            && both_agree(self.clip, outcome.clip)
    }
}

impl Cubic {
    /// The spline through `points`, or `None` when it has no direction. An
    /// end leg too short to give one is measured to the next control point
    /// instead, and the middle leg then reaches to the end point.
    pub(super) fn new(points: [Vector; 4], tolerances: &Tolerances) -> Option<Self> {
        let [p0, p1, p2, p3] = points;
        let (first, last, [from, to]) = match (tolerances.leg(p0, p1), tolerances.leg(p2, p3)) {
            (Some(first), Some(last)) => (first, last, [p1, p2]),
            (None, Some(last)) => (tolerances.leg(p0, p2)?, last, [p0, p2]),
            (Some(first), None) => (first, tolerances.leg(p1, p3)?, [p1, p3]),
            (None, None) => return None,
        };
        Some(Self {
            points,
            legs: [first, tolerances.derivative(from, to), last],
            normals: [Normal::of(first), Normal::of(last)],
        })
    }

    /// Whether the offset on the folded side, whose end terms are `f` and
    /// `g`, intersects itself.
    fn intersects_itself(&self, frame: &Frame, [f0, f1]: [f64; 2], [g0, g1]: [f64; 2]) -> bool {
        let [n0, n1] = self.normals;
        let middle = self.legs[1];
        let d2 = (f0 + frame.dc[1]) * n1.len + (f1 + frame.dc[0]) * n0.len;
        let d2 = (d2 + middle.dot(middle)) / 2.0;
        if !(d2 < g0 && d2 < g1) {
            return false;
        }
        let mut q = (d2 / (2.0 - d2)).sqrt();
        let h0 = (f0 * q + g0) * n1.len;
        let h1 = (f1 * q + g1) * n0.len;
        q *= (4.0 / 3.0) * d2;
        h0 > q && h1 > q
    }

    /// The offsets of the inner control points within the error bounds, with
    /// the sides of `sides` they fit; `expected` checks the folded side.
    fn fit(
        &self,
        tolerances: &Tolerances,
        frame: &Frame,
        expected: Option<Expectation>,
        mut sides: Sides,
    ) -> Option<(Sides, [DVector; 2])> {
        let shift = Shift::fit(frame, tolerances.err_c)?;
        if let Some(expected) = expected
            && !expected.admits(&self.outcome(frame, &shift, expected.side))
        {
            sides = sides.without(expected.side);
            if sides.is_empty() {
                return None;
            }
        }
        if !self.within_angle(frame, &shift, tolerances.err_a) {
            return None;
        }
        let [n0, n1] = self.normals;
        let [ro0, ro1] = shift.ro;
        let along = |n: DVector, ro: f64| DVector::new(n.x + n.y * ro, n.y - n.x * ro);
        Some((sides, [along(n0.unit, ro0), along(n1.unit, ro1)]))
    }

    /// The fold tests of `shift` on `side`.
    fn outcome(&self, frame: &Frame, shift: &Shift, side: Side) -> Outcome {
        let Frame { c, s, dc, .. } = *frame;
        let [n0, n1] = self.normals;
        let [ro0, ro1] = shift.ro;
        let (s, ro0, ro1) = match side {
            Side::Outer => (s, ro0, ro1),
            Side::Inner => (-s, -ro0, -ro1),
        };
        Outcome {
            intersection: 2.0 * s * shift.r < dc[0] + dc[1],
            zero: [n0.len - ro0 < 0.0, n1.len + ro1 < 0.0],
            clip: [
                n0.len + dc[0] + s - ro1 * c < 0.0,
                n1.len + dc[1] + s + ro0 * c < 0.0,
            ],
        }
    }

    /// Whether the offset with `shift` keeps within the angular error, whose
    /// tangent is `err_a`, in two stages.
    fn within_angle(&self, frame: &Frame, shift: &Shift, err_a: f64) -> bool {
        let Frame { c, s, dc, ds } = *frame;
        let [n0, n1] = self.normals;
        let [ro0, ro1] = shift.ro;
        let d0c = 2.0 * dc[0];
        let d0s = 2.0 * ds[0];
        let d1c = 2.0 * dc[1];
        let d1s = 2.0 * ds[1];
        let dot0 = d0c + 3.0 * n0.len;
        let crs0 = d0s + 3.0 * ro0 * n0.len;
        let dot1 = d1c + 3.0 * n1.len;
        let crs1 = d1s + 3.0 * ro1 * n1.len;
        if !(crs0.abs() < err_a * dot0 && crs1.abs() < err_a * dot1) {
            return false;
        }
        let cl0 = c * n0.len;
        let sl0 = s * n0.len;
        let cl1 = c * n1.len;
        let sl1 = -s * n1.len;
        let dot0 = d0c - ro0 * d0s + cl0 + ro1 * sl0 + cl1 / 3.0;
        let dot1 = d1c - ro1 * d1s + cl1 + ro0 * sl1 + cl0 / 3.0;
        let crs0 = d0s + ro0 * d0c - sl0 + ro1 * cl0 - sl1 / 3.0;
        let crs1 = d1s + ro1 * d1c - sl1 + ro0 * cl1 - sl0 / 3.0;
        crs0.abs() < err_a * dot0 && crs1.abs() < err_a * dot1
    }
}

/// How far the fitted offset's inner control points move along the offset
/// from the end normals, `ro` each, with `r` the radial term they share.
#[derive(Clone, Copy, Debug)]
struct Shift {
    r: f64,
    ro: [f64; 2],
}

impl Shift {
    /// The best fit for the offset's radius, when its radial error is below
    /// `err_c`.
    fn fit(frame: &Frame, err_c: f64) -> Option<Self> {
        let Frame { c, s, dc, ds } = *frame;
        let t = (ds[0] + ds[1]) / (dc[0] + dc[1]);
        let c1 = 1.0 + c;
        let ss = s * s;
        let ts = t * s;
        let tt = t * t;
        let ttc = tt * c1;
        let ttcc = ttc * c1;

        const W: f64 = 0.4;
        let f0 = [
            10.0 * W * (c - 1.0) + 9.0 * W * tt * c,
            2.0 * (c - 1.0) + 3.0 * tt + 2.0 * ts,
            2.0 * (c - 1.0) + 3.0 * tt - 2.0 * ts,
        ];
        let f1 = [
            18.0 * W * (ss - ttc * c),
            2.0 * ss - 6.0 * ttc - 2.0 * ts * (c + 4.0),
            2.0 * ss - 6.0 * ttc + 2.0 * ts * (c + 4.0),
        ];
        let f2 = [
            9.0 * W * (ttcc - ss) * c,
            3.0 * ss + 3.0 * ttcc + 6.0 * ts * c1,
            3.0 * ss + 3.0 * ttcc - 6.0 * ts * c1,
        ];

        let ch = (c1 / 2.0).sqrt();
        let inv_ro0 = 1.5 * ch * (ch + 1.0);
        let (mut aa, mut ab) = (0.0f64, 0.0f64);
        for i in 0..3 {
            let a = 2.0 * f2[i] + f1[i] * inv_ro0;
            let b = f2[i] - f0[i] * inv_ro0 * inv_ro0;
            aa += a * a;
            ab += a * b;
        }
        let ro = ab / (aa * inv_ro0 + 1e-9);
        let mut err2 = 0.0f64;
        for i in 0..3 {
            let err = f0[i] + ro * (f1[i] + ro * f2[i]);
            err2 += err * err;
        }
        if !(err2 < err_c) {
            return None;
        }
        let r = ro * c1 - 1.0;
        Some(Self {
            r,
            ro: [t * r - ro * s, t * r + ro * s],
        })
    }
}

impl Spline for Cubic {
    const KIND: SegmentKind = SegmentKind::Cubic;

    type Controls = [(Vector, DVector); 2];

    fn start(&self) -> Vector {
        self.points[0]
    }

    fn end(&self) -> Vector {
        self.points[3]
    }

    fn normals(&self) -> [Normal; 2] {
        self.normals
    }

    fn plan(&self, tolerances: &Tolerances, sides: Sides) -> Plan<Self::Controls> {
        let [n0, n1] = self.normals;
        let middle = self.legs[1];
        let frame = Frame {
            c: n0.unit.dot(n1.unit),
            s: n0.unit.cross(n1.unit),
            dc: [n0.unit.dot(middle), n1.unit.dot(middle)],
            ds: [n0.unit.cross(middle), n1.unit.cross(middle)],
        };
        let Frame { c, s, dc, ds } = frame;
        let fold = Side::folded(s);
        let abs_s = s.abs();
        let f0 = n0.len * c + n1.len + dc[1];
        let f1 = n1.len * c + n0.len + dc[0];
        let (g0, g1) = match fold {
            Side::Inner => (n0.len * s - ds[1], n1.len * s + ds[0]),
            Side::Outer => (-(n0.len * s - ds[1]), -(n1.len * s + ds[0])),
        };

        let mut shortcut = None;
        let mut fitted = sides;
        let mut expected = None;
        if !(dc[0] + dc[1] > 0.0) {
            // The middle runs against the ends: only halves can fit.
            fitted = Sides::EMPTY;
        } else if sides.contains(fold) {
            if f0 < abs_s && f1 < abs_s {
                // The offset may intersect itself.
                if self.intersects_itself(&frame, [f0, f1], [g0, g1]) {
                    shortcut = Some((fold, Shortcut::new([f0, f1], abs_s, n0.unit)));
                }
                fitted = fitted.without(fold);
            } else {
                expected = Expectation::new(fold, c, [f0, f1], [g0, g1], abs_s, ds);
                if expected.is_none() {
                    fitted = fitted.without(fold);
                }
            }
        }
        let fit = if fitted.is_empty() {
            None
        } else {
            let [_, p1, p2, _] = self.points;
            self.fit(tolerances, &frame, expected, fitted)
                .map(|(sides, [o1, o2])| (sides, [(p1, o1), (p2, o2)]))
        };
        Plan { shortcut, fit }
    }

    fn split(&self, stroker: &mut Stroker<'_>, sides: Sides) -> Result<()> {
        let [p0, p1, p2, p3, p4, p5, p6] = split_cubic(&self.points);
        let [n0, n1] = self.normals;
        let [d0, centre, d4] = self.legs.map(DVector::halved);
        let d1 = d0.mean(centre);
        let d3 = centre.mean(d4);
        let d2 = d1.mean(d3);
        let min_len = stroker.tolerances.min_len;

        if let Some(normal) = Normal::directed(d2, min_len) {
            let low = Self {
                points: [p0, p1, p2, p3],
                legs: [d0, d1, d2],
                normals: [n0.halved(), normal],
            };
            let high = Self {
                points: [p3, p4, p5, p6],
                legs: [d2, d3, d4],
                normals: [normal, n1.halved()],
            };
            stroker.offset_spline(&low, sides)?;
            return stroker.offset_spline(&high, sides);
        }

        // The middle has no direction: each half's middle leg takes it in,
        // and the halves meet with a join. A half without a direction is a
        // line.
        let d1 = d1 + d2;
        let d3 = d3 + d2;
        match Normal::directed(d1, min_len) {
            Some(normal) => {
                let low = Self {
                    points: [p0, p1, p2, p3],
                    legs: [d0, d1, DVector::ZERO],
                    normals: [n0.halved(), normal],
                };
                stroker.offset_spline(&low, sides)?;
            }
            None => stroker.resume(SegmentKind::Line, p0, sides)?,
        }
        let second = Normal::directed(d3, min_len);
        stroker.join(p2, second.map_or(n1.unit, |normal| normal.unit), sides)?;
        match second {
            Some(normal) => {
                let high = Self {
                    points: [p3, p4, p5, p6],
                    legs: [DVector::ZERO, d3, d4],
                    normals: [normal, n1.halved()],
                };
                stroker.offset_spline(&high, sides)
            }
            None => stroker.resume(SegmentKind::Line, p3, sides),
        }
    }
}

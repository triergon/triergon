//! Offsetting cubic splines.

use triergon_core::Result;

use super::{Normal, Side, Sides, Stroker, halve, mean, sub};
use crate::geometry::{DVector, Vector, split_cubic};
use crate::limits::charge;
use crate::outline::SegmentKind;
use crate::set::{Member, Set};

/// The tests on a cubic's offset curve that decide whether its folded side
/// must be split: a self-intersection, a zero of the offset derivative at
/// either end, and a clip at either end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Test {
    Intersection,
    Zero0,
    Zero1,
    Clip0,
    Clip1,
}

impl Member for Test {
    fn bit(self) -> u8 {
        1 << self as u8
    }
}

type Tests = Set<Test>;

/// What the cubic estimate checks on the side a curve folds over: the outcome
/// each test is expected to have, the tests whose outcome decides, and the side.
#[derive(Clone, Copy, Debug)]
struct FoldCheck {
    expected: Tests,
    decisive: Tests,
    side: Side,
}

impl FoldCheck {
    /// Whether `outcome` departs from the expectation in a deciding test.
    fn fails(&self, outcome: Tests) -> bool {
        !outcome
            .symmetric_difference(self.expected)
            .intersection(self.decisive)
            .is_empty()
    }

    /// Sets the expected outcomes and deciding tests of a fold whose offset does
    /// not intersect itself, from the end normals' dot product `c`, the end
    /// terms `g` and `f`, `|s|` and the cross products `ds` of the end normals
    /// with the middle derivative. Returns the badness: above 2, the folded side
    /// is checked as well.
    fn classify(&mut self, c: f64, g: [f64; 2], f: [f64; 2], abs_s: f64, ds: [f64; 2]) -> u32 {
        if ds[0] < 0.0 {
            self.decisive = self.decisive.toggled(Test::Intersection);
        }
        if ds[1] < 0.0 {
            self.decisive = self.decisive.toggled(Test::Intersection);
            self.expected = self.expected.toggled(Test::Intersection);
        }
        let parallel = self.decisive.contains(Test::Intersection);
        let mut badness = u32::from(!parallel);
        let ends = [(Test::Zero0, Test::Clip0), (Test::Zero1, Test::Clip1)];
        for (end, (zero, clip)) in ends.into_iter().enumerate() {
            if c + g[end] < 1.0 {
                if parallel {
                    self.decisive = self.decisive.toggled(zero);
                    self.expected = self.expected.toggled(zero);
                    if c < 0.0 {
                        self.decisive = self.decisive.toggled(clip);
                    }
                    if f[end] > abs_s {
                        self.expected = self.expected.toggled(zero).toggled(clip);
                    }
                }
                badness += 1;
            } else {
                self.decisive = self.decisive.toggled(Test::Intersection);
                if end == 0 {
                    self.expected = self.expected.toggled(Test::Intersection);
                }
                if !parallel {
                    self.decisive = self.decisive.toggled(zero);
                    if c > 0.0 {
                        self.decisive = self.decisive.toggled(clip);
                    }
                }
            }
        }
        badness
    }
}

/// A cubic spline being offset: its points, the normal-space derivatives of
/// its first leg, middle and last leg, and its end normals.
#[derive(Clone, Copy, Debug)]
struct Cubic {
    pt: [Vector; 4],
    deriv: [DVector; 3],
    normal: [Normal; 2],
}

/// The products of a cubic's end normals: with each other (`c` dot, `s`
/// cross) and with the middle derivative (`dc`, `ds`).
#[derive(Clone, Copy, Debug)]
struct CubicFrame {
    c: f64,
    s: f64,
    dc: [f64; 2],
    ds: [f64; 2],
    normal: [Normal; 2],
}

impl CubicFrame {
    fn new(k: &Cubic) -> Self {
        let (deriv, normal) = (&k.deriv, &k.normal);
        Self {
            c: normal[0].v.dot(normal[1].v),
            s: normal[0].v.cross(normal[1].v),
            dc: [normal[0].v.dot(deriv[1]), normal[1].v.dot(deriv[1])],
            ds: [normal[0].v.cross(deriv[1]), normal[1].v.cross(deriv[1])],
            normal: *normal,
        }
    }
}

impl Stroker<'_> {
    pub(super) fn add_cubic(
        &mut self,
        pt1: Vector,
        pt2: Vector,
        pt3: Vector,
        sides: Sides,
    ) -> Result<()> {
        // The points that span the middle derivative: the control points, or
        // past a tiny end step the end point beyond it.
        let (mut from, mut to) = (1, 2);
        let mut d0 = sub(pt1, self.last_point);
        if self.tolerance.tiny(d0) {
            d0 = sub(pt2, self.last_point);
            if self.tolerance.tiny(d0) {
                return self.add_line(pt3, sides);
            }
            from = 0;
        }
        let mut d2 = sub(pt3, pt2);
        if self.tolerance.tiny(d2) {
            d2 = sub(pt3, pt1);
            if self.tolerance.tiny(d2) {
                return self.add_line(pt3, sides);
            }
            to = 3;
        }
        if (from, to) == (0, 3) {
            return self.add_line(pt3, sides);
        }
        let pt = [self.last_point, pt1, pt2, pt3];
        self.last_point = pt3;
        let d1 = sub(pt[to], pt[from]);
        let deriv = [d0, d1, d2].map(|d| self.tolerance.derivative(d));
        let piece = Cubic {
            pt,
            deriv,
            normal: [Normal::of(deriv[0]), Normal::of(deriv[2])],
        };
        let first = self.joint.contour_start;
        self.start_segment(pt[0], piece.normal[0].v, sides)?;
        self.offset_cubic(&piece, sides, first)
    }

    /// The offset control points of a cubic spline when its error is within
    /// bounds, with the sides that need no further split.
    fn estimate_cubic(
        &self,
        frame: &CubicFrame,
        check: &FoldCheck,
        mut sides: Sides,
    ) -> (Sides, [DVector; 2]) {
        const NONE: (Sides, [DVector; 2]) = (Sides::EMPTY, [DVector::ZERO; 2]);
        let CubicFrame {
            c,
            s,
            dc,
            ds,
            normal,
        } = *frame;
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
        let ro = ab / (aa * inv_ro0 + 1e-9); // best fit
        let mut err2 = 0.0f64;
        for i in 0..3 {
            let err = f0[i] + ro * (f1[i] + ro * f2[i]);
            err2 += err * err;
        }
        if !(err2 < self.tolerance.err_c) {
            return NONE; // radial error
        }

        let r = ro * c1 - 1.0;
        let ro0 = t * r - ro * s;
        let ro1 = t * r + ro * s;

        if sides.contains(check.side) {
            let (mut test_s, mut test0, mut test1) = (s, ro0, ro1);
            if check.side == Side::Inner {
                test_s = -test_s;
                test0 = -test0;
                test1 = -test1;
            }
            let mut outcome = Tests::EMPTY;
            outcome.set(Test::Intersection, 2.0 * test_s * r < dc[0] + dc[1]);
            outcome.set(Test::Zero0, normal[0].len - test0 < 0.0);
            outcome.set(Test::Zero1, normal[1].len + test1 < 0.0);
            outcome.set(
                Test::Clip0,
                normal[0].len + dc[0] + test_s - test1 * c < 0.0,
            );
            outcome.set(
                Test::Clip1,
                normal[1].len + dc[1] + test_s + test0 * c < 0.0,
            );
            if check.fails(outcome) {
                sides = sides.without(check.side);
                if sides.is_empty() {
                    return NONE;
                }
            }
        }

        let d0c = 2.0 * dc[0];
        let d0s = 2.0 * ds[0];
        let d1c = 2.0 * dc[1];
        let d1s = 2.0 * ds[1];
        let dot0 = d0c + 3.0 * normal[0].len;
        let crs0 = d0s + 3.0 * ro0 * normal[0].len;
        let dot1 = d1c + 3.0 * normal[1].len;
        let crs1 = d1s + 3.0 * ro1 * normal[1].len;
        // Angular error, stage 1.
        if !(crs0.abs() < self.tolerance.err_a * dot0 && crs1.abs() < self.tolerance.err_a * dot1) {
            return NONE;
        }

        let cl0 = c * normal[0].len;
        let sl0 = s * normal[0].len;
        let cl1 = c * normal[1].len;
        let sl1 = -s * normal[1].len;
        let dot0 = d0c - ro0 * d0s + cl0 + ro1 * sl0 + cl1 / 3.0;
        let dot1 = d1c - ro1 * d1s + cl1 + ro0 * sl1 + cl0 / 3.0;
        let crs0 = d0s + ro0 * d0c - sl0 + ro1 * cl0 - sl1 / 3.0;
        let crs1 = d1s + ro1 * d1c - sl1 + ro0 * cl1 - sl0 / 3.0;
        // Angular error, stage 2.
        if !(crs0.abs() < self.tolerance.err_a * dot0 && crs1.abs() < self.tolerance.err_a * dot1) {
            return NONE;
        }

        let n0 = normal[0].v;
        let n1 = normal[1].v;
        let result = [
            DVector::new(n0.x + n0.y * ro0, n0.y - n0.x * ro0),
            DVector::new(n1.x + n1.y * ro1, n1.y - n1.x * ro1),
        ];
        (sides, result)
    }

    fn offset_cubic(&mut self, k: &Cubic, mut sides: Sides, first: bool) -> Result<()> {
        charge(self.work, 1)?;
        let frame = CubicFrame::new(k);
        let Some((rest, check, fold)) = self.fold_cubic(k, &frame, sides, first)? else {
            self.joint.last_normal = k.normal[1].v;
            return Ok(());
        };
        sides = rest;
        if !check.is_empty() {
            let (good, control) = self.estimate_cubic(&frame, &fold, check);
            if !good.is_empty() {
                self.emit_first_point(k.pt[0], SegmentKind::Cubic, good)?;
                self.emit(k.pt[1], control[0], None, good)?;
                self.emit(k.pt[2], control[1], None, good)?;
                sides = sides.minus(good);
                if sides.is_empty() {
                    self.joint.last_normal = k.normal[1].v;
                    return Ok(());
                }
            }
        }
        self.split_cubic(k, sides, first)
    }

    /// Handles the side a cubic folds over: skips a self-intersecting offset
    /// there, and decides which sides the error estimate checks and how. Returns
    /// the sides still to draw, those to check and the fold's tests, or `None`
    /// when nothing remains to draw.
    fn fold_cubic(
        &mut self,
        k: &Cubic,
        frame: &CubicFrame,
        mut sides: Sides,
        first: bool,
    ) -> Result<Option<(Sides, Sides, FoldCheck)>> {
        let (pt, deriv) = (&k.pt, &k.deriv);
        let CubicFrame {
            c,
            s,
            dc,
            ds,
            normal,
        } = *frame;
        let f0 = normal[0].len * c + normal[1].len + dc[1];
        let f1 = normal[1].len * c + normal[0].len + dc[0];
        let mut g0 = normal[0].len * s - ds[1];
        let mut g1 = normal[1].len * s + ds[0];

        let mut abs_s = s;
        let mut check = sides;
        let mut fold = FoldCheck {
            expected: Tests::of([Test::Intersection]),
            decisive: Tests::EMPTY,
            side: Side::Inner,
        };
        if s < 0.0 {
            abs_s = -s;
            fold.expected = Tests::EMPTY;
            fold.side = Side::Outer;
            g0 = -g0;
            g1 = -g1;
        }
        let side = fold.side;
        let folded = Sides::of([side]);

        if !(dc[0] + dc[1] > 0.0) {
            check = Sides::EMPTY;
        } else if sides.contains(side) {
            if f0 < abs_s && f1 < abs_s {
                // The offset curve may intersect itself.
                let d2 = (f0 + dc[1]) * normal[1].len + (f1 + dc[0]) * normal[0].len;
                let d2 = (d2 + deriv[1].dot(deriv[1])) / 2.0;
                if d2 < g0 && d2 < g1 {
                    let mut q = (d2 / (2.0 - d2)).sqrt();
                    let h0 = (f0 * q + g0) * normal[1].len;
                    let h1 = (f1 * q + g1) * normal[0].len;
                    q *= (4.0 / 3.0) * d2;
                    if h0 > q && h1 > q {
                        self.prepare_skip(pt[0], folded, first)?;
                        if f0 < 0.0 || f1 < 0.0 {
                            self.emit(pt[0], DVector::ZERO, Some(SegmentKind::Line), folded)?;
                            self.emit(pt[3], DVector::ZERO, Some(SegmentKind::Line), folded)?;
                        } else {
                            let offs = normal[0].v.scale(f0 / abs_s);
                            self.emit(pt[0], offs, Some(SegmentKind::Line), folded)?;
                        }
                        sides = sides.without(side);
                        if sides.is_empty() {
                            return Ok(None);
                        }
                    }
                }
                check = check.toggled(side);
            } else if fold.classify(c, [g0, g1], [f0, f1], abs_s, ds) > 2 {
                check = check.toggled(side);
            }
        }
        Ok(Some((sides, check, fold)))
    }

    /// Halves a cubic whose estimate failed and offsets both halves, joining
    /// them with a cap where the middle has no direction.
    fn split_cubic(&mut self, k: &Cubic, sides: Sides, first: bool) -> Result<()> {
        let next = split_cubic(&k.pt);
        let d0 = halve(k.deriv[0]);
        let centre = halve(k.deriv[1]);
        let d4 = halve(k.deriv[2]);
        let d1 = mean(d0, centre);
        let d3 = mean(centre, d4);
        let d2 = mean(d1, d3);
        let low = [next[0], next[1], next[2], next[3]];
        let high = [next[3], next[4], next[5], next[6]];
        let min_len = self.tolerance.min_len;

        if let Some(middle) = Normal::directed(d2, min_len) {
            let first_half = Cubic {
                pt: low,
                deriv: [d0, d1, d2],
                normal: [k.normal[0].halved(), middle],
            };
            let second_half = Cubic {
                pt: high,
                deriv: [d2, d3, d4],
                normal: [middle, k.normal[1].halved()],
            };
            self.offset_cubic(&first_half, sides, first)?;
            return self.offset_cubic(&second_half, sides, false);
        }

        // Degenerate middle: join the halves with a cap.
        let d1 = DVector::new(d1.x + d2.x, d1.y + d2.y);
        let d3 = DVector::new(d3.x + d2.x, d3.y + d2.y);
        match Normal::directed(d1, min_len) {
            Some(normal1) => {
                let first_half = Cubic {
                    pt: low,
                    deriv: [d0, d1, DVector::ZERO],
                    normal: [k.normal[0].halved(), normal1],
                };
                self.offset_cubic(&first_half, sides, first)?;
            }
            None => self.emit_first_point(next[0], SegmentKind::Line, sides)?,
        }
        let normal2 = Normal::directed(d3, min_len);
        self.start_segment(next[2], normal2.unwrap_or(k.normal[1]).v, sides)?;
        match normal2 {
            Some(normal2) => {
                let second_half = Cubic {
                    pt: high,
                    deriv: [DVector::ZERO, d3, d4],
                    normal: [normal2, k.normal[1].halved()],
                };
                self.offset_cubic(&second_half, sides, false)
            }
            None => self.emit_first_point(next[3], SegmentKind::Line, sides),
        }
    }
}

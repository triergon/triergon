//! Border outlines: the ±1 offset curves of an outline in the space scaled by
//! (1 / x width, 1 / y width), as libass strokes `\bord`, `\xbord` and `\ybord`.
//!
//! Lines offset directly. Splines first offset their control points, then the
//! offset curve's radial and angular error is bounded through the control points
//! of products of B-splines; a spline whose bound fails is halved and retried.
//! Joins wider than the merge angle get round caps of quadratic arcs, points too
//! close to give a direction are merged, and where the offset exceeds the radius
//! of curvature the self-intersecting part is replaced by a polyline covering
//! only the positive-winding area. Two outlines come out because a
//! self-intersecting source needs both offsets.

use std::ops::{BitAnd, BitOr, BitXor};

use triergon_core::{Error, Result, WorkBudget};

use crate::geometry::{DVector, OUTLINE_MAX, Vector, split_cubic, split_quadratic};
use crate::limits::{Limits, charge};
use crate::outline::{Curve, Outline, SegmentKind};
use crate::shape::Shape;

/// How [`Outline::stroke_into`] strokes: the half widths of the border along x
/// and y, and the tolerance the offset curves keep to, all in outline units.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Pen {
    pub width: Vector,
    pub tolerance: i32,
}

impl Outline {
    /// Strokes the outline with `pen`, writing the outer (+offset) and inner
    /// (−offset) border outlines into `border`, which is cleared first.
    ///
    /// # Errors
    ///
    /// `Invalid` when the larger width is below the tolerance, the tolerance
    /// below 1 or the width above [`OUTLINE_MAX`](crate::OUTLINE_MAX), or a
    /// border point leaves that range; `Policy` when `limits` run out; and
    /// storage errors.
    pub fn stroke_into(&self, pen: &Pen, limits: &Limits, border: &mut Shape) -> Result<()> {
        let radius = pen.width.x.max(pen.width.y);
        if pen.tolerance < 1 || radius < pen.tolerance || radius > OUTLINE_MAX {
            return Err(Error::Invalid("border size out of range"));
        }
        let result = border.rebuild(|outlines| {
            let mut stroker = Stroker::new(pen, outlines, limits);
            stroker.outline(self)
        });
        if result.is_err() {
            border.clear();
        }
        result
    }
}

/// A set of the two border outlines: bit 0 the outer (+offset), bit 1 the inner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Sides(u8);

impl Sides {
    const NONE: Self = Self(0);
    const OUTER: Self = Self(1);
    const INNER: Self = Self(2);
    const BOTH: Self = Self(3);

    const fn is_empty(self) -> bool {
        self.0 == 0
    }

    const fn without(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }

    /// The side a turn with cross product `s` folds over: the inner side of a
    /// counter-clockwise turn, the outer side of a clockwise one.
    fn folded(s: f64) -> Self {
        if s < 0.0 { Self::OUTER } else { Self::INNER }
    }

    /// Whether the set holds the outline at `index`: 0 the outer, 1 the inner.
    const fn has(self, index: usize) -> bool {
        self.0 & (1 << index) != 0
    }
}

impl BitAnd for Sides {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self {
        Self(self.0 & rhs.0)
    }
}

impl BitOr for Sides {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl BitXor for Sides {
    type Output = Self;
    fn bitxor(self, rhs: Self) -> Self {
        Self(self.0 ^ rhs.0)
    }
}

/// A unit normal and the length of the derivative it came from.
#[derive(Clone, Copy, Debug)]
struct Normal {
    v: DVector,
    len: f64,
}

impl Normal {
    fn halved(self) -> Self {
        Self {
            v: self.v,
            len: self.len / 2.0,
        }
    }
}

/// The normal-space derivative of the step `d`: (dy / ybord, −dx / xbord).
fn derivative(d: Vector, xscale: f64, yscale: f64) -> DVector {
    DVector::new(f64::from(d.y) * yscale, f64::from(-d.x) * xscale)
}

fn normal_of(deriv: DVector) -> Normal {
    let len = deriv.len();
    Normal {
        v: deriv.scale(1.0 / len),
        len,
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

/// The outcomes of the tests on a cubic's offset curve that decide whether its
/// folded side must be split: a self-intersection, a zero of the offset
/// derivative at either end, and a clip at either end.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Tests(u8);

impl Tests {
    const NONE: Self = Self(0);
    const INTERSECTION: Self = Self(1);
    const ZERO_0: Self = Self(2);
    const ZERO_1: Self = Self(4);
    const CLIP_0: Self = Self(8);
    const CLIP_1: Self = Self(16);

    const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    fn toggle(&mut self, other: Self) {
        self.0 ^= other.0;
    }

    fn set(&mut self, other: Self, on: bool) {
        if on {
            self.0 |= other.0;
        }
    }
}

/// What the cubic estimate checks on the side a curve folds over: the outcome
/// each test is expected to have, the tests whose outcome decides, and the side.
#[derive(Clone, Copy, Debug)]
struct FoldCheck {
    expected: Tests,
    decisive: Tests,
    side: Sides,
}

impl FoldCheck {
    /// Whether `outcome` departs from the expectation in a deciding test.
    fn fails(&self, outcome: Tests) -> bool {
        (outcome.0 ^ self.expected.0) & self.decisive.0 != 0
    }

    /// Sets the expected outcomes and deciding tests of a fold whose offset does
    /// not intersect itself, from the end normals' dot product `c`, the end
    /// terms `g` and `f`, `|s|` and the cross products `ds` of the end normals
    /// with the middle derivative. Returns the badness: above 2, the folded side
    /// is checked as well.
    fn classify(&mut self, c: f64, g: [f64; 2], f: [f64; 2], abs_s: f64, ds: [f64; 2]) -> u32 {
        if ds[0] < 0.0 {
            self.decisive.toggle(Tests::INTERSECTION);
        }
        if ds[1] < 0.0 {
            self.decisive.toggle(Tests::INTERSECTION);
            self.expected.toggle(Tests::INTERSECTION);
        }
        let parallel = self.decisive.contains(Tests::INTERSECTION);
        let mut badness = u32::from(!parallel);
        let ends = [
            (Tests::ZERO_0, Tests::CLIP_0),
            (Tests::ZERO_1, Tests::CLIP_1),
        ];
        for (end, (zero, clip)) in ends.into_iter().enumerate() {
            if c + g[end] < 1.0 {
                if parallel {
                    self.decisive.toggle(zero);
                    self.expected.toggle(zero);
                    if c < 0.0 {
                        self.decisive.toggle(clip);
                    }
                    if f[end] > abs_s {
                        self.expected.toggle(zero);
                        self.expected.toggle(clip);
                    }
                }
                badness += 1;
            } else {
                self.decisive.toggle(Tests::INTERSECTION);
                if end == 0 {
                    self.expected.toggle(Tests::INTERSECTION);
                }
                if !parallel {
                    self.decisive.toggle(zero);
                    if c > 0.0 {
                        self.decisive.toggle(clip);
                    }
                }
            }
        }
        badness
    }
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
    fn new(deriv: &[DVector; 3], normal: &[Normal; 2]) -> Self {
        Self {
            c: normal[0].v.dot(normal[1].v),
            s: normal[0].v.cross(normal[1].v),
            dc: [normal[0].v.dot(deriv[1]), normal[1].v.dot(deriv[1])],
            ds: [normal[0].v.cross(deriv[1]), normal[1].v.cross(deriv[1])],
            normal: *normal,
        }
    }
}

/// Whether `d` is shorter than `eps` along both axes.
fn tiny(d: Vector, eps: i32) -> bool {
    d.x > -eps && d.x < eps && d.y > -eps && d.y < eps
}

fn sub(a: Vector, b: Vector) -> Vector {
    Vector::new(a.x.wrapping_sub(b.x), a.y.wrapping_sub(b.y))
}

/// libass's stroker state over one source outline.
struct Stroker<'a> {
    /// The outer and inner border outlines.
    out: [&'a mut Outline; 2],
    /// Where the current contour starts in each output.
    contour_first: [usize; 2],
    xbord: f64,
    ybord: f64,
    xscale: f64,
    yscale: f64,
    eps: i32,
    /// No segment of the current contour has been emitted yet.
    contour_start: bool,
    first_skip: Sides,
    last_skip: Sides,
    first_normal: DVector,
    last_normal: DVector,
    first_point: Vector,
    last_point: Vector,
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
    work: WorkBudget,
    points_limit: usize,
}

impl<'a> Stroker<'a> {
    fn new(pen: &Pen, out: [&'a mut Outline; 2], limits: &Limits) -> Self {
        let (border, eps) = (pen.width, pen.tolerance);
        let rel_err = f64::from(eps) / f64::from(border.x.max(border.y));
        let e = (2.0 * rel_err).sqrt();
        Self {
            out,
            contour_first: [0, 0],
            xbord: f64::from(border.x),
            ybord: f64::from(border.y),
            xscale: 1.0 / f64::from(eps.max(border.x)),
            yscale: 1.0 / f64::from(eps.max(border.y)),
            eps,
            contour_start: true,
            first_skip: Sides::NONE,
            last_skip: Sides::NONE,
            first_normal: DVector::ZERO,
            last_normal: DVector::ZERO,
            first_point: Vector::default(),
            last_point: Vector::default(),
            merge_cos: 1.0 - rel_err,
            split_cos: 1.0 + 8.0 * rel_err - 4.0 * (1.0 + rel_err) * e,
            min_len: rel_err / 4.0,
            err_q: 8.0 * (1.0 + rel_err) * (1.0 + rel_err),
            err_c: 390.0 * rel_err * rel_err,
            err_a: e,
            work: limits.work(),
            points_limit: limits.outline_points,
        }
    }

    /// Strokes every contour of `source`.
    fn outline(&mut self, source: &Outline) -> Result<()> {
        let mut curves = source.curves();
        loop {
            let starts_contour = curves.at_contour_start();
            let Some((curve, segment)) = curves.next() else {
                return Ok(());
            };
            if starts_contour {
                self.last_point = curve.start();
            }
            match curve {
                Curve::Line([_, end]) => self.add_line(end, Sides::BOTH)?,
                Curve::Quadratic([_, p1, end]) => self.add_quadratic(p1, end, Sides::BOTH)?,
                Curve::Cubic([_, p1, p2, end]) => self.add_cubic(p1, p2, end, Sides::BOTH)?,
            }
            if segment.closes_contour {
                self.close_contour(Sides::BOTH)?;
            }
        }
    }

    /// Adds `pt` displaced by ±`offs` (scaled by the borders, truncated) to the
    /// outlines in `sides`, starting a segment of `segment` when given.
    fn emit(
        &mut self,
        pt: Vector,
        offs: DVector,
        segment: Option<SegmentKind>,
        sides: Sides,
    ) -> Result<()> {
        let dx = (self.xbord * offs.x) as i32;
        let dy = (self.ybord * offs.y) as i32;
        let displaced = [
            Vector::new(pt.x.wrapping_add(dx), pt.y.wrapping_add(dy)),
            Vector::new(pt.x.wrapping_sub(dx), pt.y.wrapping_sub(dy)),
        ];
        for (index, point) in displaced.into_iter().enumerate() {
            if !sides.has(index) {
                continue;
            }
            charge(&mut self.work, 1)?;
            let out = &mut *self.out[index];
            if out.points().len() >= self.points_limit {
                return Err(Error::Policy("border outline points"));
            }
            out.push_point(point, segment)?;
        }
        Ok(())
    }

    /// Replaces the first point of the current contour in the outlines of `sides`.
    fn fix_first_point(&mut self, pt: Vector, offs: DVector, sides: Sides) {
        let dx = (self.xbord * offs.x) as i32;
        let dy = (self.ybord * offs.y) as i32;
        let displaced = [
            Vector::new(pt.x.wrapping_add(dx), pt.y.wrapping_add(dy)),
            Vector::new(pt.x.wrapping_sub(dx), pt.y.wrapping_sub(dy)),
        ];
        for (index, point) in displaced.into_iter().enumerate() {
            if sides.has(index) {
                let first = self.contour_first[index];
                self.out[index].set_point(first, point);
            }
        }
    }

    /// Arc subdivision factors: halves the angle until its cosine reaches
    /// `split_cos`. Returns the factors and the subdivision depth.
    fn arc_factors(&self, mut c: f64) -> ([f64; 16], usize) {
        const MAX_SUBDIV: usize = 15;
        let mut mul = [0.0; 16];
        let mut pos = MAX_SUBDIV;
        while c < self.split_cos && pos > 0 {
            mul[pos] = 0.5f64.sqrt() / (1.0 + c).sqrt();
            c = (1.0 + c) * mul[pos];
            pos -= 1;
        }
        mul[pos] = 1.0 / (1.0 + c);
        let mut factors = [0.0; 16];
        factors[..=MAX_SUBDIV - pos].copy_from_slice(&mul[pos..]);
        (factors, MAX_SUBDIV - pos)
    }

    /// Emits the arc between `n0` and `n1` around `pt` as `2^level` quadratic
    /// splines; `mul[level]` is the factor of this level's midpoint normal.
    fn process_arc(
        &mut self,
        pt: Vector,
        n0: DVector,
        n1: DVector,
        mul: &[f64; 16],
        level: usize,
        sides: Sides,
    ) -> Result<()> {
        charge(&mut self.work, 1)?;
        let centre = sum_scaled(n0, n1, mul[level]);
        if level > 0 {
            self.process_arc(pt, n0, centre, mul, level - 1, sides)?;
            return self.process_arc(pt, centre, n1, mul, level - 1, sides);
        }
        self.emit(pt, n0, Some(SegmentKind::Quadratic), sides)?;
        self.emit(pt, centre, None, sides)
    }

    /// A round cap from `n0` to `n1` around `pt`; `c` is their dot product.
    fn draw_arc(
        &mut self,
        pt: Vector,
        n0: DVector,
        n1: DVector,
        mut c: f64,
        sides: Sides,
    ) -> Result<()> {
        let mut centre = None;
        if c < 0.0 {
            // Beyond a right angle: split at the bisector first.
            let sign = if (sides & Sides::INNER).is_empty() {
                1.0
            } else {
                -1.0
            };
            let mul = sign * 0.5f64.sqrt() / (1.0 - c).sqrt();
            centre = Some(DVector::new((n1.y - n0.y) * mul, (n0.x - n1.x) * mul));
            c = crate::fp::max(0.0, 0.5 + 0.5 * c).sqrt();
        }
        let (mul, level) = self.arc_factors(c);
        match centre {
            None => self.process_arc(pt, n0, n1, &mul, level, sides),
            Some(centre) => {
                self.process_arc(pt, n0, centre, &mul, level, sides)?;
                self.process_arc(pt, centre, n1, &mul, level, sides)
            }
        }
    }

    /// A full circle around `pt`, for a contour that collapsed to a point.
    fn draw_circle(&mut self, pt: Vector, sides: Sides) -> Result<()> {
        let (mul, level) = self.arc_factors(0.0);
        let normals = [
            DVector::new(1.0, 0.0),
            DVector::new(0.0, 1.0),
            DVector::new(-1.0, 0.0),
            DVector::new(0.0, -1.0),
        ];
        for k in 0..4 {
            self.process_arc(pt, normals[k], normals[(k + 1) % 4], &mul, level, sides)?;
        }
        Ok(())
    }

    /// Starts a segment at `pt` with `normal`, joining it to the previous one
    /// with a cap, or merging the normals when they nearly agree.
    fn start_segment(&mut self, pt: Vector, normal: DVector, sides: Sides) -> Result<()> {
        if self.contour_start {
            self.contour_start = false;
            self.first_skip = Sides::NONE;
            self.last_skip = Sides::NONE;
            self.first_normal = normal;
            self.last_normal = normal;
            self.first_point = pt;
            return Ok(());
        }
        let prev = self.last_normal;
        let c = prev.dot(normal);
        if c > self.merge_cos {
            self.last_normal = sum_scaled(prev, normal, 1.0 / (1.0 + c));
            return Ok(());
        }
        self.last_normal = normal;

        // The side the turn folds over gets no cap, only a spike to the centre.
        let fold = Sides::folded(prev.cross(normal));
        if !(sides & fold).is_empty() {
            self.emit(
                pt,
                prev,
                Some(SegmentKind::Line),
                fold.without(self.last_skip),
            )?;
            self.emit(pt, DVector::ZERO, Some(SegmentKind::Line), fold)?;
        }
        self.last_skip = fold;
        let rest = sides.without(fold);
        if rest.is_empty() {
            return Ok(());
        }
        self.draw_arc(pt, prev, normal, c, rest)
    }

    /// Emits the start of a segment and clears its skip flags.
    fn emit_first_point(&mut self, pt: Vector, segment: SegmentKind, sides: Sides) -> Result<()> {
        self.last_skip = self.last_skip.without(sides);
        let normal = self.last_normal;
        self.emit(pt, normal, Some(segment), sides)
    }

    /// Prepares to skip a self-intersecting part starting at `pt`.
    fn prepare_skip(&mut self, pt: Vector, sides: Sides, first: bool) -> Result<()> {
        if first {
            self.first_skip = self.first_skip | sides;
        } else {
            let normal = self.last_normal;
            self.emit(
                pt,
                normal,
                Some(SegmentKind::Line),
                sides.without(self.last_skip),
            )?;
        }
        self.last_skip = self.last_skip | sides;
        Ok(())
    }

    fn add_line(&mut self, pt1: Vector, sides: Sides) -> Result<()> {
        let d = sub(pt1, self.last_point);
        if tiny(d, self.eps) {
            return Ok(());
        }
        let normal = normal_of(derivative(d, self.xscale, self.yscale)).v;
        let start = self.last_point;
        self.start_segment(start, normal, sides)?;
        self.emit_first_point(start, SegmentKind::Line, sides)?;
        self.last_normal = normal;
        self.last_point = pt1;
        Ok(())
    }

    fn add_quadratic(&mut self, pt1: Vector, pt2: Vector, sides: Sides) -> Result<()> {
        let d0 = sub(pt1, self.last_point);
        let d1 = sub(pt2, pt1);
        if tiny(d0, self.eps) || tiny(d1, self.eps) {
            return self.add_line(pt2, sides);
        }
        let pt = [self.last_point, pt1, pt2];
        self.last_point = pt2;
        let deriv = [
            derivative(d0, self.xscale, self.yscale),
            derivative(d1, self.xscale, self.yscale),
        ];
        let normal = [normal_of(deriv[0]), normal_of(deriv[1])];
        let first = self.contour_start;
        self.start_segment(pt[0], normal[0].v, sides)?;
        self.process_quadratic(&pt, &deriv, &normal, sides, first)
    }

    /// The offset control point of a quadratic spline when its error is within
    /// bounds; `c` and `s` are the dot and cross products of its end normals.
    fn estimate_quadratic_error(&self, c: f64, s: f64, normal: &[Normal; 2]) -> Option<DVector> {
        // Radial error.
        if !((3.0 + c) * (3.0 + c) < self.err_q * (1.0 + c)) {
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
        if !(crs0.abs() < self.err_a * dot0 && crs1.abs() < self.err_a * dot1) {
            return None;
        }
        Some(sum_scaled(normal[0].v, normal[1].v, mul))
    }

    fn process_quadratic(
        &mut self,
        pt: &[Vector; 3],
        deriv: &[DVector; 2],
        normal: &[Normal; 2],
        mut sides: Sides,
        first: bool,
    ) -> Result<()> {
        charge(&mut self.work, 1)?;
        let c = normal[0].v.dot(normal[1].v);
        let s = normal[0].v.cross(normal[1].v);
        let mut check = sides;
        let fold = Sides::folded(s);
        if !(sides & fold).is_empty() {
            let abs_s = s.abs();
            let f0 = normal[0].len * c + normal[1].len;
            let f1 = normal[1].len * c + normal[0].len;
            let g0 = normal[0].len * abs_s;
            let g1 = normal[1].len * abs_s;
            if f0 < abs_s && f1 < abs_s {
                // The offset curve may intersect itself.
                let d2 = (f0 * normal[1].len + f1 * normal[0].len) / 2.0;
                if d2 < g0 && d2 < g1 {
                    self.prepare_skip(pt[0], fold, first)?;
                    if f0 < 0.0 || f1 < 0.0 {
                        self.emit(pt[0], DVector::ZERO, Some(SegmentKind::Line), fold)?;
                        self.emit(pt[2], DVector::ZERO, Some(SegmentKind::Line), fold)?;
                    } else {
                        let offs = normal[0].v.scale(f0 / abs_s);
                        self.emit(pt[0], offs, Some(SegmentKind::Line), fold)?;
                    }
                    sides = sides.without(fold);
                    if sides.is_empty() {
                        self.last_normal = normal[1].v;
                        return Ok(());
                    }
                }
                check = check ^ fold;
            } else if c + g0 < 1.0 && c + g1 < 1.0 {
                check = check ^ fold;
            }
        }

        if !check.is_empty()
            && let Some(result) = self.estimate_quadratic_error(c, s, normal)
        {
            self.emit_first_point(pt[0], SegmentKind::Quadratic, check)?;
            self.emit(pt[1], result, None, check)?;
            sides = sides.without(check);
            if sides.is_empty() {
                self.last_normal = normal[1].v;
                return Ok(());
            }
        }

        let next = split_quadratic(pt);
        let d0 = halve(deriv[0]);
        let d2 = halve(deriv[1]);
        let next_deriv = [d0, mean(d0, d2), d2];
        let len = next_deriv[1].len();
        if len < self.min_len {
            // Degenerate: the middle has no direction.
            self.emit_first_point(next[0], SegmentKind::Line, sides)?;
            self.start_segment(next[2], normal[1].v, sides)?;
            self.last_skip = self.last_skip.without(sides);
            return self.emit(next[2], normal[1].v, Some(SegmentKind::Line), sides);
        }
        let middle = Normal {
            v: next_deriv[1].scale(1.0 / len),
            len,
        };
        self.process_quadratic(
            &[next[0], next[1], next[2]],
            &[next_deriv[0], next_deriv[1]],
            &[normal[0].halved(), middle],
            sides,
            first,
        )?;
        self.process_quadratic(
            &[next[2], next[3], next[4]],
            &[next_deriv[1], next_deriv[2]],
            &[middle, normal[1].halved()],
            sides,
            false,
        )
    }

    fn add_cubic(&mut self, pt1: Vector, pt2: Vector, pt3: Vector, sides: Sides) -> Result<()> {
        // The points that span the middle derivative: the control points, or
        // past a tiny end step the end point beyond it.
        let (mut from, mut to) = (1, 2);
        let mut d0 = sub(pt1, self.last_point);
        if tiny(d0, self.eps) {
            d0 = sub(pt2, self.last_point);
            if tiny(d0, self.eps) {
                return self.add_line(pt3, sides);
            }
            from = 0;
        }
        let mut d2 = sub(pt3, pt2);
        if tiny(d2, self.eps) {
            d2 = sub(pt3, pt1);
            if tiny(d2, self.eps) {
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
        let deriv = [
            derivative(d0, self.xscale, self.yscale),
            derivative(d1, self.xscale, self.yscale),
            derivative(d2, self.xscale, self.yscale),
        ];
        let normal = [normal_of(deriv[0]), normal_of(deriv[2])];
        let first = self.contour_start;
        self.start_segment(pt[0], normal[0].v, sides)?;
        self.process_cubic(&pt, &deriv, &normal, sides, first)
    }

    /// The offset control points of a cubic spline when its error is within
    /// bounds, with the sides that need no further split.
    fn estimate_cubic_error(
        &self,
        cubic: &CubicFrame,
        check: &FoldCheck,
        mut sides: Sides,
    ) -> (Sides, [DVector; 2]) {
        const NONE: (Sides, [DVector; 2]) = (Sides::NONE, [DVector::ZERO; 2]);
        let CubicFrame {
            c,
            s,
            dc,
            ds,
            normal,
        } = *cubic;
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
        if !(err2 < self.err_c) {
            return NONE; // radial error
        }

        let r = ro * c1 - 1.0;
        let ro0 = t * r - ro * s;
        let ro1 = t * r + ro * s;

        if !(sides & check.side).is_empty() {
            let (mut test_s, mut test0, mut test1) = (s, ro0, ro1);
            if check.side == Sides::INNER {
                test_s = -test_s;
                test0 = -test0;
                test1 = -test1;
            }
            let mut outcome = Tests::NONE;
            outcome.set(Tests::INTERSECTION, 2.0 * test_s * r < dc[0] + dc[1]);
            outcome.set(Tests::ZERO_0, normal[0].len - test0 < 0.0);
            outcome.set(Tests::ZERO_1, normal[1].len + test1 < 0.0);
            outcome.set(
                Tests::CLIP_0,
                normal[0].len + dc[0] + test_s - test1 * c < 0.0,
            );
            outcome.set(
                Tests::CLIP_1,
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
        if !(crs0.abs() < self.err_a * dot0 && crs1.abs() < self.err_a * dot1) {
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
        if !(crs0.abs() < self.err_a * dot0 && crs1.abs() < self.err_a * dot1) {
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

    fn process_cubic(
        &mut self,
        pt: &[Vector; 4],
        deriv: &[DVector; 3],
        normal: &[Normal; 2],
        mut sides: Sides,
        first: bool,
    ) -> Result<()> {
        charge(&mut self.work, 1)?;
        let cubic = CubicFrame::new(deriv, normal);
        let Some((rest, check, fold)) = self.fold_cubic(pt, deriv, &cubic, sides, first)? else {
            self.last_normal = normal[1].v;
            return Ok(());
        };
        sides = rest;
        if !check.is_empty() {
            let (good, result) = self.estimate_cubic_error(&cubic, &fold, check);
            if !good.is_empty() {
                self.emit_first_point(pt[0], SegmentKind::Cubic, good)?;
                self.emit(pt[1], result[0], None, good)?;
                self.emit(pt[2], result[1], None, good)?;
                sides = sides.without(good);
                if sides.is_empty() {
                    self.last_normal = normal[1].v;
                    return Ok(());
                }
            }
        }
        self.split_cubic(pt, deriv, normal, sides, first)
    }

    /// Handles the side a cubic folds over: skips a self-intersecting offset
    /// there, and decides which sides the error estimate checks and how. Returns
    /// the sides still to draw, those to check and the fold's tests, or `None`
    /// when nothing remains to draw.
    fn fold_cubic(
        &mut self,
        pt: &[Vector; 4],
        deriv: &[DVector; 3],
        cubic: &CubicFrame,
        mut sides: Sides,
        first: bool,
    ) -> Result<Option<(Sides, Sides, FoldCheck)>> {
        let CubicFrame {
            c,
            s,
            dc,
            ds,
            normal,
        } = *cubic;
        let f0 = normal[0].len * c + normal[1].len + dc[1];
        let f1 = normal[1].len * c + normal[0].len + dc[0];
        let mut g0 = normal[0].len * s - ds[1];
        let mut g1 = normal[1].len * s + ds[0];

        let mut abs_s = s;
        let mut check = sides;
        let mut fold = FoldCheck {
            expected: Tests::INTERSECTION,
            decisive: Tests::NONE,
            side: Sides::INNER,
        };
        if s < 0.0 {
            abs_s = -s;
            fold.expected = Tests::NONE;
            fold.side = Sides::OUTER;
            g0 = -g0;
            g1 = -g1;
        }
        let side = fold.side;

        if !(dc[0] + dc[1] > 0.0) {
            check = Sides::NONE;
        } else if !(sides & side).is_empty() {
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
                        self.prepare_skip(pt[0], side, first)?;
                        if f0 < 0.0 || f1 < 0.0 {
                            self.emit(pt[0], DVector::ZERO, Some(SegmentKind::Line), side)?;
                            self.emit(pt[3], DVector::ZERO, Some(SegmentKind::Line), side)?;
                        } else {
                            let offs = normal[0].v.scale(f0 / abs_s);
                            self.emit(pt[0], offs, Some(SegmentKind::Line), side)?;
                        }
                        sides = sides.without(side);
                        if sides.is_empty() {
                            return Ok(None);
                        }
                    }
                }
                check = check ^ side;
            } else if fold.classify(c, [g0, g1], [f0, f1], abs_s, ds) > 2 {
                check = check ^ side;
            }
        }
        Ok(Some((sides, check, fold)))
    }

    /// Halves a cubic whose estimate failed and strokes both halves, joining
    /// them with a cap where the middle has no direction.
    fn split_cubic(
        &mut self,
        pt: &[Vector; 4],
        deriv: &[DVector; 3],
        normal: &[Normal; 2],
        sides: Sides,
        first: bool,
    ) -> Result<()> {
        let next = split_cubic(pt);
        let d0 = halve(deriv[0]);
        let centre = halve(deriv[1]);
        let d4 = halve(deriv[2]);
        let d1 = mean(d0, centre);
        let d3 = mean(centre, d4);
        let d2 = mean(d1, d3);
        let low = [next[0], next[1], next[2], next[3]];
        let high = [next[3], next[4], next[5], next[6]];

        let len = d2.len();
        if len >= self.min_len {
            let middle = Normal {
                v: d2.scale(1.0 / len),
                len,
            };
            self.process_cubic(
                &low,
                &[d0, d1, d2],
                &[normal[0].halved(), middle],
                sides,
                first,
            )?;
            return self.process_cubic(
                &high,
                &[d2, d3, d4],
                &[middle, normal[1].halved()],
                sides,
                false,
            );
        }

        // Degenerate middle: join the halves with a cap.
        let d1 = DVector::new(d1.x + d2.x, d1.y + d2.y);
        let d3 = DVector::new(d3.x + d2.x, d3.y + d2.y);
        let min_len = self.min_len;
        let directed = |d: DVector| {
            let len = d.len();
            (len >= min_len).then(|| Normal {
                v: d.scale(1.0 / len),
                len,
            })
        };
        match directed(d1) {
            Some(normal1) => self.process_cubic(
                &low,
                &[d0, d1, DVector::ZERO],
                &[normal[0].halved(), normal1],
                sides,
                first,
            )?,
            None => self.emit_first_point(next[0], SegmentKind::Line, sides)?,
        }
        let normal2 = directed(d3);
        self.start_segment(next[2], normal2.unwrap_or(normal[1]).v, sides)?;
        match normal2 {
            Some(normal2) => self.process_cubic(
                &high,
                &[DVector::ZERO, d3, d4],
                &[normal2, normal[1].halved()],
                sides,
                false,
            ),
            None => self.emit_first_point(next[3], SegmentKind::Line, sides),
        }
    }

    fn close_contour(&mut self, mut sides: Sides) -> Result<()> {
        if self.contour_start {
            // Nothing was drawn: the contour is a point, its border a circle.
            if sides == Sides::BOTH {
                sides = Sides::OUTER;
            }
            let pt = self.last_point;
            self.draw_circle(pt, sides)?;
        } else {
            let (first_point, first_normal) = (self.first_point, self.first_normal);
            self.add_line(first_point, sides)?;
            self.start_segment(first_point, first_normal, sides)?;
            let tail = sides.without(self.last_skip) & self.first_skip;
            self.emit(first_point, first_normal, Some(SegmentKind::Line), tail)?;
            if self.last_normal != first_normal {
                let head = sides.without(self.last_skip).without(self.first_skip);
                let normal = self.last_normal;
                self.fix_first_point(first_point, normal, head);
            }
            self.contour_start = true;
        }
        if !(sides & Sides::OUTER).is_empty() {
            self.out[0].close_contour();
        }
        if !(sides & Sides::INNER).is_empty() {
            self.out[1].close_contour();
        }
        self.contour_first = [self.out[0].points().len(), self.out[1].points().len()];
        Ok(())
    }
}

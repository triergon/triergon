//! Joins between segments: merged normals, round caps of quadratic arcs, the
//! spike a turn folds over, and the skipped self-intersecting parts.

use triergon_core::Result;

use super::{Side, Sides, Stroker, sum_scaled};
use crate::fp;
use crate::geometry::{DVector, Vector};
use crate::limits::charge;
use crate::outline::SegmentKind;

/// The deepest arc subdivision: 2¹⁵ quadratic splines per cap.
const MAX_SUBDIV: usize = 15;

/// The join state of the contour being stroked.
#[derive(Clone, Copy, Debug)]
pub(super) struct Joint {
    /// No segment of the current contour has been emitted yet.
    pub(super) contour_start: bool,
    /// The sides whose first part of the contour is skipped.
    pub(super) first_skip: Sides,
    /// The sides whose last emitted part is skipped.
    pub(super) last_skip: Sides,
    pub(super) first_normal: DVector,
    pub(super) last_normal: DVector,
    pub(super) first_point: Vector,
}

impl Default for Joint {
    fn default() -> Self {
        Self {
            contour_start: true,
            first_skip: Sides::EMPTY,
            last_skip: Sides::EMPTY,
            first_normal: DVector::ZERO,
            last_normal: DVector::ZERO,
            first_point: Vector::default(),
        }
    }
}

/// The midpoint factors of an arc's subdivision levels, deepest last.
struct ArcFactors {
    mul: [f64; MAX_SUBDIV + 1],
    depth: usize,
}

impl Stroker<'_> {
    /// Halves the arc of cosine `c` until it reaches the split cosine.
    fn arc_factors(&self, mut c: f64) -> ArcFactors {
        let mut mul = [0.0; MAX_SUBDIV + 1];
        let mut pos = MAX_SUBDIV;
        while c < self.tolerance.split_cos && pos > 0 {
            mul[pos] = 0.5f64.sqrt() / (1.0 + c).sqrt();
            c = (1.0 + c) * mul[pos];
            pos -= 1;
        }
        mul[pos] = 1.0 / (1.0 + c);
        let mut factors = [0.0; MAX_SUBDIV + 1];
        factors[..=MAX_SUBDIV - pos].copy_from_slice(&mul[pos..]);
        ArcFactors {
            mul: factors,
            depth: MAX_SUBDIV - pos,
        }
    }

    /// Emits the arc between `n0` and `n1` around `pt` as `2^level` quadratic
    /// splines.
    fn arc(
        &mut self,
        pt: Vector,
        [n0, n1]: [DVector; 2],
        factors: &ArcFactors,
        level: usize,
        sides: Sides,
    ) -> Result<()> {
        charge(self.work, 1)?;
        let centre = sum_scaled(n0, n1, factors.mul[level]);
        if level > 0 {
            self.arc(pt, [n0, centre], factors, level - 1, sides)?;
            return self.arc(pt, [centre, n1], factors, level - 1, sides);
        }
        self.emit(pt, n0, Some(SegmentKind::Quadratic), sides)?;
        self.emit(pt, centre, None, sides)
    }

    /// A round cap from `n0` to `n1` around `pt`; `c` is their dot product.
    fn cap(&mut self, pt: Vector, [n0, n1]: [DVector; 2], mut c: f64, sides: Sides) -> Result<()> {
        let mut bisector = None;
        if c < 0.0 {
            // Beyond a right angle: split at the bisector first.
            let sign = if sides.contains(Side::Inner) {
                -1.0
            } else {
                1.0
            };
            let mul = sign * 0.5f64.sqrt() / (1.0 - c).sqrt();
            bisector = Some(DVector::new((n1.y - n0.y) * mul, (n0.x - n1.x) * mul));
            c = fp::max(0.0, 0.5 + 0.5 * c).sqrt();
        }
        let factors = self.arc_factors(c);
        let depth = factors.depth;
        match bisector {
            None => self.arc(pt, [n0, n1], &factors, depth, sides),
            Some(centre) => {
                self.arc(pt, [n0, centre], &factors, depth, sides)?;
                self.arc(pt, [centre, n1], &factors, depth, sides)
            }
        }
    }

    /// A full circle around `pt`, for a contour that collapsed to a point.
    pub(super) fn draw_circle(&mut self, pt: Vector, sides: Sides) -> Result<()> {
        let factors = self.arc_factors(0.0);
        let normals = [
            DVector::new(1.0, 0.0),
            DVector::new(0.0, 1.0),
            DVector::new(-1.0, 0.0),
            DVector::new(0.0, -1.0),
        ];
        for k in 0..4 {
            let quarter = [normals[k], normals[(k + 1) % 4]];
            self.arc(pt, quarter, &factors, factors.depth, sides)?;
        }
        Ok(())
    }

    /// Starts a segment at `pt` with `normal`, joining it to the previous one
    /// with a cap, or merging the normals when they nearly agree.
    pub(super) fn start_segment(
        &mut self,
        pt: Vector,
        normal: DVector,
        sides: Sides,
    ) -> Result<()> {
        let joint = &mut self.joint;
        if joint.contour_start {
            *joint = Joint {
                contour_start: false,
                first_skip: Sides::EMPTY,
                last_skip: Sides::EMPTY,
                first_normal: normal,
                last_normal: normal,
                first_point: pt,
            };
            return Ok(());
        }
        let prev = joint.last_normal;
        let c = prev.dot(normal);
        if c > self.tolerance.merge_cos {
            joint.last_normal = sum_scaled(prev, normal, 1.0 / (1.0 + c));
            return Ok(());
        }
        joint.last_normal = normal;

        // The side the turn folds over gets no cap, only a spike to the centre.
        let fold = Side::folded(prev.cross(normal));
        let folded = Sides::of([fold]);
        if sides.contains(fold) {
            let open = folded.minus(self.joint.last_skip);
            self.emit(pt, prev, Some(SegmentKind::Line), open)?;
            self.emit(pt, DVector::ZERO, Some(SegmentKind::Line), folded)?;
        }
        self.joint.last_skip = folded;
        let rest = sides.without(fold);
        if rest.is_empty() {
            return Ok(());
        }
        self.cap(pt, [prev, normal], c, rest)
    }

    /// Emits the start of a segment and clears its sides' skips.
    pub(super) fn emit_first_point(
        &mut self,
        pt: Vector,
        segment: SegmentKind,
        sides: Sides,
    ) -> Result<()> {
        self.joint.last_skip = self.joint.last_skip.minus(sides);
        let normal = self.joint.last_normal;
        self.emit(pt, normal, Some(segment), sides)
    }

    /// Prepares to skip a self-intersecting part starting at `pt`.
    pub(super) fn prepare_skip(&mut self, pt: Vector, sides: Sides, first: bool) -> Result<()> {
        if first {
            self.joint.first_skip = self.joint.first_skip.union(sides);
        } else {
            let normal = self.joint.last_normal;
            let open = sides.minus(self.joint.last_skip);
            self.emit(pt, normal, Some(SegmentKind::Line), open)?;
        }
        self.joint.last_skip = self.joint.last_skip.union(sides);
        Ok(())
    }
}

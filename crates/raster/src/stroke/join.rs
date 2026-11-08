//! Joins between segments: normals that nearly agree merge; at a corner the
//! side the turn folds over gets a spike to the corner and the other side a
//! round cap of quadratic arcs.

use triergon_core::Result;

use super::border::{Side, Sides};
use super::{Contour, Stroker};
use crate::fp;
use crate::geometry::{DVector, Vector};
use crate::limits::charge;
use crate::outline::SegmentKind;

/// The most halvings of an arc: 2¹⁵ quadratic splines per arc.
const MAX_HALVINGS: usize = 15;

/// How the offset turns from one normal to the next.
#[derive(Clone, Copy, Debug)]
enum Turn {
    /// The normals nearly agree and merge into this one.
    Smooth(DVector),
    /// A corner of cosine `cos` that folds over `fold`.
    Corner { cos: f64, fold: Side },
}

impl Turn {
    fn between(from: DVector, to: DVector, merge_cos: f64) -> Self {
        let cos = from.dot(to);
        if cos > merge_cos {
            Self::Smooth((from + to).scale(1.0 / (1.0 + cos)))
        } else {
            Self::Corner {
                cos,
                fold: Side::folded(from.cross(to)),
            }
        }
    }
}

/// How finely a circular arc is drawn: the factor that places the middle
/// normal at each level of halving, coarsest first. The last level's halves
/// are single quadratic splines.
#[derive(Clone, Copy, Debug)]
struct Arc {
    factors: [f64; MAX_HALVINGS + 1],
    levels: usize,
}

impl Arc {
    /// An arc whose end normals have dot product `cos`, halved until each
    /// piece's cosine reaches `split_cos`.
    fn new(mut cos: f64, split_cos: f64) -> Self {
        let mut factors = [0.0; MAX_HALVINGS + 1];
        let mut halvings = 0;
        while cos < split_cos && halvings < MAX_HALVINGS {
            let factor = 0.5f64.sqrt() / (1.0 + cos).sqrt();
            factors[halvings] = factor;
            cos = (1.0 + cos) * factor;
            halvings += 1;
        }
        factors[halvings] = 1.0 / (1.0 + cos);
        Self {
            factors,
            levels: halvings + 1,
        }
    }

    fn factors(&self) -> &[f64] {
        &self.factors[..self.levels]
    }
}

impl Stroker<'_> {
    /// Joins the offset so far to a segment starting at `at` with `normal`,
    /// on `sides`; the first segment of a contour opens it instead.
    pub(super) fn join(&mut self, at: Vector, normal: DVector, sides: Sides) -> Result<()> {
        if self.contour.start.is_none() {
            self.contour = Contour::open(at, normal);
            return Ok(());
        }
        let previous = self.contour.normal;
        match Turn::between(previous, normal, self.tolerances.merge_cos) {
            Turn::Smooth(merged) => self.contour.normal = merged,
            Turn::Corner { cos, fold } => {
                if sides.contains(fold) {
                    self.spike(at, previous, fold)?;
                }
                // The folded side is off its curve at the corner; it alone
                // is skipping now, whichever sides this join draws.
                self.contour.skipping = Sides::of([fold]);
                self.contour.normal = normal;
                let cap = fold.opposite();
                if sides.contains(cap) {
                    self.round_cap(at, [previous, normal], cos, cap)?;
                }
            }
        }
        Ok(())
    }

    /// The folded side of a corner at `at`: its offset curve ends there
    /// along `normal`, unless it is skipping, and runs to the corner itself.
    fn spike(&mut self, at: Vector, normal: DVector, side: Side) -> Result<()> {
        let sides = Sides::of([side]);
        let ending = sides.minus(self.contour.skipping);
        self.begin(SegmentKind::Line, at, normal, ending)?;
        self.begin(SegmentKind::Line, at, DVector::ZERO, sides)
    }

    /// A round cap on `side` around `at` from normal `from` to `to`, whose
    /// dot product is `cos`.
    fn round_cap(
        &mut self,
        at: Vector,
        [from, to]: [DVector; 2],
        cos: f64,
        side: Side,
    ) -> Result<()> {
        let sides = Sides::of([side]);
        let split_cos = self.tolerances.split_cos;
        if cos < 0.0 {
            // Wider than a right angle: two arcs meeting at the bisector.
            let sign = match side {
                Side::Outer => 1.0,
                Side::Inner => -1.0,
            };
            let mul = sign * 0.5f64.sqrt() / (1.0 - cos).sqrt();
            let bisector = DVector::new((to.y - from.y) * mul, (from.x - to.x) * mul);
            let arc = Arc::new(fp::max(0.0, 0.5 + 0.5 * cos).sqrt(), split_cos);
            self.arc(at, [from, bisector], arc.factors(), sides)?;
            return self.arc(at, [bisector, to], arc.factors(), sides);
        }
        let arc = Arc::new(cos, split_cos);
        self.arc(at, [from, to], arc.factors(), sides)
    }

    /// A full circle on `side` around `at`, for a contour that never left a
    /// point.
    pub(super) fn circle(&mut self, at: Vector, side: Side) -> Result<()> {
        const QUARTERS: [DVector; 4] = [
            DVector::new(1.0, 0.0),
            DVector::new(0.0, 1.0),
            DVector::new(-1.0, 0.0),
            DVector::new(0.0, -1.0),
        ];
        let arc = Arc::new(0.0, self.tolerances.split_cos);
        let ends = QUARTERS.into_iter().cycle().skip(1);
        for (from, to) in QUARTERS.into_iter().zip(ends) {
            self.arc(at, [from, to], arc.factors(), Sides::of([side]))?;
        }
        Ok(())
    }

    /// The arc around `at` from `from` to `to`: halved at the middle normal
    /// `factors` places, down to quadratic splines.
    fn arc(
        &mut self,
        at: Vector,
        [from, to]: [DVector; 2],
        factors: &[f64],
        sides: Sides,
    ) -> Result<()> {
        charge(self.work, 1)?;
        let Some((&factor, finer)) = factors.split_first() else {
            return Ok(());
        };
        let middle = (from + to).scale(factor);
        if finer.is_empty() {
            self.begin(SegmentKind::Quadratic, at, from, sides)?;
            return self.control(at, middle, sides);
        }
        self.arc(at, [from, middle], finer, sides)?;
        self.arc(at, [middle, to], finer, sides)
    }
}

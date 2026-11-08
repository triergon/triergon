//! Offsetting a spline: each side is fitted by one offset spline of the same
//! order when the error bounds allow, cut short where its offset intersects
//! itself, and otherwise split in half and offset again.

use triergon_core::Result;

use super::Stroker;
use super::border::{Side, Sides};
use super::segment::{Normal, Tolerances};
use crate::geometry::{DVector, Vector};
use crate::limits::charge;
use crate::outline::SegmentKind;

/// A quadratic or cubic spline being offset.
pub(super) trait Spline: Sized {
    /// The order of the offset splines that approximate it.
    const KIND: SegmentKind;

    /// Its inner control points, each with its offset in a fitted spline.
    type Controls: IntoIterator<Item = (Vector, DVector)>;

    fn start(&self) -> Vector;

    fn end(&self) -> Vector;

    /// The normals at its start and end.
    fn normals(&self) -> [Normal; 2];

    /// How `sides` are drawn without a split.
    fn plan(&self, tolerances: &Tolerances, sides: Sides) -> Plan<Self::Controls>;

    /// Halves the spline and offsets the halves on `sides`, joining them
    /// where its middle has no direction.
    fn split(&self, stroker: &mut Stroker<'_>, sides: Sides) -> Result<()>;
}

/// How a spline's sides are drawn before any split.
#[derive(Clone, Copy, Debug)]
pub(super) struct Plan<C> {
    /// The side it folds over, when its offset there intersects itself and
    /// is replaced by a polyline.
    pub(super) shortcut: Option<(Side, Shortcut)>,
    /// The sides one offset spline fits within the error bounds, and its
    /// inner control points.
    pub(super) fit: Option<(Sides, C)>,
}

/// The polyline that replaces a self-intersecting offset, covering only the
/// area of positive winding.
#[derive(Clone, Copy, Debug)]
pub(super) enum Shortcut {
    /// The source's chord, when the offset turns back at either end.
    Chord,
    /// From the start, displaced along its normal by this offset.
    Inset(DVector),
}

impl Shortcut {
    /// The shortcut for a fold whose end terms are `f` against `abs_s`, the
    /// magnitude of the end normals' cross product, from a start with normal
    /// `start`.
    pub(super) fn new([f0, f1]: [f64; 2], abs_s: f64, start: DVector) -> Self {
        if f0 < 0.0 || f1 < 0.0 {
            Self::Chord
        } else {
            Self::Inset(start.scale(f0 / abs_s))
        }
    }
}

impl Stroker<'_> {
    /// Offsets `spline` on `sides`: the shortcut and the fit its plan allows,
    /// then a split for the sides still to draw.
    pub(super) fn offset_spline<S: Spline>(&mut self, spline: &S, mut sides: Sides) -> Result<()> {
        charge(self.work, 1)?;
        let plan = spline.plan(&self.tolerances, sides);
        if let Some((side, shortcut)) = plan.shortcut {
            self.cut_short(side, [spline.start(), spline.end()], shortcut)?;
            sides = sides.without(side);
        }
        if let Some((fitted, controls)) = plan.fit {
            self.resume(S::KIND, spline.start(), fitted)?;
            for (at, offset) in controls {
                self.control(at, offset, fitted)?;
            }
            sides = sides.minus(fitted);
        }
        if sides.is_empty() {
            let [_, end] = spline.normals();
            self.contour.normal = end.unit;
            return Ok(());
        }
        spline.split(self, sides)
    }

    /// Draws `side` of a spline from `start` to `end` as `shortcut`.
    fn cut_short(
        &mut self,
        side: Side,
        [start, end]: [Vector; 2],
        shortcut: Shortcut,
    ) -> Result<()> {
        self.leave_curve(side, start)?;
        let sides = Sides::of([side]);
        match shortcut {
            Shortcut::Chord => {
                self.begin(SegmentKind::Line, start, DVector::ZERO, sides)?;
                self.begin(SegmentKind::Line, end, DVector::ZERO, sides)
            }
            Shortcut::Inset(offset) => self.begin(SegmentKind::Line, start, offset, sides),
        }
    }

    /// `side` leaves its offset curve at `at` for a skipped part. The curve
    /// ends there along the current normal, unless the side is skipping
    /// already; a side whose contour has no point yet starts skipped, and its
    /// start is drawn when the contour closes.
    fn leave_curve(&mut self, side: Side, at: Vector) -> Result<()> {
        let sides = Sides::of([side]);
        if !self.border.has_begun(side) {
            self.contour.skipped_start = self.contour.skipped_start.union(sides);
        } else if !self.contour.skipping.contains(side) {
            self.begin(SegmentKind::Line, at, self.contour.normal, sides)?;
        }
        self.contour.skipping = self.contour.skipping.union(sides);
        Ok(())
    }
}

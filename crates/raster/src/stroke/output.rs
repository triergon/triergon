//! The two border outlines a stroke writes.

use triergon_core::{Error, Result, WorkBudget};

use super::{Side, Sides};
use crate::geometry::Vector;
use crate::limits::charge;
use crate::outline::{Outline, SegmentKind};

/// The outer and inner border outlines, with where their current contours
/// start and the bound on their points.
pub(super) struct Output<'a> {
    outlines: [&'a mut Outline; 2],
    contour_first: [usize; 2],
    points_limit: usize,
}

impl<'a> Output<'a> {
    pub(super) fn new(outlines: [&'a mut Outline; 2], points_limit: usize) -> Self {
        Self {
            outlines,
            contour_first: [0, 0],
            points_limit,
        }
    }

    /// Appends the outer and inner `points` to the outlines of `sides`, each
    /// starting a segment of `segment` when given.
    pub(super) fn push(
        &mut self,
        points: [Vector; 2],
        segment: Option<SegmentKind>,
        sides: Sides,
        work: &mut WorkBudget,
    ) -> Result<()> {
        for side in Side::BOTH {
            if !sides.contains(side) {
                continue;
            }
            charge(work, 1)?;
            let out = &mut *self.outlines[side.index()];
            if out.points().len() >= self.points_limit {
                return Err(Error::Policy("border outline points"));
            }
            out.push_point(points[side.index()], segment)?;
        }
        Ok(())
    }

    /// Replaces the first point of the current contour in the outlines of
    /// `sides` with the outer or inner of `points`.
    pub(super) fn fix_first_point(&mut self, points: [Vector; 2], sides: Sides) {
        for side in Side::BOTH {
            if sides.contains(side) {
                let first = self.contour_first[side.index()];
                self.outlines[side.index()].set_point(first, points[side.index()]);
            }
        }
    }

    /// Closes the current contour of the outlines of `sides`; the next starts
    /// after every point either holds.
    pub(super) fn close_contour(&mut self, sides: Sides) {
        for side in Side::BOTH {
            if sides.contains(side) {
                self.outlines[side.index()].close_contour();
            }
        }
        self.contour_first = self.outlines.each_ref().map(|o| o.points().len());
    }
}

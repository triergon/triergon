//! The two sides of a border and the outlines they are written to.

use std::ops::{Index, IndexMut};

use triergon_core::{Error, Result, WorkBudget};

use crate::geometry::Vector;
use crate::limits::charge;
use crate::outline::{Outline, SegmentKind};
use crate::set::{Member, Set};

/// One side of the border: the source offset along its normal or against it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Side {
    /// The +offset outline.
    Outer,
    /// The −offset outline.
    Inner,
}

impl Member for Side {
    fn bit(self) -> u8 {
        match self {
            Self::Outer => 1,
            Self::Inner => 2,
        }
    }
}

impl Side {
    const BOTH: [Self; 2] = [Self::Outer, Self::Inner];

    /// The side a turn whose normals have cross product `cross` folds over:
    /// the outer side of a clockwise turn, the inner side otherwise.
    pub(super) fn folded(cross: f64) -> Self {
        if cross < 0.0 {
            Self::Outer
        } else {
            Self::Inner
        }
    }

    pub(super) const fn opposite(self) -> Self {
        match self {
            Self::Outer => Self::Inner,
            Self::Inner => Self::Outer,
        }
    }
}

pub(super) type Sides = Set<Side>;

impl Sides {
    pub(super) fn both() -> Self {
        Self::of(Side::BOTH)
    }

    /// The sides in the set, outer first.
    pub(super) fn iter(self) -> impl Iterator<Item = Side> {
        Side::BOTH
            .into_iter()
            .filter(move |&side| self.contains(side))
    }
}

/// A value for each side.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct BySide<T> {
    pub(super) outer: T,
    pub(super) inner: T,
}

impl<T> Index<Side> for BySide<T> {
    type Output = T;

    fn index(&self, side: Side) -> &T {
        match side {
            Side::Outer => &self.outer,
            Side::Inner => &self.inner,
        }
    }
}

impl<T> IndexMut<Side> for BySide<T> {
    fn index_mut(&mut self, side: Side) -> &mut T {
        match side {
            Side::Outer => &mut self.outer,
            Side::Inner => &mut self.inner,
        }
    }
}

/// The outlines of both sides, with where their current contours start and
/// the bound on their points.
pub(super) struct Border<'a> {
    outlines: BySide<&'a mut Outline>,
    contour_start: BySide<usize>,
    points_limit: usize,
}

impl<'a> Border<'a> {
    pub(super) fn new([outer, inner]: [&'a mut Outline; 2], points_limit: usize) -> Self {
        Self {
            outlines: BySide { outer, inner },
            contour_start: BySide::default(),
            points_limit,
        }
    }

    /// Appends each side's point to the outlines of `sides`, starting a
    /// segment of `starts` there when given.
    pub(super) fn push(
        &mut self,
        points: BySide<Vector>,
        starts: Option<SegmentKind>,
        sides: Sides,
        work: &mut WorkBudget,
    ) -> Result<()> {
        for side in sides.iter() {
            charge(work, 1)?;
            let outline = &mut *self.outlines[side];
            if outline.points().len() >= self.points_limit {
                return Err(Error::Policy("border outline points"));
            }
            outline.push_point(points[side], starts)?;
        }
        Ok(())
    }

    /// Whether the current contour of `side` has a point yet.
    pub(super) fn has_begun(&self, side: Side) -> bool {
        self.outlines[side].points().len() > self.contour_start[side]
    }

    /// Replaces the first point of the current contour of `sides` with that
    /// side's point.
    pub(super) fn move_first_point(&mut self, points: BySide<Vector>, sides: Sides) {
        for side in sides.iter() {
            self.outlines[side].set_point(self.contour_start[side], points[side]);
        }
    }

    /// Closes the current contour of `sides`; the next contour of either side
    /// starts after every point it holds.
    pub(super) fn close_contour(&mut self, sides: Sides) {
        for side in sides.iter() {
            self.outlines[side].close_contour();
        }
        self.contour_start = BySide {
            outer: self.outlines.outer.points().len(),
            inner: self.outlines.inner.points().len(),
        };
    }
}

//! Zones: the twilight zone and the glyph zone, their points as the programs
//! move them, where they started (scaled, and in font units), and which have
//! been touched.

use crate::outline::Point;
use triergon_core::{Budget, BudgetVec, Result};

/// Point flags.
pub(super) const ON_CURVE: u8 = 1;
pub(super) const TOUCHED_X: u8 = 2;
pub(super) const TOUCHED_Y: u8 = 4;

/// A zone's points.
#[derive(Debug)]
pub(super) struct Zone {
    /// Current positions, 26.6.
    pub(super) current: BudgetVec<Point>,
    /// Scaled original positions, 26.6.
    pub(super) original: BudgetVec<Point>,
    /// Original positions in font units, or for a composite glyph the
    /// scaled ones.
    pub(super) units: BudgetVec<Point>,
    /// Whether `units` are font units, which original distances are
    /// scaled from.
    pub(super) font_units: bool,
    pub(super) flags: BudgetVec<u8>,
    /// The exclusive end of each contour.
    pub(super) ends: BudgetVec<u32>,
}

impl Zone {
    pub(super) fn new(budget: &Budget) -> Self {
        Self {
            current: BudgetVec::new(budget),
            original: BudgetVec::new(budget),
            units: BudgetVec::new(budget),
            font_units: false,
            flags: BudgetVec::new(budget),
            ends: BudgetVec::new(budget),
        }
    }

    /// A zone of `count` points at the origin, without contours.
    pub(super) fn reset(&mut self, count: usize) -> Result<()> {
        self.clear();
        self.current.try_resize(count, Point::default())?;
        self.original.try_resize(count, Point::default())?;
        self.units.try_resize(count, Point::default())?;
        self.flags.try_resize(count, 0)?;
        Ok(())
    }

    pub(super) fn clear(&mut self) {
        self.current.clear();
        self.original.clear();
        self.units.clear();
        self.flags.clear();
        self.ends.clear();
    }

    /// Appends a point at `position` that started at `units`.
    pub(super) fn push(&mut self, position: Point, units: Point, on_curve: bool) -> Result<()> {
        self.current.try_push(position).map_err(|e| e.error)?;
        self.original.try_push(position).map_err(|e| e.error)?;
        self.units.try_push(units).map_err(|e| e.error)?;
        self.flags
            .try_push(if on_curve { ON_CURVE } else { 0 })
            .map_err(|e| e.error)
    }

    pub(super) fn len(&self) -> usize {
        self.current.len()
    }
}

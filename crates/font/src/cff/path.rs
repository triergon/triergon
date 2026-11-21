//! The glyph path of the CFF hinter, without stem darkening: charstring
//! coordinates in 16.16 character space go to a 16.16 device space, x by the
//! horizontal scale and y through the hint map in force, and on to the
//! outline builder, which keeps them as 26.6 (`x >> 10`).
//! Translated from FreeType 2.15.0 (see `docs/provenance.md`).
//!
//! Each line or curve waits until the next one is drawn, and is then mapped
//! with the hint map of the mask in force by then: a new mask takes effect
//! from the element before the one that follows it. A subpath's last element
//! ends on its start as the hint map of its first point maps it. Without
//! darkening no element is offset, so consecutive elements always meet.
//!
//! Unhinted, both scales are 1/64 and the hint maps empty, so the points are
//! font units truncated toward negative infinity. Lines of zero length are
//! dropped in character space, unless a new hint mask takes effect at them,
//! and again in device space; contours start at their first segment; closing
//! a contour drops a final on-curve point equal to its first and a contour
//! left with one point. A path can also be run into no contours at all, for
//! the interpreter's observers alone.

use super::hint::{Blues, HintMap, Hints, mul};
use crate::outline::{Outline, Point, PointKind};
use crate::read::Reader;
use triergon_core::{Result, WorkBudget};

/// A 16.16 charstring number; sums wrap at 32 bits.
pub(super) type Fixed = i32;

/// How a glyph's path reaches device space: 16.16 scales, and the blue zones
/// of its Private DICT when hinting.
#[derive(Debug)]
pub(super) struct Scaling {
    pub(super) x: Fixed,
    pub(super) y: Fixed,
    pub(super) blues: Option<Blues>,
}

impl Scaling {
    /// Font units: a scale of 1/64 (`0x400`), unhinted.
    pub(super) const UNHINTED: Self = Self {
        x: 0x400,
        y: 0x400,
        blues: None,
    };
}

/// The builder's coordinate of a device space value: 26.6.
const fn units(p: (Fixed, Fixed)) -> (i32, i32) {
    (p.0 >> 10, p.1 >> 10)
}

/// A line or curve waiting to be mapped, by the points after its start.
#[derive(Clone, Copy, Debug)]
enum Element {
    Line((Fixed, Fixed)),
    Curve([(Fixed, Fixed); 3]),
}

/// The path state of one charstring (a `seac` component has its own) over a
/// builder shared by the whole glyph.
#[derive(Debug)]
pub(super) struct Path<'s> {
    scaling: &'s Scaling,
    hints: Hints<'s>,
    /// The current point in character space.
    current: (Fixed, Fixed),
    /// The current subpath's start.
    start: (Fixed, Fixed),
    /// The last point handed to the builder, in device space.
    device: (Fixed, Fixed),
    move_pending: bool,
    open: bool,
    closing: bool,
    queued: Option<Element>,
}

impl<'s> Path<'s> {
    /// A path whose stems move up by `origin`, a `seac` accent's offset.
    pub(super) fn new(scaling: &'s Scaling, origin: Fixed) -> Self {
        Self {
            scaling,
            hints: Hints::new(scaling.blues.as_ref(), origin),
            current: (0, 0),
            start: (0, 0),
            device: (0, 0),
            move_pending: true,
            open: false,
            closing: false,
            queued: None,
        }
    }

    /// Whether stem operators still declare stems.
    pub(super) const fn takes_stems(&self) -> bool {
        !self.hints.are_fixed()
    }

    /// Declares the stems of a stem operator's operands.
    pub(super) fn stems(&mut self, horizontal: bool, operands: &[Fixed]) {
        self.hints.add_stems(horizontal, operands);
    }

    /// Reads the mask after a `hintmask`, which takes effect from the next
    /// element on.
    pub(super) fn hint_mask(&mut self, r: &mut Reader<'_>) -> Result<()> {
        self.hints.read_mask(r)
    }

    /// Reads the mask after a `cntrmask`.
    pub(super) fn counter_mask(&mut self, r: &mut Reader<'_>, work: &mut WorkBudget) -> Result<()> {
        self.hints.read_counter_mask(r, work)
    }

    fn build_hint_map(&mut self, work: &mut WorkBudget) -> Result<()> {
        self.hints.build_current(work)
    }

    /// A point in device space: y through `map`, or scaled uniformly without
    /// one.
    fn device_point(&self, map: Option<&HintMap>, p: (Fixed, Fixed)) -> (Fixed, Fixed) {
        let y = map.map_or_else(
            || mul(p.1, self.scaling.y),
            |map| map.map(p.1, self.scaling.y),
        );
        (mul(self.scaling.x, p.0), y)
    }

    /// Moves to `to`, closing the open subpath; a new hint mask takes effect.
    pub(super) fn move_to(
        &mut self,
        b: &mut impl Contours,
        to: (Fixed, Fixed),
        work: &mut WorkBudget,
    ) -> Result<()> {
        self.close(b, work)?;
        self.current = to;
        self.start = to;
        self.move_pending = true;
        if !self.hints.current_is_valid() || self.hints.mask_is_new() {
            self.build_hint_map(work)?;
        }
        self.hints.save_first();
        Ok(())
    }

    /// A line to `to`, dropped at zero length unless a new mask takes effect.
    pub(super) fn line_to(
        &mut self,
        b: &mut impl Contours,
        to: (Fixed, Fixed),
        work: &mut WorkBudget,
    ) -> Result<()> {
        // A new mask takes effect after the queued element, and not at a
        // closing line.
        let new_map = self.hints.mask_is_new() && !self.closing;
        if to == self.current && !new_map {
            return Ok(());
        }
        self.queue(b, Element::Line(to), work)?;
        if new_map {
            self.build_hint_map(work)?;
        }
        self.current = to;
        Ok(())
    }

    /// A curve through the control points `c1` and `c2` to `to`.
    pub(super) fn curve_to(
        &mut self,
        b: &mut impl Contours,
        c1: (Fixed, Fixed),
        c2: (Fixed, Fixed),
        to: (Fixed, Fixed),
        work: &mut WorkBudget,
    ) -> Result<()> {
        self.queue(b, Element::Curve([c1, c2, to]), work)?;
        if self.hints.mask_is_new() {
            self.build_hint_map(work)?;
        }
        self.current = to;
        Ok(())
    }

    /// Closes the open subpath with a line back to its start.
    pub(super) fn close(&mut self, b: &mut impl Contours, work: &mut WorkBudget) -> Result<()> {
        if self.open {
            self.closing = true;
            self.line_to(b, self.start, work)?;
            if let Some(last) = self.queued.take() {
                self.push(b, last, self.start, true)?;
            }
            self.move_pending = true;
            self.open = false;
            self.closing = false;
        }
        Ok(())
    }

    /// Starts the subpath at the first element after a move, then hands the
    /// element before `element` to the builder and queues `element`.
    fn queue(
        &mut self,
        b: &mut impl Contours,
        element: Element,
        work: &mut WorkBudget,
    ) -> Result<()> {
        if self.move_pending {
            // A charstring that draws before its first move moves to the
            // origin first.
            if !self.hints.current_is_valid() {
                self.move_to(b, self.start, work)?;
            }
            self.device = self.device_point(self.hints.current(), self.current);
            b.move_to()?;
            self.move_pending = false;
            self.open = true;
        }
        if let Some(previous) = self.queued.replace(element) {
            self.push(b, previous, self.current, false)?;
        }
        Ok(())
    }

    /// Maps `element` with the current hint map and hands it to the builder,
    /// with a line on to `next`, the start of the next element, should they
    /// not meet in device space. Closing, a line's end and `next` map with the
    /// hint map of the subpath's first point. Lines of zero length in device
    /// space are dropped.
    fn push(
        &mut self,
        b: &mut impl Contours,
        element: Element,
        next: (Fixed, Fixed),
        close: bool,
    ) -> Result<()> {
        let map = self.hints.current();
        let end_map = if close { self.hints.first() } else { map };
        match element {
            Element::Line(to) => {
                let to = self.device_point(end_map, to);
                if to != self.device {
                    b.line(units(self.device), units(to))?;
                    self.device = to;
                }
            }
            Element::Curve(points) => {
                let points = points.map(|p| self.device_point(map, p));
                b.curve(units(self.device), points.map(units))?;
                self.device = points[2];
            }
        }
        let next = self.device_point(end_map, next);
        if next != self.device {
            b.line(units(self.device), units(next))?;
            self.device = next;
        }
        Ok(())
    }
}

/// Where a path's segments go, in 26.6: an outline, or nowhere.
pub(super) trait Contours {
    /// A move: the next segment starts a contour.
    fn move_to(&mut self) -> Result<()>;
    fn line(&mut self, from: (i32, i32), to: (i32, i32)) -> Result<()>;
    fn curve(&mut self, from: (i32, i32), points: [(i32, i32); 3]) -> Result<()>;
    /// Closes the last contour.
    fn close(&mut self) -> Result<()>;
}

/// No contours: the charstring runs only for what its observer records.
impl Contours for () {
    fn move_to(&mut self) -> Result<()> {
        Ok(())
    }

    fn line(&mut self, _: (i32, i32), _: (i32, i32)) -> Result<()> {
        Ok(())
    }

    fn curve(&mut self, _: (i32, i32), _: [(i32, i32); 3]) -> Result<()> {
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        Ok(())
    }
}

/// Builds the contours of an [`Outline`], each begun by its first segment.
#[derive(Debug)]
pub(super) struct Builder<'o> {
    outline: &'o mut Outline,
    /// A segment was drawn since the last move.
    begun: bool,
    /// The contour being drawn is not in the outline's contour ends yet.
    open: bool,
}

impl<'o> Builder<'o> {
    pub(super) const fn new(outline: &'o mut Outline) -> Self {
        Self {
            outline,
            begun: false,
            open: false,
        }
    }

    fn push(&mut self, (x, y): (i32, i32), kind: PointKind) -> Result<()> {
        self.outline.push_point(Point::new(x, y), kind)
    }

    fn start(&mut self, at: (i32, i32)) -> Result<()> {
        if !self.begun {
            self.begun = true;
            self.open = true;
            self.push(at, PointKind::On)?;
        }
        Ok(())
    }
}

impl Contours for Builder<'_> {
    fn move_to(&mut self) -> Result<()> {
        self.close()?;
        self.begun = false;
        Ok(())
    }

    fn line(&mut self, from: (i32, i32), to: (i32, i32)) -> Result<()> {
        self.start(from)?;
        self.push(to, PointKind::On)
    }

    fn curve(&mut self, from: (i32, i32), points: [(i32, i32); 3]) -> Result<()> {
        self.start(from)?;
        self.push(points[0], PointKind::Cubic)?;
        self.push(points[1], PointKind::Cubic)?;
        self.push(points[2], PointKind::On)
    }

    /// Closes the last contour: a final on-curve point equal to its first
    /// goes, and so does a contour left with one point or none. Closing it
    /// again (two moves in a row) applies the same rules once more.
    fn close(&mut self) -> Result<()> {
        let o = &mut *self.outline;
        let ends = o.contour_ends();
        let closed = ends.len();
        if closed == 0 && !self.open {
            return Ok(());
        }
        let first = if self.open {
            ends.last().map_or(0, |&e| e as usize)
        } else {
            ends.len().checked_sub(2).map_or(0, |i| ends[i] as usize)
        };
        let len = o.len();
        if first == len {
            // A contour without points.
            if self.open {
                self.open = false;
            } else {
                o.pop_contour_end();
            }
            return Ok(());
        }
        let points = o.points();
        if len > 1
            && points[first].point == points[len - 1].point
            && points[len - 1].kind == PointKind::On
        {
            o.truncate_points(len - 1);
        }
        let len = o.len();
        if !self.open {
            o.pop_contour_end();
        }
        self.open = false;
        if first + 1 == len {
            o.truncate_points(first);
        } else {
            o.end_contour()?;
        }
        Ok(())
    }
}

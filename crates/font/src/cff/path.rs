//! The glyph path of FreeType's CFF engine for unhinted outlines without stem
//! darkening: charstring coordinates in 16.16, mapped to a "device" space at
//! 1/64 scale, and built into contours of whole font units.
//!
//! FreeType renders unhinted CFF glyphs at unity scale and scales the integer
//! outline afterwards, so the points here are what `FT_LOAD_NO_SCALE` yields:
//! font units truncated toward negative infinity. Zero-length lines are dropped
//! in device space; contours start at their first segment; closing a contour
//! drops a final on-curve point equal to its first and a contour left with one
//! point.

use crate::fixed::mul_fix;
use crate::outline::{Coordinate, Outline, Point, PointKind};
use triergon_core::Result;

/// A 16.16 charstring number (`CF2_Fixed`); sums wrap at 32 bits.
pub(super) type Fixed = i32;

/// Device space at unity scale: 16.16 at 1/64 of a font unit.
fn device(p: (Fixed, Fixed)) -> (Fixed, Fixed) {
    let map = |v: Fixed| mul_fix(i64::from(v), 0x400) as Fixed;
    (map(p.0), map(p.1))
}

/// The builder's integer coordinate of a device value (26.6 at unity scale).
fn units(p: (Fixed, Fixed)) -> (i32, i32) {
    (p.0 >> 10, p.1 >> 10)
}

/// The path state of one charstring (a `seac` component has its own) over a
/// builder shared by the whole glyph.
#[derive(Debug)]
pub(super) struct Path {
    current: (Fixed, Fixed),
    start: (Fixed, Fixed),
    device: (Fixed, Fixed),
    move_pending: bool,
    open: bool,
    /// A `hintmask` came since the last segment: a zero-length line still
    /// starts a contour.
    new_mask: bool,
}

impl Path {
    pub(super) const fn new() -> Self {
        Self {
            current: (0, 0),
            start: (0, 0),
            device: (0, 0),
            move_pending: true,
            open: false,
            new_mask: false,
        }
    }

    pub(super) const fn hint_mask(&mut self) {
        self.new_mask = true;
    }

    pub(super) fn move_to<C: Coordinate>(
        &mut self,
        b: &mut Builder<'_, C>,
        to: (Fixed, Fixed),
    ) -> Result<()> {
        self.close(b)?;
        self.current = to;
        self.start = to;
        self.move_pending = true;
        self.new_mask = false;
        Ok(())
    }

    pub(super) fn line_to<C: Coordinate>(
        &mut self,
        b: &mut Builder<'_, C>,
        to: (Fixed, Fixed),
    ) -> Result<()> {
        self.line(b, to, false)
    }

    fn line<C: Coordinate>(
        &mut self,
        b: &mut Builder<'_, C>,
        to: (Fixed, Fixed),
        closing: bool,
    ) -> Result<()> {
        let new_mask = self.new_mask && !closing;
        if to == self.current && !new_mask {
            return Ok(());
        }
        if new_mask {
            self.new_mask = false;
        }
        self.begin(b)?;
        let end = device(to);
        if end != self.device {
            b.line(units(self.device), units(end))?;
            self.device = end;
        }
        self.current = to;
        Ok(())
    }

    pub(super) fn curve_to<C: Coordinate>(
        &mut self,
        b: &mut Builder<'_, C>,
        c1: (Fixed, Fixed),
        c2: (Fixed, Fixed),
        to: (Fixed, Fixed),
    ) -> Result<()> {
        self.new_mask = false;
        self.begin(b)?;
        let end = device(to);
        b.curve(
            units(self.device),
            [units(device(c1)), units(device(c2)), units(end)],
        )?;
        self.device = end;
        self.current = to;
        Ok(())
    }

    /// The first segment after a move starts the contour.
    fn begin<C: Coordinate>(&mut self, b: &mut Builder<'_, C>) -> Result<()> {
        if self.move_pending {
            self.move_pending = false;
            self.open = true;
            b.move_to()?;
            self.device = device(self.current);
        }
        Ok(())
    }

    /// Closes the open subpath with a line back to its start.
    pub(super) fn close<C: Coordinate>(&mut self, b: &mut Builder<'_, C>) -> Result<()> {
        if self.open {
            self.line(b, self.start, true)?;
            self.move_pending = true;
            self.open = false;
        }
        Ok(())
    }
}

/// FreeType's outline builder over an [`Outline`]: whole font units.
#[derive(Debug)]
pub(super) struct Builder<'o, C> {
    outline: &'o mut Outline<C>,
    /// A segment was drawn since the last move.
    begun: bool,
    /// The contour being drawn is not in the outline's contour ends yet.
    open: bool,
}

impl<'o, C: Coordinate> Builder<'o, C> {
    pub(super) const fn new(outline: &'o mut Outline<C>) -> Self {
        Self {
            outline,
            begun: false,
            open: false,
        }
    }

    fn push(&mut self, (x, y): (i32, i32), kind: PointKind) -> Result<()> {
        self.outline
            .push_point(Point::new(C::from_units(x), C::from_units(y)), kind)
    }

    fn move_to(&mut self) -> Result<()> {
        self.close()?;
        self.begun = false;
        Ok(())
    }

    fn start(&mut self, at: (i32, i32)) -> Result<()> {
        if !self.begun {
            self.begun = true;
            self.open = true;
            self.push(at, PointKind::On)?;
        }
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

    /// Closes the last contour, as `ps_builder_close_contour` does; closing it
    /// again (two moves in a row) applies the same rules once more.
    pub(super) fn close(&mut self) -> Result<()> {
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

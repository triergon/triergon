//! Glyph outlines: contours of on-curve and control points, in whole font
//! units or in 26.6 fixed-point pixels at a size, as FreeType holds them.

use triergon_core::{Budget, BudgetVec, Error, Result};

/// A position, y up: whole font units from [`Font`](crate::Font), or 26.6
/// pixels (1/64 pixel) from [`SizedFont`](crate::SizedFont).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

impl Point {
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }

    /// The point midway between two, rounded down, as libass implies an
    /// on-curve point between two quadratic control points.
    fn midpoint(self, other: Self) -> Self {
        let half = |a: i32, b: i32| ((i64::from(a) + i64::from(b)) >> 1) as i32;
        Self::new(half(self.x, other.x), half(self.y, other.y))
    }
}

/// What a point of a contour is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointKind {
    /// On the curve.
    On,
    /// A quadratic control point. Two consecutive ones imply an on-curve point
    /// midway between them (TrueType).
    Quadratic,
    /// A cubic control point; they come in pairs between on-curve points (CFF).
    Cubic,
}

/// One point of an outline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutlinePoint {
    pub point: Point,
    pub kind: PointKind,
}

/// A path element of [`Outline::segments`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Segment {
    /// Starts a contour.
    MoveTo(Point),
    LineTo(Point),
    QuadTo(Point, Point),
    CubicTo(Point, Point, Point),
    /// Ends the contour with a line back to its start, if it is not there already.
    Close,
}

/// A glyph outline: closed contours of points, in whole font units as
/// FreeType loads them unscaled (`FT_LOAD_NO_SCALE`) or in 26.6 pixels at a
/// size, depending on what loaded it. Contours are implicitly closed. TrueType contours keep their stored points,
/// including single-point contours; CFF contours are built as FreeType's CFF
/// engine builds them: a contour starts at its first segment, a final on-curve
/// point repeating the start is dropped, and a contour left with one point is
/// dropped.
///
/// Storage is charged to the [`Budget`] the outline was created with and is reused
/// across glyphs.
#[derive(Debug)]
pub struct Outline {
    points: BudgetVec<OutlinePoint>,
    ends: BudgetVec<u32>,
    /// The first point of the contour under construction.
    start: usize,
    limit: usize,
}

impl Outline {
    pub fn new(budget: &Budget) -> Self {
        Self {
            points: BudgetVec::new(budget),
            ends: BudgetVec::new(budget),
            start: 0,
            limit: usize::MAX,
        }
    }

    pub fn points(&self) -> &[OutlinePoint] {
        self.points.as_slice()
    }

    /// The exclusive end index of each contour in [`Self::points`].
    pub fn contour_ends(&self) -> &[u32] {
        self.ends.as_slice()
    }

    /// The points of each contour.
    pub fn contours(&self) -> impl Iterator<Item = &[OutlinePoint]> {
        let points = self.points();
        let mut start = 0;
        self.ends.as_slice().iter().map(move |&end| {
            let end = end as usize;
            let contour = &points[start..end];
            start = end;
            contour
        })
    }

    /// The outline as path segments, with TrueType's implied on-curve points made
    /// explicit. A contour that starts with control points starts at its last
    /// on-curve point, or midway between its last and first points when it has none.
    /// Implied points round down, as libass computes them.
    pub fn segments(&self) -> impl Iterator<Item = Segment> + '_ {
        self.contours().flat_map(contour_segments)
    }

    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    pub fn clear(&mut self) {
        self.points.clear();
        self.ends.clear();
        self.start = 0;
    }

    /// Replaces the contents with a copy of `other`'s, reusing this outline's
    /// storage (for example to style a glyph while keeping the original).
    ///
    /// # Errors
    ///
    /// The budget's errors; the outline is then empty.
    pub fn copy_from(&mut self, other: &Self) -> Result<()> {
        self.clear();
        self.points.try_reserve_exact(other.points.len())?;
        self.ends.try_reserve_exact(other.ends.len())?;
        for &p in other.points() {
            self.points.try_push(p).map_err(|e| e.error)?;
        }
        for &e in other.contour_ends() {
            self.ends.try_push(e).map_err(|e| e.error)?;
        }
        self.start = self.points.len();
        Ok(())
    }

    /// Bytes of storage currently charged.
    pub fn retained_bytes(&self) -> usize {
        self.points.capacity() * size_of::<OutlinePoint>() + self.ends.capacity() * 4
    }

    pub(crate) fn begin(&mut self, limit: usize) {
        self.clear();
        self.limit = limit;
    }

    pub(crate) fn len(&self) -> usize {
        self.points.len()
    }

    pub(crate) fn push_point(&mut self, point: Point, kind: PointKind) -> Result<()> {
        if self.points.len() >= self.limit {
            return Err(Error::Policy("outline points"));
        }
        self.points
            .try_push(OutlinePoint { point, kind })
            .map_err(|e| e.error)
    }

    /// Ends the contour started after the previous one.
    pub(crate) fn end_contour(&mut self) -> Result<()> {
        let end = u32::try_from(self.points.len()).map_err(|_| Error::Overflow)?;
        self.ends.try_push(end).map_err(|e| e.error)?;
        self.start = self.points.len();
        Ok(())
    }

    /// Removes the last contour end; its points become the open contour.
    pub(crate) fn pop_contour_end(&mut self) {
        self.ends.pop();
        self.start = self.ends.as_slice().last().map_or(0, |&e| e as usize);
    }

    /// Keeps the first `len` points.
    pub(crate) fn truncate_points(&mut self, len: usize) {
        while self.points.len() > len {
            self.points.pop();
        }
        self.start = self.start.min(len);
    }

    pub(crate) fn points_mut(&mut self) -> &mut [OutlinePoint] {
        self.points.as_mut_slice()
    }

    /// Appends the contour ends `ends`, which index points already pushed after
    /// `base` points. Storage grows geometrically, so a composite's components
    /// append in amortised constant time.
    pub(crate) fn extend_ends(
        &mut self,
        ends: impl IntoIterator<Item = u32>,
        base: u32,
    ) -> Result<()> {
        for end in ends {
            let end = end.checked_add(base).ok_or(Error::Overflow)?;
            self.ends.try_push(end).map_err(|e| e.error)?;
        }
        self.start = self.points.len();
        Ok(())
    }
}

/// The segments of one contour; see [`Outline::segments`].
#[derive(Clone, Debug)]
struct ContourSegments<'a> {
    points: &'a [OutlinePoint],
    start: Point,
    /// The index of the first point after `start`.
    offset: usize,
    /// Points left to visit.
    left: usize,
    began: bool,
    closed: bool,
    pending: Option<Point>,
    cubic: Option<Point>,
}

fn contour_segments(points: &[OutlinePoint]) -> ContourSegments<'_> {
    let first_on = points.iter().position(|p| p.kind == PointKind::On);
    let (start, offset, left) = match (first_on, points.first(), points.last()) {
        (Some(index), _, _) => (points[index].point, index + 1, points.len() - 1),
        (None, Some(first), Some(last)) => (last.point.midpoint(first.point), 0, points.len()),
        _ => (Point::default(), 0, 0),
    };
    ContourSegments {
        points,
        start,
        offset,
        left,
        began: points.is_empty(),
        closed: points.is_empty(),
        pending: None,
        cubic: None,
    }
}

impl Iterator for ContourSegments<'_> {
    type Item = Segment;

    fn next(&mut self) -> Option<Segment> {
        if !self.began {
            self.began = true;
            return Some(Segment::MoveTo(self.start));
        }
        while self.left > 0 {
            let p = self.points[self.offset % self.points.len()];
            self.offset += 1;
            self.left -= 1;
            match p.kind {
                PointKind::On => return Some(self.curve_to(p.point)),
                PointKind::Quadratic => {
                    if let Some(control) = self.pending.replace(p.point) {
                        return Some(Segment::QuadTo(control, control.midpoint(p.point)));
                    }
                }
                PointKind::Cubic => {
                    if let Some(c1) = self.pending.replace(p.point) {
                        self.cubic = Some(c1);
                    }
                }
            }
        }
        if self.pending.is_some() {
            return Some(self.curve_to(self.start));
        }
        if self.closed {
            return None;
        }
        self.closed = true;
        Some(Segment::Close)
    }
}

impl ContourSegments<'_> {
    fn curve_to(&mut self, end: Point) -> Segment {
        match (self.cubic.take(), self.pending.take()) {
            (Some(c1), Some(c2)) => Segment::CubicTo(c1, c2, end),
            (None, Some(control)) => Segment::QuadTo(control, end),
            _ => Segment::LineTo(end),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Outline, Point, PointKind, Segment};
    use triergon_core::{Budget, Error};

    fn outline(points: &[(i32, i32, PointKind)]) -> Outline {
        let budget = Budget::new(1 << 16).unwrap();
        let mut outline = Outline::new(&budget);
        outline.begin(usize::MAX);
        for &(x, y, kind) in points {
            outline.push_point(Point::new(x, y), kind).unwrap();
        }
        outline.end_contour().unwrap();
        outline
    }

    #[test]
    fn implied_points_and_closing_curves() {
        use PointKind::{On, Quadratic};
        let p = Point::new;
        // Off, on, off, off: starts at the on-curve point and implies (5, 10).
        let o = outline(&[
            (0, 0, Quadratic),
            (10, 0, On),
            (10, 10, Quadratic),
            (0, 10, Quadratic),
        ]);
        let segments: Vec<_> = o.segments().collect();
        assert_eq!(
            segments,
            [
                Segment::MoveTo(p(10, 0)),
                Segment::QuadTo(p(10, 10), p(5, 10)),
                Segment::QuadTo(p(0, 10), p(0, 5)),
                Segment::QuadTo(p(0, 0), p(10, 0)),
                Segment::Close,
            ]
        );
        // Only control points: starts midway between the last and the first.
        let o = outline(&[(0, 0, Quadratic), (10, 0, Quadratic)]);
        let segments: Vec<_> = o.segments().collect();
        assert_eq!(segments[0], Segment::MoveTo(p(5, 0)));
        assert_eq!(segments.len(), 4);
    }

    #[test]
    fn cubic_pairs_lines_and_limits() {
        use PointKind::{Cubic, On};
        let p = Point::new;
        let o = outline(&[
            (0, 0, On),
            (1, 1, Cubic),
            (2, 1, Cubic),
            (3, 0, On),
            (3, -1, On),
        ]);
        let segments: Vec<_> = o.segments().collect();
        assert_eq!(
            segments,
            [
                Segment::MoveTo(p(0, 0)),
                Segment::CubicTo(p(1, 1), p(2, 1), p(3, 0)),
                Segment::LineTo(p(3, -1)),
                Segment::Close,
            ]
        );
        assert_eq!(o.contours().count(), 1);
        let budget = Budget::new(1 << 16).unwrap();
        let mut limited = Outline::new(&budget);
        limited.begin(1);
        limited.push_point(Point::new(0, 0), On).unwrap();
        assert_eq!(
            limited.push_point(Point::new(0, 0), On),
            Err(Error::Policy("outline points"))
        );
    }

    #[test]
    fn implied_midpoints_round_down() {
        let o = outline(&[(0, 0, PointKind::Quadratic), (-3, 3, PointKind::Quadratic)]);
        assert_eq!(
            o.segments().next(),
            Some(Segment::MoveTo(Point::new(-2, 1)))
        );
    }
}

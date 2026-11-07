//! Glyph outlines in font units: contours of on-curve and control points.

use triergon_core::{Budget, BudgetVec, Error, Result};

/// A position in font units, y up.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

impl Point {
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }

    fn midpoint(self, other: Self) -> Self {
        Self::new(
            f32::midpoint(self.x, other.x),
            f32::midpoint(self.y, other.y),
        )
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
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OutlinePoint {
    pub point: Point,
    pub kind: PointKind,
}

/// A path element of [`Outline::segments`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Segment {
    /// Starts a contour.
    MoveTo(Point),
    LineTo(Point),
    QuadTo(Point, Point),
    CubicTo(Point, Point, Point),
    /// Ends the contour with a line back to its start, if it is not there already.
    Close,
}

/// A glyph outline: closed contours of points in font units, as the font stores
/// them. Contours are implicitly closed. TrueType contours keep their stored points,
/// including single-point contours; CFF contours drop a final point that repeats the
/// start, and a `moveto` that draws nothing makes no contour.
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

    pub(crate) fn push(&mut self, x: f64, y: f64, kind: PointKind) -> Result<()> {
        if self.points.len() >= self.limit {
            return Err(Error::Policy("outline points"));
        }
        let point = Point::new(finite(x)?, finite(y)?);
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

    /// The points of the contour under construction.
    pub(crate) fn open_contour(&self) -> &[OutlinePoint] {
        &self.points.as_slice()[self.start..]
    }

    pub(crate) fn pop(&mut self) {
        if self.points.len() > self.start {
            self.points.pop();
        }
    }

    pub(crate) fn points_mut(&mut self) -> &mut [OutlinePoint] {
        self.points.as_mut_slice()
    }

    /// Appends the contour ends `ends`, which index points already pushed after
    /// `base` points.
    pub(crate) fn extend_ends(&mut self, ends: &[u32], base: u32) -> Result<()> {
        self.ends.try_reserve_exact(ends.len())?;
        for &end in ends {
            let end = end.checked_add(base).ok_or(Error::Overflow)?;
            self.ends.try_push(end).map_err(|e| e.error)?;
        }
        self.start = self.points.len();
        Ok(())
    }
}

fn finite(value: f64) -> Result<f32> {
    let narrowed = value as f32;
    if narrowed.is_finite() {
        Ok(narrowed)
    } else {
        Err(Error::Invalid("glyph coordinate outside f32"))
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

    fn outline(points: &[(f64, f64, PointKind)]) -> Outline {
        let budget = Budget::new(1 << 16).unwrap();
        let mut outline = Outline::new(&budget);
        outline.begin(usize::MAX);
        for &(x, y, kind) in points {
            outline.push(x, y, kind).unwrap();
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
            (0.0, 0.0, Quadratic),
            (10.0, 0.0, On),
            (10.0, 10.0, Quadratic),
            (0.0, 10.0, Quadratic),
        ]);
        let segments: Vec<_> = o.segments().collect();
        assert_eq!(
            segments,
            [
                Segment::MoveTo(p(10.0, 0.0)),
                Segment::QuadTo(p(10.0, 10.0), p(5.0, 10.0)),
                Segment::QuadTo(p(0.0, 10.0), p(0.0, 5.0)),
                Segment::QuadTo(p(0.0, 0.0), p(10.0, 0.0)),
                Segment::Close,
            ]
        );
        // Only control points: starts midway between the last and the first.
        let o = outline(&[(0.0, 0.0, Quadratic), (10.0, 0.0, Quadratic)]);
        let segments: Vec<_> = o.segments().collect();
        assert_eq!(segments[0], Segment::MoveTo(p(5.0, 0.0)));
        assert_eq!(segments.len(), 4);
    }

    #[test]
    fn cubic_pairs_lines_and_limits() {
        use PointKind::{Cubic, On};
        let p = Point::new;
        let o = outline(&[
            (0.0, 0.0, On),
            (1.0, 1.0, Cubic),
            (2.0, 1.0, Cubic),
            (3.0, 0.0, On),
            (3.0, -1.0, On),
        ]);
        let segments: Vec<_> = o.segments().collect();
        assert_eq!(
            segments,
            [
                Segment::MoveTo(p(0.0, 0.0)),
                Segment::CubicTo(p(1.0, 1.0), p(2.0, 1.0), p(3.0, 0.0)),
                Segment::LineTo(p(3.0, -1.0)),
                Segment::Close,
            ]
        );
        assert_eq!(o.contours().count(), 1);
        let budget = Budget::new(1 << 16).unwrap();
        let mut limited = Outline::new(&budget);
        limited.begin(1);
        limited.push(0.0, 0.0, On).unwrap();
        assert_eq!(
            limited.push(0.0, 0.0, On),
            Err(Error::Policy("outline points"))
        );
        assert!(limited.push(f64::MAX, 0.0, On).is_err());
    }
}

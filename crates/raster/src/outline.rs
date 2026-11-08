//! Outlines: closed contours of lines and quadratic and cubic splines with
//! points in 26.6 fixed point, y down.

use triergon_core::{Budget, BudgetVec, Error, Result};

use crate::fp;
use crate::geometry::{OUTLINE_MAX, Rect, Vector};
use crate::transform::{Matrix, Projection};

const OUT_OF_RANGE: Error = Error::Invalid("outline point out of range");

/// The order of one outline segment: how many points it owns.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SegmentKind {
    Line = 1,
    Quadratic = 2,
    Cubic = 3,
}

impl SegmentKind {
    /// The points the segment owns: its start and its control points. Its end is
    /// the next segment's start, or the contour's first point when it closes it.
    pub const fn points(self) -> usize {
        self as usize
    }
}

/// One segment of an outline.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Segment {
    pub kind: SegmentKind,
    /// The segment ends at its contour's first point.
    pub closes_contour: bool,
}

/// A segment with its points resolved, start first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Curve {
    Line([Vector; 2]),
    Quadratic([Vector; 3]),
    Cubic([Vector; 4]),
}

impl Curve {
    /// The first point.
    pub const fn start(&self) -> Vector {
        match self {
            Self::Line([p, _]) | Self::Quadratic([p, ..]) | Self::Cubic([p, ..]) => *p,
        }
    }
}

/// What a glyph point is, as a font outline tags it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointKind {
    /// On the curve.
    On,
    /// A quadratic control point; two in a row imply an on-curve point midway.
    Quadratic,
    /// A cubic control point; they come in pairs.
    Cubic,
}

/// A glyph point in 26.6 fixed point, y up, as a scaled font outline gives it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourcePoint {
    pub x: i32,
    pub y: i32,
    pub kind: PointKind,
}

/// A glyph outline as a font presents it: tagged points and the exclusive end
/// index of each contour.
#[derive(Clone, Copy, Debug)]
pub struct GlyphSource<'a> {
    pub points: &'a [SourcePoint],
    pub contour_ends: &'a [u32],
}

/// An outline: points and the segments that own them. Each segment owns as many
/// points as its order; the last segment of a contour closes it back to the
/// contour's first point. Coordinates stay within ±[`OUTLINE_MAX`]. Storage is
/// charged to the outline's [`Budget`] and kept across [`Outline::clear`].
#[derive(Debug)]
pub struct Outline {
    points: BudgetVec<Vector>,
    segments: BudgetVec<Segment>,
}

impl Outline {
    pub fn new(budget: &Budget) -> Self {
        Self {
            points: BudgetVec::new(budget),
            segments: BudgetVec::new(budget),
        }
    }

    pub fn points(&self) -> &[Vector] {
        self.points.as_slice()
    }

    pub fn segments(&self) -> &[Segment] {
        self.segments.as_slice()
    }

    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    pub fn clear(&mut self) {
        self.points.clear();
        self.segments.clear();
    }

    /// Bytes of storage currently charged.
    pub fn retained_bytes(&self) -> usize {
        self.points.capacity() * size_of::<Vector>()
            + self.segments.capacity() * size_of::<Segment>()
    }

    /// The segments with their points. A trailing contour that is not closed, or
    /// a segment without its points, ends the walk.
    pub fn curves(&self) -> Curves<'_> {
        Curves {
            points: self.points(),
            segments: self.segments().iter(),
            start: 0,
            next: 0,
        }
    }

    /// The bounding box of all points, [`Rect::EMPTY`] without points.
    pub fn control_box(&self) -> Rect {
        let mut rect = Rect::EMPTY;
        for &point in self.points() {
            rect.include_point(point);
        }
        rect
    }

    /// Replaces the outline with a copy of `other`.
    pub fn copy_from(&mut self, other: &Self) -> Result<()> {
        self.clear();
        self.points.try_extend_from_slice(other.points())?;
        self.segments.try_extend_from_slice(other.segments())
    }

    /// Appends a segment of `kind` with the `kind.points()` points it owns, as a
    /// drawing adds them; [`Outline::close_contour`] ends the contour.
    ///
    /// # Errors
    ///
    /// `Invalid` when `points` does not hold `kind.points()` points or one lies
    /// beyond ±[`OUTLINE_MAX`]; the outline is then unchanged.
    pub fn push_segment(&mut self, kind: SegmentKind, points: &[Vector]) -> Result<()> {
        if points.len() != kind.points() {
            return Err(Error::Invalid("segment without its points"));
        }
        let segment = Segment {
            kind,
            closes_contour: false,
        };
        self.append(points, &[segment])
    }

    /// Appends points and segments, all or none.
    fn append(&mut self, points: &[Vector], segments: &[Segment]) -> Result<()> {
        if !points.iter().all(|p| p.in_outline_range()) {
            return Err(OUT_OF_RANGE);
        }
        let (n_points, n_segments) = (self.points.len(), self.segments.len());
        let pushed = points
            .iter()
            .try_for_each(|&p| self.points.try_push(p).map_err(|r| r.error))
            .and_then(|()| {
                segments
                    .iter()
                    .try_for_each(|&s| self.segments.try_push(s).map_err(|r| r.error))
            });
        pushed.inspect_err(|_| {
            self.points.truncate(n_points);
            self.segments.truncate(n_segments);
        })
    }

    /// Appends one point within ±[`OUTLINE_MAX`], starting a segment of
    /// `starts` there: how the stroker emits border points.
    pub(crate) fn push_point(&mut self, point: Vector, starts: Option<SegmentKind>) -> Result<()> {
        if !point.in_outline_range() {
            return Err(OUT_OF_RANGE);
        }
        self.points.try_push(point).map_err(|r| r.error)?;
        if let Some(kind) = starts {
            let segment = Segment {
                kind,
                closes_contour: false,
            };
            self.segments.try_push(segment).map_err(|r| r.error)?;
        }
        Ok(())
    }

    /// Overwrites point `index`, if there is one.
    pub(crate) fn set_point(&mut self, index: usize, point: Vector) {
        if let Some(slot) = self.points.as_mut_slice().get_mut(index) {
            *slot = point;
        }
    }

    /// Makes the last segment close its contour; without segments, nothing.
    pub fn close_contour(&mut self) {
        if let Some(last) = self.segments.as_mut_slice().last_mut() {
            last.closes_contour = true;
        }
    }

    /// Appends the rectangle with corners `from` and `to` as one contour of four
    /// lines, starting at `from` and running through (`to.x`, `from.y`).
    ///
    /// # Errors
    ///
    /// `Invalid` when a corner lies beyond ±[`OUTLINE_MAX`]; the outline is then
    /// unchanged.
    pub fn push_rect(&mut self, from: Vector, to: Vector) -> Result<()> {
        let corners = [
            from,
            Vector::new(to.x, from.y),
            to,
            Vector::new(from.x, to.y),
        ];
        let line = Segment {
            kind: SegmentKind::Line,
            closes_contour: false,
        };
        let close = Segment {
            closes_contour: true,
            ..line
        };
        self.append(&corners, &[line, line, line, close])
    }

    /// Appends a glyph's contours, as libass converts font outlines: y is
    /// negated, a contour that starts on a quadratic control point starts at its
    /// last point (or midway to it), implied on-curve points are made explicit
    /// with an arithmetic halving, and contours of one or two points are dropped.
    /// A glyph without points adds nothing.
    ///
    /// # Errors
    ///
    /// `Invalid` for a contour end out of order or past the points, a point
    /// beyond ±[`OUTLINE_MAX`], a lone cubic control point or a contour starting
    /// on one; the outline is then unchanged.
    pub fn push_glyph(&mut self, glyph: &GlyphSource<'_>) -> Result<()> {
        if glyph.points.is_empty() {
            return Ok(());
        }
        let (points, segments) = (self.points.len(), self.segments.len());
        // Implied on-curve points at most double the points.
        let n = glyph.points.len();
        self.points.try_reserve_exact(2 * n)?;
        self.segments.try_reserve_exact(n)?;
        let result = Converter { out: self }.glyph(glyph);
        if result.is_err() {
            self.points.truncate(points);
            self.segments.truncate(segments);
        }
        result
    }

    /// Rotates the outline by 90 degrees, (x, y) → (offset.x + y, offset.y − x),
    /// as libass turns the glyphs of a vertical font.
    ///
    /// # Errors
    ///
    /// `Invalid` when a result leaves ±[`OUTLINE_MAX`]; the outline is unchanged.
    pub fn rotate_90(&mut self, offset: Vector) -> Result<()> {
        let rotate = |p: Vector| {
            let x = i64::from(offset.x) + i64::from(p.y);
            let y = i64::from(offset.y) - i64::from(p.x);
            let limit = i64::from(OUTLINE_MAX);
            (x.abs() <= limit && y.abs() <= limit).then(|| Vector::new(x as i32, y as i32))
        };
        if self.points().iter().any(|&p| rotate(p).is_none()) {
            return Err(Error::Invalid("rotated outline out of range"));
        }
        for point in self.points.as_mut_slice() {
            *point = rotate(*point).unwrap_or(*point);
        }
        Ok(())
    }

    /// Writes the outline scaled by (2^`order[0]`, 2^`order[1]`) into `out`,
    /// rounding down for negative orders.
    ///
    /// # Errors
    ///
    /// `Invalid` when a scaled point would leave ±[`OUTLINE_MAX`]; `out` is then
    /// empty.
    pub(crate) fn scale_pow2_into(&self, out: &mut Self, order: [i32; 2]) -> Result<()> {
        out.clear();
        if self.is_empty() {
            return Ok(());
        }
        // The bound on |coordinate| and the shift of the 32.32 multiplier.
        let axis = |order: i32| {
            let limit = match order {
                1..=31 => OUTLINE_MAX >> order,
                32.. => 0,
                _ => OUTLINE_MAX,
            };
            (limit.unsigned_abs(), order.max(-32) + 32)
        };
        let [(limit_x, shift_x), (limit_y, shift_y)] = order.map(axis);
        let fits = |p: &Vector| p.x.unsigned_abs() <= limit_x && p.y.unsigned_abs() <= limit_y;
        if limit_x == 0 || limit_y == 0 || !self.points().iter().all(fits) {
            return Err(Error::Invalid("scaled outline out of range"));
        }
        let scale = |v: i32, shift: i32| ((i64::from(v) * (1i64 << shift)) >> 32) as i32;
        out.points.try_reserve_exact(self.points.len())?;
        for p in self.points() {
            let point = Vector::new(scale(p.x, shift_x), scale(p.y, shift_y));
            out.points.try_push(point).map_err(|r| r.error)?;
        }
        out.segments.try_extend_from_slice(self.segments())
    }

    /// Writes the outline through `m` into `out`, rounding to nearest: through
    /// its first two rows for [`Projection::Affine`], or with the third row's
    /// depth, clamped to at least 0.1, for [`Projection::Perspective`].
    ///
    /// # Errors
    ///
    /// `Invalid` when a point lands outside ±[`OUTLINE_MAX`]; `out` is then empty.
    pub fn transform_into(&self, m: &Matrix, projection: Projection, out: &mut Self) -> Result<()> {
        out.clear();
        out.points
            .try_resize(self.points.len(), Vector::default())?;
        let limit = f64::from(OUTLINE_MAX);
        for (o, p) in out.points.as_mut_slice().iter_mut().zip(self.points()) {
            let (x, y) = (f64::from(p.x), f64::from(p.y));
            let (x, y) = match projection {
                Projection::Affine => (m.row(0, x, y), m.row(1, x, y)),
                Projection::Perspective => {
                    let w = 1.0 / fp::max(m.row(2, x, y), 0.1);
                    (m.row(0, x, y) * w, m.row(1, x, y) * w)
                }
            };
            if !(x.abs() < limit && y.abs() < limit) {
                out.clear();
                return Err(Error::Invalid("transformed outline out of range"));
            }
            *o = Vector::new(fp::lrint32(x), fp::lrint32(y));
        }
        out.segments.try_extend_from_slice(self.segments())
    }

    /// The smaller of `min_x` and the least x of the points under the
    /// perspective transform `m`, clamped to ±[`OUTLINE_MAX`]; points whose x is
    /// NaN are ignored. libass times `\kf` karaoke from it.
    pub fn min_transformed_x(&self, m: &Matrix, mut min_x: i32) -> i32 {
        let limit = f64::from(OUTLINE_MAX);
        for p in self.points() {
            let (x, y) = (f64::from(p.x), f64::from(p.y));
            let px = m.row(0, x, y) / fp::max(m.row(2, x, y), 0.1);
            if px.is_nan() {
                continue;
            }
            min_x = min_x.min(fp::lrint32(fp::min(fp::max(px, -limit), limit)));
        }
        min_x
    }
}

/// libass's conversion of font contours (`ass_outline_convert`): a state
/// machine over each contour's point kinds.
struct Converter<'a> {
    out: &'a mut Outline,
}

/// The kinds of the points since the last on-curve one.
#[derive(Clone, Copy)]
enum Pending {
    Nothing,
    Quadratic,
    FirstCubic,
    SecondCubic,
}

const MALFORMED: Error = Error::Invalid("malformed glyph outline");

impl Converter<'_> {
    fn glyph(&mut self, glyph: &GlyphSource<'_>) -> Result<()> {
        let mut first = 0usize;
        for &end in glyph.contour_ends {
            let end = end as usize;
            if first >= end || end > glyph.points.len() {
                return Err(MALFORMED);
            }
            let contour = &glyph.points[first..end];
            first = end;
            // Degenerate contours from broken fonts are dropped.
            if contour.len() >= 3 {
                self.contour(contour)?;
            }
        }
        Ok(())
    }

    fn point(&mut self, point: Vector) -> Result<()> {
        self.out.points.try_push(point).map_err(|r| r.error)
    }

    fn segment(&mut self, kind: SegmentKind) -> Result<()> {
        let segment = Segment {
            kind,
            closes_contour: false,
        };
        self.out.segments.try_push(segment).map_err(|r| r.error)
    }

    /// The segment that ends on an on-curve point after `pending`.
    fn finish(&mut self, pending: Pending) -> Result<()> {
        let kind = match pending {
            Pending::Nothing => SegmentKind::Line,
            Pending::Quadratic => SegmentKind::Quadratic,
            Pending::SecondCubic => SegmentKind::Cubic,
            Pending::FirstCubic => return Err(MALFORMED),
        };
        self.segment(kind)
    }

    fn contour(&mut self, contour: &[SourcePoint]) -> Result<()> {
        let (head, mut body) = (&contour[0], &contour[1..]);
        let mut pending = match head.kind {
            PointKind::On => Pending::Nothing,
            PointKind::Quadratic => {
                let last = &contour[contour.len() - 1];
                let start = match last.kind {
                    PointKind::On => {
                        body = &body[..body.len() - 1];
                        flip(last)?
                    }
                    PointKind::Quadratic => halfway(flip(last)?, flip(head)?),
                    PointKind::Cubic => return Err(MALFORMED),
                };
                self.point(start)?;
                Pending::Quadratic
            }
            PointKind::Cubic => return Err(MALFORMED),
        };
        let mut previous = flip(head)?;
        self.point(previous)?;
        for source in body {
            let point = flip(source)?;
            pending = match (source.kind, pending) {
                (PointKind::On, _) => {
                    self.finish(pending)?;
                    Pending::Nothing
                }
                (PointKind::Quadratic, Pending::Nothing) => Pending::Quadratic,
                (PointKind::Quadratic, Pending::Quadratic) => {
                    self.segment(SegmentKind::Quadratic)?;
                    self.point(halfway(previous, point))?;
                    Pending::Quadratic
                }
                (PointKind::Cubic, Pending::Nothing) => Pending::FirstCubic,
                (PointKind::Cubic, Pending::FirstCubic) => Pending::SecondCubic,
                _ => return Err(MALFORMED),
            };
            previous = point;
            self.point(previous)?;
        }
        self.finish(pending)?;
        self.out.close_contour();
        Ok(())
    }
}

/// A source point with y negated, within ±[`OUTLINE_MAX`].
fn flip(p: &SourcePoint) -> Result<Vector> {
    let point = Vector::new(p.x, p.y.wrapping_neg());
    if p.x.unsigned_abs() <= OUTLINE_MAX.unsigned_abs()
        && p.y.unsigned_abs() <= OUTLINE_MAX.unsigned_abs()
    {
        Ok(point)
    } else {
        Err(MALFORMED)
    }
}

/// `(a + b) >> 1` per axis: libass's implied on-curve point.
fn halfway(a: Vector, b: Vector) -> Vector {
    let half = |a: i32, b: i32| ((i64::from(a) + i64::from(b)) >> 1) as i32;
    Vector::new(half(a.x, b.x), half(a.y, b.y))
}

/// The iterator of [`Outline::curves`].
#[derive(Clone, Debug)]
pub struct Curves<'a> {
    points: &'a [Vector],
    segments: std::slice::Iter<'a, Segment>,
    /// The first point of the current contour.
    start: usize,
    /// The first point of the next segment.
    next: usize,
}

impl Curves<'_> {
    /// Whether the next segment starts a contour.
    pub fn at_contour_start(&self) -> bool {
        self.start == self.next
    }
}

impl Iterator for Curves<'_> {
    type Item = (Curve, Segment);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        let segment = *self.segments.next()?;
        let first = self.next;
        let after = first + segment.kind.points();
        let owned = self.points.get(first..after)?;
        let end = if segment.closes_contour {
            let start = self.start;
            self.start = after;
            *self.points.get(start)?
        } else {
            *self.points.get(after)?
        };
        self.next = after;
        let curve = match *owned {
            [a] => Curve::Line([a, end]),
            [a, b] => Curve::Quadratic([a, b, end]),
            [a, b, c] => Curve::Cubic([a, b, c, end]),
            _ => return None,
        };
        Some((curve, segment))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn on(x: i32, y: i32) -> SourcePoint {
        SourcePoint {
            x,
            y,
            kind: PointKind::On,
        }
    }

    #[test]
    fn conversion_negates_y_and_makes_implied_points_explicit() {
        let budget = Budget::new(1 << 16).unwrap();
        let quadratic = |x, y| SourcePoint {
            kind: PointKind::Quadratic,
            ..on(x, y)
        };
        let points = [on(0, 0), quadratic(64, 64), quadratic(128, 0), on(128, -64)];
        let mut outline = Outline::new(&budget);
        let glyph = GlyphSource {
            points: &points,
            contour_ends: &[4],
        };
        outline.push_glyph(&glyph).unwrap();
        assert_eq!(
            outline.points(),
            [(0, 0), (64, -64), (96, -32), (128, 0), (128, 64)].map(|(x, y)| Vector::new(x, y))
        );
        let kinds: Vec<_> = outline.segments().iter().map(|s| s.kind).collect();
        assert_eq!(
            kinds,
            [
                SegmentKind::Quadratic,
                SegmentKind::Quadratic,
                SegmentKind::Line
            ]
        );
        assert!(outline.segments()[2].closes_contour);
    }

    #[test]
    fn a_malformed_glyph_leaves_the_outline_unchanged() {
        let budget = Budget::new(1 << 16).unwrap();
        let mut outline = Outline::new(&budget);
        outline
            .push_rect(Vector::new(0, 0), Vector::new(64, 64))
            .unwrap();
        let points = [on(0, 0), on(1, 1), on(OUTLINE_MAX + 1, 0)];
        let glyph = GlyphSource {
            points: &points,
            contour_ends: &[3],
        };
        assert!(outline.push_glyph(&glyph).is_err());
        assert_eq!((outline.points().len(), outline.segments().len()), (4, 4));
    }
}

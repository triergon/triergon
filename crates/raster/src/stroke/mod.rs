//! Border outlines: the ±1 offset curves of an outline in the space scaled by
//! (1 / x width, 1 / y width), as libass strokes `\bord`, `\xbord` and `\ybord`.
//!
//! One pass walks the source's contours. The pen moves from segment to
//! segment (`segment`), merging steps too short to give a direction; each
//! segment is joined to the offset before it (`join`) and offset on both
//! sides at once (`border`). A line offsets directly. A spline is fitted by
//! one offset spline when the radial and angular error, bounded through the
//! control points of products of B-splines, allows; otherwise it is halved
//! and its halves offset again (`spline`, `quadratic`, `cubic`). Where the
//! offset exceeds the radius of curvature, the side the spline folds over
//! intersects itself, and that part is replaced by a polyline covering only
//! the positive-winding area. Closing a contour joins its end to its start
//! and moves the first point to the merged normal. Two outlines come out
//! because a self-intersecting source needs both offsets.

mod border;
mod cubic;
mod join;
mod quadratic;
mod segment;
mod spline;

use triergon_core::{Error, Result, WorkBudget};

use crate::geometry::{DVector, OUTLINE_MAX, Vector};
use crate::outline::{Outline, SegmentKind};
use crate::shape::Shape;
use border::{Border, Side, Sides};
use segment::{Segment, Tolerances};

/// How [`Raster::stroke`](crate::Raster::stroke) strokes: the half widths of
/// the border along x and y, and the tolerance the offset curves keep to, all
/// in outline units.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Pen {
    /// The border's half widths along x and y (libass's `xbord` and `ybord`);
    /// the larger must lie in `tolerance..=OUTLINE_MAX`.
    pub width: Vector,
    /// The largest error the offset curves may have (libass's `eps`), at
    /// least 1.
    pub tolerance: i32,
}

/// Strokes `source` with `pen` into `border`, which is cleared first; see
/// [`Raster::stroke`](crate::Raster::stroke).
pub(crate) fn stroke(
    source: &Outline,
    pen: &Pen,
    points_limit: usize,
    border: &mut Shape,
    work: &mut WorkBudget,
) -> Result<()> {
    let radius = pen.width.x.max(pen.width.y);
    if pen.tolerance < 1 || radius < pen.tolerance || radius > OUTLINE_MAX {
        return Err(Error::Invalid("border size out of range"));
    }
    let result = border.rebuild(|outlines| {
        Stroker {
            tolerances: Tolerances::new(pen),
            border: Border::new(outlines, points_limit),
            contour: Contour::default(),
            pen: Vector::default(),
            work,
        }
        .stroke(source)
    });
    if result.is_err() {
        border.clear();
    }
    result
}

/// The contour being stroked.
#[derive(Clone, Copy, Debug, Default)]
struct Contour {
    /// Where its first segment starts and the normal there; `None` until a
    /// segment starts.
    start: Option<(Vector, DVector)>,
    /// The normal the offset continues along.
    normal: DVector,
    /// The sides in a skipped part: their offset has left the curve for the
    /// polyline that replaces a self-intersection.
    skipping: Sides,
    /// The sides whose offset began in a skipped part.
    skipped_start: Sides,
}

impl Contour {
    fn open(at: Vector, normal: DVector) -> Self {
        Self {
            start: Some((at, normal)),
            normal,
            ..Self::default()
        }
    }
}

/// The stroker over one source outline.
struct Stroker<'a> {
    tolerances: Tolerances,
    border: Border<'a>,
    contour: Contour,
    /// Where the pen stands: the end of the last segment drawn.
    pen: Vector,
    work: &'a mut WorkBudget,
}

impl Stroker<'_> {
    /// Strokes every contour of `source`.
    fn stroke(&mut self, source: &Outline) -> Result<()> {
        for contour in source.contours() {
            self.pen = contour.start;
            for curve in contour.curves {
                if let Some(segment) = Segment::new(self.pen, curve, &self.tolerances) {
                    self.offset(segment)?;
                }
            }
            if contour.closed {
                self.close_contour()?;
            }
        }
        Ok(())
    }

    /// Offsets `segment` on both sides, joined to the offset before it.
    fn offset(&mut self, segment: Segment) -> Result<()> {
        let sides = Sides::both();
        self.join(segment.start(), segment.start_normal(), sides)?;
        match &segment {
            Segment::Line(line) => {
                self.resume(SegmentKind::Line, line.from, sides)?;
                self.contour.normal = line.normal;
            }
            Segment::Quadratic(spline) => self.offset_spline(spline, sides)?,
            Segment::Cubic(spline) => self.offset_spline(spline, sides)?,
        }
        self.pen = segment.end();
        Ok(())
    }

    /// Closes the contour: a line back to its start, joined to its first
    /// segment. A side whose offset began in a skipped part starts its curve
    /// there; on the others the first point moves to the normal the join
    /// left. A contour that never left a point is a disc: a circle on the
    /// outer side.
    fn close_contour(&mut self) -> Result<()> {
        let Some((start, normal)) = self.contour.start else {
            self.circle(self.pen, Side::Outer)?;
            self.border.close_contour(Sides::of([Side::Outer]));
            return Ok(());
        };
        let sides = Sides::both();
        if let Some(line) = Segment::line(self.pen, start, &self.tolerances) {
            self.offset(line)?;
        }
        self.join(start, normal, sides)?;
        let ending = sides.minus(self.contour.skipping);
        let skipped = self.contour.skipped_start;
        self.begin(
            SegmentKind::Line,
            start,
            normal,
            ending.intersection(skipped),
        )?;
        if self.contour.normal != normal {
            let points = self.tolerances.offsets(start, self.contour.normal);
            self.border.move_first_point(points, ending.minus(skipped));
        }
        self.contour = Contour::default();
        self.border.close_contour(sides);
        Ok(())
    }

    /// Appends `at` displaced by ±`offset` to `sides`, starting a segment of
    /// `kind` there.
    fn begin(
        &mut self,
        kind: SegmentKind,
        at: Vector,
        offset: DVector,
        sides: Sides,
    ) -> Result<()> {
        let points = self.tolerances.offsets(at, offset);
        self.border.push(points, Some(kind), sides, self.work)
    }

    /// Appends the control point `at` displaced by ±`offset` to `sides`.
    fn control(&mut self, at: Vector, offset: DVector, sides: Sides) -> Result<()> {
        let points = self.tolerances.offsets(at, offset);
        self.border.push(points, None, sides, self.work)
    }

    /// Starts a segment of `kind` at `at` on the offset curve of `sides`,
    /// along the current normal; those sides leave any skipped part.
    fn resume(&mut self, kind: SegmentKind, at: Vector, sides: Sides) -> Result<()> {
        self.resume_along(kind, at, self.contour.normal, sides)
    }

    /// [`Stroker::resume`] along `normal`.
    fn resume_along(
        &mut self,
        kind: SegmentKind,
        at: Vector,
        normal: DVector,
        sides: Sides,
    ) -> Result<()> {
        self.contour.skipping = self.contour.skipping.minus(sides);
        self.begin(kind, at, normal, sides)
    }
}

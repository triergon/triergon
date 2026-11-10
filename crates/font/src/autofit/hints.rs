//! The glyph analysis the auto-hinter's writing systems share
//! (`afhints.c`): an outline's points with their directions and weak or
//! strong roles, the segments and edges found along each dimension, and the
//! alignment of points to hinted edges and between them.
//!
//! Points, segments and edges refer to each other by index. Segment and edge
//! fields FreeType keeps in 16 bits (`FT_Short`) are 16-bit here and wrap as
//! they do there.

use crate::fixed::{div_fix, mul_fix};
use crate::glyf::narrow;
use crate::outline::{Outline, PointKind};
use crate::work::charge;
use triergon_core::{Budget, BudgetVec, Result, WorkBudget};

/// A dimension (`AF_Dimension`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Dimension {
    /// x coordinates: vertical segments and edges.
    Horizontal,
    /// y coordinates: horizontal segments and edges.
    Vertical,
}

impl Dimension {
    pub(super) const BOTH: [Self; 2] = [Self::Horizontal, Self::Vertical];

    pub(super) const fn index(self) -> usize {
        match self {
            Self::Horizontal => 0,
            Self::Vertical => 1,
        }
    }
}

/// Directions (`AF_Direction`), chosen so that two are opposite exactly
/// when they sum to zero.
pub(super) mod direction {
    pub(in super::super) const NONE: i8 = 4;
    pub(in super::super) const RIGHT: i8 = 1;
    pub(in super::super) const LEFT: i8 = -1;
    pub(in super::super) const UP: i8 = 2;
    pub(in super::super) const DOWN: i8 = -2;
}

/// Point flags (`AF_FLAG_*`).
pub(super) mod point {
    /// A quadratic control point.
    pub(in super::super) const CONIC: u16 = 1 << 0;
    /// A cubic control point.
    pub(in super::super) const CUBIC: u16 = 1 << 1;
    pub(in super::super) const CONTROL: u16 = CONIC | CUBIC;
    pub(in super::super) const TOUCH_X: u16 = 1 << 2;
    pub(in super::super) const TOUCH_Y: u16 = 1 << 3;
    /// A candidate for weak interpolation.
    pub(in super::super) const WEAK: u16 = 1 << 4;
    /// Kept out of segments.
    pub(in super::super) const IGNORE: u16 = 1 << 6;
}

/// Edge and segment flags (`AF_EDGE_*`).
pub(super) mod edge {
    pub(in super::super) const ROUND: u8 = 1 << 0;
    pub(in super::super) const SERIF: u8 = 1 << 1;
    pub(in super::super) const DONE: u8 = 1 << 2;
    /// Aligned to a neutral blue zone.
    pub(in super::super) const NEUTRAL: u8 = 1 << 3;
    /// Not to be aligned to a blue zone.
    pub(in super::super) const NO_BLUE: u8 = 1 << 4;
}

/// `af_direction_compute`: the direction of a vector, or none when its arms
/// do not differ enough (by a factor of 14, about 4.1 degrees).
pub(super) fn direction_of(dx: i64, dy: i64) -> i8 {
    let (direction, long, short) = if dy >= dx {
        if dy >= -dx {
            (direction::UP, dy, dx)
        } else {
            (direction::LEFT, -dx, dy)
        }
    } else if dy >= -dx {
        (direction::RIGHT, dx, dy)
    } else {
        (direction::DOWN, -dy, dx)
    };
    if long <= 14 * short.abs() {
        direction::NONE
    } else {
        direction
    }
}

/// A width (`AF_WidthRec`): in font units, scaled, and fitted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Width {
    pub(super) org: i64,
    pub(super) cur: i64,
    pub(super) fit: i64,
}

/// `af_sort_pos`: an insertion sort.
pub(super) fn sort_positions(table: &mut [i64]) {
    for i in 1..table.len() {
        let mut j = i;
        while j > 0 && table[j] < table[j - 1] {
            table.swap(j, j - 1);
            j -= 1;
        }
    }
}

/// `af_sort_and_quantize_widths`: sorts the first `count` widths and
/// replaces each cluster no wider than `threshold` by one width; returns
/// the new count.
///
/// As in FreeType, a cluster's sum is divided by the index after it rather
/// than by its size, and no widths at all leave one: the first entry, zero
/// unless set.
pub(super) fn sort_and_quantize_widths(
    table: &mut [Width; 16],
    count: usize,
    threshold: i64,
) -> usize {
    if count == 1 {
        return 1;
    }
    for i in 1..count {
        let mut j = i;
        while j > 0 && table[j].org < table[j - 1].org {
            table.swap(j, j - 1);
            j -= 1;
        }
    }
    let mut start = 0;
    let mut current = table[0].org;
    let mut i = 1;
    while i < count {
        if table[i].org - current > threshold || i == count - 1 {
            if table[i].org - current <= threshold && i == count - 1 {
                i += 1;
            }
            let mut sum = 0;
            for width in &mut table[start..i] {
                sum += width.org;
                width.org = 0;
            }
            table[start].org = sum / i as i64;
            if i + 1 < count {
                start = i + 1;
                current = table[start].org;
            }
        }
        i += 1;
    }
    let mut kept = 1;
    for i in 1..count {
        if table[i].org != 0 {
            table[kept] = table[i];
            kept += 1;
        }
    }
    kept
}

/// An outline point under hinting (`AF_PointRec`).
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Point {
    pub(super) flags: u16,
    /// The direction of the vector into the point.
    pub(super) in_dir: i8,
    /// The direction of the vector out of it.
    pub(super) out_dir: i8,
    /// The scaled position before hinting.
    pub(super) ox: i64,
    pub(super) oy: i64,
    /// The position in font units, as 16 bits.
    pub(super) fx: i16,
    pub(super) fy: i16,
    /// The current position.
    pub(super) x: i64,
    pub(super) y: i64,
    /// Scratch values along the dimension being worked on: positions and
    /// coordinates, or index distances to neighbouring points.
    pub(super) u: i64,
    pub(super) v: i64,
    pub(super) next: usize,
    pub(super) prev: usize,
}

/// A segment (`AF_SegmentRec`): consecutive points about aligned along a
/// dimension.
#[derive(Clone, Copy, Debug)]
pub(super) struct Segment {
    pub(super) flags: u8,
    pub(super) dir: i8,
    /// The position across the segment, in font units.
    pub(super) pos: i16,
    /// Half its spread in position.
    pub(super) delta: i16,
    pub(super) min_coord: i16,
    pub(super) max_coord: i16,
    pub(super) height: i16,
    pub(super) edge: Option<usize>,
    /// The next segment of the same edge, circularly.
    pub(super) edge_next: Option<usize>,
    /// The segment it forms a stem with.
    pub(super) link: Option<usize>,
    /// The primary segment of a serif.
    pub(super) serif: Option<usize>,
    pub(super) score: i64,
    pub(super) first: usize,
    pub(super) last: usize,
}

impl Segment {
    /// A new segment, as FreeType clears one.
    pub(super) const NEW: Self = Self {
        flags: 0,
        dir: 0,
        pos: 0,
        delta: 0,
        min_coord: 0,
        max_coord: 0,
        height: 0,
        edge: None,
        edge_next: None,
        link: None,
        serif: None,
        score: 32000,
        first: 0,
        last: 0,
    };
}

/// An edge (`AF_EdgeRec`): segments at about the same position.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Edge {
    /// The position in font units.
    pub(super) fpos: i16,
    /// The scaled position before hinting.
    pub(super) opos: i64,
    /// The hinted position.
    pub(super) pos: i64,
    pub(super) flags: u8,
    pub(super) dir: i8,
    /// The scale to the next edge, computed when first needed.
    pub(super) scale: i64,
    /// The fitted position of the blue zone edge it aligns to.
    pub(super) blue_edge: Option<i64>,
    pub(super) link: Option<usize>,
    pub(super) serif: Option<usize>,
    pub(super) first: usize,
    pub(super) last: usize,
}

/// The segments and edges of one dimension (`AF_AxisHintsRec`).
#[derive(Debug)]
pub(super) struct AxisHints {
    pub(super) segments: BudgetVec<Segment>,
    /// Sorted by position, ascending, or descending for vertical edges of
    /// scripts hinted top to bottom.
    pub(super) edges: BudgetVec<Edge>,
    /// The direction of the outline's outer contours along the dimension.
    pub(super) major_dir: i8,
}

impl AxisHints {
    fn new(budget: &Budget) -> Self {
        Self {
            segments: BudgetVec::new(budget),
            edges: BudgetVec::new(budget),
            major_dir: direction::NONE,
        }
    }

    /// `af_axis_hints_new_edge`: room for a new edge at `fpos` in direction
    /// `dir`, kept in order; returns its index. An edge in the minor
    /// direction goes before edges at the same position in the major one.
    pub(super) fn new_edge(&mut self, fpos: i16, dir: i8, top_to_bottom: bool) -> Result<usize> {
        let mut at = self.edges.len();
        self.edges.try_push(Edge::default()).map_err(|e| e.error)?;
        let edges = self.edges.as_mut_slice();
        while at > 0 {
            let before = edges[at - 1];
            let ordered = if top_to_bottom {
                before.fpos > fpos
            } else {
                before.fpos < fpos
            };
            if ordered || (before.fpos == fpos && dir == self.major_dir) {
                break;
            }
            edges[at] = before;
            at -= 1;
        }
        Ok(at)
    }
}

/// The hints of one glyph (`AF_GlyphHintsRec`).
#[derive(Debug)]
pub(super) struct GlyphHints {
    pub(super) x_scale: i64,
    pub(super) x_delta: i64,
    pub(super) y_scale: i64,
    pub(super) y_delta: i64,
    pub(super) points: BudgetVec<Point>,
    /// The first point of each contour.
    pub(super) contours: BudgetVec<usize>,
    /// Each contour's vertical extrema, as the accent adjustments last
    /// measured them.
    pub(super) y_minima: BudgetVec<i64>,
    pub(super) y_maxima: BudgetVec<i64>,
    pub(super) axis: [AxisHints; 2],
}

impl GlyphHints {
    /// `af_glyph_hints_init`: empty hints at a zero scale, their storage
    /// charged to `budget`.
    pub(super) fn new(budget: &Budget) -> Self {
        Self {
            x_scale: 0,
            x_delta: 0,
            y_scale: 0,
            y_delta: 0,
            points: BudgetVec::new(budget),
            contours: BudgetVec::new(budget),
            y_minima: BudgetVec::new(budget),
            y_maxima: BudgetVec::new(budget),
            axis: [AxisHints::new(budget), AxisHints::new(budget)],
        }
    }

    /// `af_glyph_hints_reload`: loads the points of `outline`, in font
    /// units, scaling them by the hints' scales, and finds each point's
    /// directions and whether it is weak.
    ///
    /// Vectors shorter than a threshold of 20/2048 em (taxicab length) are
    /// accumulated with their neighbours, which become weak; so do points
    /// within a run of vectors into one quadrant, control points, points
    /// inside a straight segment, flat corners and spikes. The major
    /// directions follow the outline's orientation.
    pub(super) fn reload(
        &mut self,
        outline: &Outline,
        units_per_em: u16,
        work: &mut WorkBudget,
    ) -> Result<()> {
        let source = outline.points();
        let ends = outline.contour_ends();
        let count = ends.last().map_or(0, |&end| end as usize).min(source.len());
        charge(work, 4 * count + ends.len())?;
        self.points.clear();
        self.contours.clear();
        for axis in &mut self.axis {
            axis.segments.clear();
            axis.edges.clear();
        }
        let postscript = is_postscript(outline);
        self.axis[0].major_dir = if postscript {
            direction::DOWN
        } else {
            direction::UP
        };
        self.axis[1].major_dir = if postscript {
            direction::RIGHT
        } else {
            direction::LEFT
        };
        if count == 0 {
            return Ok(());
        }
        self.points.try_reserve_exact(count)?;
        self.contours.try_reserve_exact(ends.len())?;
        let near_limit = i64::from(20 * u32::from(units_per_em) / 2048);
        let mut start = 0;
        for &end in ends {
            let end = (end as usize).min(count);
            if end <= start {
                continue;
            }
            self.contours.try_push(start).map_err(|e| e.error)?;
            for i in start..end {
                let p = source[i];
                let (x, y) = (i64::from(p.point.x), i64::from(p.point.y));
                let flags = match p.kind {
                    PointKind::On => 0,
                    PointKind::Quadratic => point::CONIC,
                    PointKind::Cubic => point::CUBIC,
                };
                let ox = mul_fix(x, self.x_scale) + self.x_delta;
                let oy = mul_fix(y, self.y_scale) + self.y_delta;
                let point = Point {
                    flags,
                    in_dir: direction::NONE,
                    out_dir: direction::NONE,
                    ox,
                    oy,
                    fx: x as i16,
                    fy: y as i16,
                    x: ox,
                    y: oy,
                    u: 0,
                    v: 0,
                    next: if i + 1 == end { start } else { i + 1 },
                    prev: if i == start { end - 1 } else { i - 1 },
                };
                self.points.try_push(point).map_err(|e| e.error)?;
            }
            start = end;
        }
        let points = self.points.as_mut_slice();
        let fx = |p: &[Point], i: usize| i64::from(p[i].fx);
        let fy = |p: &[Point], i: usize| i64::from(p[i].fy);
        // In and out directions: vectors to near points accumulate into the
        // next one that is not near, and the points between are weak.
        let near_limit2 = 2 * near_limit - 1;
        for &contour in self.contours.as_slice() {
            // The first point may be inside a run of near points: start
            // after the run.
            let mut first = contour;
            let mut prev = points[first].prev;
            while prev != contour {
                let (dx, dy) = (
                    fx(points, first) - fx(points, prev),
                    fy(points, first) - fy(points, prev),
                );
                if dx.abs() + dy.abs() >= near_limit2 {
                    break;
                }
                first = prev;
                prev = points[prev].prev;
            }
            let mut current = first;
            points[current].u = 0;
            points[first].v = 0;
            let (mut out_x, mut out_y) = (0, 0);
            let mut next = first;
            loop {
                let point = next;
                next = points[point].next;
                out_x += fx(points, next) - fx(points, point);
                out_y += fy(points, next) - fy(points, point);
                if out_x.abs() + out_y.abs() < near_limit {
                    points[next].flags |= point::WEAK;
                } else {
                    points[current].u = delta_of(next, current);
                    points[next].v = -points[current].u;
                    let out_dir = direction_of(out_x, out_y);
                    points[current].out_dir = out_dir;
                    current = points[current].next;
                    while current != next {
                        points[current].in_dir = out_dir;
                        points[current].out_dir = out_dir;
                        current = points[current].next;
                    }
                    points[next].in_dir = out_dir;
                    points[current].u = delta_of(first, current);
                    points[first].v = -points[current].u;
                    out_x = 0;
                    out_y = 0;
                }
                if next == first {
                    break;
                }
            }
        }
        let offset = |i: usize, by: i64| (i as i64 + by) as usize;
        // Vectors into one quadrant act as one long vector: the points
        // between are weak.
        for i in 0..points.len() {
            let p = points[i];
            if p.flags & point::WEAK != 0
                || p.in_dir != direction::NONE
                || p.out_dir != direction::NONE
            {
                continue;
            }
            let (next, prev) = (offset(i, p.u), offset(i, p.v));
            let (in_x, in_y) = (
                fx(points, i) - fx(points, prev),
                fy(points, i) - fy(points, prev),
            );
            let (out_x, out_y) = (
                fx(points, next) - fx(points, i),
                fy(points, next) - fy(points, i),
            );
            if (in_x ^ out_x) >= 0 && (in_y ^ out_y) >= 0 {
                points[i].flags |= point::WEAK;
                points[prev].u = delta_of(next, prev);
                points[next].v = -points[prev].u;
            }
        }
        // The remaining weak points; every other point is strong.
        for i in 0..points.len() {
            let p = points[i];
            if p.flags & point::WEAK != 0 {
                continue;
            }
            let weak = if p.flags & point::CONTROL != 0 {
                true
            } else if p.out_dir == p.in_dir {
                if p.out_dir != direction::NONE {
                    // Inside a horizontal or vertical segment.
                    true
                } else {
                    let (next, prev) = (offset(i, p.u), offset(i, p.v));
                    let flat = corner_is_flat(
                        fx(points, i) - fx(points, prev),
                        fy(points, i) - fy(points, prev),
                        fx(points, next) - fx(points, i),
                        fy(points, next) - fy(points, i),
                    );
                    if flat {
                        points[prev].u = delta_of(next, prev);
                        points[next].v = -points[prev].u;
                    }
                    flat
                }
            } else {
                // A spike.
                p.in_dir == -p.out_dir
            };
            if weak {
                points[i].flags |= point::WEAK;
            }
        }
        Ok(())
    }

    /// `af_glyph_hints_save`: writes the hinted positions back to
    /// `outline`, whose points the hints were loaded from.
    pub(super) fn save(&self, outline: &mut Outline) -> Result<()> {
        for (target, point) in outline.points_mut().iter_mut().zip(self.points.as_slice()) {
            target.point.x = narrow(point.x)?;
            target.point.y = narrow(point.y)?;
        }
        Ok(())
    }

    /// `af_glyph_hints_align_edge_points`: moves the points of each segment
    /// on an edge to the edge's position and marks them touched.
    pub(super) fn align_edge_points(
        &mut self,
        dim: Dimension,
        work: &mut WorkBudget,
    ) -> Result<()> {
        let axis = &self.axis[dim.index()];
        let points = self.points.as_mut_slice();
        for segment in axis.segments.as_slice() {
            let Some(edge) = segment.edge else {
                continue;
            };
            let position = axis.edges[edge].pos;
            let mut p = segment.first;
            loop {
                charge(work, 1)?;
                match dim {
                    Dimension::Horizontal => {
                        points[p].x = position;
                        points[p].flags |= point::TOUCH_X;
                    }
                    Dimension::Vertical => {
                        points[p].y = position;
                        points[p].flags |= point::TOUCH_Y;
                    }
                }
                if p == segment.last {
                    break;
                }
                p = points[p].next;
            }
        }
        Ok(())
    }

    /// `af_glyph_hints_align_strong_points`, TrueType's `IP`: places each
    /// strong point not yet touched between the edges around it, or beyond
    /// the outermost ones by its original distance.
    pub(super) fn align_strong_points(
        &mut self,
        dim: Dimension,
        work: &mut WorkBudget,
    ) -> Result<()> {
        let touch = match dim {
            Dimension::Horizontal => point::TOUCH_X,
            Dimension::Vertical => point::TOUCH_Y,
        };
        let edges = self.axis[dim.index()].edges.as_mut_slice();
        let Some(&last) = edges.last() else {
            return Ok(());
        };
        let first = edges[0];
        charge(work, 8 * self.points.len())?;
        for p in self.points.as_mut_slice() {
            if p.flags & (touch | point::WEAK) != 0 {
                continue;
            }
            let (u, ou) = match dim {
                Dimension::Vertical => (i64::from(p.fy), p.oy),
                Dimension::Horizontal => (i64::from(p.fx), p.ox),
            };
            let position = if i64::from(first.fpos) - u >= 0 {
                first.pos - (first.opos - ou)
            } else if u - i64::from(last.fpos) >= 0 {
                last.pos + (ou - last.opos)
            } else {
                match enclosing(edges, u) {
                    Ok(on) => edges[on].pos,
                    Err(after) => {
                        // The first edge is before the point and the last
                        // after it.
                        let after = after.clamp(1, edges.len() - 1);
                        let (next, before) = (edges[after], &mut edges[after - 1]);
                        if before.scale == 0 {
                            before.scale = div_fix(
                                next.pos - before.pos,
                                i64::from(next.fpos) - i64::from(before.fpos),
                            );
                        }
                        before.pos + mul_fix(u - i64::from(before.fpos), before.scale)
                    }
                }
            };
            match dim {
                Dimension::Horizontal => p.x = position,
                Dimension::Vertical => p.y = position,
            }
            p.flags |= touch;
        }
        Ok(())
    }

    /// `af_glyph_hints_align_weak_points`, TrueType's `IUP`: interpolates
    /// each run of untouched points between the touched points around it
    /// in its contour, or shifts a contour with one touched point by that
    /// point's move.
    pub(super) fn align_weak_points(
        &mut self,
        dim: Dimension,
        work: &mut WorkBudget,
    ) -> Result<()> {
        let touch = match dim {
            Dimension::Horizontal => point::TOUCH_X,
            Dimension::Vertical => point::TOUCH_Y,
        };
        let points = self.points.as_mut_slice();
        charge(work, 2 * points.len())?;
        for p in points.iter_mut() {
            (p.u, p.v) = match dim {
                Dimension::Horizontal => (p.x, p.ox),
                Dimension::Vertical => (p.y, p.oy),
            };
        }
        for &first_point in self.contours.as_slice() {
            let end_point = points[first_point].prev;
            let touched = |p: &Point| p.flags & touch != 0;
            let Some(first_touched) = (first_point..=end_point).find(|&p| touched(&points[p]))
            else {
                continue;
            };
            let mut p = first_touched;
            let last_touched = loop {
                // Skip touched neighbours.
                while p < end_point && touched(&points[p + 1]) {
                    p += 1;
                }
                let last_touched = p;
                // The next touched point, if any.
                match (p + 1..=end_point).find(|&q| touched(&points[q])) {
                    Some(next) => {
                        interpolate(points, last_touched + 1, next - 1, last_touched, next);
                        p = next;
                    }
                    None => break last_touched,
                }
            };
            if last_touched == first_touched {
                shift(points, first_point, end_point, first_touched);
            } else {
                if last_touched < end_point {
                    interpolate(
                        points,
                        last_touched + 1,
                        end_point,
                        last_touched,
                        first_touched,
                    );
                }
                // FreeType tests against the glyph's first point, not the
                // contour's; an empty range does nothing either way.
                if first_touched > 0 {
                    interpolate(
                        points,
                        first_point,
                        first_touched - 1,
                        last_touched,
                        first_touched,
                    );
                }
            }
        }
        for p in points.iter_mut() {
            match dim {
                Dimension::Horizontal => p.x = p.u,
                Dimension::Vertical => p.y = p.u,
            }
        }
        Ok(())
    }
}

/// The index distance from `from` to `to`.
fn delta_of(to: usize, from: usize) -> i64 {
    to as i64 - from as i64
}

/// The edge at font-unit position `u` (`Ok`), or the index of the first
/// edge after it (`Err`), for a position strictly inside the edges: a linear
/// search over at most eight edges, else a binary one.
fn enclosing(edges: &[Edge], u: i64) -> std::result::Result<usize, usize> {
    let fpos = |i: usize| i64::from(edges[i].fpos);
    if edges.len() <= 8 {
        let at = (0..edges.len())
            .find(|&i| fpos(i) >= u)
            .unwrap_or(edges.len() - 1);
        return if fpos(at) == u { Ok(at) } else { Err(at) };
    }
    let (mut min, mut max) = (0, edges.len());
    while min < max {
        let mid = (min + max) >> 1;
        if u < fpos(mid) {
            max = mid;
        } else if u > fpos(mid) {
            min = mid + 1;
        } else {
            return Ok(mid);
        }
    }
    Err(min)
}

/// `af_iup_shift`: moves points `p1..=p2` but `reference` by the move of
/// `reference` (`u` hinted, `v` original).
fn shift(points: &mut [Point], p1: usize, p2: usize, reference: usize) {
    let delta = points[reference].u - points[reference].v;
    if delta == 0 {
        return;
    }
    for (i, p) in points[p1..=p2].iter_mut().enumerate() {
        if p1 + i != reference {
            p.u = p.v + delta;
        }
    }
}

/// `af_iup_interp`: interpolates points `p1..=p2` between the references,
/// or shifts them by the nearer reference's move outside the references'
/// span.
fn interpolate(points: &mut [Point], p1: usize, p2: usize, ref1: usize, ref2: usize) {
    if p1 > p2 {
        return;
    }
    let (mut a, mut b) = (points[ref1], points[ref2]);
    if a.v > b.v {
        std::mem::swap(&mut a, &mut b);
    }
    let (v1, v2, u1, u2) = (a.v, b.v, a.u, b.u);
    let (d1, d2) = (u1 - v1, u2 - v2);
    let scale = if u1 == u2 || v1 == v2 {
        None
    } else {
        Some(div_fix(u2 - u1, v2 - v1))
    };
    for p in &mut points[p1..=p2] {
        let u = p.v;
        p.u = if u <= v1 {
            u + d1
        } else if u >= v2 {
            u + d2
        } else {
            match scale {
                None => u1,
                Some(scale) => u1 + mul_fix(u - v1, scale),
            }
        };
    }
}

/// `ft_corner_is_flat`: whether the corner of an in and an out vector is
/// nearly straight, by FreeType's approximate lengths: the two arms are
/// less than 17/16 of the hypotenuse.
fn corner_is_flat(in_x: i64, in_y: i64, out_x: i64, out_y: i64) -> bool {
    // `FT_HYPOT`: the longer arm plus 3/8 of the shorter.
    let length = |x: i64, y: i64| {
        let (x, y) = (x.abs(), y.abs());
        if x > y {
            x + ((3 * y) >> 3)
        } else {
            y + ((3 * x) >> 3)
        }
    };
    let d_in = length(in_x, in_y);
    let d_out = length(out_x, out_y);
    let d_hypot = length(in_x + out_x, in_y + out_y);
    d_in + d_out - d_hypot < d_hypot >> 4
}

/// Whether `FT_Outline_Get_Orientation` finds `outline` in PostScript
/// orientation: a positive area by the shoelace formula over every point,
/// control points included, in 64 bits.
fn is_postscript(outline: &Outline) -> bool {
    let points = outline.points();
    let mut area = 0_i64;
    let mut first = 0;
    for &end in outline.contour_ends() {
        let end = (end as usize).min(points.len());
        if end <= first {
            continue;
        }
        let mut prev = points[end - 1].point;
        for p in &points[first..end] {
            let p = p.point;
            let dy = i64::from(p.y) - i64::from(prev.y);
            let sx = i64::from(p.x) + i64::from(prev.x);
            area = area.wrapping_add(dy.wrapping_mul(sx));
            prev = p;
        }
        first = end;
    }
    area > 0
}

#[cfg(test)]
mod tests {
    use super::{
        GlyphHints, Width, corner_is_flat, direction, direction_of, point,
        sort_and_quantize_widths, sort_positions,
    };
    use crate::outline::{Outline, Point, PointKind};
    use triergon_core::{Budget, WorkBudget};

    #[test]
    fn directions_need_a_dominant_arm() {
        assert_eq!(direction_of(0, 10), direction::UP);
        assert_eq!(direction_of(-10, 0), direction::LEFT);
        assert_eq!(direction_of(10, 0), direction::RIGHT);
        assert_eq!(direction_of(0, -10), direction::DOWN);
        // 14 × 1 is not less than 14.
        assert_eq!(direction_of(14, 1), direction::NONE);
        assert_eq!(direction_of(15, 1), direction::RIGHT);
    }

    #[test]
    fn widths_quantize_as_freetype_divides() {
        let mut table = [Width::default(); 16];
        for (w, org) in table.iter_mut().zip([90, 80, 200, 205]) {
            w.org = org;
        }
        // Two close widths, as from an 'o', merge to their mean.
        assert_eq!(sort_and_quantize_widths(&mut table, 2, 10), 1);
        assert_eq!(table[0].org, 85);
        // 80 and 90 merge to 170 / 2; the next cluster starts after 200,
        // the width that ended the first, so 200 stays and 205 alone ends up
        // divided by its index plus one.
        for (w, org) in table.iter_mut().zip([90, 80, 200, 205]) {
            w.org = org;
        }
        assert_eq!(sort_and_quantize_widths(&mut table, 4, 10), 3);
        let widths: Vec<i64> = table[..3].iter().map(|w| w.org).collect();
        assert_eq!(widths, [85, 200, 51]);
        // No widths leave one, the first entry.
        let mut empty = [Width::default(); 16];
        assert_eq!(sort_and_quantize_widths(&mut empty, 0, 10), 1);
        assert_eq!(empty[0].org, 0);
        let mut positions = [3, -1, 2];
        sort_positions(&mut positions);
        assert_eq!(positions, [-1, 2, 3]);
    }

    #[test]
    fn corners_are_flat_when_one_arm_dominates() {
        assert!(corner_is_flat(100, 0, 100, 2));
        assert!(!corner_is_flat(100, 0, 0, 100));
        assert!(corner_is_flat(1000, 0, 5, 5));
    }

    #[test]
    fn reload_finds_directions_and_weak_points() {
        let budget = Budget::new(1 << 20).unwrap();
        let mut outline = Outline::new(&budget);
        outline.begin(usize::MAX);
        // A counter-clockwise (PostScript) square with a midpoint on its
        // bottom edge and a control point on its right.
        for (x, y, kind) in [
            (0, 0, PointKind::On),
            (500, 0, PointKind::On),
            (1000, 0, PointKind::On),
            (1100, 500, PointKind::Quadratic),
            (1000, 1000, PointKind::On),
            (0, 1000, PointKind::On),
        ] {
            outline.push_point(Point::new(x, y), kind).unwrap();
        }
        outline.end_contour().unwrap();
        let mut hints = GlyphHints::new(&budget);
        hints.x_scale = 0x2_0000;
        hints.y_scale = 0x1_0000;
        let mut work = WorkBudget::new(1 << 20);
        hints.reload(&outline, 2048, &mut work).unwrap();
        assert_eq!(hints.axis[0].major_dir, direction::DOWN);
        let points = hints.points.as_slice();
        assert_eq!((points[2].ox, points[2].oy), (2000, 0));
        assert_eq!(
            (points[0].in_dir, points[0].out_dir),
            (direction::DOWN, direction::RIGHT)
        );
        // Inside the bottom segment, and a control point.
        assert_ne!(points[1].flags & point::WEAK, 0);
        assert_ne!(points[3].flags & point::WEAK, 0);
        assert_eq!(points[0].flags & point::WEAK, 0);
        assert_eq!((points[5].prev, points[5].next), (4, 0));
    }
}

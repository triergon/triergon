//! The tile rasteriser: outlines to antialiased coverage, as libass draws them.
//!
//! Splines are flattened into polylines by recursive halving until the control
//! points lie within 16/64 pixel of the chord. The polyline's segments are then
//! distributed over a quad tree of the bitmap: each split along a tile
//! boundary sends every segment to one side, or cuts it in two, and carries
//! the winding number across. A 16×16 tile without segments is solid or empty;
//! one with a single segment is a half-plane; otherwise its segments are summed
//! as trapezoids. Two outlines can be rasterised together as the union of
//! their own nonzero fills, which is how both border offsets combine.

mod tile;

use triergon_core::{Budget, BudgetVec, Result, WorkBudget};

use crate::bitmap::Bitmap;
use crate::geometry::{Rect, Vector, split_cubic, split_quadratic};
use crate::limits::{Limits, charge};
use crate::outline::{Curve, Outline};
use crate::shape::Shape;
use tile::{TILE, TILE_ORDER, TileMut};

/// Spline flattening tolerance in 1/64 pixel.
const OUTLINE_ERROR: i64 = 16;

/// Edge and orientation flags of a polyline segment.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct LineFlags(u8);

impl LineFlags {
    /// The segment runs downwards.
    pub(crate) const DN: Self = Self(1);
    /// The segment runs from upper left to lower right (or the reverse).
    pub(crate) const UL_DR: Self = Self(2);
    /// The bounding box edges that the segment really touches.
    pub(crate) const EXACT_LEFT: Self = Self(4);
    pub(crate) const EXACT_RIGHT: Self = Self(8);
    pub(crate) const EXACT_TOP: Self = Self(16);
    pub(crate) const EXACT_BOTTOM: Self = Self(32);

    #[inline(always)]
    pub(crate) const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    #[inline(always)]
    fn insert(&mut self, other: Self) {
        self.0 |= other.0;
    }

    #[inline(always)]
    fn remove(&mut self, other: Self) {
        self.0 &= !other.0;
    }

    #[inline(always)]
    fn toggle(&mut self, other: Self) {
        self.0 ^= other.0;
    }

    const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

/// A polyline segment as the half-plane `a·x + b·y < c` within its bounding
/// box, coefficients normalised so `max(|a|, |b|) · scale = 2⁶¹`.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Line {
    pub(crate) c: i64,
    pub(crate) a: i32,
    pub(crate) b: i32,
    pub(crate) scale: i32,
    pub(crate) flags: LineFlags,
    pub(crate) x_min: i32,
    pub(crate) x_max: i32,
    pub(crate) y_min: i32,
    pub(crate) y_max: i32,
}

/// The axis a split cuts across: `X` splits at a column, `Y` at a row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Axis {
    X,
    Y,
}

impl Line {
    fn new(p0: Vector, p1: Vector) -> Option<Self> {
        let x = p1.x - p0.x;
        let y = p1.y - p0.y;
        if x == 0 && y == 0 {
            return None;
        }
        let mut flags = LineFlags::EXACT_LEFT
            .union(LineFlags::EXACT_RIGHT)
            .union(LineFlags::EXACT_TOP)
            .union(LineFlags::EXACT_BOTTOM);
        if x < 0 {
            flags.toggle(LineFlags::UL_DR);
        }
        if y >= 0 {
            flags.toggle(LineFlags::DN.union(LineFlags::UL_DR));
        }
        // Normalise the half-plane so the larger coefficient fills 31 bits and
        // `scale` is 2⁶¹ / max_ab, from libass's polynomial approximation.
        let max_ab = x.unsigned_abs().max(y.unsigned_abs());
        let shift = 30 - max_ab.ilog2();
        let max_ab = max_ab << (shift + 1);
        let s1 = (0x5333_3333u64 * ((u64::from(max_ab) * u64::from(max_ab)) >> 32)) >> 32;
        let s2 = 0x8810_624Du64.wrapping_sub((0xBBC6_A7EFu64 * u64::from(max_ab)) >> 32);
        let scale = (s1 as u32).wrapping_add(s2 as u32) as i32;
        Some(Self {
            a: y << shift,
            b: (-x) << shift,
            c: (i64::from(y) * i64::from(p0.x) - i64::from(x) * i64::from(p0.y)) << shift,
            scale,
            flags,
            x_min: p0.x.min(p1.x),
            x_max: p0.x.max(p1.x),
            y_min: p0.y.min(p1.y),
            y_max: p0.y.max(p1.y),
        })
    }

    #[inline(always)]
    fn bounds(&self) -> Rect {
        Rect::new(self.x_min, self.y_min, self.x_max, self.y_max)
    }

    /// The coefficient, extent and edge flags along `axis`, and across it.
    #[inline(always)]
    fn along(&self, axis: Axis) -> (i32, i32, i32, i32) {
        match axis {
            Axis::X => (self.a, self.b, self.x_min, self.x_max),
            Axis::Y => (self.b, self.a, self.y_min, self.y_max),
        }
    }

    #[inline(always)]
    fn across(&self, axis: Axis) -> (i32, i32) {
        match axis {
            Axis::X => (self.y_min, self.y_max),
            Axis::Y => (self.x_min, self.x_max),
        }
    }

    #[inline(always)]
    fn set_extent(&mut self, axis: Axis, min: i32, max: i32) {
        match axis {
            Axis::X => (self.x_min, self.x_max) = (min, max),
            Axis::Y => (self.y_min, self.y_max) = (min, max),
        }
    }

    /// Whether the segment lies wholly before (`below`) or wholly from
    /// (`!below`) coordinate `at` along `axis`.
    #[inline(always)]
    fn is_on_side(&self, axis: Axis, at: i32, below: bool) -> bool {
        let (lo, hi) = exact_edges(axis);
        let (coef, other, min, max) = self.along(axis);
        let (other_min, other_max) = self.across(axis);
        if below && self.flags.contains(hi) {
            return max <= at;
        }
        if !below && self.flags.contains(lo) {
            return min >= at;
        }
        let ul_dr = self.flags.contains(LineFlags::UL_DR);
        let corner = if ul_dr == below { other_max } else { other_min };
        let mut cc =
            self.c - i64::from(coef) * i64::from(at) - i64::from(other) * i64::from(corner);
        if (below && coef > 0) || (!below && coef < 0) {
            cc = -cc;
        }
        cc >= 0
    }

    /// Moves the segment's origin to `at` along `axis`, clamping at the new edge.
    #[inline(always)]
    fn shift_origin(&mut self, axis: Axis, at: i32) {
        let (coef, _, min, max) = self.along(axis);
        let min = (min - at).max(0);
        self.set_extent(axis, min, max - at);
        self.c -= i64::from(coef) * i64::from(at);
        let (lo, _) = exact_edges(axis);
        let (other_lo, _) = exact_edges(axis.other());
        if min == 0 && self.flags.contains(lo.union(LineFlags::UL_DR)) {
            self.flags.remove(other_lo);
        }
    }

    /// Cuts the segment at `at` along `axis`, keeping the part before it and
    /// returning the part after it with its origin at `at`.
    #[inline(always)]
    fn split(&mut self, axis: Axis, at: i32) -> Self {
        let (coef, _, min, max) = self.along(axis);
        debug_assert!(at > min && at < max);
        let mut next = *self;
        next.c -= i64::from(coef) * i64::from(at);
        next.set_extent(axis, 0, max - at);
        self.set_extent(axis, min, at);
        let (lo, hi) = exact_edges(axis);
        let (other_lo, other_hi) = exact_edges(axis.other());
        self.flags.remove(other_lo);
        next.flags.remove(other_hi);
        if self.flags.contains(LineFlags::UL_DR) {
            std::mem::swap(&mut self.flags, &mut next.flags);
        }
        self.flags.insert(hi);
        next.flags.insert(lo);
        next
    }
}

impl Axis {
    const fn other(self) -> Self {
        match self {
            Self::X => Self::Y,
            Self::Y => Self::X,
        }
    }
}

/// The flags of the low and high box edges along `axis`.
#[inline(always)]
const fn exact_edges(axis: Axis) -> (LineFlags, LineFlags) {
    match axis {
        Axis::X => (LineFlags::EXACT_LEFT, LineFlags::EXACT_RIGHT),
        Axis::Y => (LineFlags::EXACT_TOP, LineFlags::EXACT_BOTTOM),
    }
}

/// How one group of segments covers a tile region.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fill {
    Empty,
    Solid,
    /// One segment; `reverse` fills the other side.
    Halfplane {
        reverse: bool,
    },
    Generic,
}

impl Fill {
    fn of(lines: &[Line], winding: i32) -> Self {
        match lines {
            [] if winding != 0 => Self::Solid,
            [] => Self::Empty,
            [line] => {
                let test = LineFlags::UL_DR.union(LineFlags::EXACT_LEFT);
                let mut winding = winding;
                if line.flags.contains(test) == line.flags.contains(LineFlags::DN) {
                    winding += 1;
                }
                match winding {
                    0 => Self::Halfplane { reverse: true },
                    1 => Self::Halfplane { reverse: false },
                    _ => Self::Solid,
                }
            }
            _ => Self::Generic,
        }
    }
}

/// A rectangle of whole tiles within the output bitmap, in pixels.
#[derive(Clone, Copy, Debug)]
struct Region {
    x: usize,
    y: usize,
    width: usize,
    height: usize,
}

/// The output bitmap's bytes and stride.
struct Canvas<'a> {
    data: &'a mut [u8],
    stride: usize,
}

impl Canvas<'_> {
    fn tile(&mut self, x: usize, y: usize) -> TileMut<'_> {
        TileMut {
            data: &mut self.data[y * self.stride + x..],
            stride: self.stride,
        }
    }
}

/// Reusable rasteriser state: two segment buffers and a scratch tile.
#[derive(Debug)]
pub struct Rasterizer {
    lines: [BudgetVec<Line>; 2],
    /// Segments of the first outline; the rest belong to the second.
    first: usize,
    bbox: Rect,
    tile: [u8; TILE * TILE],
    limits: Limits,
}

impl Rasterizer {
    /// A rasteriser whose segment storage and bitmaps are charged to `budget`,
    /// each operation bounded by `limits`.
    pub fn new(budget: &Budget, limits: Limits) -> Self {
        Self {
            lines: [BudgetVec::new(budget), BudgetVec::new(budget)],
            first: 0,
            bbox: Rect::EMPTY,
            tile: [0; TILE * TILE],
            limits,
        }
    }

    /// Bytes of segment storage currently charged.
    pub fn retained_bytes(&self) -> usize {
        (self.lines[0].capacity() + self.lines[1].capacity()) * size_of::<Line>()
    }

    /// Rasterises `shape`, the union of its outlines each filled with the
    /// nonzero rule, into a bitmap of whole tiles placed at its pixel origin.
    /// The bitmap covers the polyline's bounds widened by 1/64 pixel on the
    /// top-left and a pixel on the bottom-right. Returns `None` when there is
    /// nothing to draw, or the bitmap would exceed
    /// [`MAX_BITMAP_BYTES`](crate::MAX_BITMAP_BYTES), as libass draws nothing
    /// then.
    ///
    /// # Errors
    ///
    /// `Policy` when the limits run out, and the budget's errors.
    pub fn rasterize(&mut self, shape: &Shape) -> Result<Option<Bitmap>> {
        let mut work = self.limits.work();
        self.lines[0].clear();
        self.lines[1].clear();
        self.bbox = Rect::EMPTY;
        let [first, second] = shape.outlines();
        self.add_outline(first, &mut work)?;
        self.first = self.lines[0].len();
        self.add_outline(second, &mut work)?;
        for line in self.lines[0].as_slice() {
            self.bbox.include(line.bounds());
        }
        if self.bbox.is_empty() {
            return Ok(None);
        }
        // Widen by 1/64 pixel to avoid the slow path, and a pixel for shifts.
        let x_min = (self.bbox.x_min - 1) >> 6;
        let y_min = (self.bbox.y_min - 1) >> 6;
        let x_max = (self.bbox.x_max + 127) >> 6;
        let y_max = (self.bbox.y_max + 127) >> 6;
        let mask = TILE as i32 - 1;
        let tile_w = ((x_max - x_min + mask) & !mask) as usize;
        let tile_h = ((y_max - y_min + mask) & !mask) as usize;
        let budget = self.lines[0].budget().clone();
        let Some(mut bitmap) = Bitmap::drawable(&budget, tile_w, tile_h, &self.limits)? else {
            return Ok(None);
        };
        bitmap.left = x_min;
        bitmap.top = y_min;
        let stride = bitmap.stride();
        self.fill(bitmap.data_mut(), stride, x_min, y_min, &mut work)?;
        Ok(Some(bitmap))
    }

    fn push(&mut self, p0: Vector, p1: Vector, work: &mut WorkBudget) -> Result<()> {
        if let Some(line) = Line::new(p0, p1) {
            charge(work, 1)?;
            self.lines[0].try_push(line).map_err(|r| r.error)?;
        }
        Ok(())
    }

    fn add_outline(&mut self, outline: &Outline, work: &mut WorkBudget) -> Result<()> {
        for (curve, _) in outline.curves() {
            match curve {
                Curve::Line([p0, p1]) => self.push(p0, p1, work)?,
                Curve::Quadratic(pt) => self.add_quadratic(&pt, work)?,
                Curve::Cubic(pt) => self.add_cubic(&pt, work)?,
            }
        }
        Ok(())
    }

    fn add_quadratic(&mut self, pt: &[Vector; 3], work: &mut WorkBudget) -> Result<()> {
        let chord = Chord::new(pt[0], pt[2]);
        if !chord.needs_split(pt[0], pt[1]) {
            return self.push(pt[0], pt[2], work);
        }
        charge(work, 1)?;
        let next = split_quadratic(pt);
        self.add_quadratic(&[next[0], next[1], next[2]], work)?;
        self.add_quadratic(&[next[2], next[3], next[4]], work)
    }

    fn add_cubic(&mut self, pt: &[Vector; 4], work: &mut WorkBudget) -> Result<()> {
        let chord = Chord::new(pt[0], pt[3]);
        if !chord.needs_split(pt[0], pt[1]) && !chord.needs_split(pt[0], pt[2]) {
            return self.push(pt[0], pt[3], work);
        }
        charge(work, 1)?;
        let next = split_cubic(pt);
        self.add_cubic(&[next[0], next[1], next[2], next[3]], work)?;
        self.add_cubic(&[next[3], next[4], next[5], next[6]], work)
    }

    /// Fills the bitmap whose top-left pixel is (`x0`, `y0`) from the segments.
    fn fill(
        &mut self,
        data: &mut [u8],
        stride: usize,
        x0: i32,
        y0: i32,
        work: &mut WorkBudget,
    ) -> Result<()> {
        let height = data.len() / stride;
        let (x0, y0) = (x0 << 6, y0 << 6);
        for line in self.lines[0].as_mut_slice() {
            line.x_min -= x0;
            line.x_max -= x0;
            line.y_min -= y0;
            line.y_max -= y0;
            line.c -= i64::from(line.a) * i64::from(x0) + i64::from(line.b) * i64::from(y0);
        }
        let bbox = Rect::new(
            self.bbox.x_min - x0,
            self.bbox.y_min - y0,
            self.bbox.x_max - x0,
            self.bbox.y_max - y0,
        );

        // Drop what lies outside the bitmap; the winding carried in from the
        // left and top edges remains.
        let mut counts = [self.first, self.lines[0].len() - self.first];
        let mut winding = [0, 0];
        let size = [(stride as i32) << 6, (height as i32) << 6];
        let [lines, spare] = &mut self.lines;
        for (axis, limit) in [(Axis::X, size[0]), (Axis::Y, size[1])] {
            let max = if axis == Axis::X {
                bbox.x_max
            } else {
                bbox.y_max
            };
            if max >= limit {
                let cut = Cut {
                    axis,
                    at: limit,
                    keep: Keep::Low,
                };
                (counts, _) = cut.split(lines, 0, counts, spare, &mut winding)?;
                spare.clear();
                winding = [0, 0];
            }
        }
        for axis in [Axis::X, Axis::Y] {
            let min = if axis == Axis::X {
                bbox.x_min
            } else {
                bbox.y_min
            };
            if min <= 0 {
                let cut = Cut {
                    axis,
                    at: 0,
                    keep: Keep::High,
                };
                (counts, _) = cut.split(lines, 0, counts, spare, &mut winding)?;
                spare.clear();
            }
        }
        let region = Region {
            x: 0,
            y: 0,
            width: stride,
            height,
        };
        let mut canvas = Canvas { data, stride };
        self.fill_level(&mut canvas, region, 0, counts, winding, work)
    }

    /// Fills `region` from the last `counts` segments of buffer `index`,
    /// splitting it until each part is simple or one tile, and removes those
    /// segments from the buffer.
    fn fill_level(
        &mut self,
        canvas: &mut Canvas<'_>,
        region: Region,
        index: usize,
        counts: [usize; 2],
        winding: [i32; 2],
        work: &mut WorkBudget,
    ) -> Result<()> {
        charge(work, 1)?;
        let len = self.lines[index].len();
        let offs = len - counts[0] - counts[1];
        let all = &self.lines[index].as_slice()[offs..];
        let (group0, group1) = all.split_at(counts[0]);
        let fills = [Fill::of(group0, winding[0]), Fill::of(group1, winding[1])];
        let single = (region.width, region.height) == (TILE, TILE);
        match fills {
            [Fill::Solid, _] | [_, Fill::Solid] => solid(canvas, region, true),
            [Fill::Empty, Fill::Empty] => solid(canvas, region, false),
            [Fill::Halfplane { reverse }, Fill::Empty] => {
                halfplane(canvas, region, &group0[0], reverse);
            }
            [Fill::Empty, Fill::Halfplane { reverse }] => {
                halfplane(canvas, region, &group1[0], reverse);
            }
            [first, second] if single => {
                let mut tile = canvas.tile(region.x, region.y);
                let draw = |tile: &mut TileMut<'_>, fill, lines: &[Line], winding| match fill {
                    Fill::Halfplane { reverse } => {
                        let line = &lines[0];
                        tile::fill_halfplane(
                            tile,
                            line.a,
                            line.b,
                            line.c,
                            signed(line.scale, reverse),
                        );
                    }
                    _ => tile::fill_generic(tile, lines, winding),
                };
                match (first, second) {
                    (_, Fill::Empty) => draw(&mut tile, first, group0, winding[0]),
                    (Fill::Empty, _) => draw(&mut tile, second, group1, winding[1]),
                    _ => {
                        draw(&mut tile, first, group0, winding[0]);
                        let mut scratch = TileMut {
                            data: &mut self.tile,
                            stride: TILE,
                        };
                        draw(&mut scratch, second, group1, winding[1]);
                        tile::merge(&mut tile, &self.tile);
                    }
                }
            }
            _ => return self.split_level(canvas, region, index, counts, winding, work),
        }
        self.lines[index].truncate(offs);
        Ok(())
    }

    fn split_level(
        &mut self,
        canvas: &mut Canvas<'_>,
        region: Region,
        index: usize,
        counts: [usize; 2],
        winding: [i32; 2],
        work: &mut WorkBudget,
    ) -> Result<()> {
        let offs = self.lines[index].len() - counts[0] - counts[1];
        let (axis, low, high) = if region.width > region.height {
            let width = 1 << (region.width - 1).ilog2();
            let low = Region { width, ..region };
            let high = Region {
                x: region.x + width,
                width: region.width - width,
                ..region
            };
            (Axis::X, low, high)
        } else {
            let height = 1 << (region.height - 1).ilog2();
            let low = Region { height, ..region };
            let high = Region {
                y: region.y + height,
                height: region.height - height,
                ..region
            };
            (Axis::Y, low, high)
        };
        let at = (if axis == Axis::X {
            low.width
        } else {
            low.height
        } as i32)
            << 6;
        let mut high_winding = winding;
        let [b0, b1] = &mut self.lines;
        let (lines, other) = if index == 0 { (b0, b1) } else { (b1, b0) };
        let cut = Cut {
            axis,
            at,
            keep: Keep::Low,
        };
        let (low_counts, high_counts) = cut.split(lines, offs, counts, other, &mut high_winding)?;
        self.fill_level(canvas, low, index, low_counts, winding, work)?;
        self.fill_level(canvas, high, index ^ 1, high_counts, high_winding, work)
    }
}

/// `scale`, negated to fill the other side of the line.
fn signed(scale: i32, reverse: bool) -> i32 {
    if reverse { scale.wrapping_neg() } else { scale }
}

fn solid(canvas: &mut Canvas<'_>, region: Region, set: bool) {
    for y in (region.y..region.y + region.height).step_by(TILE) {
        for x in (region.x..region.x + region.width).step_by(TILE) {
            tile::fill_solid(&mut canvas.tile(x, y), set);
        }
    }
}

/// Fills a region with one half-plane, tiles away from the line solid.
fn halfplane(canvas: &mut Canvas<'_>, region: Region, line: &Line, reverse: bool) {
    let scale = signed(line.scale, reverse);
    let (a, b, c) = (line.a, line.b, line.c);
    if (region.width, region.height) == (TILE, TILE) {
        tile::fill_halfplane(&mut canvas.tile(region.x, region.y), a, b, c, scale);
        return;
    }
    let size = i64::from(a.unsigned_abs() + b.unsigned_abs()) << (TILE_ORDER + 5);
    let offs = (i64::from(a) + i64::from(b)) * (1 << (TILE_ORDER + 5));
    for ty in 0..region.height / TILE {
        for tx in 0..region.width / TILE {
            let cc =
                c - (i64::from(a) * tx as i64 + i64::from(b) * ty as i64) * (1 << (TILE_ORDER + 6));
            let offs_c = offs - cc;
            let mut tile = canvas.tile(region.x + tx * TILE, region.y + ty * TILE);
            if offs_c.abs() < size {
                tile::fill_halfplane(&mut tile, a, b, cc, scale);
            } else {
                let set = (((offs_c >> 32) as u32) ^ scale as u32) & 0x8000_0000 != 0;
                tile::fill_solid(&mut tile, set);
            }
        }
    }
}

/// Which part of a split stays in the buffer; the other is appended to the
/// spare buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Keep {
    Low,
    High,
}

/// A cut of a group of segments at coordinate `at` along `axis`.
#[derive(Clone, Copy, Debug)]
struct Cut {
    axis: Axis,
    at: i32,
    keep: Keep,
}

impl Cut {
    /// Splits the `counts` segments (per outline group) at `start` of `lines`.
    /// The kept part stays in place, the other is appended to `other`; the
    /// winding of the high part's corner accumulates in `winding`. Returns the
    /// per-group counts of the low and high parts.
    fn split(
        self,
        lines: &mut BudgetVec<Line>,
        start: usize,
        counts: [usize; 2],
        other: &mut BudgetVec<Line>,
        winding: &mut [i32; 2],
    ) -> Result<([usize; 2], [usize; 2])> {
        let Self { axis, at, keep } = self;
        let (other_lo, _) = exact_edges(axis.other());
        let mut kept = start;
        let mut low = [0usize; 2];
        let mut high = [0usize; 2];
        let mut emit = |line: Line,
                        side: Keep,
                        group: usize,
                        lines: &mut BudgetVec<Line>,
                        kept: &mut usize|
         -> Result<()> {
            match side {
                Keep::High => high[group] += 1,
                Keep::Low => low[group] += 1,
            }
            if side == keep {
                lines.as_mut_slice()[*kept] = line;
                *kept += 1;
                Ok(())
            } else {
                other.try_push(line).map_err(|r| r.error)
            }
        };
        for i in 0..counts[0] + counts[1] {
            let group = usize::from(i >= counts[0]);
            let line = lines.as_slice()[start + i];
            let (coef, _, min, _) = line.along(axis);
            // A segment starting on the cut's far edge crosses the high part's
            // corner ray.
            let delta = if line.across(axis).0 == 0 && line.flags.contains(other_lo) {
                if coef < 0 { 1 } else { -1 }
            } else {
                0
            };
            if line.is_on_side(axis, at, true) {
                winding[group] += delta;
                if min >= at {
                    continue;
                }
                let mut part = line;
                let (_, _, min, max) = part.along(axis);
                part.set_extent(axis, min, max.min(at));
                emit(part, Keep::Low, group, lines, &mut kept)?;
                continue;
            }
            if line.is_on_side(axis, at, false) {
                let mut part = line;
                part.shift_origin(axis, at);
                emit(part, Keep::High, group, lines, &mut kept)?;
                continue;
            }
            if line.flags.contains(LineFlags::UL_DR) {
                winding[group] += delta;
            }
            let mut part = line;
            let next = part.split(axis, at);
            emit(part, Keep::Low, group, lines, &mut kept)?;
            emit(next, Keep::High, group, lines, &mut kept)?;
        }
        lines.truncate(kept);
        Ok((low, high))
    }
}

/// The chord of a spline, to test whether a control point strays from it by
/// more than the flattening tolerance.
struct Chord {
    r: Vector,
    r2: i64,
    er: i64,
}

impl Chord {
    fn new(beg: Vector, end: Vector) -> Self {
        let (x, y) = (end.x - beg.x, end.y - beg.y);
        Self {
            r: Vector::new(x, y),
            r2: i64::from(x) * i64::from(x) + i64::from(y) * i64::from(y),
            er: OUTLINE_ERROR * i64::from(x.unsigned_abs().max(y.unsigned_abs())),
        }
    }

    /// Whether `pt` projects outside the chord or lies too far from it.
    fn needs_split(&self, beg: Vector, pt: Vector) -> bool {
        let (x, y) = (i64::from(pt.x - beg.x), i64::from(pt.y - beg.y));
        let (rx, ry) = (i64::from(self.r.x), i64::from(self.r.y));
        let pdr = rx * x + ry * y;
        let pcr = rx * y - ry * x;
        pdr < -self.er || pdr > self.r2 + self.er || pcr.abs() > self.er
    }
}

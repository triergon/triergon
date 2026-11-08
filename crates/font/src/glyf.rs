//! TrueType outlines (OpenType `glyf` and `loca`), unhinted, in font units or
//! scaled as FreeType scales them.

use crate::fixed::{hypot, mul_fix};
use crate::outline::{Coordinate, Outline, OutlinePoint, Point, PointKind};
use crate::read::{Reader, slice, u16_at, u32_at};
use crate::sfnt::Table;
use crate::work::charge;
use crate::{GlyphId, Limits};
use triergon_core::{Error, Result, WorkBudget};

/// Simple-glyph point flags.
const ON_CURVE: u8 = 0x01;
const X_SHORT: u8 = 0x02;
const Y_SHORT: u8 = 0x04;
const REPEAT: u8 = 0x08;
const X_SAME_OR_POSITIVE: u8 = 0x10;
const Y_SAME_OR_POSITIVE: u8 = 0x20;

/// Component flags.
const ARGS_ARE_WORDS: u16 = 0x0001;
const ARGS_ARE_XY_VALUES: u16 = 0x0002;
const HAVE_A_SCALE: u16 = 0x0008;
const MORE_COMPONENTS: u16 = 0x0020;
const HAVE_X_AND_Y_SCALE: u16 = 0x0040;
const HAVE_TWO_BY_TWO: u16 = 0x0080;
const WE_HAVE_INSTRUCTIONS: u16 = 0x0100;
const USE_MY_METRICS: u16 = 0x0200;
const SCALED_COMPONENT_OFFSET: u16 = 0x0800;

/// The format ceiling on nested composites: the OpenType `maxp`
/// `maxComponentDepth` field is 16 bits, but real fonts nest a few levels.
const MAX_COMPONENT_DEPTH: u32 = 64;

/// A component's 2×2 matrix as stored, F2Dot14: `[xx, yx, xy, yy]`, so that
/// `x' = xx x + xy y` and `y' = yx x + yy y`.
type Matrix = [i16; 4];

/// How stored font units become outline coordinates, and how composites move
/// their components in those coordinates.
pub(crate) trait Units: Copy {
    type Coord: Coordinate;
    /// An offset between two points, wide enough for any difference.
    type Offset: Copy + Default + PartialEq;

    fn point(self, x: i64, y: i64) -> Result<Point<Self::Coord>>;

    fn transform(self, points: &mut [OutlinePoint<Self::Coord>], m: Matrix) -> Result<()>;

    /// A component offset given in font units; `scaled` carries the matrix when
    /// the component asks for its offset to scale (`SCALED_COMPONENT_OFFSET`).
    fn offset(self, x: i32, y: i32, scaled: Option<Matrix>) -> Self::Offset;

    /// `to - from`.
    fn between(self, to: Point<Self::Coord>, from: Point<Self::Coord>) -> Self::Offset;

    fn translate(self, points: &mut [OutlinePoint<Self::Coord>], by: Self::Offset) -> Result<()>;
}

/// Font units as `f32`, transformed in `f64`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct FontUnits;

impl Units for FontUnits {
    type Coord = f32;
    type Offset = (f64, f64);

    fn point(self, x: i64, y: i64) -> Result<Point<f32>> {
        Ok(Point::new(x as f32, y as f32))
    }

    fn transform(self, points: &mut [OutlinePoint<f32>], m: Matrix) -> Result<()> {
        let m = m.map(|v| f64::from(v) / 16384.0);
        for p in points {
            let (x, y) = (f64::from(p.point.x), f64::from(p.point.y));
            p.point.x = (m[0] * x + m[2] * y) as f32;
            p.point.y = (m[1] * x + m[3] * y) as f32;
        }
        Ok(())
    }

    fn offset(self, x: i32, y: i32, scaled: Option<Matrix>) -> (f64, f64) {
        // A scaled offset is whole font units, as FreeType computes it.
        let (x, y) = scaled_offset(x, y, scaled);
        (x as f64, y as f64)
    }

    fn between(self, to: Point<f32>, from: Point<f32>) -> (f64, f64) {
        (
            f64::from(to.x) - f64::from(from.x),
            f64::from(to.y) - f64::from(from.y),
        )
    }

    fn translate(self, points: &mut [OutlinePoint<f32>], (dx, dy): (f64, f64)) -> Result<()> {
        for p in points {
            p.point.x = (f64::from(p.point.x) + dx) as f32;
            p.point.y = (f64::from(p.point.y) + dy) as f32;
        }
        Ok(())
    }
}

/// A component offset in font units, scaled when the component asks for it
/// by the length of each matrix row (FreeType's reading of Apple's rule) and
/// rounded to whole units with `FT_MulFix`.
fn scaled_offset(x: i32, y: i32, scaled: Option<Matrix>) -> (i64, i64) {
    let (x, y) = (i64::from(x), i64::from(y));
    match scaled {
        // FreeType widens F2Dot14 to 16.16 by multiplying with 4.
        Some(m) => {
            let [xx, yx, xy, yy] = m.map(|v| i64::from(v) * 4);
            (mul_fix(x, hypot(xx, xy)), mul_fix(y, hypot(yy, yx)))
        }
        None => (x, y),
    }
}

/// 26.6 pixels: font units times 16.16 scales with `FT_MulFix`, component
/// matrices applied as `FT_Outline_Transform` applies them.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Scaled {
    pub(crate) x: i64,
    pub(crate) y: i64,
}

/// A 26.6 value that fits the outline's `i32` coordinates.
pub(crate) fn narrow(v: i64) -> Result<i32> {
    i32::try_from(v).map_err(|_| Error::Overflow)
}

impl Units for Scaled {
    type Coord = i32;
    type Offset = (i64, i64);

    fn point(self, x: i64, y: i64) -> Result<Point<i32>> {
        Ok(Point::new(
            narrow(mul_fix(x, self.x))?,
            narrow(mul_fix(y, self.y))?,
        ))
    }

    fn transform(self, points: &mut [OutlinePoint<i32>], m: Matrix) -> Result<()> {
        // FreeType widens F2Dot14 to 16.16 by multiplying with 4.
        let [xx, yx, xy, yy] = m.map(|v| i64::from(v) * 4);
        for p in points {
            let (x, y) = (i64::from(p.point.x), i64::from(p.point.y));
            p.point.x = narrow(mul_fix(x, xx) + mul_fix(y, xy))?;
            p.point.y = narrow(mul_fix(x, yx) + mul_fix(y, yy))?;
        }
        Ok(())
    }

    fn offset(self, x: i32, y: i32, scaled: Option<Matrix>) -> (i64, i64) {
        let (x, y) = scaled_offset(x, y, scaled);
        (mul_fix(x, self.x), mul_fix(y, self.y))
    }

    fn between(self, to: Point<i32>, from: Point<i32>) -> (i64, i64) {
        (
            i64::from(to.x) - i64::from(from.x),
            i64::from(to.y) - i64::from(from.y),
        )
    }

    fn translate(self, points: &mut [OutlinePoint<i32>], (dx, dy): (i64, i64)) -> Result<()> {
        for p in points {
            p.point.x = narrow(i64::from(p.point.x) + dx)?;
            p.point.y = narrow(i64::from(p.point.y) + dy)?;
        }
        Ok(())
    }
}

/// The `glyf` table and its `loca` index.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Glyf {
    glyf: Table,
    loca: Table,
    long: bool,
}

/// Where a glyph's horizontal metrics come from: its own, or a component's
/// (`USE_MY_METRICS`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Metrics {
    pub(crate) glyph: u16,
    /// The `xMin` of that glyph's header, or zero for an empty glyph.
    pub(crate) x_min: i16,
}

/// What [`Glyf::outline`] needs from the face.
pub(crate) struct Context<'a> {
    pub(crate) data: &'a [u8],
    pub(crate) glyph_count: u16,
    pub(crate) limits: &'a Limits,
}

impl Glyf {
    /// Checks that `loca` holds the offset of every glyph. The final offset, which
    /// ends the last glyph, may be missing, as FreeType accepts; the last glyph then
    /// runs to the end of `glyf`.
    pub(crate) fn new(glyf: Table, loca: Table, long: bool, glyph_count: u16) -> Result<Self> {
        let entry = if long { 4 } else { 2 };
        if loca.len < usize::from(glyph_count) * entry {
            return Err(Error::Invalid("loca shorter than the glyph count"));
        }
        Ok(Self { glyf, loca, long })
    }

    /// The byte range of `glyph` in `glyf`, or `None` for an empty glyph, with
    /// FreeType's repairs of broken `loca` data: a glyph starting or (except
    /// the last) ending past the table is empty, the last glyph is cut at the
    /// table's end, and a glyph ending before it starts runs to the end.
    fn range(&self, data: &[u8], glyph: u16) -> Option<(usize, usize)> {
        let loca = self.loca.bytes(data);
        let index = usize::from(glyph);
        let entry = if self.long { 4 } else { 2 };
        let at = |i: usize| -> Option<usize> {
            if self.long {
                usize::try_from(u32_at(loca, 4 * i)?).ok()
            } else {
                u16_at(loca, 2 * i).map(|v| 2 * usize::from(v))
            }
        };
        let table = self.glyf.len;
        let start = at(index).filter(|&start| start <= table)?;
        let end = match at(index + 1) {
            None => table,
            Some(end) if end > table => (index + 2 == loca.len() / entry).then_some(table)?,
            Some(end) if end < start => table,
            Some(end) => end,
        };
        (start < end).then_some((start, end))
    }

    /// Reports whose metrics apply to `glyph` without loading its points, and
    /// passes every glyph program loading it would run to `program`: its own
    /// and those of its components.
    pub(crate) fn walk(
        &self,
        cx: &Context<'_>,
        glyph: u16,
        work: &mut WorkBudget,
        program: &mut dyn FnMut(&[u8]) -> Result<()>,
    ) -> Result<Metrics> {
        self.walk_at(cx, glyph, work, program, 0)
    }

    fn walk_at(
        &self,
        cx: &Context<'_>,
        glyph: u16,
        work: &mut WorkBudget,
        program: &mut dyn FnMut(&[u8]) -> Result<()>,
        depth: u32,
    ) -> Result<Metrics> {
        if glyph >= cx.glyph_count {
            return Err(Error::Invalid("glyph index outside the face"));
        }
        charge(work, 1)?;
        let Some((start, end)) = self.range(cx.data, glyph) else {
            return Ok(Metrics { glyph, x_min: 0 });
        };
        let mut r = Reader::new(slice(cx.data, self.glyf.offset + start, end - start)?);
        let contours = r.i16()?;
        let x_min = r.i16()?;
        r.skip(6)?;
        if contours == 0 {
            return Ok(Metrics { glyph, x_min: 0 });
        }
        let mut metrics = Metrics { glyph, x_min };
        if contours > 0 {
            r.skip(2 * contours.unsigned_abs() as usize)?;
        } else {
            if depth >= MAX_COMPONENT_DEPTH.min(cx.limits.depth) {
                return Err(Error::Policy("glyph nesting depth"));
            }
            let instructed = loop {
                charge(work, 1)?;
                let c = component(&mut r)?;
                let own = self.walk_at(cx, c.glyph, work, program, depth + 1)?;
                if c.flags & USE_MY_METRICS != 0 {
                    metrics = own;
                }
                // The last component says whether the composite has a program.
                if c.flags & MORE_COMPONENTS == 0 {
                    break c.flags & WE_HAVE_INSTRUCTIONS != 0;
                }
            };
            if !instructed {
                return Ok(metrics);
            }
        }
        let len = usize::from(r.u16()?);
        program(r.take(len)?)?;
        Ok(metrics)
    }

    /// Loads `glyph` into `outline` and reports whose metrics apply.
    pub(crate) fn outline<U: Units>(
        &self,
        cx: &Context<'_>,
        glyph: GlyphId,
        units: U,
        outline: &mut Outline<U::Coord>,
        work: &mut WorkBudget,
    ) -> Result<Metrics> {
        self.load(cx, glyph.0, units, outline, work, 0)
    }

    fn load<U: Units>(
        &self,
        cx: &Context<'_>,
        glyph: u16,
        units: U,
        outline: &mut Outline<U::Coord>,
        work: &mut WorkBudget,
        depth: u32,
    ) -> Result<Metrics> {
        if glyph >= cx.glyph_count {
            return Err(Error::Invalid("glyph index outside the face"));
        }
        charge(work, 1)?;
        let Some((start, end)) = self.range(cx.data, glyph) else {
            return Ok(Metrics { glyph, x_min: 0 });
        };
        let bytes = slice(cx.data, self.glyf.offset + start, end - start)?;
        let mut r = Reader::new(bytes);
        let contours = r.i16()?;
        let x_min = r.i16()?;
        r.skip(6)?; // yMin, xMax, yMax
        if contours == 0 {
            // FreeType treats a glyph without contours as empty, header and all.
            return Ok(Metrics { glyph, x_min: 0 });
        }
        let mut metrics = Metrics { glyph, x_min };
        if contours > 0 {
            simple(&mut r, contours.unsigned_abs(), units, outline, work)?;
        } else {
            // FreeType reads every negative count as a composite.
            if depth >= MAX_COMPONENT_DEPTH {
                return Err(Error::Invalid("composite glyphs nest too deeply"));
            }
            if depth >= cx.limits.depth {
                return Err(Error::Policy("glyph nesting depth"));
            }
            if let Some(own) = self.composite(cx, &mut r, units, outline, work, depth)? {
                metrics = own;
            }
        }
        Ok(metrics)
    }

    /// Appends the components; returns the metrics of the last `USE_MY_METRICS`
    /// component.
    fn composite<U: Units>(
        &self,
        cx: &Context<'_>,
        r: &mut Reader<'_>,
        units: U,
        outline: &mut Outline<U::Coord>,
        work: &mut WorkBudget,
        depth: u32,
    ) -> Result<Option<Metrics>> {
        let mut metrics = None;
        let first = outline.len();
        loop {
            charge(work, 1)?;
            let c = component(r)?;
            let base = outline.len();
            let component = self.load(cx, c.glyph, units, outline, work, depth + 1)?;
            if c.flags & USE_MY_METRICS != 0 {
                metrics = Some(component);
            }
            // FreeType places only components that add points.
            if outline.len() > base {
                place(&c, units, outline, first, base)?;
            }
            if c.flags & MORE_COMPONENTS == 0 {
                break;
            }
        }
        Ok(metrics)
    }
}

/// Transforms and moves the component points from `base` on: by its offset,
/// or so that its point matches a point of the composite starting at `first`.
fn place<U: Units>(
    c: &Component,
    units: U,
    outline: &mut Outline<U::Coord>,
    first: usize,
    base: usize,
) -> Result<()> {
    let (a, b) = c.args;
    if let Some(m) = c.matrix {
        units.transform(&mut outline.points_mut()[base..], m)?;
    }
    let offset = if c.flags & ARGS_ARE_XY_VALUES != 0 {
        let scaled = c.matrix.filter(|_| c.flags & SCALED_COMPONENT_OFFSET != 0);
        units.offset(a, b, scaled)
    } else {
        let parent = usize::try_from(a)
            .ok()
            .map(|i| first + i)
            .filter(|&i| i < base);
        let child = usize::try_from(b).ok().map(|i| base + i);
        let all = outline.points();
        match (parent.map(|i| all[i].point), child.and_then(|i| all.get(i))) {
            (Some(p), Some(c)) => units.between(p, c.point),
            _ => return Err(Error::Invalid("component attachment point")),
        }
    };
    if offset != U::Offset::default() {
        units.translate(&mut outline.points_mut()[base..], offset)?;
    }
    Ok(())
}

/// One component record of a composite glyph.
struct Component {
    flags: u16,
    glyph: u16,
    /// An offset, or the parent and child points to match.
    args: (i32, i32),
    matrix: Option<Matrix>,
}

fn component(r: &mut Reader<'_>) -> Result<Component> {
    let flags = r.u16()?;
    let glyph = r.u16()?;
    let args = match (flags & ARGS_ARE_WORDS != 0, flags & ARGS_ARE_XY_VALUES != 0) {
        (true, true) => (i32::from(r.i16()?), i32::from(r.i16()?)),
        (true, false) => (i32::from(r.u16()?), i32::from(r.u16()?)),
        (false, true) => (i32::from(r.i8()?), i32::from(r.i8()?)),
        (false, false) => (i32::from(r.u8()?), i32::from(r.u8()?)),
    };
    let matrix = if flags & HAVE_A_SCALE != 0 {
        let s = r.i16()?;
        Some([s, 0, 0, s])
    } else if flags & HAVE_X_AND_Y_SCALE != 0 {
        let (xx, yy) = (r.i16()?, r.i16()?);
        Some([xx, 0, 0, yy])
    } else if flags & HAVE_TWO_BY_TWO != 0 {
        Some([r.i16()?, r.i16()?, r.i16()?, r.i16()?])
    } else {
        None
    };
    Ok(Component {
        flags,
        glyph,
        args,
        matrix,
    })
}

/// Appends a simple glyph's contours.
fn simple<U: Units>(
    r: &mut Reader<'_>,
    contours: u16,
    units: U,
    outline: &mut Outline<U::Coord>,
    work: &mut WorkBudget,
) -> Result<()> {
    let ends = r.take(2 * usize::from(contours))?;
    let mut count = 0_usize;
    for end in ends.chunks_exact(2) {
        let end = usize::from(u16::from_be_bytes([end[0], end[1]]));
        if end < count {
            return Err(Error::Invalid("contour end points out of order"));
        }
        count = end + 1;
    }
    charge(work, count)?;
    let instructions = usize::from(r.u16()?);
    r.skip(instructions)?;
    // First pass: the flags, to find where the x and y coordinates start.
    let flags_at = *r;
    let mut flags = Flags::new(r, count);
    let mut x_bytes = 0;
    while let Some(flag) = flags.next()? {
        x_bytes += coordinate_len(flag, X_SHORT, X_SAME_OR_POSITIVE);
    }
    let mut xs = *r;
    r.skip(x_bytes)?;
    let mut ys = *r;
    // Second pass: the points.
    let mut reader = flags_at;
    let mut flags = Flags::new(&mut reader, count);
    let base = u32::try_from(outline.len()).map_err(|_| Error::Overflow)?;
    let (mut x, mut y) = (0_i64, 0_i64);
    while let Some(flag) = flags.next()? {
        x += delta(&mut xs, flag, X_SHORT, X_SAME_OR_POSITIVE)?;
        y += delta(&mut ys, flag, Y_SHORT, Y_SAME_OR_POSITIVE)?;
        let kind = if flag & ON_CURVE != 0 {
            PointKind::On
        } else {
            PointKind::Quadratic
        };
        outline.push_point(units.point(x, y)?, kind)?;
    }
    let ends = ends
        .chunks_exact(2)
        .map(|end| u32::from(u16::from_be_bytes([end[0], end[1]])) + 1);
    outline.extend_ends(ends, base)?;
    *r = ys;
    Ok(())
}

/// Point flags with their repeat counts expanded.
struct Flags<'r, 'a> {
    r: &'r mut Reader<'a>,
    left: usize,
    flag: u8,
    repeats: usize,
}

impl<'r, 'a> Flags<'r, 'a> {
    fn new(r: &'r mut Reader<'a>, count: usize) -> Self {
        Self {
            r,
            left: count,
            flag: 0,
            repeats: 0,
        }
    }

    fn next(&mut self) -> Result<Option<u8>> {
        if self.left == 0 {
            return Ok(None);
        }
        self.left -= 1;
        if self.repeats > 0 {
            self.repeats -= 1;
            return Ok(Some(self.flag));
        }
        self.flag = self.r.u8()?;
        if self.flag & REPEAT != 0 {
            self.repeats = usize::from(self.r.u8()?);
            if self.repeats > self.left {
                return Err(Error::Invalid("glyph flags repeat past the points"));
            }
        }
        Ok(Some(self.flag))
    }
}

const fn coordinate_len(flag: u8, short: u8, same: u8) -> usize {
    if flag & short != 0 {
        1
    } else if flag & same != 0 {
        0
    } else {
        2
    }
}

fn delta(r: &mut Reader<'_>, flag: u8, short: u8, same: u8) -> Result<i64> {
    Ok(if flag & short != 0 {
        let n = i64::from(r.u8()?);
        if flag & same != 0 { n } else { -n }
    } else if flag & same != 0 {
        0
    } else {
        i64::from(r.i16()?)
    })
}

#[cfg(test)]
mod tests {
    use super::{FontUnits, Scaled, Units};
    use crate::outline::{OutlinePoint, Point, PointKind};

    #[test]
    fn scaled_components_round_each_product() {
        let units = Scaled {
            x: 0x10_624D,
            y: 0x10_624D,
        };
        assert_eq!(units.point(1000, -3), Ok(Point::new(16384, -49)));
        let mut points = [OutlinePoint {
            point: Point::new(100, 3),
            kind: PointKind::On,
        }];
        // Half scale: 0x2000 in F2Dot14.
        units
            .transform(&mut points, [0x2000, 0, 0, 0x2000])
            .unwrap();
        assert_eq!(points[0].point, Point::new(50, 2));
        assert_eq!(units.offset(10, 0, None), (164, 0));
        assert_eq!(
            FontUnits.offset(10, 4, Some([0x2000, 0, 0, 0x4000])),
            (5.0, 4.0)
        );
    }
}

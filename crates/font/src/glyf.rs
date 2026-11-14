//! TrueType outlines (OpenType `glyf` and `loca`), unhinted, at a 16.16 scale
//! (one for font units) in FreeType's fixed-point arithmetic.

use crate::fixed::{Scale, hypot, mul_fix};
use crate::outline::{Outline, OutlinePoint, Point, PointKind};
use crate::read::{Reader, slice, u16_at, u32_at};
use crate::sfnt::Table;
use crate::work::charge;
use crate::{Bounds, GlyphId, Limits};
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
const ROUND_XY_TO_GRID: u16 = 0x0004;
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

/// A 26.6 value that fits the outline's `i32` coordinates.
pub(crate) fn narrow(v: i64) -> Result<i32> {
    i32::try_from(v).map_err(|_| Error::Overflow)
}

/// A stored point at `scale`.
fn point(scale: Scale, x: i64, y: i64) -> Result<Point> {
    Ok(Point::new(
        narrow(mul_fix(x, scale.x))?,
        narrow(mul_fix(y, scale.y))?,
    ))
}

/// Applies a component matrix as `FT_Outline_Transform` does, after FreeType
/// widens F2Dot14 to 16.16 by multiplying with 4.
fn transform(points: &mut [OutlinePoint], m: Matrix) -> Result<()> {
    let [xx, yx, xy, yy] = m.map(|v| i64::from(v) * 4);
    for p in points {
        let (x, y) = (i64::from(p.point.x), i64::from(p.point.y));
        p.point.x = narrow(mul_fix(x, xx) + mul_fix(y, xy))?;
        p.point.y = narrow(mul_fix(x, yx) + mul_fix(y, yy))?;
    }
    Ok(())
}

/// A component offset at `scale`: in whole font units first, scaled when the
/// component asks for it by the length of each matrix row (FreeType's reading
/// of Apple's rule), each product rounded with `FT_MulFix`.
fn offset(scale: Scale, x: i32, y: i32, scaled: Option<Matrix>) -> (i64, i64) {
    let (x, y) = (i64::from(x), i64::from(y));
    let (x, y) = match scaled {
        Some(m) => {
            let [xx, yx, xy, yy] = m.map(|v| i64::from(v) * 4);
            (mul_fix(x, hypot(xx, xy)), mul_fix(y, hypot(yy, yx)))
        }
        None => (x, y),
    };
    (mul_fix(x, scale.x), mul_fix(y, scale.y))
}

/// Moves points by `(dx, dy)`.
pub(crate) fn translate(points: &mut [OutlinePoint], (dx, dy): (i64, i64)) -> Result<()> {
    for p in points {
        p.point.x = narrow(i64::from(p.point.x) + dx)?;
        p.point.y = narrow(i64::from(p.point.y) + dy)?;
    }
    Ok(())
}

/// Runs each glyph's program as a hinted load builds the outline. The
/// loader is generic over it, so an unhinted load ([`Unhinted`]) compiles to
/// the plain loader.
pub(crate) trait Hint {
    /// Whether the load hints at all.
    const ACTIVE: bool;

    /// Records the font-unit position of the point a simple glyph just
    /// appended to the outline.
    fn unit(&mut self, unit: Point) -> Result<()>;

    /// Whether the programs fit glyphs to the grid at this size; the
    /// control value program may turn that off.
    fn grid_fitting(&self) -> bool;

    /// Runs a glyph's program on its points in `outline`.
    fn glyph(
        &mut self,
        glyph: Loaded<'_>,
        outline: &mut Outline,
        work: &mut WorkBudget,
    ) -> Result<()>;
}

/// The load without hinting.
pub(crate) struct Unhinted;

impl Hint for Unhinted {
    const ACTIVE: bool = false;

    fn unit(&mut self, _: Point) -> Result<()> {
        Ok(())
    }

    fn grid_fitting(&self) -> bool {
        false
    }

    fn glyph(&mut self, _: Loaded<'_>, _: &mut Outline, _: &mut WorkBudget) -> Result<()> {
        Ok(())
    }
}

/// A glyph just loaded, as [`Hint::glyph`] takes it.
pub(crate) struct Loaded<'a> {
    /// The metrics that apply to it.
    pub(crate) metrics: Metrics,
    /// Whether its points are a composite's placed components'.
    pub(crate) composite: bool,
    /// Its first point and first contour in the outline.
    pub(crate) base: usize,
    pub(crate) ends: usize,
    pub(crate) instructions: &'a [u8],
}

/// The `glyf` table and its `loca` index.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Glyf {
    glyf: Table,
    loca: Table,
    long: bool,
}

/// Where a glyph's metrics come from: its own, or a component's
/// (`USE_MY_METRICS`), with the header box values FreeType's phantom points
/// start from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Metrics {
    pub(crate) glyph: u16,
    /// The `xMin` of that glyph's header, or zero for an empty glyph.
    pub(crate) x_min: i16,
    /// The `yMax` of that glyph's header, or zero for an empty glyph.
    pub(crate) y_max: i16,
}

/// A `glyf` glyph header: its contour count, negative for a composite, and
/// its box as stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Header {
    pub(crate) contours: i16,
    pub(crate) bounds: Bounds,
}

impl Metrics {
    /// A glyph's own metrics, from its header.
    const fn of(glyph: u16, header: Header) -> Self {
        Self {
            glyph,
            x_min: header.bounds.x_min,
            y_max: header.bounds.y_max,
        }
    }

    /// The metrics of an empty glyph, whose box FreeType reads as zero.
    const fn empty(glyph: u16) -> Self {
        Self {
            glyph,
            x_min: 0,
            y_max: 0,
        }
    }
}

/// What [`Glyf::outline`] needs from the face.
pub(crate) struct Context<'a> {
    pub(crate) data: &'a [u8],
    pub(crate) glyph_count: u16,
    pub(crate) limits: &'a Limits,
}

impl Glyf {
    /// Sizes `loca` as FreeType does: at most 65536 entries are read, and one
    /// too short for the glyph count is read on into `room` (the bytes before the
    /// next table) when that holds every entry, else the glyph count drops to the
    /// glyphs it covers. Returns the table and that glyph count.
    pub(crate) fn new(
        glyf: Table,
        loca: Table,
        long: bool,
        glyph_count: u16,
        room: usize,
    ) -> Result<(Self, u16)> {
        let entry = if long { 4 } else { 2 };
        let len = loca.len.min(0x1_0000 * entry);
        let entries = len / entry;
        let needed = (usize::from(glyph_count) + 1) * entry;
        let (len, glyph_count) = if entries > usize::from(glyph_count) {
            (len, glyph_count)
        } else if needed <= room {
            (needed, glyph_count)
        } else {
            let covered = entries.saturating_sub(1);
            (len, u16::try_from(covered).map_err(|_| Error::Overflow)?)
        };
        if glyph_count == 0 {
            return Err(Error::Invalid("loca without glyphs"));
        }
        let loca = Table {
            offset: loca.offset,
            len,
        };
        Ok((Self { glyf, loca, long }, glyph_count))
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

    /// The header of `glyph`, `None` for a glyph FreeType treats as empty,
    /// header and all: one without data or without contours.
    pub(crate) fn header(&self, data: &[u8], glyph: u16) -> Result<Option<Header>> {
        Ok(self.read(data, glyph)?.map(|(header, _)| header))
    }

    /// The header of `glyph`, as [`Self::header`], and a reader at the data
    /// after it.
    fn read<'d>(&self, data: &'d [u8], glyph: u16) -> Result<Option<(Header, Reader<'d>)>> {
        let Some((start, end)) = self.range(data, glyph) else {
            return Ok(None);
        };
        let mut r = Reader::new(slice(data, self.glyf.offset + start, end - start)?);
        let contours = r.i16()?;
        let bounds = Bounds {
            x_min: r.i16()?,
            y_min: r.i16()?,
            x_max: r.i16()?,
            y_max: r.i16()?,
        };
        Ok((contours != 0).then_some((Header { contours, bounds }, r)))
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
        let Some((header, mut r)) = self.read(cx.data, glyph)? else {
            return Ok(Metrics::empty(glyph));
        };
        let contours = header.contours;
        let mut metrics = Metrics::of(glyph, header);
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

    /// Loads `glyph` into `outline` and reports whose metrics apply; with
    /// `hint`, running each glyph's program on its points as it loads.
    pub(crate) fn outline<H: Hint>(
        &self,
        cx: &Context<'_>,
        glyph: GlyphId,
        scale: Scale,
        outline: &mut Outline,
        work: &mut WorkBudget,
        hint: &mut H,
    ) -> Result<Metrics> {
        self.load(cx, glyph.0, scale, outline, work, hint, 0)
    }

    #[allow(clippy::too_many_arguments, reason = "the recursion carries the load")]
    fn load<H: Hint>(
        &self,
        cx: &Context<'_>,
        glyph: u16,
        scale: Scale,
        outline: &mut Outline,
        work: &mut WorkBudget,
        hint: &mut H,
        depth: u32,
    ) -> Result<Metrics> {
        if glyph >= cx.glyph_count {
            return Err(Error::Invalid("glyph index outside the face"));
        }
        charge(work, 1)?;
        let Some((header, mut r)) = self.read(cx.data, glyph)? else {
            return Ok(Metrics::empty(glyph));
        };
        let contours = header.contours;
        let mut metrics = Metrics::of(glyph, header);
        if contours > 0 {
            simple(
                &mut r,
                contours.unsigned_abs(),
                scale,
                outline,
                work,
                hint,
                metrics,
            )?;
        } else {
            // FreeType reads every negative count as a composite.
            if depth >= MAX_COMPONENT_DEPTH {
                return Err(Error::Invalid("composite glyphs nest too deeply"));
            }
            if depth >= cx.limits.depth {
                return Err(Error::Policy("glyph nesting depth"));
            }
            if let Some(own) =
                self.composite(cx, &mut r, scale, outline, work, hint, depth, metrics)?
            {
                metrics = own;
            }
        }
        Ok(metrics)
    }

    /// Appends the components, then runs the composite's program with
    /// `hint`; returns the metrics of the last `USE_MY_METRICS` component.
    /// `own` are the composite's own metrics.
    #[allow(clippy::too_many_arguments, reason = "the recursion carries the load")]
    fn composite<H: Hint>(
        &self,
        cx: &Context<'_>,
        r: &mut Reader<'_>,
        scale: Scale,
        outline: &mut Outline,
        work: &mut WorkBudget,
        hint: &mut H,
        depth: u32,
        own: Metrics,
    ) -> Result<Option<Metrics>> {
        let mut metrics = None;
        let first = outline.len();
        let ends = outline.contour_ends().len();
        loop {
            charge(work, 1)?;
            let c = component(r)?;
            let base = outline.len();
            let component = self.load(cx, c.glyph, scale, outline, work, hint, depth + 1)?;
            if c.flags & USE_MY_METRICS != 0 {
                metrics = Some(component);
            }
            // FreeType places only components that add points.
            if outline.len() > base {
                let fitted = H::ACTIVE && hint.grid_fitting();
                place(&c, scale, outline, first, base, fitted)?;
            }
            if c.flags & MORE_COMPONENTS == 0 {
                if H::ACTIVE {
                    // The last component says whether the composite has a
                    // program.
                    let instructions = if c.flags & WE_HAVE_INSTRUCTIONS != 0 {
                        let len = usize::from(r.u16()?);
                        r.take(len)?
                    } else {
                        &[]
                    };
                    let loaded = Loaded {
                        metrics: metrics.unwrap_or(own),
                        composite: true,
                        base: first,
                        ends,
                        instructions,
                    };
                    hint.glyph(loaded, outline, work)?;
                }
                return Ok(metrics);
            }
        }
    }
}

/// Transforms and moves the component points from `base` on: by its offset,
/// or so that its point matches a point of the composite starting at `first`.
/// A load that fits glyphs to the grid rounds an offset that asks for it
/// (`ROUND_XY_TO_GRID`) to whole pixels vertically; observed, never
/// horizontally, nor where the control value program turned grid fitting off
/// (`composites`, `composites-without-grid-fitting`).
fn place(
    c: &Component,
    scale: Scale,
    outline: &mut Outline,
    first: usize,
    base: usize,
    fitted: bool,
) -> Result<()> {
    let (a, b) = c.args;
    if let Some(m) = c.matrix {
        transform(&mut outline.points_mut()[base..], m)?;
    }
    let offset = if c.flags & ARGS_ARE_XY_VALUES != 0 {
        let scaled = c.matrix.filter(|_| c.flags & SCALED_COMPONENT_OFFSET != 0);
        let (x, y) = offset(scale, a, b, scaled);
        if fitted && c.flags & ROUND_XY_TO_GRID != 0 {
            (x, (y + 32) & !63)
        } else {
            (x, y)
        }
    } else {
        let parent = usize::try_from(a)
            .ok()
            .map(|i| first + i)
            .filter(|&i| i < base);
        let child = usize::try_from(b).ok().map(|i| base + i);
        let all = outline.points();
        match (parent.map(|i| all[i].point), child.and_then(|i| all.get(i))) {
            (Some(p), Some(c)) => (
                i64::from(p.x) - i64::from(c.point.x),
                i64::from(p.y) - i64::from(c.point.y),
            ),
            _ => return Err(Error::Invalid("component attachment point")),
        }
    };
    if offset != (0, 0) {
        translate(&mut outline.points_mut()[base..], offset)?;
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

/// Appends a simple glyph's contours, recording their font-unit points with
/// `hint`, then runs its program with `hint`; `metrics` are the glyph's.
fn simple<H: Hint>(
    r: &mut Reader<'_>,
    contours: u16,
    scale: Scale,
    outline: &mut Outline,
    work: &mut WorkBudget,
    hint: &mut H,
    metrics: Metrics,
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
    let len = usize::from(r.u16()?);
    let instructions = if H::ACTIVE {
        r.take(len)?
    } else {
        r.skip(len)?;
        &[]
    };
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
        outline.push_point(point(scale, x, y)?, kind)?;
        if H::ACTIVE {
            hint.unit(Point::new(narrow(x)?, narrow(y)?))?;
        }
    }
    let ends = ends
        .chunks_exact(2)
        .map(|end| u32::from(u16::from_be_bytes([end[0], end[1]])) + 1);
    let first_end = outline.contour_ends().len();
    outline.extend_ends(ends, base)?;
    *r = ys;
    if H::ACTIVE {
        let loaded = Loaded {
            metrics,
            composite: false,
            base: base as usize,
            ends: first_end,
            instructions,
        };
        hint.glyph(loaded, outline, work)?;
    }
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
    use super::{Glyf, offset, point, transform};
    use crate::fixed::Scale;
    use crate::outline::{OutlinePoint, Point, PointKind};
    use crate::sfnt::Table;

    #[test]
    fn a_short_loca_reads_on_or_lowers_the_glyph_count() {
        let table = |len| Table { offset: 100, len };
        let glyf = table(40);
        let sized = |len, glyphs, room| {
            Glyf::new(glyf, table(len), false, glyphs, room).map(|(g, n)| (g.loca.len, n))
        };
        // Four glyphs need five short entries.
        assert_eq!(sized(10, 4, 10), Ok((10, 4)));
        assert_eq!(sized(8, 4, 12), Ok((10, 4)));
        assert_eq!(sized(8, 4, 8), Ok((8, 3)));
        assert_eq!(sized(14, 4, 14), Ok((14, 4)));
        assert!(sized(2, 4, 2).is_err());
    }

    #[test]
    fn scaled_components_round_each_product() {
        let scale = Scale {
            x: 0x10_624D,
            y: 0x10_624D,
        };
        assert_eq!(point(scale, 1000, -3), Ok(Point::new(16384, -49)));
        let mut points = [OutlinePoint {
            point: Point::new(100, 3),
            kind: PointKind::On,
        }];
        // Half scale: 0x2000 in F2Dot14.
        transform(&mut points, [0x2000, 0, 0, 0x2000]).unwrap();
        assert_eq!(points[0].point, Point::new(50, 2));
        assert_eq!(offset(scale, 10, 0, None), (164, 0));
        // Scaled offsets are whole font units before the scale.
        assert_eq!(
            offset(Scale::UNITS, 10, 5, Some([0x2000, 0, 0, 0x6000])),
            (5, 8)
        );
    }
}

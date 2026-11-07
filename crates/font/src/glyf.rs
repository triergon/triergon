//! TrueType outlines (OpenType `glyf` and `loca`), unhinted.

use crate::outline::{Outline, PointKind};
use crate::read::{Meter, Reader, slice, u16_at, u32_at};
use crate::sfnt::Table;
use crate::{GlyphId, Limits};
use triergon_core::{Error, Result};

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
const USE_MY_METRICS: u16 = 0x0200;
const SCALED_COMPONENT_OFFSET: u16 = 0x0800;

/// The format ceiling on nested composites: the OpenType `maxp`
/// `maxComponentDepth` field is 16 bits, but real fonts nest a few levels.
const MAX_COMPONENT_DEPTH: u32 = 64;

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
    /// Checks that `loca` holds an entry for every glyph and one more.
    pub(crate) fn new(glyf: Table, loca: Table, long: bool, glyph_count: u16) -> Result<Self> {
        let entry = if long { 4 } else { 2 };
        // FreeType accepts a loca one entry short; the last glyph is then empty.
        if loca.len < usize::from(glyph_count) * entry {
            return Err(Error::Invalid("loca shorter than the glyph count"));
        }
        Ok(Self { glyf, loca, long })
    }

    /// The byte range of `glyph` in `glyf`, or `None` for an empty glyph. A range
    /// that runs past the table is cut at its end, as FreeType reads it.
    fn range(&self, data: &[u8], glyph: u16) -> Option<(usize, usize)> {
        let loca = self.loca.bytes(data);
        let index = usize::from(glyph);
        let at = |i: usize| -> Option<usize> {
            if self.long {
                usize::try_from(u32_at(loca, 4 * i)?).ok()
            } else {
                u16_at(loca, 2 * i).map(|v| 2 * usize::from(v))
            }
        };
        let start = at(index)?;
        let end = at(index + 1).unwrap_or(self.glyf.len).min(self.glyf.len);
        (start < end).then_some((start, end))
    }

    /// Loads `glyph` into `outline` and reports whose metrics apply.
    pub(crate) fn outline(
        &self,
        cx: &Context<'_>,
        glyph: GlyphId,
        outline: &mut Outline,
        meter: &mut Meter,
    ) -> Result<Metrics> {
        let glyph = u16::try_from(glyph.0).map_err(|_| Error::Invalid("glyph index"))?;
        self.load(cx, glyph, outline, meter, 0)
    }

    fn load(
        &self,
        cx: &Context<'_>,
        glyph: u16,
        outline: &mut Outline,
        meter: &mut Meter,
        depth: u32,
    ) -> Result<Metrics> {
        if glyph >= cx.glyph_count {
            return Err(Error::Invalid("glyph index outside the face"));
        }
        meter.charge(1)?;
        let Some((start, end)) = self.range(cx.data, glyph) else {
            return Ok(Metrics { glyph, x_min: 0 });
        };
        let bytes = slice(cx.data, self.glyf.offset + start, end - start)?;
        let mut r = Reader::new(bytes);
        let contours = r.i16()?;
        let x_min = r.i16()?;
        r.skip(6)?; // yMin, xMax, yMax
        let mut metrics = Metrics { glyph, x_min };
        if contours >= 0 {
            simple(&mut r, contours.unsigned_abs(), outline, meter)?;
        } else {
            // FreeType reads every negative count as a composite.
            if depth >= MAX_COMPONENT_DEPTH {
                return Err(Error::Invalid("composite glyphs nest too deeply"));
            }
            if depth >= cx.limits.depth {
                return Err(Error::Policy("glyph nesting depth"));
            }
            if let Some(own) = self.composite(cx, &mut r, outline, meter, depth)? {
                metrics = own;
            }
        }
        Ok(metrics)
    }

    /// Appends the components; returns the metrics of the last `USE_MY_METRICS`
    /// component.
    fn composite(
        &self,
        cx: &Context<'_>,
        r: &mut Reader<'_>,
        outline: &mut Outline,
        meter: &mut Meter,
        depth: u32,
    ) -> Result<Option<Metrics>> {
        let mut metrics = None;
        let first = outline.len();
        loop {
            meter.charge(1)?;
            let flags = r.u16()?;
            let glyph = r.u16()?;
            let (a, b) = match (flags & ARGS_ARE_WORDS != 0, flags & ARGS_ARE_XY_VALUES != 0) {
                (true, true) => (i32::from(r.i16()?), i32::from(r.i16()?)),
                (true, false) => (i32::from(r.u16()?), i32::from(r.u16()?)),
                (false, true) => (i32::from(r.i8()?), i32::from(r.i8()?)),
                (false, false) => (i32::from(r.u8()?), i32::from(r.u8()?)),
            };
            // [xx, yx, xy, yy]: x' = xx x + xy y, y' = yx x + yy y.
            let mut m = [1.0, 0.0, 0.0, 1.0];
            let scaled = if flags & HAVE_A_SCALE != 0 {
                let s = r.f2dot14()?;
                m = [s, 0.0, 0.0, s];
                true
            } else if flags & HAVE_X_AND_Y_SCALE != 0 {
                m = [r.f2dot14()?, 0.0, 0.0, r.f2dot14()?];
                true
            } else if flags & HAVE_TWO_BY_TWO != 0 {
                m = [r.f2dot14()?, r.f2dot14()?, r.f2dot14()?, r.f2dot14()?];
                true
            } else {
                false
            };
            let base = outline.len();
            let component = self.load(cx, glyph, outline, meter, depth + 1)?;
            if flags & USE_MY_METRICS != 0 {
                metrics = Some(component);
            }
            let points = &mut outline.points_mut()[base..];
            if scaled {
                for p in points.iter_mut() {
                    let (x, y) = (f64::from(p.point.x), f64::from(p.point.y));
                    p.point.x = (m[0] * x + m[2] * y) as f32;
                    p.point.y = (m[1] * x + m[3] * y) as f32;
                }
            }
            let (dx, dy) = if flags & ARGS_ARE_XY_VALUES != 0 {
                let (mut dx, mut dy) = (f64::from(a), f64::from(b));
                // Offsets scale only when the font asks for it, by the length of
                // each matrix column (FreeType's reading of Apple's rule).
                if scaled && flags & SCALED_COMPONENT_OFFSET != 0 {
                    dx *= m[0].hypot(m[2]);
                    dy *= m[3].hypot(m[1]);
                }
                (dx, dy)
            } else {
                // Match point `a` of the composite so far with point `b` of the
                // component.
                let parent = usize::try_from(a)
                    .ok()
                    .map(|i| first + i)
                    .filter(|&i| i < base);
                let child = usize::try_from(b).ok().map(|i| base + i);
                let all = outline.points();
                match (parent.map(|i| all[i].point), child.and_then(|i| all.get(i))) {
                    (Some(p), Some(c)) => (
                        f64::from(p.x) - f64::from(c.point.x),
                        f64::from(p.y) - f64::from(c.point.y),
                    ),
                    _ => return Err(Error::Invalid("component attachment point")),
                }
            };
            if dx != 0.0 || dy != 0.0 {
                for p in &mut outline.points_mut()[base..] {
                    p.point.x = (f64::from(p.point.x) + dx) as f32;
                    p.point.y = (f64::from(p.point.y) + dy) as f32;
                }
            }
            if flags & MORE_COMPONENTS == 0 {
                break;
            }
        }
        Ok(metrics)
    }
}

/// Appends a simple glyph's contours.
fn simple(
    r: &mut Reader<'_>,
    contours: u16,
    outline: &mut Outline,
    meter: &mut Meter,
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
    meter.charge(count)?;
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
        outline.push(x as f64, y as f64, kind)?;
    }
    for end in ends.chunks_exact(2) {
        let end = u32::from(u16::from_be_bytes([end[0], end[1]])) + 1;
        outline.extend_ends(&[end], base)?;
    }
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

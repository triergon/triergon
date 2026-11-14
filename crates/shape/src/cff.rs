//! Glyph extents of CFF and CFF2 charstrings as HarfBuzz's OpenType font
//! functions report them (`hb_font_get_glyph_extents` at units per em).
//!
//! The tables are read as Adobe's Technical Note #5176 (*The Compact Font
//! Format Specification*) and the OpenType specification's CFF2 chapter lay
//! them out, and the charstrings run as Technical Note #5177 (*The Type 2
//! Charstring Format*) defines them. Where HarfBuzz settles what the
//! specifications leave open, or departs from them, the rule names the probe
//! under `harness/shape/probes` that shows it (`cargo xtask shape probe`).

use triergon_core::{Error, Result, WorkBudget};
use triergon_font::{Font, GlyphId, OutlineFormat};

/// The argument stack's capacity, CFF2's limit, which HarfBuzz applies to CFF
/// too. probe: cff-stack, cff2-stack
const STACK: usize = 513;

/// The deepest subroutine nesting that runs: an eleventh nested call fails
/// the glyph, local and global calls counted together. probe:
/// cff-subr-depth, cff-gsubr-depth, cff2-subrs
const NESTING: u32 = 10;

/// A charstring fails at its 200000th operator or operand, the end of a
/// glyph's or subroutine's bytes counting as one; each component of an
/// accented glyph counts on its own. probe: cff-limits, cff2-limits
const OPERATIONS: u32 = 200_000;

/// The standard encoding (TN 5176 Appendix B): the string ID of each code,
/// for the accented form of `endchar`.
const STANDARD_ENCODING: [u8; 256] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26,
    27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50,
    51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 63, 64, 65, 66, 67, 68, 69, 70, 71, 72, 73, 74,
    75, 76, 77, 78, 79, 80, 81, 82, 83, 84, 85, 86, 87, 88, 89, 90, 91, 92, 93, 94, 95, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 96,
    97, 98, 99, 100, 101, 102, 103, 104, 105, 106, 107, 108, 109, 110, 0, 111, 112, 113, 114, 0,
    115, 116, 117, 118, 119, 120, 121, 122, 0, 123, 0, 124, 125, 126, 127, 128, 129, 130, 131, 0,
    132, 133, 0, 134, 135, 136, 137, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 138, 0, 139,
    0, 0, 0, 0, 140, 141, 142, 143, 0, 0, 0, 0, 0, 144, 0, 0, 0, 145, 0, 0, 146, 147, 148, 149, 0,
    0, 0, 0,
];

/// The extents HarfBuzz's OpenType font functions give `glyph` of a CFF or
/// CFF2 face, in font units: x bearing, y bearing (the top), width and height
/// (zero or negative).
///
/// The box holds every point the charstring draws, control points included:
/// the current point when a line or curve starts and each point it adds, so a
/// moveto counts only once something is drawn from it. Coordinates are kept
/// unrounded; each side is rounded half up, the bearings being the rounded
/// left and top and the size the difference of the rounded sides, each
/// saturating at the 32-bit range, and an axis over which the box has no
/// extent gives a zero bearing and size. The FontMatrix does not apply.
/// probe: cff-box, cff2-box, cff-matrix, cff-huge
///
/// `Ok(None)` is a glyph HarfBuzz gives no extents for: one outside the
/// face, a table it cannot read, or a charstring that fails, as the probes
/// under `harness/shape/probes` show each failure. A charstring that draws
/// nothing gives `Some([0; 4])`.
///
/// # Errors
///
/// `Policy` when the charstring's operators, or the charset an accented
/// glyph is looked up in, exceed `work`, or its subroutine calls nest deeper
/// than the face's [`Limits`](triergon_font::Limits) depth.
pub(crate) fn extents(
    font: &Font,
    glyph: GlyphId,
    work: &mut WorkBudget,
) -> Result<Option<[i32; 4]>> {
    let (tag, cff2) = match font.outline_format() {
        OutlineFormat::Cff => (*b"CFF ", false),
        OutlineFormat::Cff2 => (*b"CFF2", true),
        OutlineFormat::TrueType => return Ok(None),
    };
    let Some(table) = font.table(tag) else {
        return Ok(None);
    };
    table_extents(table, cff2, glyph.0, font.limits().depth, work)
}

/// [`extents`] of glyph `glyph` of the CFF (or, with `cff2`, CFF2) table
/// `table`, with calls nesting at most `depth` deep.
fn table_extents(
    table: &[u8],
    cff2: bool,
    glyph: u16,
    depth: u32,
    work: &mut WorkBudget,
) -> Result<Option<[i32; 4]>> {
    let Some(program) = Program::new(table, cff2, glyph) else {
        return Ok(None);
    };
    let Some(charstring) = program.charstrings.get(u32::from(glyph)) else {
        return Ok(None);
    };
    let mut run = Run {
        program: &program,
        work,
        depth,
        bounds: None,
    };
    match run.glyph(charstring, (0.0, 0.0), true) {
        Ok(()) => Ok(Some(run.extents())),
        Err(Fault::Glyph) => Ok(None),
        Err(Fault::Limit(error)) => Err(error),
    }
}

/// Why a charstring stopped short: it fails the glyph, or it exceeded the
/// caller's bounds.
enum Fault {
    Glyph,
    Limit(Error),
}

type Step<T> = std::result::Result<T, Fault>;

// ------------------------------------------------------------------ tables

fn u16_at(data: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(
        data.get(at..at.checked_add(2)?)?.try_into().ok()?,
    ))
}

fn u32_at(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(
        data.get(at..at.checked_add(4)?)?.try_into().ok()?,
    ))
}

/// An offset of `size` bytes (1 to 4), big-endian.
fn offset_at(data: &[u8], at: usize, size: usize) -> Option<usize> {
    let bytes = data.get(at..at.checked_add(size)?)?;
    Some(bytes.iter().fold(0, |v, &b| (v << 8) | usize::from(b)))
}

/// An INDEX (TN 5176 §5): a count, then count + 1 offsets of `size` bytes
/// from the byte before the object data. A CFF2 INDEX has a 32-bit count.
#[derive(Clone, Copy, Default)]
struct Index<'a> {
    count: u32,
    size: usize,
    offsets: &'a [u8],
    objects: &'a [u8],
}

impl<'a> Index<'a> {
    /// The INDEX at `at` and the offset just past it.
    fn read(table: &'a [u8], at: usize, cff2: bool) -> Option<(Self, usize)> {
        let (count, header) = if cff2 {
            (u32_at(table, at)?, 4)
        } else {
            (u32::from(u16_at(table, at)?), 2)
        };
        let at = at.checked_add(header)?;
        if count == 0 {
            return Some((Self::default(), at));
        }
        let size = usize::from(*table.get(at)?);
        if !(1..=4).contains(&size) {
            return None;
        }
        let start = at + 1;
        let len = (count as usize).checked_add(1)?.checked_mul(size)?;
        let offsets = table.get(start..start.checked_add(len)?)?;
        let last = offset_at(offsets, len - size, size)?;
        let data = start + len;
        let objects = table.get(data..data.checked_add(last.checked_sub(1)?)?)?;
        Some((
            Self {
                count,
                size,
                offsets,
                objects,
            },
            data + objects.len(),
        ))
    }

    fn get(&self, i: u32) -> Option<&'a [u8]> {
        if i >= self.count {
            return None;
        }
        let at = i as usize * self.size;
        let start = offset_at(self.offsets, at, self.size)?.checked_sub(1)?;
        let end = offset_at(self.offsets, at + self.size, self.size)?.checked_sub(1)?;
        self.objects.get(start..end)
    }
}

/// The last two integer operands of the first `op` entry of a DICT (TN 5176
/// §4), the last one second; a real operand reads as zero. CFF2's `blend`
/// (23) leaves its operands to the next operator.
fn dict_find(dict: &[u8], op: u16) -> Option<[i64; 2]> {
    let mut operands = [0_i64; 2];
    let mut at = 0;
    while let Some(&b0) = dict.get(at) {
        at += 1;
        let value = match b0 {
            23 => continue,
            0..=27 => {
                let key = if b0 == 12 {
                    let b1 = *dict.get(at)?;
                    at += 1;
                    0x0C00 | u16::from(b1)
                } else {
                    u16::from(b0)
                };
                if key == op {
                    return Some(operands);
                }
                operands = [0; 2];
                continue;
            }
            28 => {
                let v = u16_at(dict, at)?.cast_signed();
                at += 2;
                i64::from(v)
            }
            29 => {
                let v = u32_at(dict, at)?.cast_signed();
                at += 4;
                i64::from(v)
            }
            30 => {
                // A real's nibbles end with the byte holding 0xF.
                loop {
                    let b = *dict.get(at)?;
                    at += 1;
                    if b >> 4 == 0x0F || b & 0x0F == 0x0F {
                        break 0;
                    }
                }
            }
            32..=246 => i64::from(b0) - 139,
            247..=250 => {
                let b1 = *dict.get(at)?;
                at += 1;
                (i64::from(b0) - 247) * 256 + i64::from(b1) + 108
            }
            251..=254 => {
                let b1 = *dict.get(at)?;
                at += 1;
                -(i64::from(b0) - 251) * 256 - i64::from(b1) - 108
            }
            _ => return None,
        };
        operands = [operands[1], value];
    }
    None
}

/// A table offset from a DICT operand.
fn offset(value: i64) -> Option<usize> {
    usize::try_from(value).ok()
}

/// The parts of a table that one glyph's charstring runs with.
struct Program<'a> {
    cff2: bool,
    charstrings: Index<'a>,
    global: Index<'a>,
    /// The local subroutines of the glyph's Private DICT, if it has any.
    local: Option<Index<'a>>,
    /// A name-keyed CFF table's charset, for the accented form of `endchar`.
    charset: Option<Charset<'a>>,
    /// CFF2's ItemVariationStore, and the Private DICT's `vsindex`.
    store: Option<&'a [u8]>,
    vsindex: i64,
}

impl<'a> Program<'a> {
    fn new(table: &'a [u8], cff2: bool, glyph: u16) -> Option<Self> {
        if cff2 {
            Self::cff2(table, glyph)
        } else {
            Self::cff(table, glyph)
        }
    }

    fn cff(table: &'a [u8], glyph: u16) -> Option<Self> {
        let header = usize::from(*table.get(2)?);
        let (_, at) = Index::read(table, header, false)?;
        let (tops, at) = Index::read(table, at, false)?;
        let (_, at) = Index::read(table, at, false)?;
        let (global, _) = Index::read(table, at, false)?;
        let top = tops.get(0)?;
        let (charstrings, _) = Index::read(table, offset(dict_find(top, 17)?[1])?, false)?;
        let cid = dict_find(top, 0x0C1E).is_some();
        let private = if cid {
            let fd = fd_select(table, offset(dict_find(top, 0x0C25)?[1])?, glyph, false)?;
            let (fds, _) = Index::read(table, offset(dict_find(top, 0x0C24)?[1])?, false)?;
            dict_find(fds.get(fd)?, 18)?
        } else {
            dict_find(top, 18)?
        };
        let (local, _) = Self::private(table, private, false)?;
        let charset = (!cid).then(|| Charset {
            table,
            at: dict_find(top, 15).map_or(0, |v| v[1]),
            glyphs: charstrings.count,
        });
        Some(Self {
            cff2: false,
            charstrings,
            global,
            local,
            charset,
            store: None,
            vsindex: 0,
        })
    }

    fn cff2(table: &'a [u8], glyph: u16) -> Option<Self> {
        let header = usize::from(*table.get(2)?);
        let top = table.get(header..header + usize::from(u16_at(table, 3)?))?;
        let (global, _) = Index::read(table, header + top.len(), true)?;
        let (charstrings, _) = Index::read(table, offset(dict_find(top, 17)?[1])?, true)?;
        let fd = match dict_find(top, 0x0C25) {
            Some(v) => fd_select(table, offset(v[1])?, glyph, true)?,
            None => 0,
        };
        let (fds, _) = Index::read(table, offset(dict_find(top, 0x0C24)?[1])?, true)?;
        let (local, vsindex) = Self::private(table, dict_find(fds.get(fd)?, 18)?, true)?;
        let store = match dict_find(top, 24) {
            Some(v) => {
                let at = offset(v[1])?;
                let len = usize::from(u16_at(table, at)?);
                Some(table.get(at + 2..at + 2 + len)?)
            }
            None => None,
        };
        Some(Self {
            cff2: true,
            charstrings,
            global,
            local,
            charset: None,
            store,
            vsindex,
        })
    }

    /// The local subroutines and `vsindex` of the Private DICT a `Private`
    /// entry (size, offset) names.
    fn private(
        table: &'a [u8],
        [size, at]: [i64; 2],
        cff2: bool,
    ) -> Option<(Option<Index<'a>>, i64)> {
        let at = offset(at)?;
        let dict = table.get(at..at.checked_add(offset(size)?)?)?;
        let local = match dict_find(dict, 19) {
            Some(v) => Some(Index::read(table, at.checked_add(offset(v[1])?)?, cff2)?.0),
            None => None,
        };
        let vsindex = if cff2 {
            dict_find(dict, 22).map_or(0, |v| v[1])
        } else {
            0
        };
        Some((local, vsindex))
    }

    /// The regions of ItemVariationData `ivd`: none for one the store lacks,
    /// or without a store. probe: cff2-blend, cff2-blend-no-store
    fn regions(&self, ivd: i64) -> usize {
        let count = |store: &[u8]| -> Option<usize> {
            let data = usize::from(u16_at(store, 6)?);
            let ivd = usize::try_from(ivd).ok().filter(|&i| i < data)?;
            let at = offset_at(store, 8 + 4 * ivd, 4)?;
            Some(usize::from(u16_at(store, at.checked_add(4)?)?))
        };
        self.store.and_then(count).unwrap_or(0)
    }
}

/// The Font DICT FDSelect at `at` gives `glyph` (formats 0 and 3; 4 in
/// CFF2).
fn fd_select(table: &[u8], at: usize, glyph: u16, cff2: bool) -> Option<u32> {
    // `at` lies inside the table, so the offsets below cannot overflow.
    let data = table.get(at..)?;
    let g = u32::from(glyph);
    match *data.first()? {
        0 => data.get(1 + usize::from(glyph)).map(|&fd| u32::from(fd)),
        3 => {
            let ranges = data
                .get(3..)?
                .chunks_exact(3)
                .take(usize::from(u16_at(data, 1)?));
            ranges.enumerate().find_map(|(i, range)| {
                let first = u16::from_be_bytes([range[0], range[1]]);
                let next = u16_at(data, 3 + 3 * i + 3)?;
                (glyph >= first && glyph < next).then_some(u32::from(range[2]))
            })
        }
        4 if cff2 => {
            let ranges = data
                .get(5..)?
                .chunks_exact(6)
                .take(u32_at(data, 1)? as usize);
            ranges.enumerate().find_map(|(i, range)| {
                let first = u32::from_be_bytes([range[0], range[1], range[2], range[3]]);
                let next = u32_at(data, 5 + 6 * i + 6)?;
                (g >= first && g < next)
                    .then_some(u32::from(u16::from_be_bytes([range[4], range[5]])))
            })
        }
        _ => None,
    }
}

/// A name-keyed CFF table's charset (TN 5176 §13), which maps glyphs to
/// string IDs.
struct Charset<'a> {
    table: &'a [u8],
    /// The charset's offset, or 0 for the predefined ISOAdobe charset (1 and
    /// 2, the Expert charsets, are not read).
    at: i64,
    glyphs: u32,
}

impl Charset<'_> {
    /// The glyph after .notdef whose string ID is `sid`.
    fn glyph(&self, sid: u16, work: &mut WorkBudget) -> Step<Option<u32>> {
        let Ok(at) = usize::try_from(self.at) else {
            return Ok(None);
        };
        if at == 0 {
            return Ok(
                (sid != 0 && u32::from(sid) < self.glyphs.min(229)).then_some(u32::from(sid))
            );
        }
        if at <= 2 {
            return Ok(None);
        }
        let table = self.table;
        let Some(&format) = table.get(at) else {
            return Ok(None);
        };
        let mut glyph = 1_u32;
        let mut entry = at + 1;
        while glyph < self.glyphs {
            charge(work)?;
            let (first, left, size) = match format {
                0 => (u16_at(table, entry), Some(0), 2),
                1 => (
                    u16_at(table, entry),
                    table.get(entry + 2).map(|&n| u32::from(n)),
                    3,
                ),
                2 => (
                    u16_at(table, entry),
                    u16_at(table, entry + 2).map(u32::from),
                    4,
                ),
                _ => return Ok(None),
            };
            let (Some(first), Some(left)) = (first, left) else {
                return Ok(None);
            };
            if sid >= first && u32::from(sid - first) <= left {
                let found = glyph + u32::from(sid - first);
                return Ok((found < self.glyphs).then_some(found));
            }
            glyph += left + 1;
            entry += size;
        }
        Ok(None)
    }
}

fn charge(work: &mut WorkBudget) -> Step<()> {
    if work.try_charge(1) {
        Ok(())
    } else {
        Err(Fault::Limit(Error::Policy("charstring work")))
    }
}

/// The bias of a subroutine INDEX of `count` subroutines (TN 5176 §16).
/// probe: cff-subr-bias, cff-subr-bias-small, cff-subr-bias-large
fn bias(count: u32) -> i64 {
    match count {
        0..1240 => 107,
        1240..33900 => 1131,
        _ => 32768,
    }
}

// ------------------------------------------------------------ charstrings

/// One glyph's run: the box every component of it draws.
struct Run<'p, 'a, 'w> {
    program: &'p Program<'a>,
    work: &'w mut WorkBudget,
    depth: u32,
    /// x min, y min, x max, y max.
    bounds: Option<[f64; 4]>,
}

/// The current point.
#[derive(Clone, Copy)]
struct Pen {
    x: f64,
    y: f64,
}

/// The state of one charstring, a glyph or a component of an accented one.
struct State {
    stack: [f64; STACK],
    len: usize,
    pen: Pen,
    /// Stems declared before the first hint or counter mask.
    stems: usize,
    /// Operators and operands run.
    operations: u32,
    masked: bool,
    /// CFF2: the regions `blend` reads, whether `vsindex` ran, whether a
    /// `blend` ran.
    regions: usize,
    indexed: bool,
    blended: bool,
}

/// How running a piece of charstring ended.
enum Flow {
    /// The bytes ran out.
    End,
    Return,
    Endchar,
}

impl<'a> Run<'_, 'a, '_> {
    /// Runs a glyph's charstring from `origin`; `accents` allows the accented
    /// form of `endchar`, which a component of one may not use. probe:
    /// cff-endchar
    fn glyph(&mut self, charstring: &'a [u8], origin: (f64, f64), accents: bool) -> Step<()> {
        let mut s = State {
            stack: [0.0; STACK],
            len: 0,
            pen: Pen {
                x: origin.0,
                y: origin.1,
            },
            stems: 0,
            operations: 0,
            masked: false,
            regions: self.program.regions(self.program.vsindex),
            indexed: false,
            blended: false,
        };
        match self.code(&mut s, charstring, 0, accents)? {
            // CFF2 has no endchar; a CFF charstring must end with it, and a
            // number cut short ends it. probe: cff-box, cff-tail, cff2-tail
            Flow::End if self.program.cff2 => Ok(()),
            Flow::End | Flow::Return => Err(Fault::Glyph),
            Flow::Endchar => Ok(()),
        }
    }

    /// The extents of the box drawn so far.
    fn extents(&self) -> [i32; 4] {
        let Some([x0, y0, x1, y1]) = self.bounds else {
            return [0; 4];
        };
        let side = |v: f64| (v + 0.5).floor();
        let (x, width) = if x0 < x1 {
            (side(x0) as i32, (side(x1) - side(x0)) as i32)
        } else {
            (0, 0)
        };
        let (y, height) = if y0 < y1 {
            (side(y1) as i32, (side(y0) - side(y1)) as i32)
        } else {
            (0, 0)
        };
        [x, y, width, height]
    }

    fn include(&mut self, x: f64, y: f64) {
        self.bounds = Some(match self.bounds {
            None => [x, y, x, y],
            Some([x0, y0, x1, y1]) => [x0.min(x), y0.min(y), x1.max(x), y1.max(y)],
        });
    }

    fn line(&mut self, pen: &mut Pen, dx: f64, dy: f64) {
        self.include(pen.x, pen.y);
        pen.x += dx;
        pen.y += dy;
        self.include(pen.x, pen.y);
    }

    fn curve(&mut self, pen: &mut Pen, d: [f64; 6]) {
        self.include(pen.x, pen.y);
        for pair in d.chunks_exact(2) {
            pen.x += pair[0];
            pen.y += pair[1];
            self.include(pen.x, pen.y);
        }
    }

    /// Runs `code`, a charstring or subroutine at call depth `depth`.
    fn code(&mut self, s: &mut State, code: &'a [u8], depth: u32, accents: bool) -> Step<Flow> {
        let mut at = 0;
        while let Some(&b0) = code.get(at) {
            self.count(s)?;
            at += 1;
            let number = match b0 {
                28 => {
                    let Some(v) = u16_at(code, at) else {
                        return Err(Fault::Glyph);
                    };
                    at += 2;
                    Some(f64::from(v.cast_signed()))
                }
                32..=246 => Some(f64::from(b0) - 139.0),
                247..=254 => {
                    let Some(&b1) = code.get(at) else {
                        return Err(Fault::Glyph);
                    };
                    at += 1;
                    let v = |first: u8| f64::from(b0 - first) * 256.0 + f64::from(b1) + 108.0;
                    Some(if b0 < 251 { v(247) } else { -v(251) })
                }
                255 => {
                    // A 16.16 number cut short ends the charstring. probe:
                    // cff2-tail
                    let Some(v) = u32_at(code, at) else {
                        self.count(s)?;
                        return Ok(Flow::End);
                    };
                    at += 4;
                    Some(f64::from(v.cast_signed()) / 65536.0)
                }
                _ => None,
            };
            if let Some(v) = number {
                if s.len == STACK {
                    return Err(Fault::Glyph);
                }
                s.stack[s.len] = v;
                s.len += 1;
                continue;
            }
            let op = if b0 == 12 {
                // An escape without its second byte ends the charstring.
                // probe: cff2-tail
                let Some(&b1) = code.get(at) else {
                    self.count(s)?;
                    return Ok(Flow::End);
                };
                at += 1;
                0x0C00 | u16::from(b1)
            } else {
                u16::from(b0)
            };
            match op {
                10 | 29 => {
                    let subroutines = if op == 10 {
                        self.program.local
                    } else {
                        Some(self.program.global)
                    };
                    let subroutines = subroutines.ok_or(Fault::Glyph)?;
                    let subroutine = self.call(s, &subroutines, depth)?;
                    match self.code(s, subroutine, depth + 1, accents)? {
                        Flow::Return => {}
                        // A CFF2 subroutine ends at its end; a CFF one must
                        // return. probe: cff-subrs, cff2-subrs
                        Flow::End if self.program.cff2 => {}
                        Flow::End => return Err(Fault::Glyph),
                        Flow::Endchar => return Ok(Flow::Endchar),
                    }
                }
                // return outside a subroutine fails. probe: cff-subrs,
                // cff2-tail
                11 if depth == 0 => return Err(Fault::Glyph),
                11 => return Ok(Flow::Return),
                14 => {
                    self.endchar(s, accents)?;
                    return Ok(Flow::Endchar);
                }
                15 if self.program.cff2 => self.vsindex(s)?,
                16 if self.program.cff2 => self.blend(s)?,
                19 | 20 => {
                    // Operands left before the first mask are vstems; a mask
                    // is as many bytes as the stems declared before the first
                    // mask take. probe: cff-hints, cff-hint-counts, cff2-hints
                    if !s.masked {
                        s.stems += s.len / 2;
                        s.masked = true;
                    }
                    at += s.stems.div_ceil(8);
                    if at > code.len() {
                        return Err(Fault::Glyph);
                    }
                    s.len = 0;
                }
                _ => {
                    self.operator(s, op)?;
                    s.len = 0;
                }
            }
        }
        self.count(s)?;
        Ok(Flow::End)
    }

    /// Counts one operation against the work budget and [`OPERATIONS`].
    fn count(&mut self, s: &mut State) -> Step<()> {
        charge(self.work)?;
        s.operations += 1;
        if s.operations >= OPERATIONS {
            return Err(Fault::Glyph);
        }
        Ok(())
    }

    /// The subroutine a call's index names, popped off the stack: the index
    /// is truncated toward zero and biased; one outside the INDEX, or a call
    /// past the nesting limit, fails. probe: cff-subrs, cff-subr-depth
    fn call(&mut self, s: &mut State, subroutines: &Index<'a>, depth: u32) -> Step<&'a [u8]> {
        if s.len == 0 || depth >= NESTING {
            return Err(Fault::Glyph);
        }
        if depth >= self.depth {
            return Err(Fault::Limit(Error::Policy("charstring subroutine depth")));
        }
        s.len -= 1;
        let index = (s.stack[s.len].trunc() as i64).saturating_add(bias(subroutines.count));
        u32::try_from(index)
            .ok()
            .and_then(|i| subroutines.get(i))
            .ok_or(Fault::Glyph)
    }

    /// `endchar`: with four operands, or five (a width first), the accented
    /// form, which draws the base glyph and then the accent offset by (adx,
    /// ady) into the same box; more fail. CFF2 has no accented form. probe:
    /// cff-endchar, cff2-tail
    fn endchar(&mut self, s: &mut State, accents: bool) -> Step<()> {
        if self.program.cff2 || s.len < 4 {
            return Ok(());
        }
        if s.len > 5 || !accents {
            return Err(Fault::Glyph);
        }
        let [adx, ady, base, accent] = [0, 1, 2, 3].map(|i| s.stack[s.len - 4 + i]);
        let base = self.component(base)?;
        let accent = self.component(accent)?;
        self.glyph(base, (0.0, 0.0), false)?;
        self.glyph(accent, (adx, ady), false)
    }

    /// The charstring of the glyph a standard encoding code names, through
    /// the charset; a code outside the encoding or a glyph the charset lacks
    /// fails.
    fn component(&mut self, code: f64) -> Step<&'a [u8]> {
        let code = usize::try_from(code.trunc() as i64).map_err(|_| Fault::Glyph)?;
        let sid = *STANDARD_ENCODING.get(code).ok_or(Fault::Glyph)?;
        let program = self.program;
        let charset = program.charset.as_ref().ok_or(Fault::Glyph)?;
        let glyph = charset
            .glyph(u16::from(sid), self.work)?
            .ok_or(Fault::Glyph)?;
        program.charstrings.get(glyph).ok_or(Fault::Glyph)
    }

    /// CFF2's `vsindex`: once, before any `blend`, with an index of at least
    /// zero, truncated. probe: cff2-blend, cff2-vsindex
    fn vsindex(&mut self, s: &mut State) -> Step<()> {
        if s.len == 0 || s.indexed || s.blended {
            return Err(Fault::Glyph);
        }
        let ivd = s.stack[s.len - 1].trunc();
        if ivd < 0.0 {
            return Err(Fault::Glyph);
        }
        s.regions = self.program.regions(ivd as i64);
        s.indexed = true;
        s.len = 0;
        Ok(())
    }

    /// CFF2's `blend` at the default instance: of n defaults and their n × k
    /// deltas, the defaults stay. probe: cff2-blend
    fn blend(&mut self, s: &mut State) -> Step<()> {
        if s.len == 0 {
            return Err(Fault::Glyph);
        }
        let n = s.stack[s.len - 1].trunc();
        if n < 0.0 {
            return Err(Fault::Glyph);
        }
        let n = n as usize;
        let operands = n
            .checked_mul(s.regions + 1)
            .and_then(|v| v.checked_add(1))
            .filter(|&v| v <= s.len)
            .ok_or(Fault::Glyph)?;
        s.len = s.len - operands + n;
        s.blended = true;
        Ok(())
    }

    /// The other operators: stems, path construction and flex, and those
    /// that only clear the stack: every arithmetic, logic, storage, reserved
    /// or unknown one. Path operators use their operands as the counts allow
    /// and ignore what is left. probe: cff-operand-counts, cff-operators,
    /// cff2-operand-counts, cff2-operators
    fn operator(&mut self, s: &mut State, op: u16) -> Step<()> {
        let n = s.len;
        let State {
            stack,
            pen,
            stems,
            masked,
            ..
        } = s;
        let a = &stack[..n];
        match op {
            1 | 3 | 18 | 23 => {
                if !*masked {
                    *stems += n / 2;
                }
            }
            21 => {
                if n < 2 {
                    return Err(Fault::Glyph);
                }
                pen.x += a[n - 2];
                pen.y += a[n - 1];
            }
            22 | 4 => {
                if n < 1 {
                    return Err(Fault::Glyph);
                }
                if op == 22 {
                    pen.x += a[n - 1];
                } else {
                    pen.y += a[n - 1];
                }
            }
            5 => {
                for i in (0..n / 2).map(|i| 2 * i) {
                    self.line(pen, a[i], a[i + 1]);
                }
            }
            6 | 7 => {
                for (i, &d) in a.iter().enumerate() {
                    if (i % 2 == 0) == (op == 6) {
                        self.line(pen, d, 0.0);
                    } else {
                        self.line(pen, 0.0, d);
                    }
                }
            }
            8 => {
                for i in (0..n / 6).map(|i| 6 * i) {
                    self.curve(
                        pen,
                        [a[i], a[i + 1], a[i + 2], a[i + 3], a[i + 4], a[i + 5]],
                    );
                }
            }
            27 | 26 => {
                // hhcurveto and vvcurveto: an odd count starts with the
                // first curve's other delta.
                let first = n % 2;
                let mut other = if first == 1 { a[0] } else { 0.0 };
                for i in (0..(n - first) / 4).map(|i| first + 4 * i) {
                    let d = if op == 27 {
                        [a[i], other, a[i + 1], a[i + 2], a[i + 3], 0.0]
                    } else {
                        [other, a[i], a[i + 1], a[i + 2], 0.0, a[i + 3]]
                    };
                    self.curve(pen, d);
                    other = 0.0;
                }
            }
            31 | 30 => {
                // hvcurveto and vhcurveto alternate; the last curve's final
                // delta is the operand after the last full curve, unless the
                // count is a multiple of four or two more than a multiple of
                // eight. probe: cff-operand-counts
                let curves = n / 4;
                let last = (!n.is_multiple_of(4) && n % 8 != 2).then(|| a[4 * curves]);
                for c in 0..curves {
                    let i = 4 * c;
                    let tail = if c + 1 == curves {
                        last.unwrap_or(0.0)
                    } else {
                        0.0
                    };
                    let horizontal = (c % 2 == 0) == (op == 31);
                    let d = if horizontal {
                        [a[i], 0.0, a[i + 1], a[i + 2], tail, a[i + 3]]
                    } else {
                        [0.0, a[i], a[i + 1], a[i + 2], a[i + 3], tail]
                    };
                    self.curve(pen, d);
                }
            }
            24 => {
                // rcurveline: curves while eight operands remain, then a line;
                // nothing with fewer than eight.
                if n >= 8 {
                    let mut i = 0;
                    while n - i >= 8 {
                        self.curve(
                            pen,
                            [a[i], a[i + 1], a[i + 2], a[i + 3], a[i + 4], a[i + 5]],
                        );
                        i += 6;
                    }
                    self.line(pen, a[i], a[i + 1]);
                }
            }
            25 => {
                // rlinecurve: lines while eight operands remain, then a curve;
                // nothing with fewer than eight.
                if n >= 8 {
                    let mut i = 0;
                    while n - i >= 8 {
                        self.line(pen, a[i], a[i + 1]);
                        i += 2;
                    }
                    self.curve(
                        pen,
                        [a[i], a[i + 1], a[i + 2], a[i + 3], a[i + 4], a[i + 5]],
                    );
                }
            }
            0x0C22..=0x0C25 => self.flex(pen, op, a)?,
            _ => {}
        }
        Ok(())
    }

    /// The flex operators, each with exactly the operands TN 5177 gives it;
    /// any other count fails. probe: cff-operand-counts
    fn flex(&mut self, pen: &mut Pen, op: u16, a: &[f64]) -> Step<()> {
        match (op, a.len()) {
            (0x0C23, 13) => {
                self.curve(pen, [a[0], a[1], a[2], a[3], a[4], a[5]]);
                self.curve(pen, [a[6], a[7], a[8], a[9], a[10], a[11]]);
            }
            (0x0C22, 7) => {
                self.curve(pen, [a[0], 0.0, a[1], a[2], a[3], 0.0]);
                self.curve(pen, [a[4], 0.0, a[5], -a[2], a[6], 0.0]);
            }
            (0x0C24, 9) => {
                self.curve(pen, [a[0], a[1], a[2], a[3], a[4], 0.0]);
                let dy = -(a[1] + a[3] + a[7]);
                self.curve(pen, [a[5], 0.0, a[6], a[7], a[8], dy]);
            }
            (0x0C25, 11) => {
                // The last point returns to the start's y or x, whichever
                // the larger of the summed deltas does not run along.
                let dx: f64 = a[..10].iter().step_by(2).sum();
                let dy: f64 = a[1..10].iter().step_by(2).sum();
                let (dx6, dy6) = if dx.abs() > dy.abs() {
                    (a[10], -dy)
                } else {
                    (-dx, a[10])
                };
                self.curve(pen, [a[0], a[1], a[2], a[3], a[4], a[5]]);
                self.curve(pen, [a[6], a[7], a[8], a[9], dx6, dy6]);
            }
            _ => return Err(Fault::Glyph),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{Index, table_extents};
    use triergon_core::{Error, WorkBudget};

    /// A CFF2 INDEX with one-byte offsets.
    fn index(objects: &[&[u8]]) -> Vec<u8> {
        let mut out = (objects.len() as u32).to_be_bytes().to_vec();
        out.extend(&cff_index(objects)[2..]);
        out
    }

    /// A CFF INDEX with one-byte offsets.
    fn cff_index(objects: &[&[u8]]) -> Vec<u8> {
        let mut out = (objects.len() as u16).to_be_bytes().to_vec();
        if objects.is_empty() {
            return out;
        }
        out.push(1);
        let mut offset = 1;
        out.push(offset);
        for object in objects {
            offset += object.len() as u8;
            out.push(offset);
        }
        for object in objects {
            out.extend_from_slice(object);
        }
        out
    }

    /// A DICT integer in the five-byte form.
    fn int(value: i32) -> Vec<u8> {
        let mut out = vec![29];
        out.extend(value.to_be_bytes());
        out
    }

    /// A CFF2 table of `glyphs` with local subroutines `local`.
    fn cff2(glyphs: &[&[u8]], local: &[&[u8]]) -> Vec<u8> {
        let top_len = 5 + 1 + 5 + 2;
        let global = index(&[]);
        let charstrings_at = 5 + top_len + global.len();
        let charstrings = index(glyphs);
        let fd_array_at = charstrings_at + charstrings.len();
        let private_len = if local.is_empty() { 0 } else { 6 };
        let font_dict = [int(private_len), int(0), vec![18]].concat();
        let fd_array_len = index(&[&font_dict]).len();
        let private_at = (fd_array_at + fd_array_len) as i32;
        let font_dict = [int(private_len), int(private_at), vec![18]].concat();
        let mut out = vec![2, 0, 5];
        out.extend((top_len as u16).to_be_bytes());
        out.extend(int(charstrings_at as i32));
        out.push(17);
        out.extend(int(fd_array_at as i32));
        out.extend([12, 36]);
        out.extend(global);
        out.extend(charstrings);
        out.extend(index(&[&font_dict]));
        if !local.is_empty() {
            out.extend(int(private_len));
            out.push(19);
            out.extend(index(local));
        }
        out
    }

    /// A name-keyed CFF table of `glyphs`, glyph g after .notdef named by
    /// string ID `sids[g - 1]`, with local subroutines `local`.
    fn cff(glyphs: &[&[u8]], sids: &[u16], local: &[&[u8]]) -> Vec<u8> {
        let fixed = 4 + cff_index(&[b"a"]).len() + 3 + 25 + 2 + 2;
        let charstrings = cff_index(glyphs);
        let mut charset = vec![0];
        charset.extend(sids.iter().flat_map(|sid| sid.to_be_bytes()));
        let charstrings_at = fixed as i32;
        let charset_at = charstrings_at + charstrings.len() as i32;
        let private_at = charset_at + charset.len() as i32;
        let private = [int(6), vec![19]].concat();
        let top = [
            int(charstrings_at),
            vec![17],
            int(charset_at),
            vec![15],
            int(private.len() as i32),
            int(private_at),
            vec![18],
        ]
        .concat();
        let mut out = vec![1, 0, 4, 1];
        out.extend(cff_index(&[b"a"]));
        out.extend(cff_index(&[&top]));
        out.extend([0, 0, 0, 0]);
        assert_eq!(out.len(), fixed);
        out.extend(charstrings);
        out.extend(charset);
        out.extend(private);
        out.extend(cff_index(local));
        out
    }

    fn run(table: &[u8], glyph: u16) -> Option<[i32; 4]> {
        table_extents(table, true, glyph, 16, &mut WorkBudget::new(1 << 20)).unwrap()
    }

    /// `n` as a one-byte charstring number.
    const fn n(v: i8) -> u8 {
        (v as i16 + 139) as u8
    }

    #[test]
    fn an_index_finds_its_objects() {
        let data = index(&[b"ab", b"", b"c"]);
        let (index, end) = Index::read(&data, 0, true).unwrap();
        assert_eq!(end, data.len());
        assert_eq!(index.get(0), Some(&b"ab"[..]));
        assert_eq!(index.get(1), Some(&b""[..]));
        assert_eq!(index.get(2), Some(&b"c"[..]));
        assert_eq!(index.get(3), None);
    }

    #[test]
    fn the_box_holds_control_points_and_rounds_each_side_half_up() {
        let curve = [n(0), n(0), 21, n(0), n(100), n(100), n(0), n(0), n(-100), 8];
        // From (0.5, -1.5) a line of (100.25, 2.5).
        let line = [
            255, 0, 0, 0x80, 0, 255, 0xFF, 0xFE, 0x80, 0, 21, 255, 0, 100, 0x40, 0, 255, 0, 2,
            0x80, 0, 5,
        ];
        let table = cff2(&[&[], &curve, &line, &[n(10), n(10), 21]], &[]);
        assert_eq!(run(&table, 0), Some([0; 4]));
        assert_eq!(run(&table, 1), Some([0, 100, 100, -100]));
        assert_eq!(run(&table, 2), Some([1, 1, 100, -2]));
        assert_eq!(run(&table, 3), Some([0; 4]));
        assert_eq!(run(&table, 4), None);
    }

    #[test]
    fn an_axis_without_extent_is_zero() {
        let table = cff2(&[&[n(100), n(50), 21, n(50), 6]], &[]);
        assert_eq!(run(&table, 0), Some([100, 0, 50, 0]));
    }

    #[test]
    fn unknown_operators_clear_the_stack_and_too_few_moveto_operands_fail() {
        // 10 20 add 30 40 rlineto from (100, 100); then rmoveto with one operand.
        let add = [n(100), n(100), 21, n(10), n(20), 12, 10, n(30), n(40), 5];
        let table = cff2(&[&add, &[n(1), 21]], &[]);
        assert_eq!(run(&table, 0), Some([100, 140, 30, -40]));
        assert_eq!(run(&table, 1), None);
    }

    #[test]
    fn hvcurveto_takes_its_final_delta_as_the_probes_show() {
        // Five operands: one curve and its final delta; ten: two curves.
        let five = [n(0), n(0), 21, n(10), n(10), n(10), n(10), n(5), 31];
        let ten = [
            n(0),
            n(0),
            21,
            n(10),
            n(10),
            n(10),
            n(10),
            n(10),
            n(10),
            n(10),
            n(10),
            n(5),
            n(5),
            31,
            n(0),
            n(-100),
            5,
        ];
        let table = cff2(&[&five, &ten], &[]);
        assert_eq!(run(&table, 0), Some([0, 20, 25, -20]));
        // The curves end at (40, 40); the line down then shows the end point.
        assert_eq!(run(&table, 1), Some([0, 40, 40, -100]));
    }

    #[test]
    fn subroutines_nest_ten_deep() {
        // Local k calls k + 1 (k < 11); local 11 draws.
        let mut local: Vec<Vec<u8>> = (0..11).map(|k| vec![n(k + 1 - 107), 10]).collect();
        local.push(vec![n(10), n(10), 5]);
        let local: Vec<&[u8]> = local.iter().map(Vec::as_slice).collect();
        let ten = [n(0), n(0), 21, n(2 - 107), 10];
        let eleven = [n(0), n(0), 21, n(1 - 107), 10];
        let table = cff2(&[&ten, &eleven], &local);
        assert_eq!(run(&table, 0), Some([0, 10, 10, -10]));
        assert_eq!(run(&table, 1), None);
        let limited = table_extents(&table, true, 0, 3, &mut WorkBudget::new(1 << 20));
        assert!(matches!(limited, Err(Error::Policy(_))));
    }

    #[test]
    fn blend_keeps_the_defaults_and_vsindex_follows_its_rules() {
        // Without a store k = 0: 10 20 2 blend leaves 10 20.
        let blend = [n(0), n(0), 21, n(10), n(20), n(2), 16, 5];
        let twice = [n(0), 15, n(0), 15];
        let table = cff2(&[&blend, &twice, &[n(1), 16]], &[]);
        assert_eq!(run(&table, 0), Some([0, 20, 10, -20]));
        assert_eq!(run(&table, 1), None);
        assert_eq!(run(&table, 2), None);
    }

    #[test]
    fn cff_charstrings_end_with_endchar_and_accent_through_the_charset() {
        // 1: A (SID 34), x 100..500, y 0..600; 2: acute (SID 125), x and y
        // 100..200; 3: A with the acute moved by (50, 20); 4: an accent the
        // charset lacks (code 197); 5: no endchar; 6: a subroutine without
        // return; 7: a subroutine that returns.
        let a = [
            n(100),
            n(0),
            21,
            247,
            92,
            248,
            236,
            247,
            92,
            252,
            236,
            5,
            14,
        ];
        let acute = [n(100), n(100), 21, n(100), n(100), 5, 14];
        let table = cff(
            &[
                &[14],
                &a,
                &acute,
                &[n(50), n(20), n(65), 247, 86, 14],
                &[n(50), n(20), n(65), 247, 89, 14],
                &[n(10), n(10), 21],
                &[n(10), n(10), 21, n(-107), 10, 14],
                &[n(10), n(10), 21, n(-106), 10, 14],
            ],
            &[34, 125, 391, 392, 393, 394, 395],
            &[&[n(5), n(5), 5], &[n(5), n(5), 5, 11]],
        );
        let cff_run =
            |glyph| table_extents(&table, false, glyph, 16, &mut WorkBudget::new(1 << 20)).unwrap();
        assert_eq!(cff_run(0), Some([0; 4]));
        assert_eq!(cff_run(1), Some([100, 600, 400, -600]));
        assert_eq!(cff_run(2), Some([100, 200, 100, -100]));
        assert_eq!(cff_run(3), Some([100, 600, 400, -600]));
        assert_eq!(cff_run(4), None);
        assert_eq!(cff_run(5), None);
        assert_eq!(cff_run(6), None);
        assert_eq!(cff_run(7), Some([10, 15, 5, -5]));
    }

    #[test]
    fn work_is_bounded() {
        let table = cff2(&[&[n(1); 40]], &[]);
        let result = table_extents(&table, true, 0, 16, &mut WorkBudget::new(10));
        assert!(matches!(result, Err(Error::Policy(_))));
    }

    #[test]
    fn malformed_tables_give_no_extents() {
        let table = cff2(&[&[n(10), n(10), 21, n(5), n(5), 5]], &[]);
        for len in 0..table.len() {
            let _ = table_extents(&table[..len], true, 0, 16, &mut WorkBudget::new(1 << 20));
        }
        assert_eq!(run(&table[..3], 0), None);
        assert_eq!(
            table_extents(&table, false, 0, 16, &mut WorkBudget::new(1 << 20)).unwrap(),
            None
        );
    }
}

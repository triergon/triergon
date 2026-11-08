//! Type 2 charstrings (Adobe Technical Note 5177) and CFF2 charstrings, unhinted,
//! evaluated in 16.16 fixed point as FreeType's CFF engine evaluates them.
//!
//! Hints are counted, so that mask lengths are known, and otherwise ignored. Drawing
//! operators consume complete operand groups and drop a malformed remainder, and
//! CFF2 charstrings may leave operands on the stack, as FreeType accepts both.
//! Arithmetic wraps at 32 bits and rounds as FreeType's does; operands are not
//! typed, so an integer operator accepts any number and rounds it.

use super::dict::{CFF_OPERANDS, CFF2_OPERANDS, Stack};
use super::path::{Builder, Fixed, Path};
use super::{Cff, Private, Version};
use crate::Limits;
use crate::fixed::{div_fix, mul_fix, sqrt_fixed};
use crate::outline::{Coordinate, Outline};
use crate::read::{Meter, Reader};
use triergon_core::{Error, Result};

/// TN 5177 Appendix B: nested subroutine calls, stem hints and transient slots.
const MAX_SUBR_DEPTH: u32 = 10;
const MAX_STEM_HINTS: usize = 96;
const TRANSIENT_SLOTS: usize = 32;
const ONE: Fixed = 0x1_0000;

/// Standard Encoding: character code to SID, for `seac` (TN 5176 Appendix B).
const STANDARD_ENCODING: [u8; 256] = {
    let mut table = [0_u8; 256];
    let mut code = 32;
    while code <= 126 {
        table[code] = (code - 31) as u8;
        code += 1;
    }
    let high: [(usize, u8); 54] = [
        (161, 96),
        (162, 97),
        (163, 98),
        (164, 99),
        (165, 100),
        (166, 101),
        (167, 102),
        (168, 103),
        (169, 104),
        (170, 105),
        (171, 106),
        (172, 107),
        (173, 108),
        (174, 109),
        (175, 110),
        (177, 111),
        (178, 112),
        (179, 113),
        (180, 114),
        (182, 115),
        (183, 116),
        (184, 117),
        (185, 118),
        (186, 119),
        (187, 120),
        (188, 121),
        (189, 122),
        (191, 123),
        (193, 124),
        (194, 125),
        (195, 126),
        (196, 127),
        (197, 128),
        (198, 129),
        (199, 130),
        (200, 131),
        (202, 132),
        (203, 133),
        (205, 134),
        (206, 135),
        (207, 136),
        (208, 137),
        (225, 138),
        (227, 139),
        (232, 140),
        (233, 141),
        (234, 142),
        (235, 143),
        (241, 144),
        (245, 145),
        (248, 146),
        (249, 147),
        (250, 148),
        (251, 149),
    ];
    let mut i = 0;
    while i < high.len() {
        table[high[i].0] = high[i].1;
        i += 1;
    }
    table
};

/// An integer as a charstring number.
const fn int(v: i32) -> Fixed {
    v.wrapping_shl(16)
}

/// A charstring number as an integer, rounded and narrowed to 16 bits
/// (`cf2_fixedToInt`).
const fn to_int(v: Fixed) -> i32 {
    (((v.cast_unsigned()).wrapping_add(0x8000) >> 16) as u16).cast_signed() as i32
}

/// A charstring number as an index or count.
fn index(v: Fixed) -> Result<usize> {
    usize::try_from(to_int(v)).map_err(|_| Error::Invalid("charstring index"))
}

const fn truth(b: bool) -> Fixed {
    if b { ONE } else { 0 }
}

/// Loads `glyph`'s charstring into `outline`, in whole font units.
pub(super) fn outline<C: Coordinate>(
    cff: &Cff,
    table: &[u8],
    glyph: u32,
    outline: &mut Outline<C>,
    limits: &Limits,
    meter: &mut Meter,
) -> Result<()> {
    outline.begin(limits.points);
    let depth = match cff.version {
        Version::Cff1 => MAX_SUBR_DEPTH,
        Version::Cff2 => u32::MAX,
    }
    .min(limits.depth);
    let mut run = Run {
        cff,
        table,
        meter,
        depth,
        builder: Builder::new(outline),
    };
    match cff.version {
        Version::Cff1 => run.glyph::<CFF_OPERANDS>(glyph, (0, 0), true)?,
        Version::Cff2 => run.glyph::<CFF2_OPERANDS>(glyph, (0, 0), true)?,
    }
    run.builder.close()
}

struct Run<'a, 'o, C> {
    cff: &'a Cff,
    table: &'a [u8],
    meter: &'a mut Meter,
    depth: u32,
    builder: Builder<'o, C>,
}

/// How a charstring or subroutine ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Flow {
    Return,
    End,
}

/// A `seac` composite: accent offset and the two standard codes.
#[derive(Clone, Copy, Debug)]
struct Seac {
    dx: Fixed,
    dy: Fixed,
    base: Fixed,
    accent: Fixed,
}

struct State<'p, const N: usize> {
    private: &'p Private,
    stack: Stack<Fixed, N>,
    transient: [Fixed; TRANSIENT_SLOTS],
    /// The current point.
    x: Fixed,
    y: Fixed,
    path: Path,
    width_seen: bool,
    stems: usize,
    seac: Option<Seac>,
    vsindex: usize,
    random: u32,
}

impl<C: Coordinate> Run<'_, '_, C> {
    fn glyph<const N: usize>(
        &mut self,
        glyph: u32,
        origin: (Fixed, Fixed),
        top: bool,
    ) -> Result<()> {
        let charstring = self.cff.charstrings.get(self.table, glyph)?;
        let private = self.cff.private(self.table, glyph)?;
        let mut s = State::<N> {
            private,
            stack: Stack::new(),
            transient: [0; TRANSIENT_SLOTS],
            x: origin.0,
            y: origin.1,
            path: Path::new(),
            width_seen: self.cff.version == Version::Cff2,
            stems: 0,
            seac: None,
            vsindex: private.vsindex,
            random: 0,
        };
        self.run(charstring, 0, &mut s)?;
        s.path.close(&mut self.builder)?;
        if let Some(seac) = s.seac {
            if !top || self.cff.is_cid_keyed() {
                return Err(Error::Invalid("seac inside seac or in a CID-keyed font"));
            }
            // FreeType draws the accent, at its offset, before the base.
            for (code, at) in [(seac.accent, (seac.dx, seac.dy)), (seac.base, (0, 0))] {
                let code = usize::try_from(to_int(code))
                    .ok()
                    .filter(|&c| c < 256)
                    .ok_or(Error::Invalid("seac code"))?;
                let sid = STANDARD_ENCODING[code];
                let component = self
                    .cff
                    .glyph_for_sid(self.table, u16::from(sid))
                    .filter(|_| sid != 0)
                    .ok_or(Error::Invalid("seac component"))?;
                self.glyph::<N>(component, at, false)?;
            }
        }
        Ok(())
    }

    fn run<const N: usize>(
        &mut self,
        charstring: &[u8],
        depth: u32,
        s: &mut State<'_, N>,
    ) -> Result<Flow> {
        let mut r = Reader::new(charstring);
        while !r.at_end() {
            self.meter.charge(1)?;
            let byte = r.u8()?;
            match byte {
                28 => s.stack.push(int(i32::from(r.i16()?)))?,
                32..=246 => s.stack.push(int(i32::from(byte) - 139))?,
                247..=250 => {
                    let v = (i32::from(byte) - 247) * 256 + i32::from(r.u8()?) + 108;
                    s.stack.push(int(v))?;
                }
                251..=254 => {
                    let v = -(i32::from(byte) - 251) * 256 - i32::from(r.u8()?) - 108;
                    s.stack.push(int(v))?;
                }
                255 => s.stack.push(r.i32()?)?,
                1 | 3 | 18 | 23 => {
                    // hstem, vstem, hstemhm, vstemhm
                    Self::width(s, s.stack.len() % 2 == 1);
                    Self::stems(s)?;
                }
                19 | 20 => {
                    // hintmask, cntrmask: operands before the first mask are vstems.
                    Self::width(s, s.stack.len() % 2 == 1);
                    Self::stems(s)?;
                    r.skip(s.stems.div_ceil(8))?;
                    if byte == 19 {
                        s.path.hint_mask();
                    }
                }
                21 => {
                    Self::width(s, s.stack.len() > 2);
                    let (dx, dy) = top2(&s.stack)?;
                    self.move_by(s, dx, dy)?;
                }
                22 | 4 => {
                    Self::width(s, s.stack.len() > 1);
                    let d = top1(&s.stack)?;
                    let (dx, dy) = if byte == 22 { (d, 0) } else { (0, d) };
                    self.move_by(s, dx, dy)?;
                }
                5 => {
                    for i in (0..s.stack.len() / 2).map(|i| 2 * i) {
                        let a = &s.stack.as_slice()[i..i + 2];
                        let (dx, dy) = (a[0], a[1]);
                        self.line_by(s, dx, dy)?;
                    }
                    s.stack.clear();
                }
                6 | 7 => {
                    let mut horizontal = byte == 6;
                    for i in 0..s.stack.len() {
                        let d = s.stack.as_slice()[i];
                        if horizontal {
                            self.line_by(s, d, 0)?;
                        } else {
                            self.line_by(s, 0, d)?;
                        }
                        horizontal = !horizontal;
                    }
                    s.stack.clear();
                }
                8 => {
                    for i in (0..s.stack.len() / 6).map(|i| 6 * i) {
                        let d = six(&s.stack.as_slice()[i..]);
                        self.curve_by(s, d)?;
                    }
                    s.stack.clear();
                }
                24 => {
                    // rcurveline: curves, then a line.
                    let n = s.stack.len();
                    if n >= 8 {
                        let curves = (n - 2) / 6;
                        for i in 0..curves {
                            let d = six(&s.stack.as_slice()[6 * i..]);
                            self.curve_by(s, d)?;
                        }
                        let a = &s.stack.as_slice()[6 * curves..];
                        let (dx, dy) = (a[0], a[1]);
                        self.line_by(s, dx, dy)?;
                    }
                    s.stack.clear();
                }
                25 => {
                    // rlinecurve: lines, then a curve.
                    let n = s.stack.len();
                    if n >= 8 {
                        let lines = (n - 6) / 2;
                        for i in 0..lines {
                            let a = &s.stack.as_slice()[2 * i..2 * i + 2];
                            let (dx, dy) = (a[0], a[1]);
                            self.line_by(s, dx, dy)?;
                        }
                        let d = six(&s.stack.as_slice()[2 * lines..]);
                        self.curve_by(s, d)?;
                    }
                    s.stack.clear();
                }
                26 | 27 => {
                    // vvcurveto, hhcurveto: an odd first operand offsets the first curve.
                    let n = s.stack.len();
                    let (mut extra, mut i) = if n % 2 == 1 {
                        (s.stack.as_slice()[0], 1)
                    } else {
                        (0, 0)
                    };
                    while i + 4 <= n {
                        let a = &s.stack.as_slice()[i..i + 4];
                        let d = if byte == 26 {
                            [extra, a[0], a[1], a[2], 0, a[3]]
                        } else {
                            [a[0], extra, a[1], a[2], a[3], 0]
                        };
                        self.curve_by(s, d)?;
                        extra = 0;
                        i += 4;
                    }
                    s.stack.clear();
                }
                30 | 31 => {
                    // vhcurveto, hvcurveto: alternating; an odd last operand ends the
                    // final curve off its axis.
                    let n = s.stack.len();
                    let mut horizontal = byte == 31;
                    let mut i = 0;
                    while i + 4 <= n {
                        let a = &s.stack.as_slice()[i..i + 4];
                        let last = if n - i == 5 {
                            s.stack.as_slice()[i + 4]
                        } else {
                            0
                        };
                        let d = if horizontal {
                            [a[0], 0, a[1], a[2], last, a[3]]
                        } else {
                            [0, a[0], a[1], a[2], a[3], last]
                        };
                        self.curve_by(s, d)?;
                        horizontal = !horizontal;
                        i += 4;
                    }
                    s.stack.clear();
                }
                10 | 29 => {
                    let subrs = if byte == 10 {
                        &s.private.subrs
                    } else {
                        &self.cff.global
                    };
                    let biased = i64::from(to_int(s.stack.pop()?)) + bias(subrs.len());
                    let subr = subrs.get(
                        self.table,
                        u32::try_from(biased)
                            .map_err(|_| Error::Invalid("charstring subroutine number"))?,
                    )?;
                    if depth + 1 > self.depth {
                        return Err(
                            if depth + 1 > MAX_SUBR_DEPTH && self.cff.version == Version::Cff1 {
                                Error::Invalid("charstring subroutines nest too deeply")
                            } else {
                                Error::Policy("charstring subroutine depth")
                            },
                        );
                    }
                    if self.run(subr, depth + 1, s)? == Flow::End {
                        return Ok(Flow::End);
                    }
                }
                11 if self.cff.version == Version::Cff1 => return Ok(Flow::Return),
                14 if self.cff.version == Version::Cff1 => {
                    let n = s.stack.len();
                    Self::width(s, n == 1 || n == 5);
                    if let [dx, dy, base, accent] = *s.stack.as_slice() {
                        s.seac = Some(Seac {
                            dx,
                            dy,
                            base,
                            accent,
                        });
                    }
                    s.stack.clear();
                    return Ok(Flow::End);
                }
                15 if self.cff.version == Version::Cff2 => {
                    s.vsindex = index(s.stack.pop()?)?;
                    self.cff.region_scalars(s.vsindex)?;
                    s.stack.clear();
                }
                16 if self.cff.version == Version::Cff2 => {
                    let count = index(s.stack.pop()?)?;
                    let scalars = self.cff.region_scalars(s.vsindex)?;
                    s.stack.blend(count, scalars.len(), |value, deltas| {
                        deltas
                            .iter()
                            .zip(scalars)
                            .fold(value, |sum, (&d, &weight)| {
                                // Region scalars are 0 or 1 at the default instance.
                                let weight = if weight == 0.0 { 0 } else { 0x1_0000 };
                                sum.wrapping_add(mul_fix(i64::from(d), weight) as Fixed)
                            })
                    })?;
                }
                12 => self.escape(r.u8()?, s)?,
                _ => return Err(Error::Invalid("reserved charstring operator")),
            }
        }
        match (self.cff.version, depth) {
            // A Type 2 glyph must end with endchar; FreeType ends it at its data's end.
            (_, 0) => Ok(Flow::End),
            _ => Ok(Flow::Return),
        }
    }

    /// Takes the width operand of the first stack-clearing operator (TN 5177 3.1).
    fn width<const N: usize>(s: &mut State<'_, N>, present: bool) {
        if !s.width_seen {
            s.width_seen = true;
            if present {
                s.stack.shift();
            }
        }
    }

    fn stems<const N: usize>(s: &mut State<'_, N>) -> Result<()> {
        s.stems += s.stack.len() / 2;
        s.stack.clear();
        if s.stems > MAX_STEM_HINTS {
            return Err(Error::Invalid("more than 96 stem hints"));
        }
        Ok(())
    }

    fn move_by<const N: usize>(
        &mut self,
        s: &mut State<'_, N>,
        dx: Fixed,
        dy: Fixed,
    ) -> Result<()> {
        s.stack.clear();
        s.x = s.x.wrapping_add(dx);
        s.y = s.y.wrapping_add(dy);
        s.path.move_to(&mut self.builder, (s.x, s.y))
    }

    fn line_by<const N: usize>(
        &mut self,
        s: &mut State<'_, N>,
        dx: Fixed,
        dy: Fixed,
    ) -> Result<()> {
        s.x = s.x.wrapping_add(dx);
        s.y = s.y.wrapping_add(dy);
        s.path.line_to(&mut self.builder, (s.x, s.y))
    }

    fn curve_by<const N: usize>(&mut self, s: &mut State<'_, N>, d: [Fixed; 6]) -> Result<()> {
        let c1 = (s.x.wrapping_add(d[0]), s.y.wrapping_add(d[1]));
        let c2 = (c1.0.wrapping_add(d[2]), c1.1.wrapping_add(d[3]));
        let end = (c2.0.wrapping_add(d[4]), c2.1.wrapping_add(d[5]));
        (s.x, s.y) = end;
        s.path.curve_to(&mut self.builder, c1, c2, end)
    }

    fn escape<const N: usize>(&mut self, code: u8, s: &mut State<'_, N>) -> Result<()> {
        if let 34..=37 = code {
            let curves = flex(code, s.stack.as_slice())?;
            s.stack.clear();
            for curve in curves {
                self.curve_by(s, curve)?;
            }
            return Ok(());
        }
        if self.cff.version == Version::Cff2 {
            // FreeType's CFF2 interpreter skips the Type 2 arithmetic operators.
            s.stack.clear();
            return Ok(());
        }
        let stack = &mut s.stack;
        let saturate = |v: Fixed| if v == Fixed::MIN { Fixed::MAX } else { v };
        match code {
            0 => {} // dotsection: deprecated, a no-op.
            3 | 4 | 10 | 11 | 12 | 15 | 24 => {
                let b = stack.pop()?;
                let a = stack.pop()?;
                let value = match code {
                    3 => truth(a != 0 && b != 0),
                    4 => truth(a != 0 || b != 0),
                    10 => a.wrapping_add(b),
                    11 => a.wrapping_sub(b),
                    12 => div_fix(i64::from(a), i64::from(b)) as Fixed,
                    15 => truth(a == b),
                    _ => mul_fix(i64::from(a), i64::from(b)) as Fixed,
                };
                stack.push(value)?;
            }
            5 | 9 | 14 | 26 => {
                let a = stack.pop()?;
                let value = match code {
                    5 => truth(a == 0),
                    9 => saturate(a).abs(),
                    14 => saturate(a.wrapping_neg()),
                    _ if a > 0 => sqrt_fixed(a.cast_unsigned()).cast_signed(),
                    _ => 0,
                };
                stack.push(value)?;
            }
            18 => {
                stack.pop()?;
            }
            20 => {
                let slot = to_int(stack.pop()?);
                let value = stack.pop()?;
                if let Some(cell) = usize::try_from(slot)
                    .ok()
                    .and_then(|i| s.transient.get_mut(i))
                {
                    *cell = value;
                }
            }
            21 => {
                let slot = to_int(stack.pop()?);
                if let Some(&value) = usize::try_from(slot).ok().and_then(|i| s.transient.get(i)) {
                    stack.push(value)?;
                }
            }
            22 => {
                let v2 = stack.pop()?;
                let v1 = stack.pop()?;
                let b = stack.pop()?;
                let a = stack.pop()?;
                stack.push(if v1 <= v2 { a } else { b })?;
            }
            23 => {
                // The range is (0, 1]; FreeType seeds its generator per font.
                s.random = s.random.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                stack.push(((s.random >> 16) & 0xffff).cast_signed() + 1)?;
            }
            27 => {
                let a = stack.pop()?;
                stack.push(a)?;
                stack.push(a)?;
            }
            28 => {
                let b = stack.pop()?;
                let a = stack.pop()?;
                stack.push(b)?;
                stack.push(a)?;
            }
            29 => {
                let i = to_int(stack.pop()?);
                let len = stack.len();
                if len > 0 {
                    let from = match usize::try_from(i) {
                        Ok(i) if i < len => len - 1 - i,
                        Ok(_) => 0,
                        Err(_) => len - 1,
                    };
                    let value = stack.as_slice()[from];
                    stack.push(value)?;
                }
            }
            30 => {
                let shift = to_int(stack.pop()?);
                let count = to_int(stack.pop()?);
                roll(stack, count, shift)?;
            }
            _ => return Err(Error::Invalid("reserved charstring escape")),
        }
        Ok(())
    }
}

/// `roll`: rotates the top `count` operands upward by `shift`.
fn roll<const N: usize>(stack: &mut Stack<Fixed, N>, count: i32, shift: i32) -> Result<()> {
    let Ok(count) = usize::try_from(count) else {
        return Ok(());
    };
    if count < 2 {
        return Ok(());
    }
    let len = stack.len();
    if count > len {
        return Err(Error::Invalid("charstring roll"));
    }
    let mut values = [0; CFF2_OPERANDS];
    values[..count].copy_from_slice(&stack.as_slice()[len - count..]);
    let rotation = (i64::from(shift).rem_euclid(count as i64)) as usize;
    values[..count].rotate_right(rotation);
    for _ in 0..count {
        stack.pop()?;
    }
    for &v in &values[..count] {
        stack.push(v)?;
    }
    Ok(())
}

fn six(a: &[Fixed]) -> [Fixed; 6] {
    [a[0], a[1], a[2], a[3], a[4], a[5]]
}

fn top1<const N: usize>(stack: &Stack<Fixed, N>) -> Result<Fixed> {
    match stack.as_slice() {
        [.., d] => Ok(*d),
        [] => Err(Error::Invalid("moveto without operands")),
    }
}

fn top2<const N: usize>(stack: &Stack<Fixed, N>) -> Result<(Fixed, Fixed)> {
    match stack.as_slice() {
        [.., dx, dy] => Ok((*dx, *dy)),
        _ => Err(Error::Invalid("rmoveto without operands")),
    }
}

/// The subroutine number bias for an INDEX of `count` (TN 5177 4.7).
fn bias(count: u32) -> i64 {
    if count < 1240 {
        107
    } else if count < 33900 {
        1131
    } else {
        32768
    }
}

/// The two curves of a flex operator (TN 5177 4.1), as relative operands.
fn flex(code: u8, a: &[Fixed]) -> Result<[[Fixed; 6]; 2]> {
    Ok(match (code, a) {
        (34, &[a, b, c, d, e, f, g]) => [[a, 0, b, c, d, 0], [e, 0, f, c.wrapping_neg(), g, 0]],
        (35, &[a, b, c, d, e, f, g, h, i, j, k, l, _]) => [[a, b, c, d, e, f], [g, h, i, j, k, l]],
        (36, &[a, b, c, d, e, f, g, h, i]) => {
            let back = b.wrapping_add(d).wrapping_add(h).wrapping_neg();
            [[a, b, c, d, e, 0], [f, 0, g, h, i, back]]
        }
        (37, &[a, b, c, d, e, f, g, h, i, j, k]) => {
            let sum = |v: [Fixed; 5]| v.into_iter().fold(0, Fixed::wrapping_add);
            let dx = sum([a, c, e, g, i]);
            let dy = sum([b, d, f, h, j]);
            let abs = |v: Fixed| if v < 0 { v.wrapping_neg() } else { v };
            let last = if abs(dx) > abs(dy) {
                [k, dy.wrapping_neg()]
            } else {
                [dx.wrapping_neg(), k]
            };
            [[a, b, c, d, e, f], [g, h, i, j, last[0], last[1]]]
        }
        _ => return Err(Error::Invalid("flex operands")),
    })
}

#[cfg(test)]
mod tests {
    use super::{STANDARD_ENCODING, bias, flex, int, to_int};

    #[test]
    fn bias_flex_numbers_and_standard_codes() {
        assert_eq!(
            [bias(1239), bias(1240), bias(33899), bias(33900)],
            [107, 1131, 1131, 32768]
        );
        let hflex = flex(34, &[1, 2, 3, 4, 5, 6, 7].map(int)).unwrap();
        assert_eq!(
            hflex,
            [[1, 0, 2, 3, 4, 0].map(int), [5, 0, 6, -3, 7, 0].map(int)]
        );
        let flex1 = flex(37, &[10, 1, 10, 1, 10, 1, 10, 1, 10, 1, 9].map(int)).unwrap();
        assert_eq!(flex1[1][4..], [int(9), int(-5)]);
        assert!(flex(35, &[int(1); 12]).is_err());
        assert_eq!(
            [to_int(int(-3)), to_int(0x8000), to_int(-0x8001)],
            [-3, 1, -1]
        );
        // space, A, asciitilde, exclamdown, germandbls.
        assert_eq!(
            [32, 65, 126, 161, 251].map(|c| STANDARD_ENCODING[c]),
            [1, 34, 95, 96, 149]
        );
        assert_eq!(STANDARD_ENCODING[127], 0);
    }
}

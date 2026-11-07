//! Type 2 charstrings (Adobe Technical Note 5177) and CFF2 charstrings, unhinted.
//!
//! Hints are counted, so that mask lengths are known, and otherwise ignored. Drawing
//! operators consume complete operand groups and drop a malformed remainder, and
//! CFF2 charstrings may leave operands on the stack, as FreeType accepts both.

use super::dict::{CFF_OPERANDS, CFF2_OPERANDS, Stack, integer};
use super::{Cff, Private, Version};
use crate::Limits;
use crate::outline::{Outline, PointKind};
use crate::read::{Meter, Reader};
use triergon_core::{Error, Result};

/// TN 5177 Appendix B: nested subroutine calls, stem hints and transient slots.
const MAX_SUBR_DEPTH: u32 = 10;
const MAX_STEM_HINTS: usize = 96;
const TRANSIENT_SLOTS: usize = 32;

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

/// Loads `glyph`'s charstring into `outline`.
pub(super) fn outline(
    cff: &Cff,
    table: &[u8],
    glyph: u32,
    outline: &mut Outline,
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
        outline,
    };
    match cff.version {
        Version::Cff1 => run.glyph::<CFF_OPERANDS>(glyph, (0.0, 0.0), true),
        Version::Cff2 => run.glyph::<CFF2_OPERANDS>(glyph, (0.0, 0.0), true),
    }
}

struct Run<'a> {
    cff: &'a Cff,
    table: &'a [u8],
    meter: &'a mut Meter,
    depth: u32,
    outline: &'a mut Outline,
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
    dx: f64,
    dy: f64,
    base: f64,
    accent: f64,
}

/// The current point, whether a contour is open, and whether a `hintmask` came
/// since the last segment.
struct Pen {
    x: f64,
    y: f64,
    open: bool,
    new_mask: bool,
}

struct State<'p, const N: usize> {
    private: &'p Private,
    stack: Stack<N>,
    transient: [f64; TRANSIENT_SLOTS],
    pen: Pen,
    width_seen: bool,
    stems: usize,
    seac: Option<Seac>,
    vsindex: usize,
    random: u32,
}

impl Run<'_> {
    fn glyph<const N: usize>(&mut self, glyph: u32, origin: (f64, f64), top: bool) -> Result<()> {
        let charstring = self.cff.charstrings.get(self.table, glyph)?;
        let private = self.cff.private(self.table, glyph)?;
        let mut s = State::<N> {
            private,
            stack: Stack::new(),
            transient: [0.0; TRANSIENT_SLOTS],
            pen: Pen {
                x: origin.0,
                y: origin.1,
                open: false,
                new_mask: false,
            },
            width_seen: self.cff.version == Version::Cff2,
            stems: 0,
            seac: None,
            vsindex: private.vsindex,
            random: 0,
        };
        self.run(charstring, 0, &mut s)?;
        self.close(&mut s.pen)?;
        if let Some(seac) = s.seac {
            if !top || self.cff.is_cid_keyed() {
                return Err(Error::Invalid("seac inside seac or in a CID-keyed font"));
            }
            for (code, offset) in [(seac.base, (0.0, 0.0)), (seac.accent, (seac.dx, seac.dy))] {
                let code = integer(code)
                    .ok()
                    .filter(|&c| c < 256)
                    .ok_or(Error::Invalid("seac code"))?;
                let sid = STANDARD_ENCODING[code];
                let component = self
                    .cff
                    .glyph_for_sid(self.table, u16::from(sid))
                    .filter(|_| sid != 0)
                    .ok_or(Error::Invalid("seac component"))?;
                self.glyph::<N>(component, (origin.0 + offset.0, origin.1 + offset.1), false)?;
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
                28 => s.stack.push(f64::from(r.i16()?))?,
                32..=246 => s.stack.push(f64::from(i16::from(byte) - 139))?,
                247..=250 => s.stack.push(f64::from(
                    (i16::from(byte) - 247) * 256 + i16::from(r.u8()?) + 108,
                ))?,
                251..=254 => s.stack.push(f64::from(
                    -(i16::from(byte) - 251) * 256 - i16::from(r.u8()?) - 108,
                ))?,
                255 => s.stack.push(r.fixed()?)?,
                1 | 3 | 18 | 23 => {
                    // hstem, vstem, hstemhm, vstemhm
                    self.width(s, s.stack.len() % 2 == 1);
                    self.stems(s)?;
                }
                19 | 20 => {
                    // hintmask, cntrmask: operands before the first mask are vstems.
                    self.width(s, s.stack.len() % 2 == 1);
                    self.stems(s)?;
                    r.skip(s.stems.div_ceil(8))?;
                    s.pen.new_mask |= byte == 19;
                }
                21 => {
                    self.width(s, s.stack.len() > 2);
                    let (dx, dy) = top2(&s.stack)?;
                    self.move_to(s, dx, dy)?;
                }
                22 | 4 => {
                    self.width(s, s.stack.len() > 1);
                    let d = top1(&s.stack)?;
                    let (dx, dy) = if byte == 22 { (d, 0.0) } else { (0.0, d) };
                    self.move_to(s, dx, dy)?;
                }
                5 => {
                    for pair in s.stack.as_slice().chunks_exact(2) {
                        self.line(&mut s.pen, pair[0], pair[1])?;
                    }
                    s.stack.clear();
                }
                6 | 7 => {
                    let mut horizontal = byte == 6;
                    for i in 0..s.stack.len() {
                        let d = s.stack.as_slice()[i];
                        if horizontal {
                            self.line(&mut s.pen, d, 0.0)?;
                        } else {
                            self.line(&mut s.pen, 0.0, d)?;
                        }
                        horizontal = !horizontal;
                    }
                    s.stack.clear();
                }
                8 => {
                    for i in (0..s.stack.len() / 6).map(|i| 6 * i) {
                        let a = &s.stack.as_slice()[i..i + 6];
                        let d = [a[0], a[1], a[2], a[3], a[4], a[5]];
                        self.curve(&mut s.pen, d)?;
                    }
                    s.stack.clear();
                }
                24 => {
                    // rcurveline: curves, then a line.
                    let n = s.stack.len();
                    if n >= 8 {
                        let curves = (n - 2) / 6;
                        for i in 0..curves {
                            let a = &s.stack.as_slice()[6 * i..6 * i + 6];
                            self.curve(&mut s.pen, [a[0], a[1], a[2], a[3], a[4], a[5]])?;
                        }
                        let a = &s.stack.as_slice()[6 * curves..];
                        let (dx, dy) = (a[0], a[1]);
                        self.line(&mut s.pen, dx, dy)?;
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
                            self.line(&mut s.pen, a[0], a[1])?;
                        }
                        let a = &s.stack.as_slice()[2 * lines..2 * lines + 6];
                        self.curve(&mut s.pen, [a[0], a[1], a[2], a[3], a[4], a[5]])?;
                    }
                    s.stack.clear();
                }
                26 | 27 => {
                    // vvcurveto, hhcurveto: an odd first operand offsets the first curve.
                    let n = s.stack.len();
                    let (mut extra, mut i) = if n % 2 == 1 {
                        (s.stack.as_slice()[0], 1)
                    } else {
                        (0.0, 0)
                    };
                    while i + 4 <= n {
                        let a = &s.stack.as_slice()[i..i + 4];
                        let d = if byte == 26 {
                            [extra, a[0], a[1], a[2], 0.0, a[3]]
                        } else {
                            [a[0], extra, a[1], a[2], a[3], 0.0]
                        };
                        self.curve(&mut s.pen, d)?;
                        extra = 0.0;
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
                            0.0
                        };
                        let d = if horizontal {
                            [a[0], 0.0, a[1], a[2], last, a[3]]
                        } else {
                            [0.0, a[0], a[1], a[2], a[3], last]
                        };
                        self.curve(&mut s.pen, d)?;
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
                    let biased = s.stack.pop()? + bias(subrs.len());
                    let index = integer(biased).ok().and_then(|i| u32::try_from(i).ok());
                    let subr = subrs.get(
                        self.table,
                        index.ok_or(Error::Invalid("charstring subroutine number"))?,
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
                    self.width(s, n == 1 || n == 5);
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
                    s.vsindex = integer(s.stack.pop()?)?;
                    self.cff.region_scalars(s.vsindex)?;
                    s.stack.clear();
                }
                16 if self.cff.version == Version::Cff2 => {
                    let count = integer(s.stack.pop()?)?;
                    s.stack.blend(count, self.cff.region_scalars(s.vsindex)?)?;
                }
                12 => self.escape(r.u8()?, s)?,
                _ => return Err(Error::Invalid("reserved charstring operator")),
            }
        }
        match (self.cff.version, depth) {
            (Version::Cff2, 0) => Ok(Flow::End),
            (Version::Cff2, _) => Ok(Flow::Return),
            // A Type 2 glyph must end with endchar; FreeType ends it at its data's end.
            (Version::Cff1, 0) => Ok(Flow::End),
            (Version::Cff1, _) => Ok(Flow::Return),
        }
    }

    /// Takes the width operand of the first stack-clearing operator (TN 5177 3.1).
    fn width<const N: usize>(&self, s: &mut State<'_, N>, present: bool) {
        if !s.width_seen {
            s.width_seen = true;
            if present {
                s.stack.shift();
            }
        }
    }

    fn stems<const N: usize>(&self, s: &mut State<'_, N>) -> Result<()> {
        s.stems += s.stack.len() / 2;
        s.stack.clear();
        if s.stems > MAX_STEM_HINTS {
            return Err(Error::Invalid("more than 96 stem hints"));
        }
        Ok(())
    }

    fn move_to<const N: usize>(&mut self, s: &mut State<'_, N>, dx: f64, dy: f64) -> Result<()> {
        s.stack.clear();
        self.close(&mut s.pen)?;
        s.pen.x += dx;
        s.pen.y += dy;
        Ok(())
    }

    /// Starts the contour at the current point before its first segment.
    fn begin(&mut self, s: &mut Pen) -> Result<()> {
        if !s.open {
            s.open = true;
            self.outline.push(s.x, s.y, PointKind::On)?;
        }
        Ok(())
    }

    /// A line to the current point adds nothing unless a `hintmask` came since
    /// the last segment, as in FreeType's CFF engine.
    fn line(&mut self, s: &mut Pen, dx: f64, dy: f64) -> Result<()> {
        if dx == 0.0 && dy == 0.0 && !s.new_mask {
            return Ok(());
        }
        s.new_mask = false;
        self.begin(s)?;
        s.x += dx;
        s.y += dy;
        self.outline.push(s.x, s.y, PointKind::On)
    }

    fn curve(&mut self, s: &mut Pen, d: [f64; 6]) -> Result<()> {
        s.new_mask = false;
        self.begin(s)?;
        for (i, kind) in [PointKind::Cubic, PointKind::Cubic, PointKind::On]
            .into_iter()
            .enumerate()
        {
            s.x += d[2 * i];
            s.y += d[2 * i + 1];
            self.outline.push(s.x, s.y, kind)?;
        }
        Ok(())
    }

    /// Ends the open contour, dropping a final on-curve point that repeats its start.
    fn close(&mut self, s: &mut Pen) -> Result<()> {
        if !s.open {
            return Ok(());
        }
        s.open = false;
        let contour = self.outline.open_contour();
        if let (Some(first), Some(last)) = (contour.first(), contour.last())
            && contour.len() > 1
            && last.kind == PointKind::On
            && last.point == first.point
        {
            self.outline.pop();
        }
        self.outline.end_contour()
    }

    // `eq` compares its operands exactly (TN 5177 4.4).
    #[allow(clippy::float_cmp)]
    fn escape<const N: usize>(&mut self, code: u8, s: &mut State<'_, N>) -> Result<()> {
        let stack = &mut s.stack;
        let truth = |b: bool| if b { 1.0 } else { 0.0 };
        match code {
            0 => {} // dotsection: deprecated, a no-op.
            3 | 4 | 10 | 11 | 12 | 15 | 24 => {
                let b = stack.pop()?;
                let a = stack.pop()?;
                let value = match code {
                    3 => truth(a != 0.0 && b != 0.0),
                    4 => truth(a != 0.0 || b != 0.0),
                    10 => a + b,
                    11 => a - b,
                    12 if b != 0.0 => a / b,
                    12 => 0.0,
                    15 => truth(a == b),
                    _ => a * b,
                };
                stack.push(finite(value)?)?;
            }
            5 | 9 | 14 | 26 => {
                let a = stack.pop()?;
                let value = match code {
                    5 => truth(a == 0.0),
                    9 => a.abs(),
                    14 => -a,
                    _ => a.max(0.0).sqrt(),
                };
                stack.push(value)?;
            }
            18 => {
                stack.pop()?;
            }
            20 => {
                let index = integer(stack.pop()?)?;
                let value = stack.pop()?;
                *s.transient
                    .get_mut(index)
                    .ok_or(Error::Invalid("transient array index"))? = value;
            }
            21 => {
                let index = integer(stack.pop()?)?;
                let value = *s
                    .transient
                    .get(index)
                    .ok_or(Error::Invalid("transient array index"))?;
                stack.push(value)?;
            }
            22 => {
                let v2 = stack.pop()?;
                let v1 = stack.pop()?;
                let b = stack.pop()?;
                let a = stack.pop()?;
                stack.push(if v1 <= v2 { a } else { b })?;
            }
            23 => {
                // TN 5177 fixes only the range (0, 1]; a 32-bit LCG generates it.
                s.random = s.random.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                stack.push((f64::from(s.random) + 1.0) / 4_294_967_296.0)?;
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
                let i = stack.pop()?;
                let len = stack.len();
                let index = if i < 0.0 { 0 } else { integer(i.trunc())? };
                let value = *stack
                    .as_slice()
                    .get(
                        len.checked_sub(index + 1)
                            .ok_or(Error::Invalid("charstring index"))?,
                    )
                    .ok_or(Error::Invalid("charstring index"))?;
                stack.push(value)?;
            }
            30 => {
                let shift = stack.pop()?;
                let count = integer(stack.pop()?)?;
                let len = stack.len();
                if count > len || shift.fract() != 0.0 || shift.abs() > f64::from(i32::MAX) {
                    return Err(Error::Invalid("charstring roll"));
                }
                if count > 0 {
                    let mut values = [0.0; CFF2_OPERANDS];
                    let top = &stack.as_slice()[len - count..];
                    values[..count].copy_from_slice(top);
                    let rotation = (shift as i64).rem_euclid(count as i64) as usize;
                    values[..count].rotate_right(rotation);
                    for _ in 0..count {
                        stack.pop()?;
                    }
                    for &v in &values[..count] {
                        stack.push(v)?;
                    }
                }
            }
            34..=37 => {
                let curves = flex(code, stack.as_slice())?;
                s.stack.clear();
                for curve in curves {
                    self.curve(&mut s.pen, curve)?;
                }
            }
            _ => return Err(Error::Invalid("reserved charstring escape")),
        }
        Ok(())
    }
}

fn finite(value: f64) -> Result<f64> {
    if value.is_finite() {
        Ok(value)
    } else {
        Err(Error::Invalid("charstring arithmetic overflow"))
    }
}

fn top1<const N: usize>(stack: &Stack<N>) -> Result<f64> {
    match stack.as_slice() {
        [.., d] => Ok(*d),
        [] => Err(Error::Invalid("moveto without operands")),
    }
}

fn top2<const N: usize>(stack: &Stack<N>) -> Result<(f64, f64)> {
    match stack.as_slice() {
        [.., dx, dy] => Ok((*dx, *dy)),
        _ => Err(Error::Invalid("rmoveto without operands")),
    }
}

/// The subroutine number bias for an INDEX of `count` (TN 5177 4.7).
fn bias(count: u32) -> f64 {
    if count < 1240 {
        107.0
    } else if count < 33900 {
        1131.0
    } else {
        32768.0
    }
}

/// The two curves of a flex operator (TN 5177 4.1).
fn flex(code: u8, a: &[f64]) -> Result<[[f64; 6]; 2]> {
    Ok(match (code, a) {
        (34, &[a, b, c, d, e, f, g]) => [[a, 0.0, b, c, d, 0.0], [e, 0.0, f, -c, g, 0.0]],
        (35, &[a, b, c, d, e, f, g, h, i, j, k, l, _]) => [[a, b, c, d, e, f], [g, h, i, j, k, l]],
        (36, &[a, b, c, d, e, f, g, h, i]) => {
            [[a, b, c, d, e, 0.0], [f, 0.0, g, h, i, -(b + d + h)]]
        }
        (37, &[a, b, c, d, e, f, g, h, i, j, k]) => {
            let dx = a + c + e + g + i;
            let dy = b + d + f + h + j;
            let last = if dx.abs() > dy.abs() {
                [k, -dy]
            } else {
                [-dx, k]
            };
            [[a, b, c, d, e, f], [g, h, i, j, last[0], last[1]]]
        }
        _ => return Err(Error::Invalid("flex operands")),
    })
}

#[cfg(test)]
mod tests {
    use super::{STANDARD_ENCODING, bias, flex};

    #[test]
    fn bias_flex_and_standard_codes() {
        assert_eq!(
            [bias(1239), bias(1240), bias(33899), bias(33900)],
            [107.0, 1131.0, 1131.0, 32768.0]
        );
        let hflex = flex(34, &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0]).unwrap();
        assert_eq!(
            hflex,
            [
                [1.0, 0.0, 2.0, 3.0, 4.0, 0.0],
                [5.0, 0.0, 6.0, -3.0, 7.0, 0.0]
            ]
        );
        let flex1 = flex(
            37,
            &[10.0, 1.0, 10.0, 1.0, 10.0, 1.0, 10.0, 1.0, 10.0, 1.0, 9.0],
        )
        .unwrap();
        assert_eq!(flex1[1][4..], [9.0, -5.0]);
        assert!(flex(35, &[1.0; 12]).is_err());
        // space, A, asciitilde, exclamdown, germandbls.
        assert_eq!(
            [32, 65, 126, 161, 251].map(|c| STANDARD_ENCODING[c]),
            [1, 34, 95, 96, 149]
        );
        assert_eq!(STANDARD_ENCODING[127], 0);
    }
}

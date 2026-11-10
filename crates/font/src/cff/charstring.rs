//! Type 2 charstrings (Adobe Technical Note 5177) and CFF2 charstrings, evaluated
//! in 16.16 fixed point as FreeType's CFF engine evaluates them.
//!
//! Stem hints and masks go to the glyph path, which hints with them at a size
//! and otherwise only needs the mask lengths. As in FreeType, the stems are
//! fixed by the first hint mask or move: later stem operators are ignored, and
//! so is a later mask operator with more than one operand, whose mask bytes then
//! run as charstring. Drawing operators consume complete operand groups and
//! drop a malformed remainder, and CFF2 charstrings may leave operands on the
//! stack, as FreeType accepts both.
//! Arithmetic wraps at 32 bits and rounds as FreeType's does; operands are not
//! typed, so an integer operator accepts any number and rounds it.
//!
//! Bytes decode into [`Token`]s first; each operator family has its own handler.

use super::dict::{CFF_OPERANDS, CFF2_OPERANDS, Stack};
use super::number::shared_integer;
use super::path::{Builder, Contours, Fixed, Path, Scaling};
use super::{Cff, Private, Version};
use crate::Limits;
use crate::fixed::{div_fix, mul_fix, sqrt_fixed};
use crate::outline::Outline;
use crate::postscript::STANDARD_ENCODING;
use crate::read::Reader;
use crate::work::charge;
use triergon_core::{Error, Result, WorkBudget};

/// Nested subroutine calls FreeType's CFF engine allows (`CF2_MAX_SUBR`). TN 5177
/// Appendix B states 10, but real fonts nest deeper and FreeType renders them.
const MAX_SUBR_DEPTH: u32 = 16;
/// TN 5177 Appendix B: transient slots.
const TRANSIENT_SLOTS: usize = 32;
const ONE: Fixed = 0x1_0000;

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

/// A charstring operator (TN 5177 Appendix A; CFF2 adds `vsindex` and `blend`
/// and drops `return` and `endchar`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CharstringOperator {
    HStem,
    VStem,
    HStemHm,
    VStemHm,
    HintMask,
    CntrMask,
    RMoveTo,
    HMoveTo,
    VMoveTo,
    RLineTo,
    HLineTo,
    VLineTo,
    RRCurveTo,
    RCurveLine,
    RLineCurve,
    VVCurveTo,
    HHCurveTo,
    VHCurveTo,
    HVCurveTo,
    CallSubr,
    CallGSubr,
    Return,
    EndChar,
    VsIndex,
    Blend,
    Flex,
    HFlex,
    Flex1,
    HFlex1,
    DotSection,
    And,
    Or,
    Not,
    Abs,
    Add,
    Sub,
    Div,
    Neg,
    Eq,
    Drop,
    Put,
    Get,
    IfElse,
    Random,
    Mul,
    Sqrt,
    Dup,
    Exch,
    Index,
    Roll,
}

impl CharstringOperator {
    /// Every operator, in [`Self::name`] order of the specification's tables.
    pub const ALL: [Self; 50] = [
        Self::HStem,
        Self::VStem,
        Self::HStemHm,
        Self::VStemHm,
        Self::HintMask,
        Self::CntrMask,
        Self::RMoveTo,
        Self::HMoveTo,
        Self::VMoveTo,
        Self::RLineTo,
        Self::HLineTo,
        Self::VLineTo,
        Self::RRCurveTo,
        Self::RCurveLine,
        Self::RLineCurve,
        Self::VVCurveTo,
        Self::HHCurveTo,
        Self::VHCurveTo,
        Self::HVCurveTo,
        Self::CallSubr,
        Self::CallGSubr,
        Self::Return,
        Self::EndChar,
        Self::VsIndex,
        Self::Blend,
        Self::Flex,
        Self::HFlex,
        Self::Flex1,
        Self::HFlex1,
        Self::DotSection,
        Self::And,
        Self::Or,
        Self::Not,
        Self::Abs,
        Self::Add,
        Self::Sub,
        Self::Div,
        Self::Neg,
        Self::Eq,
        Self::Drop,
        Self::Put,
        Self::Get,
        Self::IfElse,
        Self::Random,
        Self::Mul,
        Self::Sqrt,
        Self::Dup,
        Self::Exch,
        Self::Index,
        Self::Roll,
    ];

    /// The operator's name in TN 5177.
    pub const fn name(self) -> &'static str {
        match self {
            Self::HStem => "hstem",
            Self::VStem => "vstem",
            Self::HStemHm => "hstemhm",
            Self::VStemHm => "vstemhm",
            Self::HintMask => "hintmask",
            Self::CntrMask => "cntrmask",
            Self::RMoveTo => "rmoveto",
            Self::HMoveTo => "hmoveto",
            Self::VMoveTo => "vmoveto",
            Self::RLineTo => "rlineto",
            Self::HLineTo => "hlineto",
            Self::VLineTo => "vlineto",
            Self::RRCurveTo => "rrcurveto",
            Self::RCurveLine => "rcurveline",
            Self::RLineCurve => "rlinecurve",
            Self::VVCurveTo => "vvcurveto",
            Self::HHCurveTo => "hhcurveto",
            Self::VHCurveTo => "vhcurveto",
            Self::HVCurveTo => "hvcurveto",
            Self::CallSubr => "callsubr",
            Self::CallGSubr => "callgsubr",
            Self::Return => "return",
            Self::EndChar => "endchar",
            Self::VsIndex => "vsindex",
            Self::Blend => "blend",
            Self::Flex => "flex",
            Self::HFlex => "hflex",
            Self::Flex1 => "flex1",
            Self::HFlex1 => "hflex1",
            Self::DotSection => "dotsection",
            Self::And => "and",
            Self::Or => "or",
            Self::Not => "not",
            Self::Abs => "abs",
            Self::Add => "add",
            Self::Sub => "sub",
            Self::Div => "div",
            Self::Neg => "neg",
            Self::Eq => "eq",
            Self::Drop => "drop",
            Self::Put => "put",
            Self::Get => "get",
            Self::IfElse => "ifelse",
            Self::Random => "random",
            Self::Mul => "mul",
            Self::Sqrt => "sqrt",
            Self::Dup => "dup",
            Self::Exch => "exch",
            Self::Index => "index",
            Self::Roll => "roll",
        }
    }

    const fn bit(self) -> u64 {
        1 << self as u8
    }

    /// The one-byte operator `byte`, or `None` for numbers, the escape byte and
    /// reserved values.
    const fn one_byte(byte: u8, version: Version) -> Option<Self> {
        let cff2 = matches!(version, Version::Cff2);
        Some(match byte {
            1 => Self::HStem,
            3 => Self::VStem,
            4 => Self::VMoveTo,
            5 => Self::RLineTo,
            6 => Self::HLineTo,
            7 => Self::VLineTo,
            8 => Self::RRCurveTo,
            10 => Self::CallSubr,
            11 if !cff2 => Self::Return,
            14 if !cff2 => Self::EndChar,
            15 if cff2 => Self::VsIndex,
            16 if cff2 => Self::Blend,
            18 => Self::HStemHm,
            19 => Self::HintMask,
            20 => Self::CntrMask,
            21 => Self::RMoveTo,
            22 => Self::HMoveTo,
            23 => Self::VStemHm,
            24 => Self::RCurveLine,
            25 => Self::RLineCurve,
            26 => Self::VVCurveTo,
            27 => Self::HHCurveTo,
            29 => Self::CallGSubr,
            30 => Self::VHCurveTo,
            31 => Self::HVCurveTo,
            _ => return None,
        })
    }

    /// The escaped operator `12 code`, or `None` for a reserved code.
    const fn escaped(code: u8) -> Option<Self> {
        Some(match code {
            0 => Self::DotSection,
            3 => Self::And,
            4 => Self::Or,
            5 => Self::Not,
            9 => Self::Abs,
            10 => Self::Add,
            11 => Self::Sub,
            12 => Self::Div,
            14 => Self::Neg,
            15 => Self::Eq,
            18 => Self::Drop,
            20 => Self::Put,
            21 => Self::Get,
            22 => Self::IfElse,
            23 => Self::Random,
            24 => Self::Mul,
            26 => Self::Sqrt,
            27 => Self::Dup,
            28 => Self::Exch,
            29 => Self::Index,
            30 => Self::Roll,
            34 => Self::HFlex,
            35 => Self::Flex,
            36 => Self::HFlex1,
            37 => Self::Flex1,
            _ => return None,
        })
    }
}

/// A set of charstring operators, such as those one glyph's program uses.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OperatorSet(u64);

impl OperatorSet {
    pub const fn contains(self, op: CharstringOperator) -> bool {
        self.0 & op.bit() != 0
    }

    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub fn iter(self) -> impl Iterator<Item = CharstringOperator> {
        CharstringOperator::ALL
            .into_iter()
            .filter(move |&op| self.contains(op))
    }
}

/// What the interpreter reports while it runs: nothing on the outline path, the
/// operators it executes when coverage is asked for, the points it draws when
/// bounds are.
pub(super) trait Observe {
    #[inline(always)]
    fn operator(&mut self, _: CharstringOperator) {}

    /// A move, and the start of each charstring or `seac` component.
    #[inline(always)]
    fn move_to(&mut self) {}

    /// A line or curve from the current point through `points`, in 16.16
    /// charstring coordinates.
    #[inline(always)]
    fn draw(&mut self, _from: (Fixed, Fixed), _points: &[(Fixed, Fixed)]) {}
}

impl Observe for () {}

impl Observe for OperatorSet {
    fn operator(&mut self, op: CharstringOperator) {
        self.0 |= op.bit();
    }
}

/// The box of every point a glyph's program draws, control points included, in
/// 16.16 charstring coordinates before any FontMatrix: the bounds HarfBuzz
/// takes CFF glyph extents from. A subpath's start counts once it draws.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CharstringBounds {
    pub x_min: i32,
    pub y_min: i32,
    pub x_max: i32,
    pub y_max: i32,
}

/// [`CharstringBounds`] as the interpreter runs.
#[derive(Debug, Default)]
pub(super) struct Bounds {
    bounds: Option<CharstringBounds>,
    open: bool,
}

impl Bounds {
    fn add(&mut self, (x, y): (Fixed, Fixed)) {
        let b = self.bounds.get_or_insert(CharstringBounds {
            x_min: x,
            y_min: y,
            x_max: x,
            y_max: y,
        });
        b.x_min = b.x_min.min(x);
        b.y_min = b.y_min.min(y);
        b.x_max = b.x_max.max(x);
        b.y_max = b.y_max.max(y);
    }
}

impl Observe for Bounds {
    fn move_to(&mut self) {
        self.open = false;
    }

    fn draw(&mut self, from: (Fixed, Fixed), points: &[(Fixed, Fixed)]) {
        if !self.open {
            self.open = true;
            self.add(from);
        }
        for &p in points {
            self.add(p);
        }
    }
}

/// One decoded charstring element.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Token {
    Number(Fixed),
    Operator(CharstringOperator),
    /// A reserved escape, which FreeType's CFF2 interpreter skips.
    ReservedEscape,
}

/// The next element of a charstring.
fn token(r: &mut Reader<'_>, version: Version) -> Result<Token> {
    let byte = r.u8()?;
    if byte == 255 {
        return Ok(Token::Number(r.i32()?));
    }
    if let Some(v) = shared_integer(byte, r)? {
        return Ok(Token::Number(int(v)));
    }
    if byte == 12 {
        return Ok(
            CharstringOperator::escaped(r.u8()?).map_or(Token::ReservedEscape, Token::Operator)
        );
    }
    CharstringOperator::one_byte(byte, version)
        .map(Token::Operator)
        .ok_or(Error::Invalid("reserved charstring operator"))
}

/// Loads `glyph`'s charstring into `outline` at `scaling`: whole font units
/// unhinted ([`Scaling::UNHINTED`]), else hinted 26.6 pixels.
pub(super) fn outline(
    cff: &Cff,
    table: &[u8],
    glyph: u32,
    scaling: &Scaling,
    outline: &mut Outline,
    limits: &Limits,
    work: &mut WorkBudget,
) -> Result<()> {
    outline.begin(limits.points);
    let mut run = Run::new(cff, table, work, limits, scaling, Builder::new(outline), ());
    run.top_glyph(glyph)?;
    run.builder.close()
}

/// The operators `glyph`'s program executes, including subroutines and `seac`
/// components, into an outline that is then discarded.
pub(super) fn operators(
    cff: &Cff,
    table: &[u8],
    glyph: u32,
    outline: &mut Outline,
    limits: &Limits,
    work: &mut WorkBudget,
) -> Result<OperatorSet> {
    outline.begin(limits.points);
    let mut run = Run::new(
        cff,
        table,
        work,
        limits,
        &Scaling::UNHINTED,
        Builder::new(outline),
        OperatorSet::default(),
    );
    run.top_glyph(glyph)?;
    Ok(run.observer)
}

/// The bounds of what `glyph`'s program draws, `None` if it draws nothing. No
/// outline is built, so the point limit does not apply.
pub(super) fn bounds(
    cff: &Cff,
    table: &[u8],
    glyph: u32,
    limits: &Limits,
    work: &mut WorkBudget,
) -> Result<Option<CharstringBounds>> {
    let mut run = Run::new(
        cff,
        table,
        work,
        limits,
        &Scaling::UNHINTED,
        (),
        Bounds::default(),
    );
    run.top_glyph(glyph)?;
    Ok(run.observer.bounds)
}

struct Run<'a, C, O> {
    cff: &'a Cff,
    table: &'a [u8],
    work: &'a mut WorkBudget,
    depth: u32,
    scaling: &'a Scaling,
    builder: C,
    observer: O,
}

/// How a charstring or subroutine ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Flow {
    Continue,
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

/// The current point and the path under construction.
struct Pen<'s> {
    x: Fixed,
    y: Fixed,
    path: Path<'s>,
}

struct State<'p, const N: usize> {
    private: &'p Private,
    stack: Stack<Fixed, N>,
    transient: [Fixed; TRANSIENT_SLOTS],
    pen: Pen<'p>,
    width_seen: bool,
    seac: Option<Seac>,
    vsindex: usize,
    random: u32,
}

impl<'a, C: Contours, O: Observe> Run<'a, C, O> {
    fn new(
        cff: &'a Cff,
        table: &'a [u8],
        work: &'a mut WorkBudget,
        limits: &Limits,
        scaling: &'a Scaling,
        builder: C,
        observer: O,
    ) -> Self {
        let depth = match cff.version {
            Version::Cff1 => MAX_SUBR_DEPTH,
            Version::Cff2 => u32::MAX,
        }
        .min(limits.depth);
        Self {
            cff,
            table,
            work,
            depth,
            scaling,
            builder,
            observer,
        }
    }

    fn top_glyph(&mut self, glyph: u32) -> Result<()> {
        match self.cff.version {
            Version::Cff1 => self.glyph::<CFF_OPERANDS>(glyph, (0, 0), true),
            Version::Cff2 => self.glyph::<CFF2_OPERANDS>(glyph, (0, 0), true),
        }
    }

    fn glyph<const N: usize>(
        &mut self,
        glyph: u32,
        origin: (Fixed, Fixed),
        top: bool,
    ) -> Result<()> {
        let charstring = self.cff.charstrings.get(self.table, glyph)?;
        let private = self.cff.private(self.table, glyph)?;
        self.observer.move_to();
        let mut s = State::<N> {
            private,
            stack: Stack::new(),
            transient: [0; TRANSIENT_SLOTS],
            pen: Pen {
                x: origin.0,
                y: origin.1,
                path: Path::new(self.scaling, origin.1),
            },
            width_seen: self.cff.version == Version::Cff2,
            seac: None,
            vsindex: private.vsindex,
            random: 0,
        };
        self.run(charstring, 0, &mut s)?;
        s.pen.path.close(&mut self.builder, self.work)?;
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
                    .glyph_for_sid(sid)
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
            charge(self.work, 1)?;
            let op = match token(&mut r, self.cff.version)? {
                Token::Number(v) => {
                    s.stack.push(v)?;
                    continue;
                }
                Token::Operator(op) => op,
                Token::ReservedEscape if self.cff.version == Version::Cff2 => {
                    s.stack.clear();
                    continue;
                }
                Token::ReservedEscape => return Err(Error::Invalid("reserved charstring escape")),
            };
            self.observer.operator(op);
            match self.operate(op, &mut r, depth, s)? {
                Flow::Continue => {}
                flow => return Ok(flow),
            }
        }
        // A Type 2 glyph must end with endchar; FreeType ends it at its data's end.
        Ok(if depth == 0 { Flow::End } else { Flow::Return })
    }

    fn operate<const N: usize>(
        &mut self,
        op: CharstringOperator,
        r: &mut Reader<'_>,
        depth: u32,
        s: &mut State<'_, N>,
    ) -> Result<Flow> {
        use CharstringOperator as Op;
        match op {
            Op::HStem | Op::VStem | Op::HStemHm | Op::VStemHm => {
                if s.pen.path.takes_stems() {
                    Self::width(s, s.stack.len() % 2 == 1);
                    let horizontal = matches!(op, Op::HStem | Op::HStemHm);
                    s.pen.path.stems(horizontal, s.stack.as_slice());
                }
                s.stack.clear();
            }
            // With the stems fixed, FreeType skips a mask operator that has
            // operands, but not the mask bytes after it.
            Op::HintMask | Op::CntrMask if s.stack.len() > 1 && !s.pen.path.takes_stems() => {
                s.stack.clear();
            }
            Op::HintMask | Op::CntrMask => {
                // Operands before the first mask are vertical stems.
                Self::width(s, s.stack.len() % 2 == 1);
                s.pen.path.stems(false, s.stack.as_slice());
                s.stack.clear();
                if op == Op::HintMask {
                    s.pen.path.hint_mask(r)?;
                } else {
                    s.pen.path.counter_mask(r, self.work)?;
                }
            }
            Op::RMoveTo | Op::HMoveTo | Op::VMoveTo => self.move_to(op, s)?,
            Op::RLineTo | Op::HLineTo | Op::VLineTo => self.lines(op, s)?,
            Op::RRCurveTo
            | Op::RCurveLine
            | Op::RLineCurve
            | Op::VVCurveTo
            | Op::HHCurveTo
            | Op::VHCurveTo
            | Op::HVCurveTo => self.curves(op, s)?,
            Op::Flex | Op::HFlex | Op::Flex1 | Op::HFlex1 => {
                let curves = flex(op, s.stack.as_slice())?;
                s.stack.clear();
                for curve in curves {
                    self.curve_by(&mut s.pen, curve)?;
                }
            }
            Op::CallSubr | Op::CallGSubr => return self.call(op, depth, s),
            Op::Return => return Ok(Flow::Return),
            Op::EndChar => {
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
            Op::VsIndex => {
                s.vsindex = index(s.stack.pop()?)?;
                self.cff.region_scalars(s.vsindex)?;
                s.stack.clear();
            }
            Op::Blend => self.blend(s)?,
            // FreeType's CFF2 interpreter skips the Type 2 arithmetic operators.
            _ if self.cff.version == Version::Cff2 => s.stack.clear(),
            _ => arithmetic(op, s)?,
        }
        Ok(Flow::Continue)
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

    fn move_to<const N: usize>(
        &mut self,
        op: CharstringOperator,
        s: &mut State<'_, N>,
    ) -> Result<()> {
        let (dx, dy) = if op == CharstringOperator::RMoveTo {
            Self::width(s, s.stack.len() > 2);
            top2(&s.stack)?
        } else {
            Self::width(s, s.stack.len() > 1);
            let d = top1(&s.stack)?;
            if op == CharstringOperator::HMoveTo {
                (d, 0)
            } else {
                (0, d)
            }
        };
        s.stack.clear();
        let pen = &mut s.pen;
        pen.x = pen.x.wrapping_add(dx);
        pen.y = pen.y.wrapping_add(dy);
        self.observer.move_to();
        pen.path
            .move_to(&mut self.builder, (pen.x, pen.y), self.work)
    }

    /// `rlineto` takes pairs; `hlineto` and `vlineto` alternate the axis. A
    /// pair missing its second operand fails the glyph, as in FreeType.
    fn lines<const N: usize>(
        &mut self,
        op: CharstringOperator,
        s: &mut State<'_, N>,
    ) -> Result<()> {
        let (args, pen) = (Operands(s.stack.as_slice()), &mut s.pen);
        if op == CharstringOperator::RLineTo {
            for i in (0..args.len()).step_by(2) {
                self.line_by(pen, args.get(i)?, args.get(i + 1)?)?;
            }
        } else {
            let mut horizontal = op == CharstringOperator::HLineTo;
            for &d in args.0 {
                let (dx, dy) = if horizontal { (d, 0) } else { (0, d) };
                self.line_by(pen, dx, dy)?;
                horizontal = !horizontal;
            }
        }
        s.stack.clear();
        Ok(())
    }

    /// The curve operators, reading their operands as FreeType's CFF engine
    /// does also for counts the format does not allow: whole groups from the
    /// bottom, and for the `vv`, `hh`, `vh` and `hv` forms the two bottom
    /// operands skipped when the count has bit 1 set. Reading past the
    /// operands fails the glyph.
    fn curves<const N: usize>(
        &mut self,
        op: CharstringOperator,
        s: &mut State<'_, N>,
    ) -> Result<()> {
        use CharstringOperator as Op;
        let (args, pen) = (Operands(s.stack.as_slice()), &mut s.pen);
        let n = args.len();
        let mut i = 0;
        match op {
            Op::RRCurveTo | Op::RCurveLine => {
                while i + 6 <= n {
                    self.curve_by(pen, args.six(i)?)?;
                    i += 6;
                }
                if op == Op::RCurveLine {
                    self.line_by(pen, args.get(i)?, args.get(i + 1)?)?;
                }
            }
            Op::RLineCurve => {
                while i + 6 < n {
                    self.line_by(pen, args.get(i)?, args.get(i + 1)?)?;
                    i += 2;
                }
                while i < n {
                    self.curve_by(pen, args.six(i)?)?;
                    i += 6;
                }
            }
            Op::VVCurveTo | Op::HHCurveTo => {
                let count = n & !2;
                i = n - count;
                while i < count {
                    // An odd remainder offsets the curve across its axis.
                    let extra = if (count - i) % 2 == 1 {
                        i += 1;
                        args.get(i - 1)?
                    } else {
                        0
                    };
                    let a = args.four(i)?;
                    let d = if op == Op::VVCurveTo {
                        [extra, a[0], a[1], a[2], 0, a[3]]
                    } else {
                        [a[0], extra, a[1], a[2], a[3], 0]
                    };
                    self.curve_by(pen, d)?;
                    i += 4;
                }
            }
            Op::VHCurveTo | Op::HVCurveTo => {
                // Alternating axes; a fifth operand ends the final curve off
                // its axis.
                let count = n & !2;
                i = n - count;
                let mut horizontal = op == Op::HVCurveTo;
                while i < count {
                    let a = args.four(i)?;
                    let last = if count - i == 5 {
                        i += 1;
                        args.get(i + 3)?
                    } else {
                        0
                    };
                    let d = if horizontal {
                        [a[0], 0, a[1], a[2], last, a[3]]
                    } else {
                        [0, a[0], a[1], a[2], a[3], last]
                    };
                    self.curve_by(pen, d)?;
                    horizontal = !horizontal;
                    i += 4;
                }
            }
            _ => {}
        }
        s.stack.clear();
        Ok(())
    }

    fn call<const N: usize>(
        &mut self,
        op: CharstringOperator,
        depth: u32,
        s: &mut State<'_, N>,
    ) -> Result<Flow> {
        let subrs = if op == CharstringOperator::CallSubr {
            &s.private.subrs
        } else {
            &self.cff.global
        };
        let biased = i64::from(to_int(s.stack.pop()?)) + bias(subrs.len());
        let number =
            u32::try_from(biased).map_err(|_| Error::Invalid("charstring subroutine number"))?;
        let subr = subrs.get(self.table, number)?;
        let nested = depth.checked_add(1).ok_or(Error::Overflow)?;
        if nested > self.depth {
            return Err(if nested > MAX_SUBR_DEPTH {
                Error::Invalid("charstring subroutines nest too deeply")
            } else {
                Error::Policy("charstring subroutine depth")
            });
        }
        Ok(match self.run(subr, nested, s)? {
            Flow::End => Flow::End,
            Flow::Continue | Flow::Return => Flow::Continue,
        })
    }

    fn blend<const N: usize>(&mut self, s: &mut State<'_, N>) -> Result<()> {
        let count = index(s.stack.pop()?)?;
        let scalars = self.cff.region_scalars(s.vsindex)?;
        s.stack.blend(count, scalars.len(), |value, deltas| {
            deltas
                .iter()
                .zip(scalars)
                .fold(value, |sum, (&d, &weight)| {
                    sum.wrapping_add(mul_fix(i64::from(d), i64::from(weight)) as Fixed)
                })
        })
    }

    fn line_by(&mut self, pen: &mut Pen<'_>, dx: Fixed, dy: Fixed) -> Result<()> {
        let from = (pen.x, pen.y);
        pen.x = pen.x.wrapping_add(dx);
        pen.y = pen.y.wrapping_add(dy);
        self.observer.draw(from, &[(pen.x, pen.y)]);
        pen.path
            .line_to(&mut self.builder, (pen.x, pen.y), self.work)
    }

    fn curve_by(&mut self, pen: &mut Pen<'_>, d: [Fixed; 6]) -> Result<()> {
        let c1 = (pen.x.wrapping_add(d[0]), pen.y.wrapping_add(d[1]));
        let c2 = (c1.0.wrapping_add(d[2]), c1.1.wrapping_add(d[3]));
        let end = (c2.0.wrapping_add(d[4]), c2.1.wrapping_add(d[5]));
        self.observer.draw((pen.x, pen.y), &[c1, c2, end]);
        (pen.x, pen.y) = end;
        pen.path.curve_to(&mut self.builder, c1, c2, end, self.work)
    }
}

/// The Type 2 arithmetic, storage and stack operators (TN 5177 4.4 and 4.5).
fn arithmetic<const N: usize>(op: CharstringOperator, s: &mut State<'_, N>) -> Result<()> {
    use CharstringOperator as Op;
    let stack = &mut s.stack;
    let saturate = |v: Fixed| if v == Fixed::MIN { Fixed::MAX } else { v };
    match op {
        Op::DotSection => {} // Deprecated, a no-op.
        Op::And | Op::Or | Op::Add | Op::Sub | Op::Div | Op::Eq | Op::Mul => {
            let b = stack.pop()?;
            let a = stack.pop()?;
            stack.push(match op {
                Op::And => truth(a != 0 && b != 0),
                Op::Or => truth(a != 0 || b != 0),
                Op::Add => a.wrapping_add(b),
                Op::Sub => a.wrapping_sub(b),
                Op::Div => div_fix(i64::from(a), i64::from(b)) as Fixed,
                Op::Eq => truth(a == b),
                _ => mul_fix(i64::from(a), i64::from(b)) as Fixed,
            })?;
        }
        Op::Not | Op::Abs | Op::Neg | Op::Sqrt => {
            let a = stack.pop()?;
            stack.push(match op {
                Op::Not => truth(a == 0),
                Op::Abs => saturate(a).abs(),
                Op::Neg => saturate(a.wrapping_neg()),
                _ if a > 0 => sqrt_fixed(a.cast_unsigned()).cast_signed(),
                _ => 0,
            })?;
        }
        Op::Drop => {
            stack.pop()?;
        }
        Op::Put => {
            let slot = to_int(stack.pop()?);
            let value = stack.pop()?;
            if let Some(cell) = usize::try_from(slot)
                .ok()
                .and_then(|i| s.transient.get_mut(i))
            {
                *cell = value;
            }
        }
        Op::Get => {
            let slot = to_int(stack.pop()?);
            if let Some(&value) = usize::try_from(slot).ok().and_then(|i| s.transient.get(i)) {
                stack.push(value)?;
            }
        }
        Op::IfElse => {
            let v2 = stack.pop()?;
            let v1 = stack.pop()?;
            let b = stack.pop()?;
            let a = stack.pop()?;
            stack.push(if v1 <= v2 { a } else { b })?;
        }
        Op::Random => {
            // The range is (0, 1]; FreeType seeds its generator per font.
            s.random = s.random.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            stack.push(((s.random >> 16) & 0xffff).cast_signed() + 1)?;
        }
        Op::Dup => {
            let a = stack.pop()?;
            stack.push(a)?;
            stack.push(a)?;
        }
        Op::Exch => {
            let b = stack.pop()?;
            let a = stack.pop()?;
            stack.push(b)?;
            stack.push(a)?;
        }
        Op::Index => {
            let i = to_int(stack.pop()?);
            if let Some(last) = stack.len().checked_sub(1) {
                // Negative indexes copy the top, ones beyond the stack the bottom.
                let from = usize::try_from(i).map_or(last, |i| last.saturating_sub(i));
                let value = stack.as_slice()[from];
                stack.push(value)?;
            }
        }
        Op::Roll => {
            let shift = to_int(stack.pop()?);
            let count = to_int(stack.pop()?);
            roll(stack, count, shift)?;
        }
        _ => unreachable_operator()?,
    }
    Ok(())
}

/// Only arithmetic operators reach [`arithmetic`].
fn unreachable_operator() -> Result<()> {
    Err(Error::Invalid("charstring operator outside its family"))
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

/// A path operator's operands, bottom first.
#[derive(Clone, Copy)]
struct Operands<'s>(&'s [Fixed]);

impl Operands<'_> {
    const fn len(self) -> usize {
        self.0.len()
    }

    fn get(self, i: usize) -> Result<Fixed> {
        self.0
            .get(i)
            .copied()
            .ok_or(Error::Invalid("CFF path operator without enough operands"))
    }

    fn four(self, i: usize) -> Result<[Fixed; 4]> {
        Ok([
            self.get(i)?,
            self.get(i + 1)?,
            self.get(i + 2)?,
            self.get(i + 3)?,
        ])
    }

    fn six(self, i: usize) -> Result<[Fixed; 6]> {
        let [a, b, c, d] = self.four(i)?;
        Ok([a, b, c, d, self.get(i + 4)?, self.get(i + 5)?])
    }
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
fn flex(op: CharstringOperator, a: &[Fixed]) -> Result<[[Fixed; 6]; 2]> {
    use CharstringOperator as Op;
    Ok(match (op, a) {
        (Op::HFlex, &[a, b, c, d, e, f, g]) => {
            [[a, 0, b, c, d, 0], [e, 0, f, c.wrapping_neg(), g, 0]]
        }
        (Op::Flex, &[a, b, c, d, e, f, g, h, i, j, k, l, _]) => {
            [[a, b, c, d, e, f], [g, h, i, j, k, l]]
        }
        (Op::HFlex1, &[a, b, c, d, e, f, g, h, i]) => {
            let back = b.wrapping_add(d).wrapping_add(h).wrapping_neg();
            [[a, b, c, d, e, 0], [f, 0, g, h, i, back]]
        }
        (Op::Flex1, &[a, b, c, d, e, f, g, h, i, j, k]) => {
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
    use super::{CharstringOperator as Op, bias, flex, int, to_int};
    use crate::postscript::STANDARD_ENCODING;

    #[test]
    fn bias_flex_numbers_and_standard_codes() {
        assert_eq!(
            [bias(1239), bias(1240), bias(33899), bias(33900)],
            [107, 1131, 1131, 32768]
        );
        let hflex = flex(Op::HFlex, &[1, 2, 3, 4, 5, 6, 7].map(int)).unwrap();
        assert_eq!(
            hflex,
            [[1, 0, 2, 3, 4, 0].map(int), [5, 0, 6, -3, 7, 0].map(int)]
        );
        let flex1 = flex(Op::Flex1, &[10, 1, 10, 1, 10, 1, 10, 1, 10, 1, 9].map(int)).unwrap();
        assert_eq!(flex1[1][4..], [int(9), int(-5)]);
        assert!(flex(Op::Flex, &[int(1); 12]).is_err());
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

//! Override tags: libass's lexing of `{...}` blocks into typed tags.
//!
//! A block is read as libass's `ass_parse_tags` reads it: from each backslash,
//! the name runs to the next `(` or backslash; parenthesised arguments are
//! split at commas until a backslash, after which everything up to the next
//! `)` is one argument (the tags of `\t`); a missing `)` ends at the block's
//! end. The text after a plain tag's name is one more argument, after the
//! parenthesised ones. Names match by prefix in libass's order, so `\fsize`
//! is `\fs` with the argument `ize`. Numbers convert from the argument's
//! start and, as in libass, may read on past its end.

use std::ops::Range;

use crate::number::{dtoi32, hex_tag_value, strtod, strtoi32};
use crate::scan::is_blank;
use crate::{Alignment, Bold, ColourSlot, Italic, Rgba, WrapStyle};

/// libass's `MAX_VALID_NARGS` plus one, to detect excess arguments.
const MAX_ARGS: usize = 8;

/// The times of a `\t` in milliseconds from the event start, with the
/// acceleration exponent.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TransformTimes {
    /// The animation starts.
    pub t1: i32,
    /// The animation ends; 0 stands for the event's end.
    pub t2: i32,
    /// The progress is raised to this power.
    pub accel: f64,
}

/// `\move(x1,y1,x2,y2[,t1,t2])`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Move {
    /// Where the event starts.
    pub from: (f64, f64),
    /// Where it ends.
    pub to: (f64, f64),
    /// The movement starts, in milliseconds from the event start; at most
    /// `t2`. Both 0 or negative move over the whole event.
    pub t1: i32,
    /// The movement ends.
    pub t2: i32,
}

/// `\fad` and `\fade`: the event's alpha over time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fade {
    /// `\fad(in,out)`: from transparent over the first `fade_in` milliseconds
    /// and back over the last `fade_out`.
    InOut {
        /// Milliseconds of fading in.
        fade_in: i32,
        /// Milliseconds of fading out.
        fade_out: i32,
    },
    /// `\fade(a1,a2,a3,t1,t2,t3,t4)`: alpha `a1` until `t1`, to `a2` by `t2`,
    /// held until `t3` and to `a3` by `t4`, in milliseconds from the event
    /// start. As in libass, `t1` and `t4` both -1 read `t2` and `t3` as
    /// `\fad` does, with the given alphas.
    Full {
        /// The alpha before `t1`.
        a1: i32,
        /// The alpha from `t2` to `t3`.
        a2: i32,
        /// The alpha after `t4`.
        a3: i32,
        /// Fading to `a2` starts.
        t1: i32,
        /// Fading to `a2` ends.
        t2: i32,
        /// Fading to `a3` starts.
        t3: i32,
        /// Fading to `a3` ends.
        t4: i32,
    },
}

/// The karaoke tags.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Karaoke {
    /// `\k`: the syllable switches to the primary colour at its start.
    Switch,
    /// `\kf`, `\K`: the primary colour sweeps across the syllable.
    Fill,
    /// `\ko`: the outline appears at the syllable's start.
    Outline,
}

/// A vector clip: `\clip` or `\iclip` with a drawing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct VectorClip<'s> {
    /// The drawing commands.
    pub drawing: &'s [u8],
    /// The `\p`-like level; see [`drawing_scale`](crate::drawing_scale).
    pub scale: i32,
    /// `\iclip`: draw outside the drawing.
    pub inverse: bool,
}

/// A rectangular clip in script pixels: `\clip` or `\iclip` with four
/// coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Clip {
    /// The left edge.
    pub x0: i32,
    /// The top edge.
    pub y0: i32,
    /// The right edge.
    pub x1: i32,
    /// The bottom edge.
    pub y1: i32,
    /// `\iclip`: draw outside the rectangle.
    pub inverse: bool,
}

/// A `\fs` argument.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FontSize {
    /// A size in script pixels.
    Absolute(f64),
    /// Written with a sign: the size scaled by `1 + value / 10`.
    Relative(f64),
}

/// One override tag with its arguments as libass interprets them. `None`
/// means the tag had no valid argument, which resets the value to the
/// style's (or a fixed default).
#[derive(Clone, Debug, PartialEq)]
pub enum Tag<'a> {
    /// `\bord`: both border widths, clamped at 0.
    Border(Option<f64>),
    /// `\xbord`, clamped at 0.
    XBorder(Option<f64>),
    /// `\ybord`, clamped at 0.
    YBorder(Option<f64>),
    /// `\shad`: both shadow depths, clamped at 0.
    Shadow(Option<f64>),
    /// `\xshad`, not clamped.
    XShadow(Option<f64>),
    /// `\yshad`, not clamped.
    YShadow(Option<f64>),
    /// `\fax`: horizontal shear; no argument is 0.
    ShearX(Option<f64>),
    /// `\fay`: vertical shear; no argument is 0.
    ShearY(Option<f64>),
    /// `\frx`: rotation about the x axis in degrees; no argument is 0.
    RotateX(Option<f64>),
    /// `\fry`: no argument is 0.
    RotateY(Option<f64>),
    /// `\frz` and `\fr`: no argument is the style angle.
    RotateZ(Option<f64>),
    /// `\blur`, clamped to 0..=100; no argument is 0.
    Blur(Option<f64>),
    /// `\be`, rounded and clamped to 0..=127; no argument is 0.
    BlurEdges(Option<f64>),
    /// `\fscx` in percent.
    ScaleX(Option<f64>),
    /// `\fscy` in percent.
    ScaleY(Option<f64>),
    /// `\fsc`: both scales from the style.
    ScaleReset,
    /// `\fsp`: letter spacing.
    Spacing(Option<f64>),
    /// `\fs`; a size of 0 or less selects the style's.
    FontSize(Option<FontSize>),
    /// `\fn`; `None` also for the argument `0`.
    FontName(Option<&'a [u8]>),
    /// `\b` with 0, 1 or a weight of 100 or more.
    Bold(Option<Bold>),
    /// `\i` with 0 or 1.
    Italic(Option<Italic>),
    /// `\u` with 0 or 1.
    Underline(Option<bool>),
    /// `\s` with 0 or 1.
    StrikeOut(Option<bool>),
    /// `\c` and `\1c` to `\4c`, already byte-swapped to [`Rgba`] order (alpha
    /// bits zero).
    Colour(ColourSlot, Option<Rgba>),
    /// `\1a` to `\4a`, as libass reads the value: any 32-bit integer.
    Alpha(ColourSlot, Option<i32>),
    /// `\alpha`: all four alphas.
    AlphaAll(Option<i32>),
    /// `\an` with 1 to 9 and `\a` with 1 to 11 (VSFilter reads the invalid
    /// `\a4` and `\a8` as `\a5`); only the first alignment tag of an event
    /// applies.
    Alignment(Option<Alignment>),
    /// `\q` with 0 to 3; others select the script's.
    WrapStyle(Option<WrapStyle>),
    /// `\pos(x,y)`.
    Pos(f64, f64),
    /// `\org(x,y)`.
    Origin(f64, f64),
    /// `\move`.
    Move(Move),
    /// `\fad` or `\fade`.
    Fade(Fade),
    /// `\clip(x0,y0,x1,y1)`, or `\iclip` with `inverse`.
    Clip(Clip),
    /// `\clip([scale,]drawing)`, or `\iclip` with `inverse`.
    VectorClip(VectorClip<'a>),
    /// `\t([t1,t2,][accel,]tags)`. `tags` is `None` when libass does not
    /// parse any (no backslash in the arguments, or more than four).
    Transform {
        /// When and how the animation runs.
        times: TransformTimes,
        /// The animated tags.
        tags: Option<Tags<'a>>,
    },
    /// `\r`, `\r<style>`.
    Reset(Option<&'a [u8]>),
    /// `\k`, `\kf` (and `\K`), `\ko` in centiseconds; no argument is 100.
    Karaoke(Karaoke, Option<f64>),
    /// `\kt`: the karaoke time base in centiseconds; no argument is 0.
    KaraokeTime(Option<f64>),
    /// `\p`: drawing mode level, clamped at 0; 0 ends drawing mode.
    Drawing(i32),
    /// `\pbo`: drawing baseline offset.
    DrawingBaseline(f64),
    /// `\fe`: font encoding.
    Encoding(Option<i32>),
}

/// What a tag does with its arguments, as libass interprets them (with
/// whether one of them held a backslash), or `None` when libass ignores it.
type Interpret = for<'a> fn(&Args<'a>, bool) -> Option<Tag<'a>>;

/// How a tag's name ends: a plain tag takes the rest of its name as one more
/// argument (libass's `tag`), a complex one only parenthesised arguments
/// (`complex_tag`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Plain,
    Complex,
}

/// Every name prefix in the order libass tries them, with what the tag
/// does; the first prefix that the written name starts with wins.
const NAMES: [(&[u8], Kind, Interpret); 54] = {
    use ColourSlot::{Back, Outline, Primary, Secondary};
    use Kind::{Complex, Plain};
    [
        (b"xbord", Plain, |a, _| Some(Tag::XBorder(a.first_float()))),
        (b"ybord", Plain, |a, _| Some(Tag::YBorder(a.first_float()))),
        (b"xshad", Plain, |a, _| Some(Tag::XShadow(a.first_float()))),
        (b"yshad", Plain, |a, _| Some(Tag::YShadow(a.first_float()))),
        (b"fax", Plain, |a, _| Some(Tag::ShearX(a.first_float()))),
        (b"fay", Plain, |a, _| Some(Tag::ShearY(a.first_float()))),
        (b"iclip", Complex, |a, _| clip(a, true)),
        (b"blur", Plain, |a, _| Some(Tag::Blur(a.first_float()))),
        (b"fscx", Plain, |a, _| Some(Tag::ScaleX(a.first_float()))),
        (b"fscy", Plain, |a, _| Some(Tag::ScaleY(a.first_float()))),
        (b"fsc", Plain, |_, _| Some(Tag::ScaleReset)),
        (b"fsp", Plain, |a, _| Some(Tag::Spacing(a.first_float()))),
        (b"fs", Plain, |a, _| Some(font_size(a))),
        (b"bord", Plain, |a, _| Some(Tag::Border(a.first_float()))),
        (b"move", Complex, |a, _| movement(a)),
        (b"frx", Plain, |a, _| Some(Tag::RotateX(a.first_float()))),
        (b"fry", Plain, |a, _| Some(Tag::RotateY(a.first_float()))),
        (b"frz", Plain, |a, _| Some(Tag::RotateZ(a.first_float()))),
        (b"fr", Plain, |a, _| Some(Tag::RotateZ(a.first_float()))),
        (b"fn", Plain, |a, _| Some(font_name(a))),
        (b"alpha", Plain, |a, _| Some(Tag::AlphaAll(a.hex()))),
        (b"an", Plain, |a, _| Some(align_numpad(a))),
        (b"a", Plain, |a, _| Some(align_legacy(a))),
        (b"pos", Complex, |a, _| {
            point(a).map(|(x, y)| Tag::Pos(x, y))
        }),
        (b"fade", Complex, |a, _| fade(a)),
        (b"fad", Complex, |a, _| fade(a)),
        (b"org", Complex, |a, _| {
            point(a).map(|(x, y)| Tag::Origin(x, y))
        }),
        (b"t", Complex, transform),
        (b"clip", Complex, |a, _| clip(a, false)),
        (b"c", Plain, |a, _| Some(Tag::Colour(Primary, a.colour()))),
        (b"1c", Plain, |a, _| Some(Tag::Colour(Primary, a.colour()))),
        (b"2c", Plain, |a, _| {
            Some(Tag::Colour(Secondary, a.colour()))
        }),
        (b"3c", Plain, |a, _| Some(Tag::Colour(Outline, a.colour()))),
        (b"4c", Plain, |a, _| Some(Tag::Colour(Back, a.colour()))),
        (b"1a", Plain, |a, _| Some(Tag::Alpha(Primary, a.hex()))),
        (b"2a", Plain, |a, _| Some(Tag::Alpha(Secondary, a.hex()))),
        (b"3a", Plain, |a, _| Some(Tag::Alpha(Outline, a.hex()))),
        (b"4a", Plain, |a, _| Some(Tag::Alpha(Back, a.hex()))),
        (b"r", Plain, |a, _| {
            Some(Tag::Reset((a.len() > 0).then(|| a.bytes(0))))
        }),
        (b"be", Plain, |a, _| Some(Tag::BlurEdges(a.first_float()))),
        (b"b", Plain, |a, _| Some(bold(a))),
        (b"i", Plain, |a, _| Some(Tag::Italic(a.flag().map(Italic)))),
        (b"kt", Plain, |a, _| Some(Tag::KaraokeTime(a.first_float()))),
        (b"kf", Plain, |a, _| {
            Some(Tag::Karaoke(Karaoke::Fill, a.first_float()))
        }),
        (b"K", Plain, |a, _| {
            Some(Tag::Karaoke(Karaoke::Fill, a.first_float()))
        }),
        (b"ko", Plain, |a, _| {
            Some(Tag::Karaoke(Karaoke::Outline, a.first_float()))
        }),
        (b"k", Plain, |a, _| {
            Some(Tag::Karaoke(Karaoke::Switch, a.first_float()))
        }),
        (b"shad", Plain, |a, _| Some(Tag::Shadow(a.first_float()))),
        (b"s", Plain, |a, _| {
            Some(Tag::StrikeOut(a.flag().map(|v| v != 0)))
        }),
        (b"u", Plain, |a, _| {
            Some(Tag::Underline(a.flag().map(|v| v != 0)))
        }),
        (b"pbo", Plain, |a, _| {
            Some(Tag::DrawingBaseline(a.first_float().unwrap_or(0.0)))
        }),
        (b"p", Plain, |a, _| {
            Some(Tag::Drawing(a.first_int().unwrap_or(0).max(0)))
        }),
        (b"q", Plain, |a, _| Some(wrap_style(a))),
        (b"fe", Plain, |a, _| Some(Tag::Encoding(a.first_int()))),
    ]
};

/// The tags of one block, in order. Tags libass ignores (unknown names,
/// `\pos` without two arguments and the like) are skipped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tags<'a> {
    text: &'a [u8],
    at: usize,
    end: usize,
}

/// The arguments of one tag, as ranges of the text, read from the text they
/// were split from.
struct Args<'a> {
    text: &'a [u8],
    ranges: [Range<usize>; MAX_ARGS],
    count: usize,
}

impl<'a> Args<'a> {
    const fn new(text: &'a [u8]) -> Self {
        const EMPTY: Range<usize> = 0..0;
        Self {
            text,
            ranges: [EMPTY; MAX_ARGS],
            count: 0,
        }
    }

    /// libass's `push_arg`: trailing blanks trimmed, empty arguments and
    /// those beyond the eighth dropped.
    fn push(&mut self, start: usize, mut end: usize) {
        if self.count < MAX_ARGS {
            while end > start && is_blank(self.text[end - 1]) {
                end -= 1;
            }
            if end > start {
                self.ranges[self.count] = start..end;
                self.count += 1;
            }
        }
    }

    const fn len(&self) -> usize {
        self.count
    }

    fn bytes(&self, i: usize) -> &'a [u8] {
        &self.text[self.ranges[i].clone()]
    }

    /// The text from argument `i` to the end: libass converts numbers from an
    /// argument's start without stopping at its end.
    fn from(&self, i: usize) -> &'a [u8] {
        &self.text[self.ranges[i].start..]
    }

    /// `argtod`; an absent argument converts as an empty string.
    fn float(&self, i: usize) -> f64 {
        strtod(self.from(i)).0
    }

    /// `argtoi32`.
    fn int(&self, i: usize) -> i32 {
        strtoi32(self.from(i), 10).0
    }

    fn first_float(&self) -> Option<f64> {
        (self.count > 0).then(|| self.float(0))
    }

    fn first_int(&self) -> Option<i32> {
        (self.count > 0).then(|| self.int(0))
    }

    /// The first argument as libass's `parse_alpha_tag` reads it.
    fn hex(&self) -> Option<i32> {
        (self.count > 0).then(|| hex_tag_value(self.from(0)))
    }

    /// The first argument as `parse_color_tag` reads it, byte-swapped.
    fn colour(&self) -> Option<Rgba> {
        self.hex().map(|c| Rgba((c as u32).swap_bytes()))
    }

    /// A 0-or-1 flag; other values and none select the style's.
    fn flag(&self) -> Option<i32> {
        self.first_int().filter(|&v| v == 0 || v == 1)
    }
}

impl<'a> Tags<'a> {
    /// The tags of `text[range]`, which ends at a block's `}`. `text` is the
    /// whole event text, which numbers may read into.
    pub fn new(text: &'a [u8], range: Range<usize>) -> Self {
        let end = range.end.min(text.len());
        Self {
            text,
            at: range.start.min(end),
            end,
        }
    }

    /// Whether the lexer has reached the end of its range.
    pub(crate) const fn is_exhausted(&self) -> bool {
        self.at >= self.end
    }

    fn skip_blanks(&self, mut at: usize) -> usize {
        while at < self.end && is_blank(self.text[at]) {
            at += 1;
        }
        at
    }

    /// Splits parenthesised arguments starting after `(` at `q`; returns the
    /// position after them and whether a backslash argument was seen.
    fn parenthesised(&self, mut q: usize, args: &mut Args<'a>) -> (usize, bool) {
        let (text, end) = (self.text, self.end);
        loop {
            q = self.skip_blanks(q);
            let mut r = q;
            while r < end && !matches!(text[r], b',' | b'\\' | b')') {
                r += 1;
            }
            if r < end && text[r] == b',' {
                args.push(q, r);
                q = r + 1;
                continue;
            }
            let backslash = r < end && text[r] == b'\\';
            if backslash {
                r = text[r..end]
                    .iter()
                    .position(|&c| c == b')')
                    .map_or(end, |i| r + i);
            }
            args.push(q, r);
            return (if r < end { r + 1 } else { r }, backslash);
        }
    }

    /// The next tag libass would act on.
    fn lex(&mut self) -> Option<Tag<'a>> {
        let (text, end) = (self.text, self.end);
        while self.at < end {
            let Some(backslash) = text[self.at..end].iter().position(|&c| c == b'\\') else {
                self.at = end;
                return None;
            };
            let p = self.skip_blanks(self.at + backslash + 1);
            let name_end = text[p..end]
                .iter()
                .position(|&c| c == b'(' || c == b'\\')
                .map_or(end, |i| p + i);
            self.at = name_end;
            if name_end == p {
                continue;
            }
            let mut args = Args::new(text);
            let mut backslash_arg = false;
            if name_end < end && text[name_end] == b'(' {
                (self.at, backslash_arg) = self.parenthesised(name_end + 1, &mut args);
            }
            let name = &text[p..name_end];
            let Some(&(prefix, kind, interpret)) =
                NAMES.iter().find(|(prefix, ..)| name.starts_with(prefix))
            else {
                continue;
            };
            if kind == Kind::Plain {
                args.push(p + prefix.len(), name_end);
            }
            if let Some(tag) = interpret(&args, backslash_arg) {
                return Some(tag);
            }
        }
        None
    }
}

/// `\fs`: a sign before the size makes it relative.
fn font_size<'a>(args: &Args<'a>) -> Tag<'a> {
    Tag::FontSize(args.first_float().map(|v| {
        if matches!(args.text[args.ranges[0].start], b'+' | b'-') {
            FontSize::Relative(v)
        } else {
            FontSize::Absolute(v)
        }
    }))
}

/// `\move` with four or six arguments, the times ordered.
fn movement<'a>(args: &Args<'a>) -> Option<Tag<'a>> {
    let (t1, t2) = match args.len() {
        4 => (0, 0),
        6 => {
            let (t1, t2) = (args.int(4), args.int(5));
            (t1.min(t2), t1.max(t2))
        }
        _ => return None,
    };
    Some(Tag::Move(Move {
        from: (args.float(0), args.float(1)),
        to: (args.float(2), args.float(3)),
        t1,
        t2,
    }))
}

/// `\fn`, leading blanks dropped; `0` is no name.
fn font_name<'a>(args: &Args<'a>) -> Tag<'a> {
    Tag::FontName((args.len() > 0 && args.bytes(0) != b"0").then(|| {
        let family = args.bytes(0);
        let start = family
            .iter()
            .position(|&c| !is_blank(c))
            .unwrap_or(family.len());
        &family[start..]
    }))
}

/// `\an` with 1 to 9.
fn align_numpad<'a>(args: &Args<'a>) -> Tag<'a> {
    Tag::Alignment(
        Some(args.first_int().unwrap_or(0))
            .filter(|v| (1..=9).contains(v))
            .map(Alignment::from_numpad),
    )
}

/// `\a` with 1 to 11, the invalid `\a4` and `\a8` read as `\a5`.
fn align_legacy<'a>(args: &Args<'a>) -> Tag<'a> {
    Tag::Alignment(match args.first_int().unwrap_or(0) {
        v @ 1..=11 if v & 3 == 0 => Some(Alignment::from_bits(5)),
        v @ 1..=11 => Some(Alignment::from_bits(v)),
        _ => None,
    })
}

/// The two coordinates of `\pos` and `\org`.
fn point(args: &Args<'_>) -> Option<(f64, f64)> {
    (args.len() == 2).then(|| (args.float(0), args.float(1)))
}

/// `\fad` with two arguments or `\fade` with seven.
fn fade<'a>(args: &Args<'a>) -> Option<Tag<'a>> {
    Some(Tag::Fade(match args.len() {
        2 => Fade::InOut {
            fade_in: args.int(0),
            fade_out: args.int(1),
        },
        7 => Fade::Full {
            a1: args.int(0),
            a2: args.int(1),
            a3: args.int(2),
            t1: args.int(3),
            t2: args.int(4),
            t3: args.int(5),
            t4: args.int(6),
        },
        _ => return None,
    }))
}

/// `\t`: its times by argument count, and its tags when one argument held a
/// backslash and there are at most four.
fn transform<'a>(args: &Args<'a>, backslash_arg: bool) -> Option<Tag<'a>> {
    let count = args.len().checked_sub(1);
    let times = match count {
        Some(3) => TransformTimes {
            t1: args.int(0),
            t2: args.int(1),
            accel: args.float(2),
        },
        Some(2) => TransformTimes {
            t1: dtoi32(args.float(0)),
            t2: dtoi32(args.float(1)),
            accel: 1.0,
        },
        Some(1) => TransformTimes {
            t1: 0,
            t2: 0,
            accel: args.float(0),
        },
        _ => TransformTimes {
            t1: 0,
            t2: 0,
            accel: 1.0,
        },
    };
    let tags = count
        .filter(|&c| c <= 3 && backslash_arg)
        .map(|c| Tags::new(args.text, args.ranges[c].clone()));
    Some(Tag::Transform { times, tags })
}

/// `\b` with 0, 1 or a weight of 100 or more.
fn bold<'a>(args: &Args<'a>) -> Tag<'a> {
    Tag::Bold(
        args.first_int()
            .filter(|&v| v == 0 || v == 1 || v >= 100)
            .map(Bold),
    )
}

/// `\q` with 0 to 3.
fn wrap_style<'a>(args: &Args<'a>) -> Tag<'a> {
    Tag::WrapStyle(
        args.first_int()
            .filter(|v| (0..=3).contains(v))
            .map(WrapStyle::from_value),
    )
}

/// `\clip` and `\iclip`: four integers, or a drawing with an optional scale
/// before it.
fn clip<'a>(args: &Args<'a>, inverse: bool) -> Option<Tag<'a>> {
    match args.len() {
        4 => Some(Tag::Clip(Clip {
            x0: args.int(0),
            y0: args.int(1),
            x1: args.int(2),
            y1: args.int(3),
            inverse,
        })),
        count @ (1 | 2) => Some(Tag::VectorClip(VectorClip {
            scale: if count == 2 { args.int(0) } else { 1 },
            drawing: args.bytes(count - 1),
            inverse,
        })),
        _ => None,
    }
}

impl<'a> Iterator for Tags<'a> {
    type Item = Tag<'a>;

    fn next(&mut self) -> Option<Tag<'a>> {
        self.lex()
    }
}

#[cfg(test)]
mod tests {
    use super::{FontSize, Tag, Tags};
    use crate::{Alignment, Bold, Clip, ColourSlot, Rgba, VectorClip};

    fn tags(block: &str) -> Vec<Tag<'_>> {
        Tags::new(block.as_bytes(), 0..block.len()).collect()
    }

    #[test]
    fn names_match_by_prefix_in_libass_order() {
        assert_eq!(
            tags(r"\fs20"),
            [Tag::FontSize(Some(FontSize::Absolute(20.0)))]
        );
        assert_eq!(
            tags(r"\fs+2"),
            [Tag::FontSize(Some(FontSize::Relative(2.0)))]
        );
        assert_eq!(
            tags(r"\fsize"),
            [Tag::FontSize(Some(FontSize::Absolute(0.0)))]
        );
        assert_eq!(
            tags(r"\fscx50\fsc"),
            [Tag::ScaleX(Some(50.0)), Tag::ScaleReset]
        );
        assert_eq!(
            tags(r"\frz10\fr5"),
            [Tag::RotateZ(Some(10.0)), Tag::RotateZ(Some(5.0))]
        );
        assert_eq!(tags(r"\b"), [Tag::Bold(None)]);
        assert_eq!(
            tags(r"\b700\b2"),
            [Tag::Bold(Some(Bold(700))), Tag::Bold(None)]
        );
        let centre = Some(Alignment::from_bits(10));
        assert_eq!(tags(r"\xyz\an5"), [Tag::Alignment(centre)]);
        assert_eq!(tags(r"\ an 5"), [Tag::Alignment(centre)]);
        assert_eq!(
            tags(r"\a8"),
            [Tag::Alignment(Some(Alignment::from_bits(5)))]
        );
        assert_eq!(tags(r"\an0"), [Tag::Alignment(None)]);
        assert_eq!(
            tags(r"\pbo2\p1"),
            [Tag::DrawingBaseline(2.0), Tag::Drawing(1)]
        );
    }

    #[test]
    fn parentheses_and_backslash_arguments_split_as_in_libass() {
        assert_eq!(tags(r"\pos(1, 2 )"), [Tag::Pos(1.0, 2.0)]);
        assert!(tags(r"\pos(1)").is_empty());
        // Parenthesised arguments come before the rest of the name.
        assert_eq!(
            tags(r"\fs20(30)"),
            [Tag::FontSize(Some(FontSize::Absolute(30.0)))]
        );
        let t = tags(r"\t(0,100,\fs20\clip(1,2,3,4))\b1");
        let [
            Tag::Transform {
                times,
                tags: Some(inner),
            },
            Tag::Bold(Some(Bold(1))),
        ] = &t[..]
        else {
            panic!("{t:?}");
        };
        assert_eq!((times.t1, times.t2), (0, 100));
        assert_eq!(times.accel, 1.0);
        assert_eq!(
            inner.clone().collect::<Vec<_>>(),
            [
                Tag::FontSize(Some(FontSize::Absolute(20.0))),
                Tag::Clip(Clip {
                    x0: 1,
                    y0: 2,
                    x1: 3,
                    y1: 4,
                    inverse: false
                })
            ]
        );
        assert_eq!(
            tags(r"\clip(2,m 0 0 l 1 1"),
            [Tag::VectorClip(VectorClip {
                scale: 2,
                drawing: b"m 0 0 l 1 1",
                inverse: false
            })]
        );
    }

    #[test]
    fn colours_swap_bytes_and_alphas_skip_their_prefix() {
        assert_eq!(
            tags(r"\c&H0000FF&"),
            [Tag::Colour(ColourSlot::Primary, Some(Rgba(0xff00_0000)))]
        );
        assert_eq!(tags(r"\3c"), [Tag::Colour(ColourSlot::Outline, None)]);
        assert_eq!(
            tags(r"\alpha&H80&\2a&HFF"),
            [
                Tag::AlphaAll(Some(0x80)),
                Tag::Alpha(ColourSlot::Secondary, Some(255))
            ]
        );
        assert_eq!(
            tags(r"\fn Arial \fn0"),
            [Tag::FontName(Some(b"Arial")), Tag::FontName(None)]
        );
        assert_eq!(tags(r"\p-1"), [Tag::Drawing(0)]);
    }
}

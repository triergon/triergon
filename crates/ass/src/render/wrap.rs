//! Breaking an event's text into lines as libass does (`wrap_lines_smart`):
//! a hard break after every `\N`, a soft one before a word that overflows
//! (`wrap_lines_naive`), soft breaks moved back while that evens out the
//! lines (`wrap_lines_rebalance`), the whitespace at the ends of each line
//! trimmed (`trim_whitespace`) and the lines measured (`measure_text`).

use std::ops::Range;

use triergon_core::{Budget, BudgetVec, Result, WorkBudget};

use super::geometry::Scales;
use super::layout::d6_to_double;
use crate::{WrapStyle, charge};

const SPACE: u32 = b' ' as u32;
const NEWLINE: u32 = b'\n' as u32;

/// What wrapping reads of a character: its own glyph's horizontal extent on
/// one unbroken line, 26.6, its ascent and descent, and its border.
#[derive(Clone, Copy, Debug)]
pub(super) struct WrapChar {
    pub(super) symbol: u32,
    pub(super) left: i32,
    pub(super) right: i32,
    pub(super) asc: i32,
    pub(super) desc: i32,
    pub(super) border: [f64; 2],
}

impl WrapChar {
    fn is_space(&self) -> bool {
        self.symbol == SPACE || self.symbol == NEWLINE
    }
}

/// One line.
#[derive(Clone, Debug)]
pub(super) struct Line {
    /// Its characters, in text order.
    pub(super) chars: Range<usize>,
    /// Those left after its leading and trailing whitespace is trimmed
    /// (libass's `skip`); empty, at the line's end, for a line of
    /// whitespace.
    pub(super) shown: Range<usize>,
    /// Its ascent and descent in pixels.
    pub(super) asc: f64,
    pub(super) desc: f64,
}

/// An event's lines, measured.
#[derive(Debug)]
pub(super) struct Lines {
    pub(super) lines: BudgetVec<Line>,
    /// The text's height in pixels.
    pub(super) height: f64,
    /// Border extents in whole pixels, rounded as VSFilter rounds them.
    pub(super) border_top: i32,
    pub(super) border_bottom: i32,
    pub(super) border_x: i32,
}

impl Lines {
    pub(super) fn first(&self) -> &Line {
        &self.lines[0]
    }
}

/// Where a line starts, and whether wrapping chose the place, so that
/// rebalancing may move it.
#[derive(Clone, Copy, Debug)]
struct Start {
    at: usize,
    soft: bool,
}

/// How lines are wrapped and spaced.
#[derive(Clone, Copy, Debug)]
pub(super) struct Wrapping<'a> {
    pub(super) max_width: f64,
    pub(super) style: WrapStyle,
    pub(super) line_spacing: f64,
    pub(super) scales: &'a Scales,
}

/// `wrap_lines_smart` over `chars`, which are not empty: breaks, balanced
/// unless `\q1`, then the lines trimmed and measured.
pub(super) fn wrap(
    chars: &[WrapChar],
    wrapping: &Wrapping<'_>,
    budget: &Budget,
    work: &mut WorkBudget,
) -> Result<Lines> {
    let mut starts = BudgetVec::new(budget);
    break_naively(chars, wrapping, &mut starts)?;
    if wrapping.style != WrapStyle::EndOfLine {
        rebalance(chars, &mut starts, work)?;
    }
    measure(chars, &starts, wrapping, budget)
}

/// The right end of character `e` minus the left end of `s`, in pixels.
fn width(chars: &[WrapChar], s: usize, e: usize) -> f64 {
    d6_to_double(chars[e].right.wrapping_sub(chars[s].left))
}

/// `wrap_lines_naive`: a hard break after every `\N`, a soft one after the
/// last space before a character that overflows.
fn break_naively(
    chars: &[WrapChar],
    wrapping: &Wrapping<'_>,
    starts: &mut BudgetVec<Start>,
) -> Result<()> {
    starts.clear();
    starts
        .try_push(Start { at: 0, soft: false })
        .map_err(|e| e.error)?;
    let mut line = 0;
    let mut last_breakable: Option<usize> = None;
    for (i, c) in chars.iter().enumerate() {
        let break_after = if c.symbol == NEWLINE {
            Some((i, false))
        } else if width(chars, line, i) >= wrapping.max_width
            && c.symbol != SPACE
            && wrapping.style != WrapStyle::Manual
        {
            last_breakable.map(|at| (at, true))
        } else {
            None
        };
        if c.symbol == SPACE {
            last_breakable = Some(i);
        }
        if let Some((at, soft)) = break_after
            && at + 1 < chars.len()
        {
            line = at + 1;
            starts
                .try_push(Start { at: line, soft })
                .map_err(|e| e.error)?;
            last_breakable = None;
        }
    }
    Ok(())
}

/// The last character before `end` that is not a space, or `start`.
fn rewind_spaces(chars: &[WrapChar], start: usize, end: usize) -> usize {
    let mut g = end;
    loop {
        g -= 1;
        if !(g > start && chars[g].symbol == SPACE) {
            return g;
        }
    }
}

/// `wrap_lines_rebalance`: moves soft breaks back by a word while that
/// evens out the two lines around them; each pass over the text is charged,
/// as breaks only move back and a pass may move each by one word.
fn rebalance(chars: &[WrapChar], starts: &mut [Start], work: &mut WorkBudget) -> Result<()> {
    let len = chars.len();
    loop {
        charge(work, len)?;
        let mut changed = false;
        for k in 1..starts.len() {
            let (a, b) = (starts[k - 1].at, starts[k].at);
            let end = starts.get(k + 1).map_or(len, |s| s.at);
            if !starts[k].soft {
                continue;
            }
            let mut w = rewind_spaces(chars, a, b);
            let e1_old = w;
            while w > a && chars[w].symbol != SPACE {
                w -= 1;
            }
            let mut e1 = w;
            while e1 > a && chars[e1].symbol == SPACE {
                e1 -= 1;
            }
            if chars[w].symbol == SPACE {
                w += 1;
            }
            if w == a {
                continue;
            }
            let e2 = rewind_spaces(chars, b, end);
            let l1 = width(chars, a, e1_old);
            let l2 = width(chars, b, e2);
            let l1_new = width(chars, a, e1);
            let l2_new = width(chars, w, e2);
            let diff = |x: f64, y: f64| if x < y { y - x } else { x - y };
            if diff(l1_new, l2_new) < diff(l1, l2) {
                starts[k].at = w;
                changed = true;
            }
        }
        if !changed {
            return Ok(());
        }
    }
}

/// `trim_whitespace` on one line: its leading spaces and `\n`s, and its
/// trailing ones after its first character, are hidden.
fn shown(chars: &[WrapChar], line: Range<usize>) -> Range<usize> {
    let all = &chars[line.clone()];
    let lead = all.iter().take_while(|c| c.is_space()).count();
    if lead == all.len() {
        return line.end..line.end;
    }
    let trail = all[1..].iter().rev().take_while(|c| c.is_space()).count();
    line.start + lead..line.end - trail
}

/// libass's `FFMAX` on doubles: `a > b ? a : b`.
pub(super) fn fmax(a: f64, b: f64) -> f64 {
    if a > b { a } else { b }
}

/// `measure_text`: each line's ascent and descent, the height and the
/// border extents. A line counts what trimming left of it, or, when that is
/// nothing, all of it, at half height if it is only a `\n`.
fn measure(
    chars: &[WrapChar],
    starts: &[Start],
    wrapping: &Wrapping<'_>,
    budget: &Budget,
) -> Result<Lines> {
    let scales = wrapping.scales;
    let mut out = Lines {
        lines: BudgetVec::new(budget),
        height: 0.0,
        border_top: 0,
        border_bottom: 0,
        border_x: 0,
    };
    out.lines.try_reserve_exact(starts.len())?;
    for (k, start) in starts.iter().enumerate() {
        let end = starts.get(k + 1).map_or(chars.len(), |s| s.at);
        let line = start.at..end;
        let shown = shown(chars, line.clone());
        let measured = &chars[if shown.is_empty() {
            line.clone()
        } else {
            shown.clone()
        }];
        let (mut asc, mut desc, mut border_x, mut border_y) = (0, 0, 0.0, 0.0);
        for c in measured {
            asc = c.asc.max(asc);
            desc = c.desc.max(desc);
            border_y = fmax(border_y, c.border[1]);
            border_x = fmax(border_x, c.border[0]);
        }
        let scale = if measured.iter().all(|c| c.symbol == NEWLINE) {
            0.5 / 64.0
        } else {
            1.0 / 64.0
        };
        let (asc, desc) = (scale * f64::from(asc), scale * f64::from(desc));
        out.height += asc + desc;
        out.border_bottom = (scales.border[1] * border_y + 0.5) as i32;
        if k == 0 {
            out.border_top = out.border_bottom;
        }
        out.border_x = out.border_x.max((scales.border[0] * border_x + 0.5) as i32);
        out.lines
            .try_push(Line {
                chars: line,
                shown,
                asc,
                desc,
            })
            .map_err(|e| e.error)?;
    }
    out.height += (starts.len() - 1) as f64 * wrapping.line_spacing;
    Ok(out)
}

//! Breaking an event's text into lines as libass does (`wrap_lines_smart`):
//! a hard break after every `\N`, a soft one before a word that overflows
//! (`wrap_lines_naive`), soft breaks moved back while that evens out the
//! lines (`wrap_lines_rebalance`), the whitespace at the ends of each line
//! trimmed (`trim_whitespace`) and the lines measured (`measure_text`).

use std::ops::Range;

use triergon_core::{Budget, BudgetVec, Result, WorkBudget};

use super::geometry::Scales;
use super::units::{d6_to_double, fmax};
use crate::{WrapStyle, charge};

const SPACE: u32 = b' ' as u32;
const NEWLINE: u32 = b'\n' as u32;

/// What wrapping reads of an event's characters, by index.
pub(super) trait WrapText {
    fn len(&self) -> usize;
    fn symbol(&self, i: usize) -> u32;
    /// The left and right of character `i`'s own glyph at its pen position
    /// on one unbroken line, 26.6.
    fn extent(&self, i: usize) -> (i32, i32);
    /// Its own glyph's ascent and descent, 26.6, and its border.
    fn metrics(&self, i: usize) -> (i32, i32, [f64; 2]);

    fn is_space(&self, i: usize) -> bool {
        matches!(self.symbol(i), SPACE | NEWLINE)
    }

    /// The right end of character `e` minus the left end of `s`, in pixels.
    fn width(&self, s: usize, e: usize) -> f64 {
        d6_to_double(self.extent(e).1.wrapping_sub(self.extent(s).0))
    }

    /// The last character before `end` that is not a space, or `start`.
    fn rewind_spaces(&self, start: usize, end: usize) -> usize {
        let mut g = end;
        loop {
            g -= 1;
            if !(g > start && self.symbol(g) == SPACE) {
                return g;
            }
        }
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

/// An event's lines, measured. Wrapped again for each event, into the same
/// storage.
#[derive(Debug)]
pub(super) struct Lines {
    pub(super) lines: BudgetVec<Line>,
    /// The text's height in pixels.
    pub(super) height: f64,
    /// Border extents in whole pixels, rounded as VSFilter rounds them.
    pub(super) border_top: i32,
    pub(super) border_bottom: i32,
    pub(super) border_x: i32,
    starts: BudgetVec<Start>,
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

impl Lines {
    pub(super) fn new(budget: &Budget) -> Self {
        Self {
            lines: BudgetVec::new(budget),
            height: 0.0,
            border_top: 0,
            border_bottom: 0,
            border_x: 0,
            starts: BudgetVec::new(budget),
        }
    }

    pub(super) fn first(&self) -> &Line {
        &self.lines[0]
    }

    /// `wrap_lines_smart` over `chars`, which are not none: breaks,
    /// balanced unless `\q1`, then the lines trimmed and measured.
    pub(super) fn wrap(
        &mut self,
        chars: &impl WrapText,
        wrapping: &Wrapping<'_>,
        work: &mut WorkBudget,
    ) -> Result<()> {
        self.break_naively(chars, wrapping)?;
        if wrapping.style != WrapStyle::EndOfLine {
            self.rebalance(chars, work)?;
        }
        self.measure(chars, wrapping)
    }

    /// `wrap_lines_naive`: a hard break after every `\N`, a soft one after
    /// the last space before a character that overflows.
    fn break_naively(&mut self, chars: &impl WrapText, wrapping: &Wrapping<'_>) -> Result<()> {
        let starts = &mut self.starts;
        starts.clear();
        starts
            .try_push(Start { at: 0, soft: false })
            .map_err(|e| e.error)?;
        let mut line = 0;
        let mut last_breakable: Option<usize> = None;
        for i in 0..chars.len() {
            let symbol = chars.symbol(i);
            let break_after = if symbol == NEWLINE {
                Some((i, false))
            } else if chars.width(line, i) >= wrapping.max_width
                && symbol != SPACE
                && wrapping.style != WrapStyle::Manual
            {
                last_breakable.map(|at| (at, true))
            } else {
                None
            };
            if symbol == SPACE {
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

    /// `wrap_lines_rebalance`: moves soft breaks back by a word while that
    /// evens out the two lines around them; each pass over the text is
    /// charged, as breaks only move back and a pass may move each by one
    /// word.
    fn rebalance(&mut self, chars: &impl WrapText, work: &mut WorkBudget) -> Result<()> {
        let starts = self.starts.as_mut_slice();
        loop {
            charge(work, chars.len())?;
            let mut changed = false;
            for k in 1..starts.len() {
                let (a, b) = (starts[k - 1].at, starts[k].at);
                let end = starts.get(k + 1).map_or(chars.len(), |s| s.at);
                if !starts[k].soft {
                    continue;
                }
                let mut w = chars.rewind_spaces(a, b);
                let e1_old = w;
                while w > a && chars.symbol(w) != SPACE {
                    w -= 1;
                }
                let mut e1 = w;
                while e1 > a && chars.symbol(e1) == SPACE {
                    e1 -= 1;
                }
                if chars.symbol(w) == SPACE {
                    w += 1;
                }
                if w == a {
                    continue;
                }
                let e2 = chars.rewind_spaces(b, end);
                let l1 = chars.width(a, e1_old);
                let l2 = chars.width(b, e2);
                let l1_new = chars.width(a, e1);
                let l2_new = chars.width(w, e2);
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

    /// `measure_text`: each line's ascent and descent, the height and the
    /// border extents. A line counts what trimming left of it, or, when
    /// that is nothing, all of it, at half height if it is only a `\n`.
    fn measure(&mut self, chars: &impl WrapText, wrapping: &Wrapping<'_>) -> Result<()> {
        let scales = wrapping.scales;
        let starts = self.starts.as_slice();
        self.lines.clear();
        self.lines.try_reserve_exact(starts.len())?;
        (
            self.height,
            self.border_top,
            self.border_bottom,
            self.border_x,
        ) = (0.0, 0, 0, 0);
        for (k, start) in starts.iter().enumerate() {
            let end = starts.get(k + 1).map_or(chars.len(), |s| s.at);
            let line = start.at..end;
            let shown = shown(chars, line.clone());
            let measured = if shown.is_empty() {
                line.clone()
            } else {
                shown.clone()
            };
            let (mut asc, mut desc, mut border_x, mut border_y) = (0, 0, 0.0, 0.0);
            let mut newlines = true;
            for i in measured {
                let (a, d, border) = chars.metrics(i);
                asc = a.max(asc);
                desc = d.max(desc);
                border_y = fmax(border_y, border[1]);
                border_x = fmax(border_x, border[0]);
                newlines &= chars.symbol(i) == NEWLINE;
            }
            let scale = if newlines { 0.5 / 64.0 } else { 1.0 / 64.0 };
            let (asc, desc) = (scale * f64::from(asc), scale * f64::from(desc));
            self.height += asc + desc;
            self.border_bottom = (scales.border[1] * border_y + 0.5) as i32;
            if k == 0 {
                self.border_top = self.border_bottom;
            }
            self.border_x = self
                .border_x
                .max((scales.border[0] * border_x + 0.5) as i32);
            self.lines
                .try_push(Line {
                    chars: line,
                    shown,
                    asc,
                    desc,
                })
                .map_err(|e| e.error)?;
        }
        self.height += (starts.len() - 1) as f64 * wrapping.line_spacing;
        Ok(())
    }
}

/// `trim_whitespace` on one line: its leading spaces and `\n`s, and its
/// trailing ones after its first character, are hidden.
fn shown(chars: &impl WrapText, line: Range<usize>) -> Range<usize> {
    let is_space = |&i: &usize| chars.is_space(i);
    let lead = line.clone().take_while(is_space).count();
    if lead == line.len() {
        return line.end..line.end;
    }
    let trail = (line.start + 1..line.end)
        .rev()
        .take_while(is_space)
        .count();
    line.start + lead..line.end - trail
}

//! Contextual and chaining contextual lookups (GSUB types 5 and 6, GPOS types 7
//! and 8): rules that match a glyph sequence and apply nested lookups to it.

use super::apply::{Apply, Values};
use super::data::Data;
use super::layout::{class, coverage};
use triergon_core::Result;

/// One rule's sequences and its nested lookup records.
struct Rule<'a> {
    backtrack: (usize, Values<'a>),
    /// The input length, including the first glyph, and the values after it.
    input: (usize, Values<'a>),
    lookahead: (usize, Values<'a>),
    records: Data<'a>,
    record_count: usize,
}

impl<'a> Rule<'a> {
    fn apply(&self, ctx: &mut Apply<'a, '_>) -> Result<bool> {
        let Some((match_end, _)) = ctx.match_input(self.input.0, self.input.1)? else {
            return Ok(false);
        };
        if !ctx.match_lookahead(self.lookahead.0, self.lookahead.1, match_end)?
            || !ctx.match_backtrack(self.backtrack.0, self.backtrack.1)?
        {
            return Ok(false);
        }
        let records = self.record_count.min(self.records.0.len() / 4);
        ctx.apply_records(self.input.0, self.records, records, match_end)?;
        Ok(true)
    }
}

/// How a format 1 or 2 rule's values are read.
#[derive(Clone, Copy)]
enum Kind<'a> {
    Glyphs,
    /// Backtrack, input and lookahead class definitions.
    Classes([Data<'a>; 3]),
}

impl<'a> Kind<'a> {
    fn values(self, data: Data<'a>, which: usize) -> Values<'a> {
        match self {
            Self::Glyphs => Values::Glyphs(data),
            Self::Classes(defs) => Values::Classes(data, defs[which]),
        }
    }
}

/// The `count` 16-bit values at `at` in `data`, and the offset after them.
fn array(data: Data<'_>, at: usize, count: usize) -> (Data<'_>, usize) {
    (data.at(at), at + 2 * count)
}

/// The rule set a format 1 or 2 subtable selects for the current glyph, and how
/// its rules' values are read.
fn rule_set(data: Data<'_>, glyph: u32, chained: bool) -> Option<(Kind<'_>, Data<'_>)> {
    let index = coverage(data.offset16(2), glyph)?;
    let (kind, sets_at, set) = match (data.u16(0), chained) {
        (1, _) => (Kind::Glyphs, 4, index as usize),
        (_, false) => {
            let classes = data.offset16(4);
            (
                Kind::Classes([classes; 3]),
                6,
                usize::from(class(classes, glyph)),
            )
        }
        (_, true) => {
            let defs = [data.offset16(4), data.offset16(6), data.offset16(8)];
            (Kind::Classes(defs), 10, usize::from(class(defs[1], glyph)))
        }
    };
    (set < data.count(sets_at, sets_at + 2, 2))
        .then(|| (kind, data.offset16(sets_at + 2 + 2 * set)))
}

/// Applies a contextual lookup subtable.
pub(crate) fn apply_context<'a>(ctx: &mut Apply<'a, '_>, data: Data<'a>) -> Result<bool> {
    let glyph = ctx.buffer.cur().glyph;
    match data.u16(0) {
        1 | 2 => {
            let Some((kind, rules)) = rule_set(data, glyph, false) else {
                return Ok(false);
            };
            for r in 0..rules.count(0, 2, 2) {
                ctx.buffer.charge(1)?;
                let rule = rules.offset16(2 + 2 * r);
                let inputs = usize::from(rule.u16(0));
                let (input, next) = array(rule, 4, inputs.saturating_sub(1));
                let rule = Rule {
                    backtrack: (0, Values::Any),
                    input: (inputs, kind.values(input, 1)),
                    lookahead: (0, Values::Any),
                    records: rule.at(next),
                    record_count: usize::from(rule.u16(2)),
                };
                if rule.apply(ctx)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        3 => {
            let inputs = usize::from(data.u16(2));
            if inputs == 0 || coverage(data.offset16(6), glyph).is_none() {
                return Ok(false);
            }
            let rule = Rule {
                backtrack: (0, Values::Any),
                input: (inputs, Values::Coverages(data.at(8), data)),
                lookahead: (0, Values::Any),
                records: data.at(6 + 2 * inputs),
                record_count: usize::from(data.u16(4)),
            };
            rule.apply(ctx)
        }
        _ => Ok(false),
    }
}

/// Applies a chaining contextual lookup subtable.
pub(crate) fn apply_chain_context<'a>(ctx: &mut Apply<'a, '_>, data: Data<'a>) -> Result<bool> {
    let glyph = ctx.buffer.cur().glyph;
    match data.u16(0) {
        1 | 2 => {
            let Some((kind, rules)) = rule_set(data, glyph, true) else {
                return Ok(false);
            };
            for r in 0..rules.count(0, 2, 2) {
                ctx.buffer.charge(1)?;
                let rule = rules.offset16(2 + 2 * r);
                let backtracks = usize::from(rule.u16(0));
                let (backtrack, at) = array(rule, 2, backtracks);
                let inputs = usize::from(rule.u16(at));
                let (input, at) = array(rule, at + 2, inputs.saturating_sub(1));
                let lookaheads = usize::from(rule.u16(at));
                let (lookahead, at) = array(rule, at + 2, lookaheads);
                let rule = Rule {
                    backtrack: (backtracks, kind.values(backtrack, 0)),
                    input: (inputs, kind.values(input, 1)),
                    lookahead: (lookaheads, kind.values(lookahead, 2)),
                    records: rule.at(at + 2),
                    record_count: usize::from(rule.u16(at)),
                };
                if rule.apply(ctx)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        3 => {
            let backtracks = usize::from(data.u16(2));
            let (backtrack, at) = array(data, 4, backtracks);
            let inputs = usize::from(data.u16(at));
            let input_at = at + 2;
            let (input, at) = array(data, input_at, inputs);
            let lookaheads = usize::from(data.u16(at));
            let (lookahead, at) = array(data, at + 2, lookaheads);
            if inputs == 0 || coverage(data.offset16(input_at), glyph).is_none() {
                return Ok(false);
            }
            let rule = Rule {
                backtrack: (backtracks, Values::Coverages(backtrack, data)),
                input: (inputs, Values::Coverages(input.at(2), data)),
                lookahead: (lookaheads, Values::Coverages(lookahead, data)),
                records: data.at(at + 2),
                record_count: usize::from(data.u16(at)),
            };
            rule.apply(ctx)
        }
        _ => Ok(false),
    }
}

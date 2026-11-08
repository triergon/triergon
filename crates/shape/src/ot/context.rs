//! Contextual and chaining contextual lookups (GSUB types 5 and 6, GPOS types 7
//! and 8): rules that match a glyph sequence and apply nested lookups to it.
//! The rules are read here once, for applying them and for asking whether one
//! would apply ([`super::would_apply`]).

use super::apply::{Apply, Values};
use super::data::Data;
use super::layout::{class, coverage};
use triergon_core::Result;

/// A glyph sequence a rule matches: its length and what each glyph must be.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Sequence<'a> {
    pub(crate) len: usize,
    pub(crate) values: Values<'a>,
}

impl Sequence<'_> {
    const NONE: Self = Self {
        len: 0,
        values: Values::Any,
    };
}

/// How a rule's values are read: glyph ids, classes (backtrack, input and
/// lookahead class definitions), or coverage offsets from the subtable.
#[derive(Clone, Copy, Debug)]
enum Kind<'a> {
    Glyphs,
    Classes([Data<'a>; 3]),
    Coverages(Data<'a>),
}

impl<'a> Kind<'a> {
    /// The values at `data` of sequence `which` (backtrack, input, lookahead).
    fn values(self, which: usize, data: Data<'a>) -> Values<'a> {
        match self {
            Self::Glyphs => Values::Glyphs(data),
            Self::Classes(defs) => Values::Classes(data, defs[which]),
            Self::Coverages(subtable) => Values::Coverages(data, subtable),
        }
    }
}

/// One rule: the glyphs before, at and after the current one, and the nested
/// lookups to apply. What follows the input is read only once the input
/// matches.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Rule<'a> {
    data: Data<'a>,
    kind: Kind<'a>,
    pub(crate) backtrack: Sequence<'a>,
    /// The input, whose length includes the current glyph; its values are
    /// those of the glyphs after it.
    pub(crate) input: Sequence<'a>,
    /// Where the lookahead count is, in a chained rule; in an unchained one,
    /// where the records are.
    after_input: usize,
    chained: bool,
    /// An unchained rule's record count.
    record_count: usize,
}

impl<'a> Rule<'a> {
    /// A chained rule from `at` in `data`: the backtrack count and glyphs, then
    /// the input count and glyphs (`headless`: without the first).
    fn chained(data: Data<'a>, at: usize, kind: Kind<'a>, headless: bool) -> Self {
        let backtracks = data.count(at, at + 2, 2);
        let at = at + 2 + 2 * backtracks;
        let (inputs, stored) = if headless {
            let inputs = data.headless_count(at, at + 2, 2);
            (inputs, inputs.saturating_sub(1))
        } else {
            let inputs = data.count(at, at + 2, 2);
            (inputs, inputs)
        };
        // Stored first glyphs (format 3) are passed over: matching starts after.
        let first = usize::from(!headless && inputs > 0);
        Self {
            data,
            kind,
            backtrack: Sequence {
                len: backtracks,
                values: kind.values(0, data.at(at - 2 * backtracks)),
            },
            input: Sequence {
                len: inputs,
                values: kind.values(1, data.at(at + 2 + 2 * first)),
            },
            after_input: at + 2 + 2 * stored,
            chained: true,
            record_count: 0,
        }
    }

    /// An unchained rule whose input count is at `at`, its record count after
    /// it, and its input glyphs after both (`headless`: without the first).
    fn unchained(data: Data<'a>, at: usize, kind: Kind<'a>, headless: bool) -> Self {
        let values_at = at + 4;
        let (inputs, stored) = if headless {
            let inputs = data.headless_count(at, values_at, 2);
            (inputs, inputs.saturating_sub(1))
        } else {
            let inputs = data.count(at, values_at, 2);
            (inputs, inputs)
        };
        let first = usize::from(!headless && inputs > 0);
        let records_at = values_at + 2 * stored;
        Self {
            data,
            kind,
            backtrack: Sequence::NONE,
            input: Sequence {
                len: inputs,
                values: kind.values(1, data.at(values_at + 2 * first)),
            },
            after_input: records_at,
            chained: false,
            record_count: data.count(at + 2, records_at, 4),
        }
    }

    /// The rule at `data` of a format 1 or 2 subtable, its values read by
    /// `kind`.
    fn read(data: Data<'a>, chained: bool, kind: Kind<'a>) -> Self {
        if chained {
            Self::chained(data, 0, kind, true)
        } else {
            Self::unchained(data, 0, kind, true)
        }
    }

    /// The single rule of a format 3 subtable: coverage offsets from the
    /// subtable for every glyph, the input's first included.
    fn of_coverages(data: Data<'a>, chained: bool) -> Self {
        let kind = Kind::Coverages(data);
        if chained {
            Self::chained(data, 2, kind, false)
        } else {
            Self::unchained(data, 2, kind, false)
        }
    }

    /// The glyphs after the input.
    pub(crate) fn lookahead(&self) -> Sequence<'a> {
        if !self.chained {
            return Sequence::NONE;
        }
        let at = self.after_input;
        Sequence {
            len: self.data.count(at, at + 2, 2),
            values: self.kind.values(2, self.data.at(at + 2)),
        }
    }

    /// The nested lookup records and their number.
    fn records(&self) -> (Data<'a>, usize) {
        if !self.chained {
            return (self.data.at(self.after_input), self.record_count);
        }
        let at = self.after_input + 2 + 2 * self.lookahead().len;
        (self.data.at(at + 2), self.data.count(at, at + 2, 4))
    }

    /// Whether the rule needs no context around its input.
    pub(crate) fn has_no_context(&self) -> bool {
        self.backtrack.len == 0 && self.lookahead().len == 0
    }

    /// Matches the rule at the current glyph and applies its nested lookups.
    fn apply(&self, ctx: &mut Apply<'a, '_>) -> Result<bool> {
        let Some((match_end, _)) = ctx.match_input(self.input.len, self.input.values)? else {
            return Ok(false);
        };
        let lookahead = self.lookahead();
        if !ctx.match_lookahead(lookahead.len, lookahead.values, match_end)?
            || !ctx.match_backtrack(self.backtrack.len, self.backtrack.values)?
        {
            return Ok(false);
        }
        let (records, count) = self.records();
        ctx.apply_records(self.input.len, records, count, match_end)?;
        Ok(true)
    }
}

/// Whether the first glyph must be in a format 2 or 3 subtable's coverage for
/// its rules to be read. Applying checks it; asking whether a rule would apply
/// does not, as in HarfBuzz, where the lookup's digest stands in for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FirstGlyph {
    Covered,
    Unchecked,
}

/// Tries the rules of a contextual subtable that may apply at glyph `first`
/// with `try_rule`, in order, until one is taken: the rules of the set it
/// selects in formats 1 and 2, the one rule of format 3. Whether one was.
pub(crate) fn any_rule<'a>(
    data: Data<'a>,
    chained: bool,
    first: u32,
    check: FirstGlyph,
    mut try_rule: impl FnMut(&Rule<'a>) -> Result<bool>,
) -> Result<bool> {
    let covered =
        |at: usize| check == FirstGlyph::Unchecked || coverage(data.offset16(at), first).is_some();
    let (set, kind) = match data.u16(0) {
        1 => match coverage(data.offset16(2), first) {
            Some(index) => (rule_set(data, 4, index as usize), Kind::Glyphs),
            None => return Ok(false),
        },
        2 if covered(2) => {
            let defs = if chained {
                [data.offset16(4), data.offset16(6), data.offset16(8)]
            } else {
                [data.offset16(4); 3]
            };
            let sets_at = if chained { 10 } else { 6 };
            let index = usize::from(class(defs[1], first));
            (rule_set(data, sets_at, index), Kind::Classes(defs))
        }
        3 => {
            // The input count and the first input coverage.
            let (inputs, first_at) = if chained {
                let at = 4 + 2 * data.count(2, 4, 2);
                (data.count(at, at + 2, 2), at + 2)
            } else {
                (data.count(2, 6, 2), 6)
            };
            if inputs == 0 || !covered(first_at) {
                return Ok(false);
            }
            return try_rule(&Rule::of_coverages(data, chained));
        }
        _ => return Ok(false),
    };
    for r in 0..set.count(0, 2, 2) {
        if try_rule(&Rule::read(set.offset16(2 + 2 * r), chained, kind))? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Rule set `index` of the counted array of offsets at `at`; empty beyond it.
fn rule_set(data: Data<'_>, at: usize, index: usize) -> Data<'_> {
    if index < data.count(at, at + 2, 2) {
        data.offset16(at + 2 + 2 * index)
    } else {
        Data::EMPTY
    }
}

/// Applies a contextual (`chained` false) or chaining contextual subtable:
/// the first of its rules that matches. Each rule tried is charged.
pub(crate) fn apply<'a>(ctx: &mut Apply<'a, '_>, data: Data<'a>, chained: bool) -> Result<bool> {
    let glyph = ctx.buffer.cur().glyph;
    any_rule(data, chained, glyph, FirstGlyph::Covered, |rule| {
        ctx.buffer.charge(1)?;
        rule.apply(ctx)
    })
}

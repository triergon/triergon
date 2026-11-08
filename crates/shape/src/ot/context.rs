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

impl<'a> Sequence<'a> {
    /// The `count` 16-bit values at `at` in `data`, read as `values` reads
    /// them, and the offset after them.
    fn read(
        data: Data<'a>,
        at: usize,
        count: usize,
        values: impl Fn(Data<'a>) -> Values<'a>,
    ) -> (Self, usize) {
        let sequence = Self {
            len: count,
            values: values(data.at(at)),
        };
        (sequence, at + 2 * count)
    }

    const NONE: Self = Self {
        len: 0,
        values: Values::Any,
    };
}

/// One rule: the glyphs before, at and after the current one, and the nested
/// lookups to apply.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Rule<'a> {
    pub(crate) backtrack: Sequence<'a>,
    /// The input, whose length includes the current glyph; its values are
    /// those of the glyphs after it.
    pub(crate) input: Sequence<'a>,
    pub(crate) lookahead: Sequence<'a>,
    /// The nested lookup records: a sequence index and a lookup index each.
    records: Data<'a>,
    record_count: usize,
}

impl<'a> Rule<'a> {
    /// Whether the rule needs no context around its input.
    pub(crate) const fn has_no_context(&self) -> bool {
        self.backtrack.len == 0 && self.lookahead.len == 0
    }

    /// The rule at `data`: a format 1 or 2 rule (counts before each array) of
    /// a chained subtable or not, its values read by `kind`.
    fn read(data: Data<'a>, chained: bool, kind: Kind<'a>) -> Self {
        if !chained {
            let inputs = data.headless_count(0, 4, 2);
            let records = data.count(2, 4 + 2 * inputs.saturating_sub(1), 4);
            let (input, next) = Sequence::read(data, 4, inputs.saturating_sub(1), kind.values(1));
            return Self {
                backtrack: Sequence::NONE,
                input: Sequence {
                    len: inputs,
                    ..input
                },
                lookahead: Sequence::NONE,
                records: data.at(next),
                record_count: records,
            };
        }
        let (backtrack, at) = Sequence::read(data, 2, data.count(0, 2, 2), kind.values(0));
        let inputs = data.headless_count(at, at + 2, 2);
        let (input, at) = Sequence::read(data, at + 2, inputs.saturating_sub(1), kind.values(1));
        let (lookahead, at) =
            Sequence::read(data, at + 2, data.count(at, at + 2, 2), kind.values(2));
        Self {
            backtrack,
            input: Sequence {
                len: inputs,
                ..input
            },
            lookahead,
            records: data.at(at + 2),
            record_count: data.count(at, at + 2, 4),
        }
    }

    /// The single rule of a format 3 subtable: coverage offsets from the
    /// subtable for every glyph, the input's first included.
    fn of_coverages(data: Data<'a>, chained: bool) -> Self {
        let coverages = |offsets: Data<'a>| Values::Coverages(offsets, data);
        if !chained {
            let inputs = data.count(2, 6, 2);
            return Self {
                backtrack: Sequence::NONE,
                input: Sequence {
                    len: inputs,
                    values: coverages(data.at(8)),
                },
                lookahead: Sequence::NONE,
                records: data.at(6 + 2 * inputs),
                record_count: data.count(4, 6 + 2 * inputs, 4),
            };
        }
        let (backtrack, at) = Sequence::read(data, 4, data.count(2, 4, 2), coverages);
        let inputs = data.count(at, at + 2, 2);
        // The input's values are those after its first glyph.
        let (input, _) = Sequence::read(data, at + 4, inputs.saturating_sub(1), coverages);
        let at = at + 2 + 2 * inputs;
        let (lookahead, at) = Sequence::read(data, at + 2, data.count(at, at + 2, 2), coverages);
        Self {
            backtrack,
            input: Sequence {
                len: inputs,
                ..input
            },
            lookahead,
            records: data.at(at + 2),
            record_count: data.count(at, at + 2, 4),
        }
    }

    /// Matches the rule at the current glyph and applies its nested lookups.
    fn apply(&self, ctx: &mut Apply<'a, '_>) -> Result<bool> {
        let Some((match_end, _)) = ctx.match_input(self.input.len, self.input.values)? else {
            return Ok(false);
        };
        if !ctx.match_lookahead(self.lookahead.len, self.lookahead.values, match_end)?
            || !ctx.match_backtrack(self.backtrack.len, self.backtrack.values)?
        {
            return Ok(false);
        }
        ctx.apply_records(self.input.len, self.records, self.record_count, match_end)?;
        Ok(true)
    }
}

/// How a format 1 or 2 rule's values are read.
#[derive(Clone, Copy, Debug)]
enum Kind<'a> {
    Glyphs,
    /// Backtrack, input and lookahead class definitions.
    Classes([Data<'a>; 3]),
}

impl<'a> Kind<'a> {
    /// How the values of sequence `which` (backtrack, input, lookahead) read.
    fn values(self, which: usize) -> impl Fn(Data<'a>) -> Values<'a> {
        move |data| match self {
            Self::Glyphs => Values::Glyphs(data),
            Self::Classes(defs) => Values::Classes(data, defs[which]),
        }
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

/// The rules of a contextual subtable that may apply at glyph `first`, in
/// order: the rules of the set it selects in formats 1 and 2, the one rule of
/// format 3.
pub(crate) fn rules(
    data: Data<'_>,
    chained: bool,
    first: u32,
    check: FirstGlyph,
) -> impl Iterator<Item = Rule<'_>> {
    let covered =
        |at: usize| check == FirstGlyph::Unchecked || coverage(data.offset16(at), first).is_some();
    let (set, kind, single) = match data.u16(0) {
        1 => match coverage(data.offset16(2), first) {
            Some(index) => (rule_set(data, 4, index as usize), Kind::Glyphs, None),
            None => (Data::EMPTY, Kind::Glyphs, None),
        },
        2 if covered(2) => {
            let defs = if chained {
                [data.offset16(4), data.offset16(6), data.offset16(8)]
            } else {
                [data.offset16(4); 3]
            };
            let sets_at = if chained { 10 } else { 6 };
            let index = usize::from(class(defs[1], first));
            (rule_set(data, sets_at, index), Kind::Classes(defs), None)
        }
        3 => {
            let rule = Rule::of_coverages(data, chained);
            let first_at = if chained {
                4 + 2 * rule.backtrack.len + 2
            } else {
                6
            };
            let single = (rule.input.len > 0 && covered(first_at)).then_some(rule);
            (Data::EMPTY, Kind::Glyphs, single)
        }
        _ => (Data::EMPTY, Kind::Glyphs, None),
    };
    let set_rules =
        (0..set.count(0, 2, 2)).map(move |r| Rule::read(set.offset16(2 + 2 * r), chained, kind));
    set_rules.chain(single)
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
    for rule in rules(data, chained, glyph, FirstGlyph::Covered) {
        ctx.buffer.charge(1)?;
        if rule.apply(ctx)? {
            return Ok(true);
        }
    }
    Ok(false)
}

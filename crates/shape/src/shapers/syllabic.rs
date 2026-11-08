//! What the syllabic shapers (Indic, Khmer, Myanmar and the Universal Shaping
//! Engine) share: splitting text into syllables by a grammar over character
//! categories, walking the syllables, dotted circles for broken syllables and
//! for vowel sequences that look like another vowel.
//!
//! A grammar is a list of rules, each a regular expression over categories. It
//! compiles at build time into a position automaton (Glushkov's construction):
//! one state per category class in the rules, with the states that may follow
//! each. Scanning keeps the set of live states as a bit set, so it needs no
//! allocation and no backtracking within a syllable. As HarfBuzz's Ragel
//! scanners do, each syllable is the longest match of any rule, the earliest
//! rule winning among equally long ones.

use crate::buffer::{Buffer, Info, UnicodeProps};
use crate::metrics::Scaled;
use crate::tag::Script;
use crate::unicode::vowel_constraints;
use std::ops::Range;
use triergon_core::Result;

/// A regular expression over the categories a shaper stores in
/// [`Info::shaper`]`[0]`.
#[derive(Clone, Copy, Debug)]
pub(super) enum Expr {
    /// One glyph whose category bit is set in the mask.
    Class(u64),
    Seq(&'static [Expr]),
    Alt(&'static [Expr]),
    Opt(&'static Expr),
    Star(&'static Expr),
    Plus(&'static Expr),
}

/// The class of the categories with these discriminants.
pub(super) const fn class(categories: &[u8]) -> Expr {
    let mut mask = 0;
    let mut i = 0;
    while i < categories.len() {
        assert!(categories[i] < 64);
        mask |= 1 << categories[i];
        i += 1;
    }
    Expr::Class(mask)
}

/// Any one glyph.
pub(super) const ANY: Expr = Expr::Class(u64::MAX);

/// A set of automaton states.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct States([u64; 4]);

impl States {
    const EMPTY: Self = Self([0; 4]);
    const CAPACITY: usize = 256;

    const fn with(mut self, state: usize) -> Self {
        self.0[state / 64] |= 1 << (state % 64);
        self
    }

    const fn union(mut self, other: Self) -> Self {
        let mut i = 0;
        while i < 4 {
            self.0[i] |= other.0[i];
            i += 1;
        }
        self
    }

    fn intersection(self, other: Self) -> Self {
        Self(std::array::from_fn(|i| self.0[i] & other.0[i]))
    }

    fn is_empty(self) -> bool {
        self.0 == [0; 4]
    }

    fn first(self) -> Option<usize> {
        let word = self.0.iter().position(|&w| w != 0)?;
        Some(word * 64 + self.0[word].trailing_zeros() as usize)
    }

    fn iter(self) -> impl Iterator<Item = usize> {
        (0..4).flat_map(move |word| {
            let mut bits = self.0[word];
            std::iter::from_fn(move || {
                (bits != 0).then(|| {
                    let bit = bits.trailing_zeros() as usize;
                    bits &= bits - 1;
                    word * 64 + bit
                })
            })
        })
    }
}

/// What the construction knows of a subexpression.
#[derive(Clone, Copy)]
struct Part {
    nullable: bool,
    first: States,
    last: States,
}

/// A compiled grammar: the scanner of one shaper.
#[derive(Debug)]
pub(super) struct Machine {
    /// The syllable type of the rule each state belongs to; states are
    /// numbered in rule order.
    kind: [u8; States::CAPACITY],
    /// The states that consume each category.
    by_category: [States; 64],
    /// The states a match may start with.
    first: States,
    /// The states that may follow each state.
    follow: [States; States::CAPACITY],
    /// The states a match of their rule may end with.
    last: States,
}

impl Machine {
    /// Compiles `rules`, each with the syllable type (below 16) it finds; a
    /// const evaluation error if they need more than 256 states.
    pub(super) const fn new(rules: &[(Expr, u8)]) -> Self {
        let mut machine = Self {
            kind: [0; States::CAPACITY],
            by_category: [States::EMPTY; 64],
            first: States::EMPTY,
            follow: [States::EMPTY; States::CAPACITY],
            last: States::EMPTY,
        };
        let mut next = 0;
        let mut r = 0;
        while r < rules.len() {
            let (expr, kind) = &rules[r];
            assert!(*kind < 16);
            let start = next;
            let part = machine.compile(expr, &mut next);
            let mut state = start;
            while state < next {
                machine.kind[state] = *kind;
                state += 1;
            }
            machine.first = machine.first.union(part.first);
            machine.last = machine.last.union(part.last);
            r += 1;
        }
        machine
    }

    /// Adds the states of `expr` from `*next` on and links them.
    const fn compile(&mut self, expr: &Expr, next: &mut usize) -> Part {
        match *expr {
            Expr::Class(mask) => {
                let state = *next;
                assert!(state < States::CAPACITY);
                *next += 1;
                let mut c = 0;
                while c < 64 {
                    if mask & (1 << c) != 0 {
                        self.by_category[c] = self.by_category[c].with(state);
                    }
                    c += 1;
                }
                let only = States::EMPTY.with(state);
                Part {
                    nullable: false,
                    first: only,
                    last: only,
                }
            }
            Expr::Seq(items) => {
                let mut part = Part {
                    nullable: true,
                    first: States::EMPTY,
                    last: States::EMPTY,
                };
                let mut i = 0;
                while i < items.len() {
                    let item = self.compile(&items[i], next);
                    self.link(part.last, item.first);
                    if part.nullable {
                        part.first = part.first.union(item.first);
                    }
                    part.last = if item.nullable {
                        part.last.union(item.last)
                    } else {
                        item.last
                    };
                    part.nullable &= item.nullable;
                    i += 1;
                }
                part
            }
            Expr::Alt(items) => {
                let mut part = Part {
                    nullable: false,
                    first: States::EMPTY,
                    last: States::EMPTY,
                };
                let mut i = 0;
                while i < items.len() {
                    let item = self.compile(&items[i], next);
                    part.nullable |= item.nullable;
                    part.first = part.first.union(item.first);
                    part.last = part.last.union(item.last);
                    i += 1;
                }
                part
            }
            Expr::Opt(item) => {
                let mut part = self.compile(item, next);
                part.nullable = true;
                part
            }
            Expr::Star(item) | Expr::Plus(item) => {
                let mut part = self.compile(item, next);
                self.link(part.last, part.first);
                if matches!(expr, Expr::Star(_)) {
                    part.nullable = true;
                }
                part
            }
        }
    }

    /// Lets every state of `from` be followed by every state of `to`.
    const fn link(&mut self, from: States, to: States) {
        let mut word = 0;
        while word < from.0.len() {
            let mut bits = from.0[word];
            while bits != 0 {
                let state = word * 64 + bits.trailing_zeros() as usize;
                self.follow[state] = self.follow[state].union(to);
                bits &= bits - 1;
            }
            word += 1;
        }
    }

    /// The longest match at the start of `categories`: its length (at least one
    /// category) and syllable type, or `None` if no rule matches; and the
    /// categories read.
    pub(super) fn longest(
        &self,
        categories: impl Iterator<Item = u8>,
    ) -> (Option<(usize, u8)>, usize) {
        let mut candidates = self.first;
        let mut best = None;
        let mut read = 0;
        for category in categories {
            read += 1;
            let consumed = self
                .by_category
                .get(usize::from(category))
                .map_or(States::EMPTY, |&states| candidates.intersection(states));
            if consumed.is_empty() {
                break;
            }
            if let Some(state) = consumed.intersection(self.last).first() {
                best = Some((read, self.kind[state]));
            }
            candidates = consumed
                .iter()
                .fold(States::EMPTY, |acc, state| acc.union(self.follow[state]));
        }
        (best, read)
    }
}

/// HarfBuzz walks syllables in pieces of at most this many glyphs.
const MAX_SYLLABLE_LENGTH: usize = 64;

/// The syllable type in the low four bits of [`Info::syllable`]; a serial
/// number that tells neighbouring syllables apart is in the high four.
pub(super) fn syllable_type(info: &Info) -> u8 {
    info.syllable & 0x0F
}

/// Gives every glyph its syllable: the glyphs from where a match starts to
/// where the next starts get the matched rule's syllable type (a glyph no rule
/// matches gets type 0) and a serial number counting from 1 to 15 and around.
/// Glyphs `skipped` passes over join the syllable before them (and keep their
/// value before the first).
pub(super) fn find_syllables(
    machine: &Machine,
    buffer: &mut Buffer,
    skipped: impl Fn(&[Info], usize) -> bool,
) -> Result<()> {
    fn visible<'a>(
        infos: &'a [Info],
        from: usize,
        skipped: &'a impl Fn(&[Info], usize) -> bool,
    ) -> impl Iterator<Item = usize> + 'a {
        (from..infos.len()).filter(move |&i| !skipped(infos, i))
    }
    let len = buffer.len();
    let Some(mut start) = visible(buffer.infos(), 0, &skipped).next() else {
        return Ok(());
    };
    let mut serial = 1;
    while start < len {
        let infos = buffer.infos();
        let categories = visible(infos, start, &skipped).map(|i| infos[i].shaper[0]);
        let (found, read) = machine.longest(categories);
        let (length, kind) = found.unwrap_or((1, 0));
        let end = visible(infos, start, &skipped).nth(length).unwrap_or(len);
        buffer.charge(read)?;
        let syllable = (serial << 4) | kind;
        for info in &mut buffer.infos_mut()[start..end] {
            info.syllable = syllable;
        }
        serial = if serial == 15 { 1 } else { serial + 1 };
        start = end;
    }
    Ok(())
}

/// Calls `f` for each syllable as HarfBuzz walks them: runs of glyphs with the
/// same syllable value, of at most [`MAX_SYLLABLE_LENGTH`]. `f` may reorder
/// glyphs within the syllable but not change their number.
pub(super) fn for_each_syllable(
    buffer: &mut Buffer,
    mut f: impl FnMut(&mut Buffer, Range<usize>) -> Result<()>,
) -> Result<()> {
    let mut start = 0;
    while let Some(range) = next_syllable(buffer, start) {
        start = range.end;
        f(buffer, range)?;
    }
    Ok(())
}

/// The syllable starting at `start`, if any.
fn next_syllable(buffer: &Buffer, start: usize) -> Option<Range<usize>> {
    let infos = buffer.infos();
    if start >= infos.len() {
        return None;
    }
    let limit = infos.len().min(start + MAX_SYLLABLE_LENGTH);
    let syllable = infos[start].syllable;
    let end = (start + 1..limit)
        .find(|&i| infos[i].syllable != syllable)
        .unwrap_or(limit);
    Some(start..end)
}

/// How a dotted circle is classified when it is inserted into a broken
/// syllable.
#[derive(Clone, Copy, Debug)]
pub(super) struct DottedCircle {
    /// The syllable type of broken syllables.
    pub(super) broken: u8,
    /// The shaper values of the dotted circle.
    pub(super) shaper: [u8; 2],
    /// A category the dotted circle goes after, at the start of the syllable.
    pub(super) after: Option<u8>,
}

/// Puts a dotted circle at the start of every broken syllable, after any
/// glyphs of category `after`, for the marks to attach to; nothing if the font
/// has no dotted circle. The dotted circle takes the cluster, mask and syllable
/// of the glyph it precedes and no other properties.
pub(super) fn insert_dotted_circles(
    buffer: &mut Buffer,
    font: &Scaled<'_>,
    circle: DottedCircle,
) -> Result<()> {
    if !buffer
        .infos()
        .iter()
        .any(|info| syllable_type(info) == circle.broken)
    {
        return Ok(());
    }
    let Some(glyph) = font.glyph(DOTTED_CIRCLE) else {
        return Ok(());
    };
    buffer.clear_output();
    let mut last_syllable = 0;
    while buffer.idx < buffer.len() {
        buffer.charge(1)?;
        let cur = *buffer.cur();
        if cur.syllable == last_syllable || syllable_type(&cur) != circle.broken {
            buffer.next_glyph()?;
            continue;
        }
        last_syllable = cur.syllable;
        if let Some(after) = circle.after {
            while buffer.idx < buffer.len()
                && buffer.cur().syllable == last_syllable
                && buffer.cur().shaper[0] == after
            {
                buffer.next_glyph()?;
            }
        }
        buffer.output_glyph(glyph)?;
        let out = buffer.out_len - 1;
        *buffer.out_info_mut(out) = Info {
            glyph,
            cluster: cur.cluster,
            mask: cur.mask,
            syllable: cur.syllable,
            shaper: circle.shaper,
            ..Info::default()
        };
    }
    buffer.sync()
}

const DOTTED_CIRCLE: u32 = 0x25CC;

/// Puts a dotted circle before the last character of each vowel sequence of
/// the script that looks like another vowel, as HarfBuzz's Indic and USE
/// shapers do before normalisation. The circle is a copy of that character
/// that does not continue the grapheme.
pub(super) fn insert_vowel_constraint_circles(script: Script, buffer: &mut Buffer) -> Result<()> {
    let sequences = vowel_constraints(script);
    if sequences.is_empty() {
        return Ok(());
    }
    buffer.clear_output();
    while buffer.idx + 1 < buffer.len() {
        buffer.charge(1)?;
        let rest = &buffer.infos()[buffer.idx..];
        let matched = sequences.iter().find(|sequence| {
            sequence.len() <= rest.len()
                && sequence.iter().zip(rest).all(|(&c, info)| info.glyph == c)
        });
        let Some(sequence) = matched else {
            buffer.next_glyph()?;
            continue;
        };
        buffer.next_glyphs(sequence.len() - 1)?;
        buffer.output_glyph(DOTTED_CIRCLE)?;
        let out = buffer.out_len - 1;
        buffer
            .out_info_mut(out)
            .unicode
            .set(UnicodeProps::CONTINUATION, false);
        buffer.next_glyph()?;
    }
    buffer.sync()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shapers::testing::{TestFont, contents, scaled, text_buffer};
    use triergon_core::Budget;

    const CONSONANT: u8 = 0;
    const BROKEN: u8 = 1;
    const OTHER: u8 = 2;
    const REPHA: u8 = 7;

    /// Consonants (1) with marks (2) after them, marks alone after an optional
    /// repha (7), anything else alone.
    static MACHINE: Machine = Machine::new(&[
        (
            Expr::Seq(&[class(&[1]), Expr::Star(&class(&[2]))]),
            CONSONANT,
        ),
        (
            Expr::Seq(&[Expr::Opt(&class(&[REPHA])), Expr::Plus(&class(&[2]))]),
            BROKEN,
        ),
        (ANY, OTHER),
    ]);

    fn categorised(budget: &Budget, categories: &[u8]) -> Buffer {
        let text: Vec<u32> = (0..categories.len() as u32).map(|i| 0x41 + i).collect();
        let mut buffer = text_buffer(budget, &text);
        for (info, &category) in buffer.infos_mut().iter_mut().zip(categories) {
            info.shaper[0] = category;
        }
        buffer
    }

    fn syllable_list(buffer: &mut Buffer) -> Vec<(Range<usize>, u8)> {
        let mut out = Vec::new();
        for_each_syllable(buffer, |buffer, range| {
            out.push((range.clone(), syllable_type(&buffer.infos()[range.start])));
            Ok(())
        })
        .unwrap();
        out
    }

    #[test]
    fn longest_match_and_earliest_rule_win() {
        let budget = Budget::new(1 << 16).unwrap();
        let mut buffer = categorised(&budget, &[1, 2, 2, 2, 1, 3, 2]);
        find_syllables(&MACHINE, &mut buffer, |_, _| false).unwrap();
        assert_eq!(
            syllable_list(&mut buffer),
            [
                (0..4, CONSONANT),
                (4..5, CONSONANT),
                (5..6, OTHER),
                (6..7, BROKEN)
            ]
        );
        let serials: Vec<u8> = buffer.infos().iter().map(|i| i.syllable >> 4).collect();
        assert_eq!(serials, [1, 1, 1, 1, 2, 3, 4]);
    }

    #[test]
    fn skipped_glyphs_join_the_syllable_before_them() {
        let budget = Budget::new(1 << 16).unwrap();
        let mut buffer = categorised(&budget, &[9, 1, 9, 2, 9]);
        find_syllables(&MACHINE, &mut buffer, |infos, i| infos[i].shaper[0] == 9).unwrap();
        let syllables: Vec<u8> = buffer.infos().iter().map(|i| i.syllable).collect();
        assert_eq!(syllables, [0, 0x10, 0x10, 0x10, 0x10]);
    }

    #[test]
    fn long_syllables_are_walked_in_pieces() {
        let budget = Budget::new(1 << 16).unwrap();
        let mut categories = vec![1];
        categories.resize(70, 2);
        let mut buffer = categorised(&budget, &categories);
        find_syllables(&MACHINE, &mut buffer, |_, _| false).unwrap();
        let ranges: Vec<_> = syllable_list(&mut buffer)
            .into_iter()
            .map(|(range, _)| range)
            .collect();
        assert_eq!(ranges, [0..64, 64..70]);
    }

    #[test]
    fn broken_syllables_get_a_dotted_circle_after_repha() {
        let budget = Budget::new(1 << 16).unwrap();
        let font = TestFont(&[(0x25CC, 500)]);
        let font = scaled(&font);
        let mut buffer = categorised(&budget, &[REPHA, 2, 2, 1, 2]);
        find_syllables(&MACHINE, &mut buffer, |_, _| false).unwrap();
        let circle = DottedCircle {
            broken: BROKEN,
            shaper: [1, 0],
            after: Some(REPHA),
        };
        insert_dotted_circles(&mut buffer, &font, circle).unwrap();
        // The circle takes the cluster of the syllable's first glyph.
        assert_eq!(
            contents(&buffer),
            [
                (0x41, 0),
                (1, 0),
                (0x42, 1),
                (0x43, 2),
                (0x44, 3),
                (0x45, 4)
            ]
        );
        let infos = buffer.infos();
        assert_eq!(infos[1].shaper, [1, 0]);
        assert_eq!(infos[1].syllable, infos[0].syllable);
    }

    #[test]
    fn vowel_sequences_that_look_like_vowels_are_split() {
        let budget = Budget::new(1 << 16).unwrap();
        // Devanagari A and vowel sign AA look like letter AA; Ra, virama, I is
        // the three-character case.
        let text = [0x0905, 0x093E, 0x0930, 0x094D, 0x0907, 0x0905];
        let mut buffer = text_buffer(&budget, &text);
        let mut scratch = buffer.scratch;
        crate::props::set_all(buffer.infos_mut(), &mut scratch, 0);
        insert_vowel_constraint_circles(Script(*b"Deva"), &mut buffer).unwrap();
        let glyphs: Vec<u32> = contents(&buffer).into_iter().map(|(g, _)| g).collect();
        assert_eq!(
            glyphs,
            [
                0x0905, 0x25CC, 0x093E, 0x0930, 0x094D, 0x25CC, 0x0907, 0x0905
            ]
        );
        let circle = buffer.infos()[1];
        assert_eq!(circle.cluster, 1);
        assert!(!circle.is_continuation());
        assert!(buffer.infos()[2].is_continuation());
    }
}

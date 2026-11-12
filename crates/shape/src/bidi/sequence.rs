//! Rule X10 and the implicit rules: the isolating run sequences of a paragraph,
//! each resolved by rules W1 to W7 and N1 and N2, the bracket pairs of the
//! paragraph resolved by rule N0 in between, then rules I1 and I2.
//!
//! The sequences are FriBidi's: the characters X9 keeps, grouped by isolate
//! depth, each sequence a stretch of one depth and one level that deeper
//! characters may interrupt. A shallower character ends the sequences deeper
//! than it, and a character at another level ends the sequence of its depth.
//! Where every isolate initiator has a matching PDI at its own level, these are
//! UAX #9's isolating run sequences; they differ where an isolate overflows the
//! deepest level (see [`super::explicit::levels`]).
//!
//! At a level change within a depth, sos and eos take the direction of the
//! higher of the two levels; at the start of a depth and at its end they take
//! that of the higher of the sequence's level and the paragraph's. Where the
//! level changes at an isolate initiator's matching PDI right after it, which
//! happens at the deepest depth, eos takes the higher level's direction only
//! where the stretch from the PDI holds a strong class, and is otherwise that
//! of the end of the depth. After a paragraph separator inside the paragraph
//! they compare with level 0, and a sequence may continue past the separator
//! (see `paragraph.rs`).

use super::class::{BidiClass, Bracket, Level};
use super::explicit::{MAX_ISOLATE_DEPTH, NONE};
use super::{charge, neutral, weak};
use triergon_core::{Budget, BudgetVec, Result, WorkBudget};

/// One isolating run sequence.
#[derive(Clone, Copy, Debug)]
struct Sequence {
    level: u8,
    /// [`Paragraph::base`] at its first character.
    base: u8,
    /// The position of its last character.
    last: u32,
    /// Where N1 stops joining neutrals: its first character after the first
    /// paragraph separator, when an isolate initiator open across the
    /// separator ended it there, else [`NONE`].
    break_at: u32,
    sos: BidiClass,
    eos: BidiClass,
    /// Where its characters start in [`Storage::order`], and how many there are.
    start: u32,
    len: u32,
}

/// The working storage of the implicit rules, kept between paragraphs.
#[derive(Debug)]
pub(super) struct Storage {
    sequences: BudgetVec<Sequence>,
    /// The sequence of each character, [`NONE`] for those X9 removed.
    sequence_of: BudgetVec<u32>,
    /// The kept characters, sequence by sequence, each in logical order.
    order: BudgetVec<u32>,
    /// One sequence's classes as the rules resolve them.
    types: BudgetVec<BidiClass>,
    /// The characters that W1 joins to the run of a neutral before marks.
    absorbed: BudgetVec<bool>,
    brackets: neutral::Storage,
}

impl Storage {
    pub(super) fn new(budget: &Budget) -> Self {
        Self {
            sequences: BudgetVec::new(budget),
            sequence_of: BudgetVec::new(budget),
            order: BudgetVec::new(budget),
            types: BudgetVec::new(budget),
            absorbed: BudgetVec::new(budget),
            brackets: neutral::Storage::new(budget),
        }
    }
}

/// The paragraph as the explicit rules leave it.
pub(super) struct Paragraph<'a> {
    /// The characters' original classes.
    pub classes: &'a [BidiClass],
    pub brackets: Option<&'a [Option<Bracket>]>,
    /// [`super::explicit::match_isolates`]'s partners.
    pub partner: &'a [u32],
    pub level: u8,
    /// The characters' isolate depths.
    pub depths: &'a [u8],
    /// Where the explicit rules stopped: after the first paragraph separator.
    /// The characters from here on keep their classes, formatting characters
    /// included, and are all kept.
    pub explicit_end: usize,
}

impl Paragraph<'_> {
    /// Whether the character at `last` is an isolate initiator before the
    /// first paragraph separator, without a matching PDI before it, and `next`,
    /// the first character after the separator, is not a PDI that closes an
    /// isolate opened before the separator.
    fn open_across(&self, last: u32, next: usize) -> bool {
        let last = last as usize;
        // The paragraph has partners: it holds an isolate initiator.
        last < self.explicit_end
            && self.classes[last].is_isolate_initiator()
            && (self.partner[last] == NONE || self.partner[last] as usize >= self.explicit_end)
            && !(self.classes[next] == BidiClass::Pdi
                && (self.partner[next] as usize) < self.explicit_end)
    }

    /// The level sos and eos compare with at the start and end of a depth for
    /// a sequence starting at `at`: the paragraph's, but 0 from the first
    /// paragraph separator on, which with what follows it is at level 0.
    fn base(&self, at: usize) -> u8 {
        let separator = self.explicit_end.saturating_sub(1);
        if at >= separator && self.classes.get(separator) == Some(&BidiClass::B) {
            0
        } else {
            self.level
        }
    }
}

/// Resolves the paragraph, whose classes after X1 to X9 are `types` (BN marking
/// the characters X9 removed, up to [`Paragraph::explicit_end`]) and whose
/// explicit levels are `levels`: the implicit rules resolve `types` and raise
/// `levels` by I1 and I2.
pub(super) fn resolve(
    storage: &mut Storage,
    paragraph: &Paragraph<'_>,
    types: &mut [BidiClass],
    levels: &mut [Level],
    work: &mut WorkBudget,
) -> Result<()> {
    sequences(storage, paragraph, types, levels, work)?;
    storage.absorbed.clear();
    for s in 0..storage.sequences.len() {
        let sequence = storage.sequences[s];
        let positions = sequence_positions(&storage.order, sequence);
        let runs = paragraph.brackets.map(|brackets| weak::Runs {
            brackets,
            positions,
            absorbed: &mut storage.absorbed,
        });
        if let Some(range) = contiguous(positions) {
            weak::resolve(&mut types[range], sequence.sos, runs, work)?;
        } else {
            load(&mut storage.types, types, positions)?;
            weak::resolve(storage.types.as_mut_slice(), sequence.sos, runs, work)?;
            store(&storage.types, types, positions);
        }
    }
    if let Some(brackets) = paragraph.brackets {
        neutral::resolve_brackets(
            &mut storage.brackets,
            types,
            levels,
            &storage.sequence_of,
            &storage.absorbed,
            paragraph,
            brackets,
            work,
        )?;
    }
    for s in 0..storage.sequences.len() {
        let sequence = storage.sequences[s];
        let positions = sequence_positions(&storage.order, sequence);
        let ends = (sequence.sos, sequence.eos);
        let stop = (sequence.break_at != NONE).then_some(sequence.break_at as usize);
        if let Some(range) = contiguous(positions) {
            neutral::resolve(&mut types[range.clone()], sequence.level, ends, stop, work)?;
            for (&class, level) in types[range.clone()].iter().zip(&mut levels[range]) {
                raise(level, class);
            }
        } else {
            load(&mut storage.types, types, positions)?;
            neutral::resolve(
                storage.types.as_mut_slice(),
                sequence.level,
                ends,
                stop,
                work,
            )?;
            for (&at, &class) in positions.iter().zip(storage.types.iter()) {
                types[at as usize] = class;
                raise(&mut levels[at as usize], class);
            }
        }
    }
    Ok(())
}

/// Resolves a paragraph without explicit formatting characters, isolates or
/// bracket pairs, but perhaps BN and a final paragraph separator: one isolating
/// run sequence, at the paragraph level, of every character before the
/// separator that X9 keeps. A BN takes the level of the character before it.
pub(super) fn resolve_plain(
    storage: &mut Storage,
    types: &mut [BidiClass],
    level: u8,
    levels: &mut [Level],
    work: &mut WorkBudget,
) -> Result<()> {
    let ends = BidiClass::of_level(level);
    if !types.contains(&BidiClass::Bn) {
        weak::resolve(types, ends, None, work)?;
        neutral::resolve(types, level, (ends, ends), None, work)?;
        for (&class, level) in types.iter().zip(levels) {
            raise(level, class);
        }
        return Ok(());
    }
    storage.order.clear();
    for (at, &class) in types.iter().enumerate() {
        if class != BidiClass::Bn {
            storage.order.try_push(at as u32).map_err(|e| e.error)?;
        }
    }
    load(&mut storage.types, types, &storage.order)?;
    weak::resolve(storage.types.as_mut_slice(), ends, None, work)?;
    neutral::resolve(
        storage.types.as_mut_slice(),
        level,
        (ends, ends),
        None,
        work,
    )?;
    for (&at, &class) in storage.order.iter().zip(storage.types.iter()) {
        raise(&mut levels[at as usize], class);
    }
    let mut before = Level(level);
    for (&class, level) in types.iter().zip(levels) {
        if class == BidiClass::Bn {
            *level = before;
        }
        before = *level;
    }
    Ok(())
}

/// Rules I1 and I2 for one character. The embedding and override characters,
/// kept after a paragraph separator (see `paragraph.rs`), take the direction
/// of their class.
fn raise(level: &mut Level, class: BidiClass) {
    level.0 += match (level.0 & 1 == 1, class) {
        (false, BidiClass::R | BidiClass::Rle | BidiClass::Rlo)
        | (true, BidiClass::L | BidiClass::Lre | BidiClass::Lro | BidiClass::En | BidiClass::An) => {
            1
        }
        (false, BidiClass::En | BidiClass::An) => 2,
        _ => 0,
    };
}

/// The range of characters `positions` cover when they are consecutive, so
/// that the rules can work on them in place.
fn contiguous(positions: &[u32]) -> Option<std::ops::Range<usize>> {
    let (&first, &last) = (positions.first()?, positions.last()?);
    let range = first as usize..last as usize + 1;
    (range.len() == positions.len()).then_some(range)
}

/// Whether the stretch of one depth and level from `at` holds a strong class
/// before a character at another depth or level ends it.
fn strong_ahead(
    paragraph: &Paragraph<'_>,
    types: &[BidiClass],
    levels: &[Level],
    at: usize,
    work: &mut WorkBudget,
) -> Result<bool> {
    let (depth, level) = (paragraph.depths[at], levels[at]);
    let mut found = false;
    let mut steps = 0;
    for next in at..types.len() {
        let class = types[next];
        if class == BidiClass::Bn && next < paragraph.explicit_end {
            continue;
        }
        steps += 1;
        if paragraph.depths[next] != depth || levels[next] != level {
            break;
        }
        if matches!(
            class,
            BidiClass::L | BidiClass::R | BidiClass::Al | BidiClass::En | BidiClass::An
        ) {
            found = true;
            break;
        }
    }
    charge(work, steps)?;
    Ok(found)
}

/// `vector` at `len` elements, filled with `value` only where it grows.
fn sized<T: Copy>(vector: &mut BudgetVec<T>, len: usize, value: T) -> Result<()> {
    if vector.len() < len {
        vector.try_resize(len, value)?;
    }
    vector.truncate(len);
    Ok(())
}

fn sequence_positions(order: &[u32], sequence: Sequence) -> &[u32] {
    &order[sequence.start as usize..(sequence.start + sequence.len) as usize]
}

/// Copies the classes at `positions` into `into`.
fn load(into: &mut BudgetVec<BidiClass>, types: &[BidiClass], positions: &[u32]) -> Result<()> {
    into.clear();
    into.try_reserve(positions.len())?;
    for &at in positions {
        into.try_push(types[at as usize]).map_err(|e| e.error)?;
    }
    Ok(())
}

/// Writes `from` back to the classes at `positions`.
fn store(from: &[BidiClass], types: &mut [BidiClass], positions: &[u32]) {
    for (&at, &class) in positions.iter().zip(from) {
        types[at as usize] = class;
    }
}

/// X10: the paragraph's sequences, each character's sequence, and the
/// characters ordered sequence by sequence.
fn sequences(
    storage: &mut Storage,
    paragraph: &Paragraph<'_>,
    types: &[BidiClass],
    levels: &[Level],
    work: &mut WorkBudget,
) -> Result<()> {
    charge(work, 2 * types.len())?;
    let sequences = &mut storage.sequences;
    sequences.clear();
    sized(&mut storage.sequence_of, types.len(), NONE)?;
    let direction = BidiClass::of_level;
    // The open sequence of each depth, and one past the deepest open depth.
    let mut open = [NONE; MAX_ISOLATE_DEPTH as usize + 1];
    let mut deepest = 0;
    for (at, &class) in types.iter().enumerate() {
        if class == BidiClass::Bn && at < paragraph.explicit_end {
            storage.sequence_of[at] = NONE;
            continue;
        }
        let depth = usize::from(paragraph.depths[at]);
        let level = levels[at].0;
        while deepest > depth + 1 {
            deepest -= 1;
            if let Some(s) = sequences.get_mut(open[deepest] as usize) {
                s.eos = direction(s.level.max(s.base));
            }
            open[deepest] = NONE;
        }
        let after_separator = at >= paragraph.explicit_end;
        let sos = match sequences.get_mut(open[depth] as usize) {
            Some(s) if s.level == level && after_separator && paragraph.open_across(s.last, at) => {
                // The text after the separator continues a sequence that
                // ends with an isolate initiator open across it, but N1 does
                // not join neutrals across.
                s.break_at = s.len;
                None
            }
            Some(s) if s.level == level => None,
            Some(s) => {
                let boundary = direction(s.level.max(level));
                // After an isolate initiator and its matching PDI right after
                // it, the higher level counts only where the stretch from the
                // PDI holds a strong class.
                let empty = paragraph.partner.get(s.last as usize) == Some(&(at as u32));
                s.eos = if empty && !strong_ahead(paragraph, types, levels, at, work)? {
                    direction(s.level.max(s.base))
                } else {
                    boundary
                };
                Some(boundary)
            }
            None => Some(direction(level.max(paragraph.base(at)))),
        };
        if let Some(sos) = sos {
            open[depth] = sequences.len() as u32;
            sequences
                .try_push(Sequence {
                    level,
                    base: paragraph.base(at),
                    last: at as u32,
                    break_at: NONE,
                    sos,
                    eos: sos,
                    start: 0,
                    len: 0,
                })
                .map_err(|e| e.error)?;
        }
        let s = open[depth];
        let sequence = &mut sequences[s as usize];
        sequence.len += 1;
        sequence.last = at as u32;
        storage.sequence_of[at] = s;
        deepest = deepest.max(depth + 1);
    }
    for &s in &open[..deepest] {
        if let Some(s) = sequences.get_mut(s as usize) {
            s.eos = direction(s.level.max(s.base));
        }
    }
    // Lay the sequences out one after another, then place each character.
    let mut start = 0;
    for s in sequences.as_mut_slice() {
        s.start = start;
        start += s.len;
        s.len = 0;
    }
    sized(&mut storage.order, start as usize, 0)?;
    for (at, &s) in storage.sequence_of.iter().enumerate() {
        if s == NONE {
            continue;
        }
        let sequence = &mut sequences[s as usize];
        storage.order[(sequence.start + sequence.len) as usize] = at as u32;
        sequence.len += 1;
    }
    Ok(())
}

//! Rules BD16 and N0 for the paired brackets of a paragraph, and N1 and N2 on
//! the classes of one isolating run sequence.
//!
//! The bracket pairs are FriBidi's, as probes of it show them. UAX #9 pairs
//! brackets within an isolating run sequence; FriBidi pairs them per isolate
//! depth over the whole paragraph, which differs from it in these ways:
//! - Each depth has one stack of open brackets, shared by separate isolates of
//!   that depth, so that brackets in two isolates can pair. It holds at most
//!   63: the 64th opening bracket at any depth ends the pairing of the rest of
//!   the paragraph, and the pairs found before it stay.
//! - A depth's stack is emptied where a level run of that depth starts because
//!   of an embedding or a PDF, but not where one starts with a PDI that closes
//!   an isolate, nor on entering or leaving a deeper isolate.
//! - At depths 0 to 15 a stack's slot `i` is slot `i - 16q` of depth `d + q`'s
//!   stack, up to depth 15: a bracket pushed at either depth overwrites the
//!   other's, and a closing bracket, which searches its own depth's open
//!   brackets, may so pair with one pushed at another depth. Deeper stacks
//!   share nothing.
//! - N0b counts a strong class between the brackets, at any depth, as the
//!   embedding direction where the level I1 and I2 would give it is the
//!   opening bracket's level; one at the opening bracket's depth otherwise
//!   counts as the opposite direction, and one at another depth not at all.
//!   N0c takes for the prior context the last strong class before the opening
//!   bracket at its depth, across earlier isolates and embeddings, and the
//!   embedding direction where there is none.
//!
//! Where brackets pair within one isolating run sequence, as in every case of
//! BidiCharacterTest.txt, this is UAX #9's N0.

use super::charge;
use super::class::{BidiClass, Bracket, Level, fill};
use super::explicit::{MAX_ISOLATE_DEPTH, NONE};
use super::sequence::Paragraph;
use triergon_core::{Budget, BudgetVec, Result, WorkBudget};

/// The opening brackets a depth keeps unpaired at most.
const MAX_OPEN_BRACKETS: usize = 63;

/// The depths whose stacks share slots, and how far apart in them consecutive
/// depths' stacks start.
const SHARED_DEPTHS: usize = 16;
const SHARED_ROW: usize = 16;

/// The slots of the shared array: the last shared stack runs to its full size.
const SHARED_SLOTS: usize = SHARED_ROW * (SHARED_DEPTHS - 1) + MAX_OPEN_BRACKETS;

/// The bracket stack of one isolate depth.
#[derive(Clone, Copy, Debug)]
struct Depth {
    /// How many opening brackets it holds.
    open: u8,
    /// The level of the last character seen at this depth, `u8::MAX` where a
    /// shallower character came after it.
    level: u8,
    /// Where its stack starts in [`Storage::slots`], [`NONE`] until it is used.
    start: u32,
    /// The paragraph it was last used in, so that a new paragraph starts it
    /// afresh without clearing every depth.
    paragraph: u32,
}

impl Depth {
    const UNUSED: Self = Self {
        open: 0,
        level: u8::MAX,
        start: NONE,
        paragraph: 0,
    };
}

/// Storage for pairing brackets, kept between paragraphs.
#[derive(Debug)]
pub(super) struct Storage {
    /// The opening brackets of the stacks: each one's pair identity and
    /// position.
    slots: BudgetVec<(char, u32)>,
    depths: BudgetVec<Depth>,
    /// The number of the current paragraph, from 1.
    paragraph: u32,
    /// The bracket pairs: the positions of each opening and closing bracket.
    pairs: BudgetVec<(u32, u32)>,
}

impl Storage {
    pub(super) fn new(budget: &Budget) -> Self {
        Self {
            slots: BudgetVec::new(budget),
            depths: BudgetVec::new(budget),
            paragraph: 0,
            pairs: BudgetVec::new(budget),
        }
    }

    /// Starts a paragraph: every depth's stack empty, and the deep stacks'
    /// room given back.
    fn start_paragraph(&mut self) -> Result<()> {
        self.paragraph = self.paragraph.wrapping_add(1);
        if self.paragraph == 0 {
            self.depths.clear();
            self.paragraph = 1;
        }
        let depths = usize::from(MAX_ISOLATE_DEPTH) + 1;
        if self.depths.len() < depths {
            self.depths.try_resize(depths, Depth::UNUSED)?;
        }
        if self.slots.len() < SHARED_SLOTS {
            self.slots.try_resize(SHARED_SLOTS, ('\0', 0))?;
        }
        self.slots.truncate(SHARED_SLOTS);
        Ok(())
    }

    /// The stack of `depth` in this paragraph.
    fn depth(&mut self, depth: usize) -> &mut Depth {
        let state = &mut self.depths[depth];
        if state.paragraph != self.paragraph {
            *state = Depth {
                paragraph: self.paragraph,
                ..Depth::UNUSED
            };
        }
        state
    }

    /// Where the stack of `depth` starts, giving deep stacks room of their own.
    fn start(&mut self, depth: usize) -> Result<usize> {
        if self.depths[depth].start == NONE {
            let start = if depth < SHARED_DEPTHS {
                depth * SHARED_ROW
            } else {
                let start = self.slots.len();
                self.slots
                    .try_resize(start + MAX_OPEN_BRACKETS, ('\0', 0))?;
                start
            };
            self.depths[depth].start = start as u32;
        }
        Ok(self.depths[depth].start as usize)
    }
}

/// BD16 and N0 over a paragraph whose weak types are resolved: `levels` are
/// the explicit levels, `sequence_of` the isolating run sequence of each
/// character and `absorbed` the characters W1 joined to a run before marks.
#[allow(clippy::too_many_arguments)]
pub(super) fn resolve_brackets(
    storage: &mut Storage,
    types: &mut [BidiClass],
    levels: &[Level],
    sequence_of: &[u32],
    absorbed: &[bool],
    paragraph: &Paragraph<'_>,
    brackets: &[Option<Bracket>],
    work: &mut WorkBudget,
) -> Result<()> {
    pair(
        storage,
        types,
        levels,
        sequence_of,
        paragraph,
        absorbed,
        brackets,
        work,
    )?;
    for p in 0..storage.pairs.len() {
        let (open, close) = storage.pairs[p];
        let (open, close) = (open as usize, close as usize);
        let Some(rtl) = pair_direction(types, levels, paragraph.depths, open, close, work)? else {
            continue;
        };
        let class = if rtl { BidiClass::R } else { BidiClass::L };
        for (bracket, end) in [(open, close), (close, types.len())] {
            types[bracket] = class;
            // Marks after a bracket in its sequence follow it, and so do the
            // characters W1 joined to its run.
            for at in bracket + 1..end {
                if sequence_of[at] == NONE {
                    continue;
                }
                let joined =
                    paragraph.classes[at] == BidiClass::Nsm || absorbed.get(at) == Some(&true);
                if !joined || sequence_of[at] != sequence_of[bracket] {
                    break;
                }
                types[at] = class;
            }
        }
    }
    Ok(())
}

/// BD16 as FriBidi pairs brackets: the pairs, in order of their opening
/// brackets, into `storage.pairs`. Only brackets of class ON that W1 did not
/// join to another run pair.
#[allow(clippy::too_many_arguments)]
fn pair(
    storage: &mut Storage,
    types: &[BidiClass],
    levels: &[Level],
    sequence_of: &[u32],
    paragraph: &Paragraph<'_>,
    absorbed: &[bool],
    brackets: &[Option<Bracket>],
    work: &mut WorkBudget,
) -> Result<()> {
    let depths = paragraph.depths;
    charge(work, types.len())?;
    storage.pairs.clear();
    storage.start_paragraph()?;
    // One past the deepest depth seen since a shallower character, and the
    // depth and level of the last character.
    let mut deepest = 0;
    let mut last = None;
    for (at, &class) in types.iter().enumerate() {
        if sequence_of[at] == NONE {
            continue;
        }
        let depth = usize::from(depths[at]);
        if last != Some((depth, levels[at].0)) {
            last = Some((depth, levels[at].0));
            for deeper in depth + 1..deepest.max(depth + 1) {
                storage.depth(deeper).level = u8::MAX;
            }
            deepest = depth + 1;
            let state = storage.depth(depth);
            if state.level != levels[at].0 {
                let closes_isolate = paragraph.classes[at] == BidiClass::Pdi
                    && paragraph.partner.get(at).is_some_and(|&p| p != NONE);
                if state.level != u8::MAX && !closes_isolate {
                    state.open = 0;
                }
                state.level = levels[at].0;
            }
        }
        let joined = absorbed.get(at) == Some(&true);
        let Some(bracket) = brackets[at].filter(|_| class == BidiClass::On && !joined) else {
            continue;
        };
        let open = usize::from(storage.depth(depth).open);
        let start = storage.start(depth)?;
        if bracket.opens() {
            if open == MAX_OPEN_BRACKETS {
                break;
            }
            storage.slots[start + open] = (bracket.pair(), at as u32);
            storage.depths[depth].open += 1;
            continue;
        }
        charge(work, open)?;
        let stack = &storage.slots[start..start + open];
        if let Some(found) = stack.iter().rposition(|&(pair, _)| pair == bracket.pair()) {
            let opening = stack[found].1;
            storage.depths[depth].open = found as u8;
            storage
                .pairs
                .try_push((opening, at as u32))
                .map_err(|e| e.error)?;
        }
    }
    storage.pairs.as_mut_slice().sort_unstable();
    Ok(())
}

/// N0b and N0c for the pair at `open` and `close`: the direction its brackets
/// take (right-to-left when true), if one is found between them.
fn pair_direction(
    types: &[BidiClass],
    levels: &[Level],
    depths: &[u8],
    open: usize,
    close: usize,
    work: &mut WorkBudget,
) -> Result<Option<bool>> {
    let level = levels[open].0;
    let embedding = level & 1 == 1;
    let depth = depths[open];
    charge(work, close - open)?;
    let mut strong_at_depth = false;
    for at in open + 1..close {
        let Some(rtl) = types[at].strong_rtl() else {
            continue;
        };
        let own = levels[at].0;
        if own + u8::from((own & 1 == 1) != rtl) == level {
            return Ok(Some(embedding));
        }
        strong_at_depth |= depths[at] == depth;
    }
    if !strong_at_depth {
        return Ok(None);
    }
    // N0c: the brackets take the opposite direction where the context before
    // them has it, else the embedding direction.
    let mut before = None;
    let mut looked = 0;
    for at in (0..open).rev() {
        looked += 1;
        if depths[at] != depth || types[at] == BidiClass::Bn {
            continue;
        }
        if let Some(rtl) = types[at].strong_rtl() {
            before = Some(rtl);
            break;
        }
    }
    charge(work, looked)?;
    let opposite = before == Some(!embedding);
    Ok(Some(if opposite { !embedding } else { embedding }))
}

/// The class a neutral stretch compares on one side of it in N1: L, or R for
/// R, AL and the numbers. The embedding and override characters, which are
/// only in a sequence after a paragraph separator (see `paragraph.rs`),
/// compare as themselves: the stretch takes their direction only between two
/// of a kind.
fn context(class: BidiClass) -> Option<BidiClass> {
    match class {
        BidiClass::R | BidiClass::Al | BidiClass::En | BidiClass::An => Some(BidiClass::R),
        BidiClass::L | BidiClass::Lre | BidiClass::Lro | BidiClass::Rle | BidiClass::Rlo => {
            Some(class)
        }
        _ => None,
    }
}

/// N1 and N2 on one sequence: a stretch of neutrals takes the direction of the
/// classes on both sides where they agree (numbers counting as R), else the
/// embedding direction of `level`. A stretch does not reach across `stop`.
pub(super) fn resolve(
    types: &mut [BidiClass],
    level: u8,
    (sos, eos): (BidiClass, BidiClass),
    stop: Option<usize>,
    work: &mut WorkBudget,
) -> Result<()> {
    charge(work, types.len())?;
    let embedding = BidiClass::of_level(level);
    let mut at = 0;
    while at < types.len() {
        if !types[at].is_neutral_or_isolate() {
            at += 1;
            continue;
        }
        let mut end = types[at..]
            .iter()
            .position(|c| !c.is_neutral_or_isolate())
            .map_or(types.len(), |n| at + n);
        if let Some(stop) = stop.filter(|&stop| at < stop && stop < end) {
            end = stop;
        }
        let before = context(if at == 0 { sos } else { types[at - 1] });
        let after = context(types.get(end).copied().unwrap_or(eos));
        let class = match before.filter(|_| before == after) {
            Some(class) if class.strong_rtl() == Some(true) => BidiClass::R,
            Some(_) => BidiClass::L,
            None => embedding,
        };
        fill(&mut types[at..end], class);
        at = end;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use BidiClass::*;

    #[test]
    fn neutrals_take_their_context() {
        let mut work = WorkBudget::new(1000);
        let mut types = [L, Ws, On, L, Ws, R, Ws, En, On];
        resolve(&mut types, 0, (L, R), None, &mut work).unwrap();
        assert_eq!(types, [L, L, L, L, L, R, R, En, R]);
    }

    #[test]
    fn pairs_resolve_by_their_contents_and_context() {
        let mut work = WorkBudget::new(1000);
        let levels = [Level(0); 4];
        let depths = [0; 4];
        let direction = |types: &[BidiClass], open, close, work: &mut WorkBudget| {
            pair_direction(types, &levels, &depths, open, close, work)
        };
        // N0b: a strong class of the embedding direction inside.
        assert_eq!(direction(&[On, L, On], 0, 2, &mut work), Ok(Some(false)));
        // N0c1: only the opposite direction inside, and before.
        assert_eq!(direction(&[R, On, R, On], 1, 3, &mut work), Ok(Some(true)));
        // N0c2: the opposite direction inside, the embedding one before.
        assert_eq!(
            direction(&[L, On, An, On], 1, 3, &mut work),
            Ok(Some(false))
        );
        // Nothing strong inside.
        assert_eq!(direction(&[On, On], 0, 1, &mut work), Ok(None));
    }
}

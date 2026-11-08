//! Rules N0–N2.
//!
//! FriBidi's choices for paired brackets (BD16 and N0), reproduced here:
//! - Brackets pair per isolate depth: each depth keeps a stack of at most 63
//!   opening brackets, cleared when the level changes at the same depth but
//!   kept across separate isolates of one depth. An opening bracket that finds
//!   its stack full ends the pairing of the paragraph. A bracket is paired only
//!   while its run is ON, so an overridden bracket does not take part.
//! - The stacks of the first 16 depths share storage as FriBidi lays them out
//!   (see [`stack_start`]): from its 17th bracket on, a depth's stack overwrites
//!   the stacks of the depths after it.
//! - N0 looks inside a pair at every run between the brackets, at any depth.
//!   N0b takes the first strong class (numbers as R) whose level, raised as I1
//!   and I2 would raise it, is the pair's embedding level. N0c takes the first
//!   strong class at the pair's isolate depth and gives the brackets the
//!   direction of the strong class last seen before the opening bracket at that
//!   depth, or the embedding direction; that class is looked up as resolved
//!   when the pair is reached, earlier pairs included.
//! - Marks after a bracket follow it because W1 merged them into its run.

use super::charge;
use super::class::BidiClass;
use super::runs::{MAX_ISOLATE_DEPTH, Run, contexts};
use triergon_core::{Budget, BudgetVec, Error, Result, WorkBudget};

/// The opening brackets one isolate depth keeps unpaired.
const MAX_OPEN: usize = 63;

const DEPTHS: usize = MAX_ISOLATE_DEPTH as usize + 1;

/// The depths whose stacks start [`SHARED_ROW`] slots apart.
const SHARED_DEPTHS: usize = 16;
const SHARED_ROW: usize = 16;
const SHARED_END: usize = (SHARED_DEPTHS - 1) * SHARED_ROW + MAX_OPEN;

/// Where the stack of opening brackets of an isolate depth starts. FriBidi
/// keeps the stacks of the first 16 depths in rows of 16 slots of one array yet
/// lets each hold 63 brackets, so they overlap; deeper stacks have room of their
/// own.
const fn stack_start(depth: usize) -> usize {
    if depth < SHARED_DEPTHS {
        depth * SHARED_ROW
    } else {
        SHARED_END + (depth - SHARED_DEPTHS) * MAX_OPEN
    }
}

/// Storage for pairing brackets, kept between paragraphs.
#[derive(Debug)]
pub(super) struct Brackets {
    /// The stacks of every isolate depth: indices of unpaired opening runs.
    open: BudgetVec<u32>,
    /// Paired runs: opening and closing.
    pairs: BudgetVec<(u32, u32)>,
}

impl Brackets {
    pub(super) fn new(budget: &Budget) -> Self {
        Self {
            open: BudgetVec::new(budget),
            pairs: BudgetVec::new(budget),
        }
    }

    /// N0 for every bracket pair of `runs`.
    pub(super) fn resolve(&mut self, runs: &mut [Run], work: &mut WorkBudget) -> Result<()> {
        self.pair(runs, work)?;
        let pairs = self.pairs.as_mut_slice();
        // Pairs of one opening bracket, which the shared rows allow, in the order found.
        charge(work, pairs.len())?;
        pairs.sort_unstable();
        resolve_pairs(runs, pairs, work)
    }

    /// BD16. Each run, and each open bracket a closing one is compared with,
    /// is charged to `work`.
    fn pair(&mut self, runs: &[Run], work: &mut WorkBudget) -> Result<()> {
        charge(work, runs.len())?;
        self.pairs.clear();
        let depths = runs
            .iter()
            .map(|r| usize::from(r.isolate))
            .max()
            .unwrap_or(0)
            + 1;
        self.open.clear();
        self.open
            .try_resize(stack_start(depths - 1) + MAX_OPEN, 0)?;
        let open = self.open.as_mut_slice();
        let mut open_len = [0_usize; DEPTHS];
        let mut last: Option<(u8, u8)> = None;
        for (at, run) in runs.iter().enumerate() {
            let depth = usize::from(run.isolate);
            if last.is_none_or(|(level, isolate)| level != run.level && isolate == run.isolate) {
                open_len[depth] = 0;
            }
            last = Some((run.level, run.isolate));
            let Some(bracket) = run.bracket.filter(|_| run.class == BidiClass::On) else {
                continue;
            };
            let at = u32::try_from(at).map_err(|_| Error::Overflow)?;
            let stack = &mut open[stack_start(depth)..][..MAX_OPEN];
            let len = &mut open_len[depth];
            if bracket.opens() {
                if *len == MAX_OPEN {
                    break;
                }
                stack[*len] = at;
                *len += 1;
            } else if let Some(found) = {
                charge(work, *len)?;
                stack[..*len].iter().rposition(|&o| {
                    runs.get(o as usize)
                        .and_then(|r| r.bracket)
                        .is_some_and(|b| b.pairs_with(bracket))
                })
            } {
                self.pairs
                    .try_push((stack[found], at))
                    .map_err(|e| e.error)?;
                *len = found;
            }
        }
        Ok(())
    }
}

/// The level I1 and I2 would give a strong class: up by one where the class's
/// direction differs from the level's.
fn raised(level: u8, class: BidiClass) -> u8 {
    level + ((level % 2) ^ class.rtl_bit())
}

/// N0b and N0c for pairs in order of their opening brackets. The runs inside
/// each pair are charged to `work` as they are looked at, as are the runs swept
/// before them.
fn resolve_pairs(runs: &mut [Run], pairs: &[(u32, u32)], work: &mut WorkBudget) -> Result<()> {
    // The level of the strong class last passed per isolate depth, swept forward
    // once as pairs are reached.
    let mut preceding: [Option<u8>; DEPTHS] = [None; DEPTHS];
    let mut swept = 0;
    for &(open, close) in pairs {
        let (open, close) = (open as usize, close as usize);
        let embedding = runs[open].level;
        let isolate = runs[open].isolate;
        let strong = |run: &Run| {
            let class = run.class.numbers_as_r();
            class.is_strong().then(|| raised(run.level, class))
        };

        let inside = &runs[open..close];
        charge(work, inside.len())?;
        let direction = if inside.iter().any(|run| strong(run) == Some(embedding)) {
            Some(embedding)
        } else {
            charge(work, inside.len() + open.saturating_sub(swept))?;
            for run in &runs[swept.min(open)..open] {
                if let Some(level) = strong(run) {
                    preceding[usize::from(run.isolate)] = Some(level);
                }
            }
            swept = swept.max(open);
            inside
                .iter()
                .any(|run| run.isolate == isolate && strong(run).is_some())
                .then(|| preceding[usize::from(isolate)].unwrap_or(embedding))
        };
        if let Some(level) = direction {
            let class = BidiClass::of_level(level);
            runs[open].class = class;
            runs[close].class = class;
        }
    }
    Ok(())
}

/// N1 and N2: a neutral takes the class on both sides where they agree
/// (numbers as R), else the embedding direction.
pub(super) fn resolve(runs: &mut [Run], work: &mut WorkBudget) -> Result<()> {
    for at in 0..runs.len() {
        charge(work, 1)?;
        if runs[at].class.is_neutral() {
            let (before, after) = contexts(runs, at, work)?;
            let (before, after) = (before.numbers_as_r(), after.numbers_as_r());
            runs[at].class = if before == after {
                before
            } else {
                BidiClass::of_level(runs[at].level)
            };
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bidi::class::Bracket;
    use BidiClass::*;
    use triergon_core::{Budget, WorkBudget};

    fn run(class: BidiClass, level: u8, bracket: Option<char>) -> Run {
        Run {
            len: 1,
            class,
            level,
            isolate: 0,
            bracket: bracket.and_then(Bracket::of),
        }
    }

    fn brackets(runs: &mut [Run]) {
        let budget = Budget::new(1 << 20).expect("budget");
        let mut work = WorkBudget::new(1000);
        Brackets::new(&budget)
            .resolve(runs, &mut work)
            .expect("brackets");
    }

    #[test]
    fn brackets_take_the_embedding_direction() {
        // a ( b ) in a left-to-right paragraph.
        let mut runs = [
            run(R, 0, None),
            run(On, 0, Some('(')),
            run(L, 0, None),
            run(On, 0, Some(')')),
        ];
        brackets(&mut runs);
        assert_eq!((runs[1].class, runs[3].class), (L, L));
    }

    #[test]
    fn brackets_take_the_preceding_opposite_direction() {
        // R ( R ) at level 0: N0c1.
        let mut runs = [
            run(R, 0, None),
            run(On, 0, Some('(')),
            run(R, 0, None),
            run(On, 0, Some(')')),
        ];
        brackets(&mut runs);
        assert_eq!((runs[1].class, runs[3].class), (R, R));
        // L ( R ) at level 0: N0c2.
        runs[0].class = L;
        runs[1].class = On;
        runs[3].class = On;
        brackets(&mut runs);
        assert_eq!((runs[1].class, runs[3].class), (L, L));
    }

    #[test]
    fn unmatched_and_overridden_brackets_stay() {
        let mut runs = [
            run(On, 0, Some('(')),
            run(R, 0, None),
            run(On, 0, Some(']')),
            run(L, 0, Some(')')),
        ];
        brackets(&mut runs);
        assert_eq!(runs.map(|r| r.class), [On, R, On, L]);
    }

    #[test]
    fn neutrals_between_agreeing_sides() {
        let mut runs = [run(R, 0, None), run(On, 0, None), run(An, 0, None)];
        resolve(&mut runs, &mut WorkBudget::new(100)).expect("work");
        assert_eq!(runs[1].class, R);
        let mut runs = [run(R, 0, None), run(Ws, 0, None), run(L, 0, None)];
        resolve(&mut runs, &mut WorkBudget::new(100)).expect("work");
        assert_eq!(runs[1].class, L);
    }
}

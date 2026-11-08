//! Level runs: the unit the weak and neutral rules work on.
//!
//! As in FriBidi, resolution after rule X9 works on runs of characters rather
//! than on characters: the characters X9 keeps, in order, grouped into runs that
//! share a class, an embedding level and an isolate depth. Which characters share
//! a run matters, because FriBidi's rules look at whole runs: W4 changes only a
//! separator that is a run of its own, and runs the weak rules merge are resolved
//! as one by the neutral rules. Brackets never share a run with another
//! character until rule N0 is done with them.
//!
//! The isolating run sequence a run belongs to is found by its isolate depth:
//! the runs of one sequence are those of equal depth with no shallower run in
//! between, as FriBidi links them.

use super::class::{BidiClass, Bracket};
use triergon_core::BudgetVec;

/// The deepest isolate nesting FriBidi tracks: depths stop growing at 124.
pub(super) const MAX_ISOLATE_DEPTH: u8 = 124;

/// Characters that X9 keeps, sharing a class, a level and an isolate depth.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Run {
    /// The number of characters; the paragraph's length bounds the sum.
    pub(super) len: u32,
    pub(super) class: BidiClass,
    pub(super) level: u8,
    /// How many isolates enclose the run, as FriBidi counts them.
    pub(super) isolate: u8,
    /// The bracket of a run of one opening or closing bracket, until N0.
    pub(super) bracket: Option<Bracket>,
}

impl Run {
    /// Whether `next`, directly after `self`, continues the same run.
    pub(super) fn continues_with(&self, next: &Self) -> bool {
        self.class == next.class && self.joins_level_of(next)
    }

    /// Whether `next` is a neutral continuing a neutral run, or the same class.
    pub(super) fn continues_neutrally_with(&self, next: &Self) -> bool {
        (self.class == next.class || (self.class.is_neutral() && next.class.is_neutral()))
            && self.joins_level_of(next)
    }

    fn joins_level_of(&self, next: &Self) -> bool {
        self.level == next.level
            && self.isolate == next.isolate
            && self.bracket.is_none()
            && next.bracket.is_none()
    }

    /// The class the rules see next to the run on one side, given the run there
    /// in its isolating run sequence: that run's class at the same level,
    /// otherwise the direction of the higher level (sos or eos), or of the run's
    /// own level at the end of the sequence.
    pub(super) fn context(&self, neighbour: Option<&Self>) -> BidiClass {
        match neighbour {
            Some(n) if n.level == self.level => n.class,
            Some(n) => BidiClass::of_level(n.level.max(self.level)),
            None => BidiClass::of_level(self.level),
        }
    }
}

/// The index of the last run of `before` in the isolating run sequence at
/// isolate depth `isolate`, if the sequence continues that far back.
pub(super) fn sequence_before(before: &[Run], isolate: u8) -> Option<usize> {
    for (at, run) in before.iter().enumerate().rev() {
        if run.isolate < isolate {
            return None;
        }
        if run.isolate == isolate {
            return Some(at);
        }
    }
    None
}

/// The index of the first run from `from` on in the isolating run sequence at
/// isolate depth `isolate`. Looking forward, FriBidi passes over a closing PDI
/// to the run after it.
pub(super) fn sequence_after(runs: &[Run], from: usize, isolate: u8) -> Option<usize> {
    let rest = runs.get(from..)?;
    for (offset, run) in rest.iter().enumerate() {
        if run.isolate < isolate {
            return None;
        }
        if run.isolate == isolate && run.class != BidiClass::Pdi {
            return Some(from + offset);
        }
    }
    None
}

/// The context classes on both sides of `runs[at]`.
pub(super) fn contexts(runs: &[Run], at: usize) -> (BidiClass, BidiClass) {
    let run = &runs[at];
    let before = sequence_before(&runs[..at], run.isolate).map(|i| &runs[i]);
    let after = sequence_after(runs, at + 1, run.isolate).map(|i| &runs[i]);
    (run.context(before), run.context(after))
}

/// Merges every run into the one before it while `continues` says so; a merged
/// run keeps the class and bracket of its first part.
pub(super) fn compact(runs: &mut BudgetVec<Run>, continues: impl Fn(&Run, &Run) -> bool) {
    let slice = runs.as_mut_slice();
    let mut kept = 0_usize;
    for at in 0..slice.len() {
        let run = slice[at];
        match kept.checked_sub(1) {
            Some(last) if continues(&slice[last], &run) => slice[last].len += run.len,
            _ => {
                slice[kept] = run;
                kept += 1;
            }
        }
    }
    truncate(runs, kept);
}

pub(super) fn truncate(runs: &mut BudgetVec<Run>, len: usize) {
    while runs.len() > len {
        runs.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use BidiClass::*;

    fn run(class: BidiClass, level: u8, isolate: u8) -> Run {
        Run {
            len: 1,
            class,
            level,
            isolate,
            bracket: None,
        }
    }

    #[test]
    fn sequences_follow_isolate_depth() {
        // a LRI b PDI c, with b inside the isolate.
        let runs = [
            run(L, 0, 0),
            run(Lri, 0, 0),
            run(R, 2, 1),
            run(Pdi, 0, 0),
            run(On, 0, 0),
        ];
        assert_eq!(sequence_before(&runs[..1], 0), Some(0));
        assert_eq!(
            sequence_before(&runs[..3], 0),
            Some(1),
            "past the isolate's content"
        );
        assert_eq!(sequence_before(&runs[..2], 1), None);
        // Forward, the PDI is passed over.
        assert_eq!(sequence_after(&runs, 2, 0), Some(4));
        assert_eq!(sequence_after(&runs, 3, 1), None);
        assert_eq!(contexts(&runs, 2), (L, L));
    }
}

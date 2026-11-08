//! Rules W1–W7 on the runs of a paragraph, in FriBidi's two passes: W1 and W2,
//! then W3 to W7.
//!
//! FriBidi's choices, reproduced here:
//! - The last strong class W2 and W7 look back to is kept per isolate depth and
//!   updated from the class each run sees before it, already resolved by the
//!   current pass (an EN that W7 made L counts as strong L).
//! - W1 gives an NSM the class of the run before it in its sequence by merging
//!   it into that run when the two are adjacent at the same level, and sos at a
//!   level change; an NSM whose predecessor at the same level is not adjacent
//!   keeps its class. An NSM right after an isolate formatting character
//!   becomes ON first. When the classes on both sides of the NSM agree, the run
//!   after it joins the merged run too.
//! - W4 changes a separator only when it is a run of one character, between a
//!   number the previous run left behind (after W7 a European number still
//!   counts) and a number after it, and gives it the class of the run before.

use super::class::BidiClass;
use super::runs::{MAX_ISOLATE_DEPTH, Run, contexts, sequence_after, sequence_before, truncate};
use triergon_core::{BudgetVec, Result, WorkBudget};

/// The last strong class seen before a run, per isolate depth.
type LastStrong = [BidiClass; MAX_ISOLATE_DEPTH as usize + 1];

pub(super) fn resolve(
    runs: &mut BudgetVec<Run>,
    base_level: u8,
    work: &mut WorkBudget,
) -> Result<()> {
    let mut last_strong: LastStrong =
        [BidiClass::of_level(base_level); MAX_ISOLATE_DEPTH as usize + 1];
    let kept = marks_and_european_numbers(runs.as_mut_slice(), &mut last_strong, work)?;
    truncate(runs, kept);
    last_strong[0] = BidiClass::of_level(base_level);
    separators_and_terminators(runs.as_mut_slice(), &mut last_strong, work)
}

/// W1 and W2, merging runs as W1 resolves marks. Returns how many runs remain
/// at the start of `runs`.
fn marks_and_european_numbers(
    runs: &mut [Run],
    last_strong: &mut LastStrong,
    work: &mut WorkBudget,
) -> Result<usize> {
    // `runs[..kept]` are resolved, `runs[next..]` are not; between them lie
    // runs already merged away.
    let (mut kept, mut next) = (0, 0);
    while let Some(&run) = runs.get(next) {
        let mut run = run;
        let rest = next + 1;
        next = rest;
        let before = sequence_before(&runs[..kept], run.isolate, work)?;
        let after = sequence_after(runs, rest, run.isolate, work)?;
        let before_class = run.context(before.map(|i| &runs[i]));
        let after_class = run.context(after.map(|i| &runs[i]));
        if before_class.is_strong() {
            last_strong[usize::from(run.isolate)] = before_class;
        }

        if run.class == BidiClass::Nsm {
            let adjacent = kept.checked_sub(1);
            if adjacent.is_some_and(|i| runs[i].class.is_isolate()) {
                run.class = BidiClass::On;
            }
            let merges = match before {
                Some(i) if runs[i].level == run.level => Some(i) == adjacent,
                _ => {
                    run.class = before_class;
                    false
                }
            };
            if merges {
                runs[kept - 1].len += run.len;
            } else {
                runs[kept] = run;
                kept += 1;
            }
            if before_class == after_class && after == Some(rest) && runs[rest].level == run.level {
                runs[kept - 1].len += runs[rest].len;
                next = rest + 1;
            }
            continue;
        }

        if run.class == BidiClass::En && last_strong[usize::from(run.isolate)] == BidiClass::Al {
            run.class = BidiClass::An;
            if after_class == BidiClass::Nsm
                && let Some(i) = after
            {
                runs[i].class = BidiClass::An;
            }
        }
        runs[kept] = run;
        kept += 1;
    }
    Ok(kept)
}

/// W3–W7.
fn separators_and_terminators(
    runs: &mut [Run],
    last_strong: &mut LastStrong,
    work: &mut WorkBudget,
) -> Result<()> {
    // Whether W4 may apply, and the class W4 and W5 see before a run.
    let mut w4 = true;
    let mut before_number = BidiClass::On;
    for at in 0..runs.len() {
        let run = runs[at];
        let (before_class, after_class) = contexts(runs, at, work)?;
        let last = &mut last_strong[usize::from(run.isolate)];
        if before_class.is_strong() {
            *last = before_class;
        }

        let mut class = run.class;
        if class == BidiClass::Al {
            runs[at].class = BidiClass::R;
            w4 = true;
            before_number = BidiClass::On;
            continue;
        }
        if w4
            && run.len == 1
            && matches!(class, BidiClass::Es | BidiClass::Cs)
            && before_number.is_number()
            && before_number == after_class
            && (before_number == BidiClass::En || class == BidiClass::Cs)
        {
            class = before_class;
        }
        w4 = true;
        if class == BidiClass::Et
            && (before_number == BidiClass::En || after_class == BidiClass::En)
        {
            class = BidiClass::En;
            w4 = false;
        }

        // The class the next run sees before it: this run's at the same level.
        let after_level = runs.get(at + 1).map(|n| n.level);
        if class == BidiClass::En && *last == BidiClass::L {
            runs[at].class = BidiClass::L;
            before_number = if after_level == Some(run.level) {
                BidiClass::En
            } else {
                BidiClass::On
            };
        } else {
            // W6.
            let class = if class.is_number_punctuation() {
                BidiClass::On
            } else {
                class
            };
            runs[at].class = class;
            before_number = match after_level {
                Some(level) if level == run.level => class,
                Some(level) => BidiClass::of_level(level.max(run.level)),
                None => BidiClass::of_level(run.level),
            };
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use BidiClass::*;
    use triergon_core::Budget;

    fn resolved(classes: &[(BidiClass, u32)], level: u8) -> Vec<(BidiClass, u32)> {
        let budget = Budget::new(1 << 20).expect("budget");
        let mut runs = BudgetVec::new(&budget);
        for &(class, len) in classes {
            let run = Run {
                len,
                class,
                level,
                isolate: 0,
                bracket: None,
            };
            runs.try_push(run).map_err(|e| e.error).expect("push");
        }
        resolve(&mut runs, level, &mut WorkBudget::new(1000)).expect("work");
        runs.as_slice().iter().map(|r| (r.class, r.len)).collect()
    }

    #[test]
    fn marks_take_the_class_before() {
        assert_eq!(resolved(&[(R, 1), (Nsm, 2)], 0), [(R, 3)]);
        assert_eq!(resolved(&[(Nsm, 1), (L, 1)], 1), [(R, 1), (L, 1)]);
        // Both sides agree: the run after joins as well.
        assert_eq!(resolved(&[(R, 1), (Nsm, 1), (R, 1)], 0), [(R, 3)]);
    }

    #[test]
    fn numbers() {
        // W2 and W3: EN after AL is AN.
        assert_eq!(resolved(&[(Al, 1), (En, 1)], 0), [(R, 1), (An, 1)]);
        // W4: one separator between European numbers; W7 then makes them L.
        assert_eq!(
            resolved(&[(En, 1), (Es, 1), (En, 1)], 0),
            [(L, 1), (L, 1), (L, 1)]
        );
        // Two separators are not changed.
        assert_eq!(
            resolved(&[(En, 1), (Cs, 2), (En, 1)], 1),
            [(En, 1), (On, 2), (En, 1)]
        );
        // W5: terminators next to European numbers.
        assert_eq!(resolved(&[(Et, 2), (En, 1)], 1), [(En, 2), (En, 1)]);
        // W6.
        assert_eq!(resolved(&[(R, 1), (Et, 1)], 1), [(R, 1), (On, 1)]);
    }
}

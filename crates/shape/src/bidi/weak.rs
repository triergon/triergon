//! Rules W1 to W7 on the classes of one isolating run sequence.
//!
//! FriBidi's choices, reproduced here:
//! - Marks that W1 makes ON after a neutral (other than an isolate initiator
//!   or PDI) join the neutral's run, and so does the neutral run after them:
//!   a bracket there loses its bracket property, and where the run before the
//!   marks is a bracket that N0 resolves, the marks and the run after them
//!   take its direction.
//! - The embedding and override characters, which are only in a sequence
//!   after a paragraph separator, are strong classes that are neither L nor
//!   AL for W2 and W7.
//! - Marks after a PDI join its run, which looking forward W4 and W5 pass
//!   over when the PDI closes an overflowed isolate (in a sequence, the only
//!   PDIs after a character that is not an isolate initiator).

use super::charge;
use super::class::{BidiClass, Bracket, fill};
use triergon_core::{BudgetVec, Result, WorkBudget};

/// The bracket property of a sequence's characters, for W1 to record which
/// brackets lose it: `brackets` for the paragraph, `positions` of the
/// sequence's characters in it, and `absorbed`, set for each character that
/// joins the run of the neutral before the marks before it (empty until one
/// does, as long as the paragraph).
pub(super) struct Runs<'a> {
    pub brackets: &'a [Option<Bracket>],
    pub positions: &'a [u32],
    pub absorbed: &'a mut BudgetVec<bool>,
}

/// Rules W1 to W7, each over the whole sequence before the next. `sos` is the
/// sequence's start-of-sequence class; `runs` is given when brackets pair.
pub(super) fn resolve(
    types: &mut [BidiClass],
    sos: BidiClass,
    mut runs: Option<Runs<'_>>,
    work: &mut WorkBudget,
) -> Result<()> {
    use BidiClass::{
        Al, An, Cs, En, Es, Et, Fsi, L, Lre, Lri, Lro, Nsm, On, Pdi, R, Rle, Rli, Rlo,
    };
    charge(work, 3 * types.len())?;
    // W1 to W3 in one pass: a mark takes the class of the character before it
    // as W1 leaves it, and W2 and W3 then change it as they change that class.
    // W1: a mark takes the class of the character before it, ON after an
    // isolate initiator, and sos at the start; after a PDI it joins the PDI's
    // run, a neutral as ON is.
    // W2 and W3: a European number after an Arabic letter (the last strong
    // class before it, or sos) is an Arabic number; Arabic letters become R.
    let mut before = sos;
    let mut strong = sos;
    let mut at = 0;
    while at < types.len() {
        let (class, end) = if types[at] == Nsm {
            let end = types[at..]
                .iter()
                .position(|&c| c != Nsm)
                .map_or(types.len(), |n| at + n);
            let class = match before {
                Pdi => Pdi,
                Lri | Rli | Fsi => On,
                _ => before,
            };
            if let Some(runs) = runs.as_mut().filter(|_| before == On) {
                absorb(types, end, runs)?;
            }
            (class, end)
        } else {
            (types[at], at + 1)
        };
        before = class;
        let class = match class {
            L | R | Lre | Lro | Rle | Rlo => {
                strong = class;
                class
            }
            Al => {
                strong = Al;
                R
            }
            En if strong == Al => An,
            _ => class,
        };
        fill(&mut types[at..end], class);
        at = end;
    }
    // W4: one separator between two numbers of a type it separates takes
    // their type.
    for at in 1..types.len().saturating_sub(1) {
        let previous = types[at - 1];
        let Some(next) = following(types, at + 1) else {
            continue;
        };
        types[at] = match (types[at], previous, next) {
            (Es | Cs, En, En) => En,
            (Cs, An, An) => An,
            (class, _, _) => class,
        };
    }
    // W5 to W7 in one pass. W5: terminators next to a European number become
    // European numbers. W6: the remaining separators and terminators are
    // neutral. W7: a European number after L (the last strong class before it,
    // or sos) is L.
    let mut strong = sos;
    // The class before W7 of the character before.
    let mut before = None;
    let mut at = 0;
    while at < types.len() {
        let (class, end) = match types[at] {
            Et => {
                let end = types[at..]
                    .iter()
                    .position(|&c| c != Et)
                    .map_or(types.len(), |n| at + n);
                let touches = before == Some(En) || following(types, end) == Some(En);
                (if touches { En } else { On }, end)
            }
            Es | Cs => (On, at + 1),
            class => (class, at + 1),
        };
        if matches!(class, L | R | Lre | Lro | Rle | Rlo) {
            strong = class;
        }
        fill(
            &mut types[at..end],
            if class == En && strong == L { L } else { class },
        );
        before = Some(class);
        at = end;
    }
    Ok(())
}

/// The first class from `at` on that is not PDI.
fn following(types: &[BidiClass], at: usize) -> Option<BidiClass> {
    types
        .get(at..)?
        .iter()
        .copied()
        .find(|&c| c != BidiClass::Pdi)
}

/// The neutral run at `at`, after marks that joined a neutral run, joins that
/// run too: a bracket alone, or the neutrals up to the next bracket.
fn absorb(types: &[BidiClass], at: usize, runs: &mut Runs<'_>) -> Result<()> {
    let bracket = |at: usize| runs.brackets[runs.positions[at] as usize].is_some();
    if types.get(at) != Some(&BidiClass::On) {
        return Ok(());
    }
    let end = if bracket(at) {
        at + 1
    } else {
        (at..types.len())
            .find(|&n| types[n] != BidiClass::On || bracket(n))
            .unwrap_or(types.len())
    };
    if runs.absorbed.is_empty() {
        runs.absorbed.try_resize(runs.brackets.len(), false)?;
    }
    for &position in &runs.positions[at..end] {
        runs.absorbed[position as usize] = true;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use BidiClass::*;

    fn weak(types: &[BidiClass], sos: BidiClass) -> Vec<BidiClass> {
        let mut types = types.to_vec();
        let mut work = WorkBudget::new(1000);
        resolve(&mut types, sos, None, &mut work).unwrap();
        types
    }

    #[test]
    fn marks_and_numbers() {
        assert_eq!(weak(&[Nsm, Al, Nsm], R), [R, R, R]);
        assert_eq!(weak(&[Lri, Nsm], L), [Lri, On]);
        assert_eq!(weak(&[Al, En, Cs, En], L), [R, An, An, An]);
        assert_eq!(weak(&[En, Es, En, Cs, Cs, En], R), [En, En, En, On, On, En]);
        assert_eq!(weak(&[Et, Et, En, Et], R), [En, En, En, En]);
        assert_eq!(weak(&[L, Et, En, R, En], R), [L, L, L, R, En]);
        assert_eq!(weak(&[An, Cs, En], R), [An, On, En]);
    }
}

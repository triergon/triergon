//! Rules BD9, P2 and P3, and X1 to X9: matching isolates, the paragraph level
//! and the explicit embedding levels.

use super::charge;
use super::class::{BidiClass, Level};
use triergon_core::{BudgetVec, Result, WorkBudget};

/// The partner of a character that has none.
pub(super) const NONE: u32 = u32::MAX;

/// The deepest explicit embedding level (BD2).
const MAX_DEPTH: u8 = 125;

/// Rule BD9: for every isolate initiator its matching PDI, and for every PDI
/// that matches one its initiator; [`NONE`] for every other character.
pub(super) fn match_isolates(
    classes: &[BidiClass],
    partner: &mut BudgetVec<u32>,
    work: &mut WorkBudget,
) -> Result<()> {
    charge(work, classes.len())?;
    partner.clear();
    partner.try_resize(classes.len(), NONE)?;
    // The open initiators are chained through their own entries, which hold
    // the initiator opened before them until their PDI is found.
    let mut open = NONE;
    for (i, &class) in classes.iter().enumerate() {
        let i = i as u32;
        if class.is_isolate_initiator() {
            partner[i as usize] = open;
            open = i;
        } else if class == BidiClass::Pdi && open != NONE {
            let initiator = open;
            open = partner[initiator as usize];
            partner[initiator as usize] = i;
            partner[i as usize] = initiator;
        }
    }
    // Initiators left open have no matching PDI.
    while open != NONE {
        let next = partner[open as usize];
        partner[open as usize] = NONE;
        open = next;
    }
    Ok(())
}

/// Rules P2 and P3 on `classes[range]`: whether the first strong letter outside
/// isolates is right-to-left, `None` without one. An isolate initiator without
/// a matching PDI isolates the rest of the text.
pub(super) fn first_strong(
    classes: &[BidiClass],
    partner: &[u32],
    mut at: usize,
    end: usize,
    work: &mut WorkBudget,
) -> Result<Option<bool>> {
    let mut steps = 0;
    let mut found = None;
    while at < end {
        steps += 1;
        match classes[at] {
            BidiClass::L => found = Some(false),
            BidiClass::R | BidiClass::Al => found = Some(true),
            class if class.is_isolate_initiator() => match partner[at] {
                NONE => break,
                pdi => at = pdi as usize,
            },
            _ => {}
        }
        if found.is_some() {
            break;
        }
        at += 1;
    }
    charge(work, steps)?;
    Ok(found)
}

/// The embedding state: the current one, and those the embedding and isolate
/// characters save on the stack for the matching PDF or PDI to restore.
#[derive(Clone, Copy, Debug, Default)]
struct Status {
    level: u8,
    /// The class the override gives characters, if there is one.
    overrides: Option<BidiClass>,
    /// Whether an isolate initiator set this level, so that a PDF leaves it.
    isolate: bool,
    /// Whether an isolate initiator saved this state, so that its PDI stops
    /// popping here.
    saved_by_isolate: bool,
    /// The isolate depth when it was saved.
    depth: u8,
}

/// The deepest isolate depth tracked: deeper isolates share it.
pub(super) const MAX_ISOLATE_DEPTH: u8 = 124;

/// Rules X1 to X9 over the first `end` characters of a paragraph, up to its
/// first paragraph separator: the explicit level of every character, the class
/// the overrides and FSI resolution give it (BN in `types` for the characters
/// X9 removes) and its isolate depth. The separator is at level 0.
///
/// FriBidi's choices, reproduced here, change only text that reaches the
/// deepest level, 125, except the last:
/// - An isolate initiator whose level fits raises the level even when an
///   overflowed embedding or isolate keeps it off the stack, and then leaves
///   the override as it was; kept off by an embedding, it counts as one more
///   overflowed embedding for the PDFs after it. The PDI that closes it pops
///   every embedding the stack holds down to the state an isolate saved, and
///   the stack may hold none.
/// - A PDF pops an embedding even inside an overflowed isolate, and returns to
///   the isolate depth the embedding saved.
/// - Every isolate initiator, overflowed or not, deepens the isolate depth of
///   what follows it, up to [`MAX_ISOLATE_DEPTH`]. A PDI that closes a valid
///   isolate returns to the depth of the initiator whose saved state it pops
///   back to, 0 where it empties the stack; the PDI of an overflowed isolate
///   leaves the depth as it is.
/// - A PDI keeps its class inside an override.
/// - A PDI that closes no isolate becomes ON.
#[allow(clippy::too_many_arguments)]
pub(super) fn levels(
    classes: &[BidiClass],
    end: usize,
    partner: &[u32],
    paragraph_level: u8,
    types: &mut [BidiClass],
    levels: &mut [Level],
    depths: &mut [u8],
    work: &mut WorkBudget,
) -> Result<()> {
    charge(work, end)?;
    let mut stack = [Status::default(); MAX_DEPTH as usize + 1];
    let mut saved = 0;
    let mut current = Status {
        level: paragraph_level,
        ..Status::default()
    };
    let mut depth = 0u8;
    let mut valid_isolates = 0u32;
    let mut overflow_isolates = 0u32;
    let mut overflow_embeddings = 0u32;
    for i in 0..end {
        let class = classes[i];
        levels[i] = Level(current.level);
        depths[i] = depth;
        match class {
            BidiClass::Rle | BidiClass::Lre | BidiClass::Rlo | BidiClass::Lro => {
                let rtl = matches!(class, BidiClass::Rle | BidiClass::Rlo);
                let level = next_level(current.level, rtl);
                if level <= MAX_DEPTH && overflow_isolates == 0 && overflow_embeddings == 0 {
                    stack[saved] = Status { depth, ..current };
                    saved += 1;
                    current = Status {
                        level,
                        overrides: match class {
                            BidiClass::Rlo => Some(BidiClass::R),
                            BidiClass::Lro => Some(BidiClass::L),
                            _ => None,
                        },
                        ..Status::default()
                    };
                } else if overflow_isolates == 0 {
                    overflow_embeddings += 1;
                }
                types[i] = BidiClass::Bn;
            }
            BidiClass::Rli | BidiClass::Lri | BidiClass::Fsi => {
                if let Some(overrides) = current.overrides {
                    types[i] = overrides;
                }
                let rtl = match class {
                    BidiClass::Rli => true,
                    BidiClass::Lri => false,
                    _ => {
                        let end = match partner[i] {
                            NONE => classes.len(),
                            pdi => pdi as usize,
                        };
                        first_strong(classes, partner, i + 1, end, work)? == Some(true)
                    }
                };
                let level = next_level(current.level, rtl);
                if level <= MAX_DEPTH {
                    valid_isolates += 1;
                    if overflow_isolates == 0 && overflow_embeddings == 0 {
                        stack[saved] = Status {
                            saved_by_isolate: true,
                            depth,
                            ..current
                        };
                        saved += 1;
                        current = Status {
                            level,
                            isolate: true,
                            ..Status::default()
                        };
                    } else {
                        if overflow_isolates == 0 {
                            overflow_embeddings += 1;
                        }
                        current.level = level;
                    }
                } else {
                    overflow_isolates += 1;
                }
                depth = (depth + 1).min(MAX_ISOLATE_DEPTH);
            }
            BidiClass::Pdi => {
                if overflow_isolates > 0 {
                    overflow_isolates -= 1;
                } else if valid_isolates > 0 {
                    overflow_embeddings = 0;
                    depth = 0;
                    while saved > 0 {
                        saved -= 1;
                        current = stack[saved];
                        if current.saved_by_isolate {
                            depth = current.depth;
                            break;
                        }
                    }
                    current.saved_by_isolate = false;
                    valid_isolates -= 1;
                } else {
                    types[i] = BidiClass::On;
                }
                levels[i] = Level(current.level);
                depths[i] = depth;
            }
            BidiClass::Pdf => {
                if overflow_embeddings > 0 {
                    overflow_embeddings -= 1;
                } else if saved > 0 && !current.isolate {
                    saved -= 1;
                    current = stack[saved];
                    depth = current.depth;
                }
                types[i] = BidiClass::Bn;
            }
            BidiClass::B => levels[i] = Level(0),
            BidiClass::Bn => {}
            _ => {
                if let Some(overrides) = current.overrides {
                    types[i] = overrides;
                }
            }
        }
    }
    Ok(())
}

/// The least odd (`rtl`) or even level above `level`.
const fn next_level(level: u8, rtl: bool) -> u8 {
    if rtl {
        (level + 1) | 1
    } else {
        (level + 2) & !1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use BidiClass::*;
    use triergon_core::Budget;

    #[test]
    fn isolates_match_by_nesting() {
        let budget = Budget::new(1 << 16).unwrap();
        let mut partner = BudgetVec::new(&budget);
        let mut work = WorkBudget::new(1000);
        let classes = [Lri, Rli, Pdi, L, Fsi, Pdi, Pdi, Pdi];
        match_isolates(&classes, &mut partner, &mut work).unwrap();
        assert_eq!(partner.as_slice(), [6, 2, 1, NONE, 5, 4, 0, NONE]);
        match_isolates(&[Lri, Lri, Pdi], &mut partner, &mut work).unwrap();
        assert_eq!(partner.as_slice(), [NONE, 2, 1]);
    }

    #[test]
    fn first_strong_skips_isolates() {
        let budget = Budget::new(1 << 16).unwrap();
        let mut partner = BudgetVec::new(&budget);
        let mut work = WorkBudget::new(1000);
        let classes = [En, Rli, L, Pdi, Al, L];
        match_isolates(&classes, &mut partner, &mut work).unwrap();
        assert_eq!(
            first_strong(&classes, &partner, 0, 6, &mut work),
            Ok(Some(true))
        );
        let classes = [On, Lri, R];
        match_isolates(&classes, &mut partner, &mut work).unwrap();
        assert_eq!(first_strong(&classes, &partner, 0, 3, &mut work), Ok(None));
    }

    #[test]
    fn embeddings_stop_at_125() {
        let budget = Budget::new(1 << 16).unwrap();
        let mut partner = BudgetVec::new(&budget);
        let mut work = WorkBudget::new(10_000);
        let mut classes = vec![Rle; 70];
        classes.extend([Lri, L, Pdi, L]);
        match_isolates(&classes, &mut partner, &mut work).unwrap();
        let mut types = classes.clone();
        let mut out = vec![Level(0); classes.len()];
        let mut depths = vec![0; classes.len()];
        levels(
            &classes,
            classes.len(),
            &partner,
            0,
            &mut types,
            &mut out,
            &mut depths,
            &mut work,
        )
        .unwrap();
        // 63 RLEs reach level 125; the isolate overflows, yet deepens what
        // follows until its PDI.
        assert_eq!(out[70].0, 125);
        assert_eq!(out[71].0, 125);
        assert_eq!(types[69], Bn);
        assert_eq!(types[70], Lri);
        assert_eq!(depths[70..], [0, 1, 1, 1]);
    }
}

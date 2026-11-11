//! Resolving one paragraph: the explicit rules, the isolating run sequences,
//! then the levels of the characters X9 removed and the paragraph's part of
//! rule L1.

use super::charge;
use super::class::{BaseDirection, BidiClass, Bracket, Level};
use super::{explicit, sequence};
use crate::Direction;
use triergon_core::{Budget, BudgetVec, Result, WorkBudget};

/// A resolver's working storage.
#[derive(Debug)]
pub(super) struct Storage {
    /// The classes as the rules resolve them.
    types: BudgetVec<BidiClass>,
    /// [`explicit::match_isolates`]'s partners.
    partner: BudgetVec<u32>,
    /// The isolate depth of each character.
    depths: BudgetVec<u8>,
    sequences: sequence::Storage,
}

impl Storage {
    pub(super) fn new(budget: &Budget) -> Self {
        Self {
            types: BudgetVec::new(budget),
            partner: BudgetVec::new(budget),
            depths: BudgetVec::new(budget),
            sequences: sequence::Storage::new(budget),
        }
    }
}

/// Resolves the paragraph of `classes`, which [`super::Resolver::resolve`] has
/// checked.
pub(super) fn resolve(
    storage: &mut Storage,
    classes: &[BidiClass],
    brackets: Option<&[Option<Bracket>]>,
    base: BaseDirection,
    levels: &mut [Level],
    work: &mut WorkBudget,
) -> Result<Direction> {
    // The explicit rules stop after the first paragraph separator.
    let end = classes
        .iter()
        .position(|&c| c == BidiClass::B)
        .map_or(classes.len(), |at| at + 1);
    // Isolates match, and the first strong letter is found, past it.
    explicit::match_isolates(classes, &mut storage.partner, work)?;
    let rtl = match base {
        BaseDirection::LeftToRight => false,
        BaseDirection::RightToLeft => true,
        BaseDirection::Auto => {
            explicit::first_strong(classes, &storage.partner, 0, classes.len(), work)? == Some(true)
        }
    };
    let level = u8::from(rtl);
    if classes.is_empty() {
        return Ok(Level(level).direction());
    }
    charge(work, classes.len())?;
    storage.types.clear();
    storage.types.try_extend_from_slice(classes)?;
    storage.depths.clear();
    storage.depths.try_resize(classes.len(), 0)?;
    levels[end..].fill(Level(0));
    explicit::levels(
        classes,
        end,
        &storage.partner,
        level,
        &mut storage.types.as_mut_slice()[..end],
        &mut levels[..end],
        &mut storage.depths.as_mut_slice()[..end],
        work,
    )?;
    // Separators right after the first stay at its depth.
    let depth = storage.depths[end - 1];
    for (class, d) in classes[end..]
        .iter()
        .zip(&mut storage.depths.as_mut_slice()[end..])
    {
        if *class != BidiClass::B {
            break;
        }
        *d = depth;
    }
    let paragraph = sequence::Paragraph {
        classes,
        brackets,
        partner: &storage.partner,
        level,
        depths: &storage.depths,
        explicit_end: end,
    };
    sequence::resolve(
        &mut storage.sequences,
        &paragraph,
        storage.types.as_mut_slice(),
        levels,
        work,
    )?;
    // The characters X9 removed take the level of the character before them.
    let mut before = Level(level);
    for (at, l) in levels[..end].iter_mut().enumerate() {
        if storage.types[at] == BidiClass::Bn {
            *l = before;
        }
        before = *l;
    }
    reset_separators(classes, Level(level), levels);
    Ok(Level(level).direction())
}

/// The paragraph's part of rule L1: segment and paragraph separators, and the
/// whitespace and formatting characters before one or at the end of the text,
/// take the paragraph level.
fn reset_separators(classes: &[BidiClass], level: Level, levels: &mut [Level]) {
    let mut trailing = true;
    for (&class, l) in classes.iter().zip(levels.iter_mut()).rev() {
        match class {
            BidiClass::S | BidiClass::B => {
                *l = level;
                trailing = true;
            }
            class
                if trailing
                    && (class == BidiClass::Ws
                        || class.is_isolate_control()
                        || class.is_removed_by_x9()) =>
            {
                *l = level;
            }
            _ => trailing = false,
        }
    }
}

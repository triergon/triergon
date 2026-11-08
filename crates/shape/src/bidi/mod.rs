//! The Unicode bidirectional algorithm (UAX #9) as FriBidi 1.0.17 computes it
//! for libass, and libass's simple shaping.
//!
//! The steps follow libass's `ass_shaper_shape` and `ass_shaper_reorder`:
//!
//! 1. [`BidiClass::of`] every character, and [`Bracket::of`] each when bracket
//!    pairs take part (libass's bidi-brackets feature).
//! 2. [`Resolver::resolve`] each paragraph, or each piece libass resolves on its
//!    own; [`paragraphs`] splits text after every paragraph separator.
//! 3. [`reorder_line`] each line with its paragraph's resolved direction.
//!
//! [`shape_simple`] is the joining, Arabic presentation forms and mirroring of
//! libass's simple shaping level, between steps 2 and 3.
//!
//! Results equal FriBidi's, including where it reads UAX #9 its own way; each
//! module lists those choices. In short: rules X9 to I2 work on runs of
//! characters as FriBidi forms them; the characters X9 removes return with the
//! level of the character before them; the explicit levels stop at 125 with
//! FriBidi's overflow counting; brackets pair within 63 open brackets per
//! isolate depth; processing stops at a paragraph separator inside the text.

mod arabic;
mod class;
mod explicit;
mod levels;
mod line;
mod neutral;
mod runs;
mod weak;

use std::ops::Range;

pub use arabic::shape_simple;
pub use class::{BaseDirection, BidiClass, Bracket, Level};
pub use line::{reorder_line, reorder_line_with_limits};

use crate::Direction;
use crate::unicode::fribidi;
use runs::Run;
use triergon_core::{Budget, BudgetVec, Error, Result, WorkBudget};

/// Limits of resolving one paragraph or reordering one line: its work is
/// bounded by `work_per_char` units for each character, and at least
/// `minimum_work` in all.
///
/// The work is the runs each rule looks at: once per pass for most rules, and
/// again for each run whose context lies past runs of deeper isolates (at most
/// once per enclosing isolate depth, up to 125), for each bracket pair that
/// encloses a run (up to 63 per isolate depth) and for each level line
/// reordering reverses at (up to 126). The default, 4096 units per character
/// and 65536 in all, as for shaping, is 19 times the most any text of the bidi
/// and shaping corpora needs (216 units per character, for a seeded random
/// paragraph of every class); it stops only text that nests brackets and
/// isolates to multiply the work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Work units per character.
    pub work_per_char: u64,
    /// The work units of a paragraph or line of any length.
    pub minimum_work: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            work_per_char: 4096,
            minimum_work: 65536,
        }
    }
}

impl Limits {
    fn budget(self, chars: usize) -> WorkBudget {
        let chars = u64::try_from(chars).unwrap_or(u64::MAX);
        WorkBudget::new(
            chars
                .saturating_mul(self.work_per_char)
                .max(self.minimum_work),
        )
    }
}

/// Charges `units` before the work is done; running out is `Error::Policy`.
fn charge(work: &mut WorkBudget, units: usize) -> Result<()> {
    crate::work::charge_for(work, units, "bidi work")
}

/// The Bidi_Mirroring_Glyph of `c`, if it has one, as `fribidi_get_mirror_char`
/// gives it.
#[must_use]
pub fn mirror(c: char) -> Option<char> {
    fribidi::mirror(u32::from(c)).and_then(char::from_u32)
}

/// The paragraphs of a text: ranges that end after each paragraph separator
/// (class B) and at the end of the text.
pub fn paragraphs(classes: &[BidiClass]) -> impl Iterator<Item = Range<usize>> + '_ {
    let mut start = 0;
    std::iter::from_fn(move || {
        let rest = classes.get(start..).filter(|rest| !rest.is_empty())?;
        let len = rest
            .iter()
            .position(|&c| c == BidiClass::B)
            .map_or(rest.len(), |at| at + 1);
        let paragraph = start..start + len;
        start += len;
        Some(paragraph)
    })
}

/// Resolves embedding levels, one paragraph at a time. It keeps its working
/// storage, charged to its budget, from one paragraph to the next.
#[derive(Debug)]
pub struct Resolver {
    runs: BudgetVec<Run>,
    fsi_rtl: BudgetVec<bool>,
    open_isolates: BudgetVec<Option<u32>>,
    brackets: neutral::Brackets,
    limits: Limits,
    /// The work of the last paragraph.
    work: WorkBudget,
}

impl Resolver {
    /// A resolver with the default [`Limits`] whose storage is charged to
    /// `budget`; it allocates nothing until the first paragraph.
    #[must_use]
    pub fn new(budget: &Budget) -> Self {
        Self::with_limits(budget, Limits::default())
    }

    /// A resolver whose paragraphs are each bounded by `limits`.
    #[must_use]
    pub fn with_limits(budget: &Budget, limits: Limits) -> Self {
        Self {
            runs: BudgetVec::new(budget),
            fsi_rtl: BudgetVec::new(budget),
            open_isolates: BudgetVec::new(budget),
            brackets: neutral::Brackets::new(budget),
            limits,
            work: WorkBudget::new(0),
        }
    }

    /// The work units the last paragraph spent of the [`Limits`].
    #[must_use]
    pub fn work_spent(&self) -> u64 {
        self.work.spent()
    }

    /// Resolves the embedding level of every character of one paragraph, as
    /// `fribidi_get_par_embedding_levels_ex` does, and returns the paragraph's
    /// direction (left-to-right for [`BaseDirection::Auto`] without a strong
    /// letter).
    ///
    /// `brackets`, one per character, makes paired brackets take part (rule
    /// N0); only brackets of class ON pair. The levels are those before line
    /// reordering: [`reorder_line`] completes L1 for each line.
    ///
    /// # Errors
    ///
    /// `Error::Invalid` if the slices differ in length, `Error::Policy` for a
    /// paragraph of 2³² characters or more or when the work exceeds the
    /// resolver's [`Limits`], and the budget's errors.
    pub fn resolve(
        &mut self,
        classes: &[BidiClass],
        brackets: Option<&[Option<Bracket>]>,
        base: BaseDirection,
        levels: &mut [Level],
    ) -> Result<Direction> {
        if levels.len() != classes.len() || brackets.is_some_and(|b| b.len() != classes.len()) {
            return Err(Error::Invalid("bidi paragraph lengths differ"));
        }
        if u32::try_from(classes.len()).is_err() {
            return Err(Error::Policy("bidi paragraph length"));
        }
        let direction = match base {
            BaseDirection::LeftToRight => Direction::LeftToRight,
            BaseDirection::RightToLeft => Direction::RightToLeft,
            BaseDirection::Auto => {
                explicit::first_strong(classes).unwrap_or(Direction::LeftToRight)
            }
        };
        let base_level = Level::of(direction).0;
        let work = &mut self.work;
        *work = self.limits.budget(classes.len());

        // The passes over every character: P2, X5c, X1–X9 and the levels.
        charge(work, 4 * classes.len())?;
        explicit::first_strong_isolates(classes, &mut self.fsi_rtl, &mut self.open_isolates)?;
        explicit::runs(
            classes,
            brackets,
            base_level,
            self.fsi_rtl.as_slice(),
            &mut self.runs,
        )?;
        weak::resolve(&mut self.runs, base_level, work)?;
        charge(work, self.runs.len())?;
        runs::compact(&mut self.runs, Run::continues_neutrally_with);
        if self.runs.as_slice().iter().any(|r| r.bracket.is_some()) {
            self.brackets.resolve(self.runs.as_mut_slice(), work)?;
            for run in self.runs.as_mut_slice() {
                run.bracket = None;
            }
            charge(work, self.runs.len())?;
            runs::compact(&mut self.runs, Run::continues_neutrally_with);
        }
        neutral::resolve(self.runs.as_mut_slice(), work)?;
        levels::write(self.runs.as_slice(), classes, base_level, levels);
        Ok(direction)
    }
}

#[cfg(test)]
mod tests;

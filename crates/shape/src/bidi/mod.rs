//! The Unicode bidirectional algorithm (UAX #9, revision 44, for Unicode
//! 14.0.0) as libass resolves text with it, and libass's simple shaping.
//!
//! The rules follow the Unicode Consortium's C reference implementation of the
//! algorithm for that version, which pass BidiTest.txt and
//! BidiCharacterTest.txt. libass resolves text with FriBidi 1.0.17, and where
//! FriBidi's results differ from UAX #9's on text outside those tests, the
//! results here are FriBidi's: each such deviation is named in the module of
//! the rule it changes.
//!
//! The steps are libass's:
//!
//! 1. [`BidiClass::of`] every character, and [`Bracket::of`] each when bracket
//!    pairs take part.
//! 2. [`Resolver::resolve`] each paragraph, or each piece of text resolved on
//!    its own; [`paragraphs`] splits text after every paragraph separator.
//! 3. [`reorder_line`] each line with its paragraph's resolved direction.
//!
//! [`shape_simple`] is the cursive joining, Arabic presentation forms and
//! mirroring of libass's simple shaping, between steps 2 and 3.

mod arabic;
mod class;
mod explicit;
mod line;
mod neutral;
mod paragraph;
mod sequence;
mod weak;

use std::ops::Range;

pub use arabic::shape_simple;
pub use class::{BaseDirection, BidiClass, Bracket, JoiningType, Level};
pub use line::{reorder_line, reorder_line_with_limits};

use crate::Direction;
use crate::unicode::bidi as data;
use triergon_core::{Budget, Error, Result, WorkBudget};

/// Limits of resolving one paragraph or reordering one line: its work is
/// bounded by `work_per_char` units for each character, and at least
/// `minimum_work` in all.
///
/// The work is the characters each step looks at. Resolving a paragraph looks
/// at each character in about twenty passes, and N0 again for each bracket pair
/// that encloses it (up to 63 per isolate depth, at every depth) and for each
/// opening bracket still unpaired when a closing one is looked up; reordering a
/// line looks at each character once for each level it reverses at (up to
/// 126). The default, 4096 units per character and 65536 in all, as for
/// shaping, is 259 times the most a paragraph of the bidi corpora needs (15.8
/// per character). Text that nests brackets in deep isolates needs thousands of
/// units per character.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Work units for each character.
    pub work_per_char: u64,
    /// Work units for any text, however short.
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
    /// The work allowed for `chars` characters.
    fn work(self, chars: usize) -> WorkBudget {
        let chars = u64::try_from(chars).unwrap_or(u64::MAX);
        WorkBudget::new(
            self.work_per_char
                .saturating_mul(chars)
                .max(self.minimum_work),
        )
    }
}

/// Charges `units` before the work is done; running out is `Error::Policy`.
fn charge(work: &mut WorkBudget, units: usize) -> Result<()> {
    let units = u64::try_from(units).unwrap_or(u64::MAX);
    if work.try_charge(units) {
        Ok(())
    } else {
        Err(Error::Policy("bidi work"))
    }
}

/// The Bidi_Mirroring_Glyph of `c`, if it has one (BidiMirroring.txt).
#[must_use]
pub fn mirror(c: char) -> Option<char> {
    data::mirror(u32::from(c)).and_then(char::from_u32)
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
    storage: paragraph::Storage,
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
            storage: paragraph::Storage::new(budget),
            limits,
            work: WorkBudget::new(0),
        }
    }

    /// The work units the last paragraph spent of the [`Limits`].
    #[must_use]
    pub fn work_spent(&self) -> u64 {
        self.work.spent()
    }

    /// Resolves the embedding level of every character of one paragraph and
    /// returns the paragraph's direction (left-to-right for
    /// [`BaseDirection::Auto`] without a strong letter).
    ///
    /// `brackets`, one per character, makes paired brackets take part (rule
    /// N0); only brackets of class ON pair. The levels are those before line
    /// reordering: [`reorder_line`] completes rule L1 for each line.
    ///
    /// As in FriBidi, the explicit rules stop at the first paragraph separator:
    /// it and the characters after it keep their classes, formatting characters
    /// included, at explicit level 0 and isolate depth 0, and the implicit rules
    /// resolve them as if the paragraph's level were 0 there. Isolates still
    /// match, and [`BaseDirection::Auto`] still finds the first strong letter,
    /// past it. Where an isolate opened before the separator is closed after
    /// it, FriBidi resolves the text between apart from the isolate and this
    /// resolver does not. [`paragraphs`] splits text into the pieces to resolve
    /// one by one.
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
            return Err(Error::Invalid("bidi input lengths differ"));
        }
        if u32::try_from(classes.len()).is_err() {
            return Err(Error::Policy("bidi paragraph length"));
        }
        self.work = self.limits.work(classes.len());
        paragraph::resolve(
            &mut self.storage,
            classes,
            brackets,
            base,
            levels,
            &mut self.work,
        )
    }
}

#[cfg(test)]
mod tests;

//! The work bounds of planning and of one shaping call, over core's
//! [`WorkBudget`].
//!
//! Every step whose count the text or the font decides is charged before it is
//! done: each lookup's visit of a glyph, each subtable and rule tried, each
//! glyph a match skips over, each nested lookup, each glyph a buffer edit moves
//! or a cluster merge touches, and each link of an attachment chain followed.
//! Planning charges each feature variation condition evaluated, each language
//! system or feature record searched, each lookup a feature lists and each
//! coverage record a lookup's digest reads. Loops bounded by a constant (the 64
//! glyphs of one match, the 64 nesting levels) are not charged separately.

use crate::face::Face;
use triergon_core::{Error, Result, WorkBudget};

/// Limits of one shaping call: its work is bounded by `work_per_char` units for
/// each character of the item, and at least `minimum_work` in all, and its
/// buffer by `glyphs_per_char` glyphs for each character, and at least
/// `minimum_glyphs`. The defaults are HarfBuzz's operation and length limits
/// (`HB_BUFFER_MAX_OPS_FACTOR`, `HB_BUFFER_MAX_OPS_MIN`,
/// `HB_BUFFER_MAX_LEN_FACTOR` and `HB_BUFFER_MAX_LEN_MIN`); the work limit
/// covers more of the steps than HarfBuzz's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Work units per character of the item.
    pub work_per_char: u64,
    /// The work units of an item of any length.
    pub minimum_work: u64,
    /// Glyphs per character of the item.
    pub glyphs_per_char: usize,
    /// The glyphs of an item of any length.
    pub minimum_glyphs: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            work_per_char: 4096,
            minimum_work: 65536,
            glyphs_per_char: 64,
            minimum_glyphs: 16384,
        }
    }
}

impl Limits {
    /// The work budget of an item of `chars` characters.
    pub(crate) fn budget(self, chars: usize) -> WorkBudget {
        let chars = u64::try_from(chars).unwrap_or(u64::MAX);
        WorkBudget::new(
            chars
                .saturating_mul(self.work_per_char)
                .max(self.minimum_work),
        )
    }

    /// The most glyphs the buffer may hold for an item of `chars` characters.
    pub(crate) fn max_glyphs(self, chars: usize) -> usize {
        chars
            .saturating_mul(self.glyphs_per_char)
            .max(self.minimum_glyphs)
    }
}

/// Planning work per byte of a face's GSUB and GPOS tables.
const PLAN_WORK_PER_BYTE: u64 = 4;
/// Planning work for a face of any size.
const MINIMUM_PLAN_WORK: u64 = 65536;

/// The work bound of building one plan for `face`: 65536 units and 4 per byte
/// of its GSUB and GPOS tables. Planning reads each structure of those tables
/// a few times at most unless the structures are shared to multiply the work:
/// over the shaping corpus (every face with every script, both directions) a
/// plan needs at most 0.3 units per byte and 14,917 in all.
pub(crate) fn plan_budget(face: &Face<'_>) -> WorkBudget {
    let bytes = face.gsub.len() + face.gpos.len();
    let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
    WorkBudget::new(
        bytes
            .saturating_mul(PLAN_WORK_PER_BYTE)
            .saturating_add(MINIMUM_PLAN_WORK),
    )
}

/// Charges `units` before the work is done; running out is `Error::Policy`.
pub(crate) fn charge(work: &mut WorkBudget, units: usize) -> Result<()> {
    charge_for(work, units, "shaping work")
}

/// [`charge`], running out being the policy error `what`.
pub(crate) fn charge_for(work: &mut WorkBudget, units: usize, what: &'static str) -> Result<()> {
    let units = u64::try_from(units).map_err(|_| Error::Overflow)?;
    if work.try_charge(units) {
        Ok(())
    } else {
        Err(Error::Policy(what))
    }
}

#[cfg(test)]
mod tests {
    use super::{Limits, charge};
    use triergon_core::Error;

    #[test]
    fn the_budget_grows_with_the_item_and_stops_with_a_policy_error() {
        let limits = Limits {
            work_per_char: 2,
            minimum_work: 3,
            ..Limits::default()
        };
        assert_eq!(limits.budget(0).remaining(), 3);
        let mut work = limits.budget(4);
        assert_eq!(work.remaining(), 8);
        assert_eq!(charge(&mut work, 8), Ok(()));
        assert_eq!(charge(&mut work, 1), Err(Error::Policy("shaping work")));
    }

    #[test]
    fn the_buffer_length_grows_with_the_item() {
        let limits = Limits::default();
        assert_eq!(limits.max_glyphs(1), 16384);
        assert_eq!(limits.max_glyphs(1000), 64_000);
    }
}

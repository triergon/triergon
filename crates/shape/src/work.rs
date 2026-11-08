//! The work bound of one shaping call, over core's [`WorkBudget`].
//!
//! Every step whose count the text or the font decides is charged before it is
//! done: each lookup's visit of a glyph, each subtable and rule tried, each
//! glyph a match skips over, each nested lookup, each glyph a buffer edit moves
//! or a cluster merge touches, and each link of an attachment chain followed.
//! Loops bounded by a constant (the 64 glyphs of one match, the 64 nesting
//! levels) are not charged separately.

use triergon_core::{Error, Result, WorkBudget};

/// Limits of one shaping call: its work is bounded by `work_per_char` units for
/// each character of the item, and at least `minimum_work` in all. The default
/// is HarfBuzz's operation limit (`HB_BUFFER_MAX_OPS_FACTOR` and
/// `HB_BUFFER_MAX_OPS_MIN`), which this crate charges more of its steps to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub work_per_char: u64,
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
    /// The work budget of an item of `chars` characters.
    pub(crate) fn budget(self, chars: usize) -> WorkBudget {
        let chars = u64::try_from(chars).unwrap_or(u64::MAX);
        WorkBudget::new(
            chars
                .saturating_mul(self.work_per_char)
                .max(self.minimum_work),
        )
    }
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
        };
        assert_eq!(limits.budget(0).remaining(), 3);
        let mut work = limits.budget(4);
        assert_eq!(work.remaining(), 8);
        assert_eq!(charge(&mut work, 8), Ok(()));
        assert_eq!(charge(&mut work, 1), Err(Error::Policy("shaping work")));
    }
}

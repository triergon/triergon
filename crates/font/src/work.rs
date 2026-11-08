//! Work accounting for one parse or one glyph, over core's [`WorkBudget`].

use triergon_core::{Error, Result, WorkBudget};

/// Charges `units` before the work is done; running out is `Error::Policy`.
pub(crate) fn charge(work: &mut WorkBudget, units: usize) -> Result<()> {
    let units = u64::try_from(units).map_err(|_| Error::Overflow)?;
    if work.try_charge(units) {
        Ok(())
    } else {
        Err(Error::Policy("font work"))
    }
}

#[cfg(test)]
mod tests {
    use super::charge;
    use triergon_core::{Error, WorkBudget};

    #[test]
    fn work_beyond_the_budget_is_a_policy_error() {
        let mut work = WorkBudget::new(3);
        assert_eq!(charge(&mut work, 3), Ok(()));
        assert_eq!(charge(&mut work, 1), Err(Error::Policy("font work")));
    }
}

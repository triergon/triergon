use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Error, Result, WorkBudget};

/// Work charged to the caller's budget and to the decoder's cumulative limit.
pub(super) struct Work<'a> {
    pub(super) caller: &'a mut WorkBudget,
    pub(super) spent: &'a mut usize,
    pub(super) maximum: usize,
    pub(super) cancel: &'a AtomicBool,
}

impl Work<'_> {
    pub(super) fn pay(&mut self, credit: &mut usize, cost: usize) -> Result<bool> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if *credit == cost {
            return Ok(true);
        }
        let needed = cost
            .checked_sub(*credit)
            .ok_or(Error::Invalid("PGS work credit"))?;
        if self.caller.remaining() == 0 {
            return Ok(false);
        }
        let available = self.maximum.saturating_sub(*self.spent);
        if available == 0 {
            return Err(Error::Limit {
                requested: self.spent.checked_add(1).ok_or(Error::Overflow)?,
                maximum: self.maximum,
            });
        }
        let amount = needed
            .min(available)
            .min(usize::try_from(self.caller.remaining()).unwrap_or(usize::MAX));
        if !self
            .caller
            .try_charge(u64::try_from(amount).map_err(|_| Error::Overflow)?)
        {
            return Ok(false);
        }
        *self.spent = self.spent.checked_add(amount).ok_or(Error::Overflow)?;
        *credit = credit.checked_add(amount).ok_or(Error::Overflow)?;
        Ok(*credit == cost)
    }
}

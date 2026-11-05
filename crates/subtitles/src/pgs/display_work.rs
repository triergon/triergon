use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Error, Result, WorkBudget};

pub(crate) struct DisplayWork<'a> {
    pub caller: &'a mut WorkBudget,
    pub spent: &'a mut usize,
    pub maximum: usize,
    pub cancel: &'a AtomicBool,
}

impl DisplayWork<'_> {
    pub fn pay(&mut self, credit: &mut usize, cost: usize) -> Result<bool> {
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

    pub fn child<T>(&mut self, action: impl FnOnce(&mut WorkBudget) -> Result<T>) -> Result<T> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let available = self.maximum.saturating_sub(*self.spent);
        if available == 0 {
            return Err(Error::Limit {
                requested: self.spent.checked_add(1).ok_or(Error::Overflow)?,
                maximum: self.maximum,
            });
        }
        let amount = self
            .caller
            .remaining()
            .min(u64::try_from(available).unwrap_or(u64::MAX));
        let mut child = WorkBudget::new(amount);
        let result = action(&mut child);
        let used = child.spent();
        if !self.caller.try_charge(used) {
            return Err(Error::Invalid("PGS child work accounting"));
        }
        *self.spent = self
            .spent
            .checked_add(usize::try_from(used).map_err(|_| Error::Overflow)?)
            .ok_or(Error::Overflow)?;
        result
    }
}

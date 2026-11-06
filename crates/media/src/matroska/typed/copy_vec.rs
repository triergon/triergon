//! Format-local growth work state; all storage/reservations remain core BudgetVec.
//! Copy records only: committing drops the old allocation without element destructors.
use super::syntax::check;
use crate::matroska::typed::syntax::Required;
use crate::{Error, Result, incremental::ParseStep};
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, BudgetVec, WorkBudget};
#[derive(Debug)]
enum Operation<T> {
    Reserve(usize),
    Push(T),
}
#[derive(Debug)]
pub(crate) struct CopyVec<T: Copy> {
    data: BudgetVec<T>,
    operation: Option<Operation<T>>,
    growth: Option<(BudgetVec<T>, usize)>,
}
impl<T: Copy> CopyVec<T> {
    pub fn new(pool: &Budget) -> Self {
        Self {
            data: BudgetVec::new(pool),
            operation: None,
            growth: None,
        }
    }
    pub fn len(&self) -> usize {
        self.data.len()
    }
    pub fn as_slice(&self) -> &[T] {
        self.data.as_slice()
    }
    pub fn cancel_pending(&mut self) {
        self.operation = None;
        self.growth = None;
    }
    /// Drops every element and the allocation (and its charge), keeping only
    /// the pool: the same state as a new arena, without another pool handle.
    pub fn reset(&mut self) {
        self.cancel_pending();
        self.data = BudgetVec::new(self.data.budget());
    }
    pub fn truncate(&mut self, len: usize) -> Result<()> {
        if len > self.data.len() {
            return Err(Error::Invalid("copy arena truncate grows"));
        }
        if let Some(value) = self.data.as_slice().first().copied() {
            self.data.try_resize(len, value)?;
        }
        Ok(())
    }
    pub fn push_precharged(&mut self, value: T) -> Result<()> {
        if self.operation.is_some()
            || self.growth.is_some()
            || self.data.len() == self.data.capacity()
        {
            return Err(Error::Invalid("copy arena admission missing"));
        }
        self.data.try_push(value).map_err(|e| e.error)
    }
    pub fn reserve(&mut self, additional: usize) -> Result<()> {
        if self.operation.is_some() {
            return Err(Error::Invalid("copy arena operation already pending"));
        }
        self.operation = Some(Operation::Reserve(
            self.len().checked_add(additional).ok_or(Error::Overflow)?,
        ));
        Ok(())
    }
    pub fn push(&mut self, value: T) -> Result<()> {
        if self.operation.is_some() {
            return Err(Error::Invalid("copy arena operation already pending"));
        }
        self.operation = Some(Operation::Push(value));
        Ok(())
    }
    pub fn finish(self) -> Result<BudgetVec<T>> {
        if self.operation.is_some() || self.growth.is_some() {
            return Err(Error::Invalid("copy arena is incomplete"));
        }
        Ok(self.data)
    }
    pub fn step(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<ParseStep<()>> {
        loop {
            check(cancel)?;
            if let Some((replacement, at)) = &mut self.growth {
                if *at == self.data.len() {
                    let (replacement, _) = self.growth.take().required("missing arena growth")?;
                    self.data = replacement;
                    continue;
                }
                let count = work.take((self.data.len() - *at).min(64) as u64) as usize;
                if count == 0 {
                    return Ok(ParseStep::Yield);
                }
                replacement.try_extend_from_slice(&self.data.as_slice()[*at..*at + count])?;
                *at += count;
                continue;
            }
            let Some(operation) = &self.operation else {
                return Ok(ParseStep::Finished);
            };
            let needed = match operation {
                Operation::Reserve(needed) => *needed,
                Operation::Push(_) => self.data.len().checked_add(1).ok_or(Error::Overflow)?,
            };
            if !work.try_charge(1) {
                return Ok(ParseStep::Yield);
            }
            if needed > self.data.capacity() {
                let target = needed.max(self.data.capacity().saturating_mul(2)).max(4);
                let mut replacement = BudgetVec::new(self.data.budget());
                replacement.try_reserve_exact(target).or_else(|error| {
                    if target > needed
                        && matches!(
                            error,
                            Error::Backpressure { .. } | Error::Limit { .. } | Error::Overflow
                        )
                    {
                        replacement.try_reserve_exact(needed)
                    } else {
                        Err(error)
                    }
                })?;
                self.growth = Some((replacement, 0));
                continue;
            }
            if let Some(Operation::Push(value)) = self.operation.take()
                && let Err(error) = self.data.try_push(value)
            {
                self.operation = Some(Operation::Push(error.value));
                return Err(error.error);
            }
            return Ok(ParseStep::Event(()));
        }
    }
}

impl<T: Copy> super::usage::Storage for CopyVec<T> {
    fn storage_slots(&self) -> Result<usize> {
        super::usage::slots(&[&self.data, &self.growth.as_ref().map(|(buffer, _)| buffer)])
    }
    fn storage_owner(&self, index: usize) -> Result<Option<super::usage::Owner>> {
        super::usage::owner(
            &[&self.data, &self.growth.as_ref().map(|(buffer, _)| buffer)],
            index,
        )
    }
}

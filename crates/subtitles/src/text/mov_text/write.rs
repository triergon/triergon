use super::Funding;
use std::fmt;
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result, WorkBudget};

#[derive(Debug)]
pub(super) struct Fixed<const N: usize> {
    bytes: [u8; N],
    length: usize,
}
impl<const N: usize> Fixed<N> {
    pub(super) fn new() -> Self {
        Self {
            bytes: [0; N],
            length: 0,
        }
    }
    pub(super) fn append(&mut self, bytes: &[u8]) -> Result<()> {
        let end = self
            .length
            .checked_add(bytes.len())
            .ok_or(Error::Overflow)?;
        self.bytes
            .get_mut(self.length..end)
            .ok_or(Error::Overflow)?
            .copy_from_slice(bytes);
        self.length = end;
        Ok(())
    }
    pub(super) fn clear(&mut self) {
        self.length = 0;
    }
}
impl<const N: usize> fmt::Write for Fixed<N> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.append(s.as_bytes()).map_err(|_| fmt::Error)
    }
}

#[derive(Debug)]
pub(super) struct Writer {
    pub pending: Fixed<2048>,
    at: usize,
    data: BudgetVec<u8>,
    pub frozen: Option<Bytes>,
}
impl Writer {
    pub(super) fn new(pool: &Budget) -> Self {
        Self {
            pending: Fixed::new(),
            at: 0,
            data: BudgetVec::new(pool),
            frozen: None,
        }
    }
    pub(super) fn drain(
        &mut self,
        maximum: usize,
        funding: &mut Funding,
        caller: &mut WorkBudget,
    ) -> Result<bool> {
        while self.at < self.pending.length {
            let moving = if self.data.len() == self.data.capacity() {
                self.data.len()
            } else {
                0
            };
            if !funding.pay(moving.checked_add(1).ok_or(Error::Overflow)?, caller)? {
                return Ok(false);
            }
            if self.data.len() >= maximum {
                return Err(Error::Limit {
                    requested: self.data.len().saturating_add(1),
                    maximum,
                });
            }
            self.data
                .try_push(self.pending.bytes[self.at])
                .map_err(|e| e.error)?;
            self.at += 1;
            funding.commit();
        }
        self.at = 0;
        self.pending.clear();
        Ok(true)
    }
    pub(super) fn freeze(
        &mut self,
        funding: &mut Funding,
        caller: &mut WorkBudget,
    ) -> Result<bool> {
        if self.frozen.is_some() {
            return Ok(true);
        }
        if !funding.pay(1, caller)? {
            return Ok(false);
        }
        let empty = BudgetVec::new(self.data.budget());
        let data = std::mem::replace(&mut self.data, empty);
        match data.freeze() {
            Ok(bytes) => {
                self.frozen = Some(bytes);
                funding.commit();
                Ok(true)
            }
            Err(failure) => {
                self.data = failure.buffer;
                Err(failure.error)
            }
        }
    }
}

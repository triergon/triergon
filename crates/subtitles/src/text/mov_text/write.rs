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
    /// Moves what is pending to the output, one unit per byte, refusing to
    /// grow it beyond `maximum` bytes.
    pub(super) fn drain(
        &mut self,
        maximum: usize,
        funding: &mut Funding,
        caller: &mut WorkBudget,
    ) -> Result<bool> {
        let pending = &self.pending.bytes[self.at..self.pending.length];
        if pending.is_empty() {
            return Ok(true);
        }
        if !funding.pay(pending.len(), caller)? {
            return Ok(false);
        }
        if self.data.len() + pending.len() > maximum {
            return Err(Error::Limit {
                requested: maximum.saturating_add(1),
                maximum,
            });
        }
        self.data.try_reserve(pending.len())?;
        self.data.try_extend_from_slice(pending)?;
        funding.commit();
        self.at = 0;
        self.pending.clear();
        Ok(true)
    }
    /// Room for `additional` more output bytes, in one allocation.
    pub(super) fn reserve(&mut self, additional: usize) -> Result<()> {
        self.data.try_reserve_exact(additional)
    }
    /// Appends `bytes` to the output, once [`drain`](Self::drain) has moved
    /// what is pending; the output never grows beyond `maximum` bytes.
    pub(super) fn push(&mut self, bytes: &[u8], maximum: usize) -> Result<()> {
        if self.data.len().saturating_add(bytes.len()) > maximum {
            return Err(Error::Limit {
                requested: maximum.saturating_add(1),
                maximum,
            });
        }
        // Amortised growth: an exact reservation per run would copy the
        // output again for every run.
        self.data.try_reserve(bytes.len())?;
        self.data.try_extend_from_slice(bytes)
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

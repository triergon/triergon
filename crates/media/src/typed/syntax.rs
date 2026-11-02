//! Bounded EBML syntax over one already charged leaf. No I/O/pool/time layer.
use crate::{Error, Result, ebml, incremental::ParseStep};
use std::{
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};
use triergon_core::WorkBudget;

pub(crate) fn check(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}
#[derive(Debug)]
pub(crate) struct Child {
    pub id: u64,
    pub header: usize,
    pub payload: Range<usize>,
}
#[derive(Debug)]
pub(crate) struct ChildCursor {
    at: usize,
    end: usize,
}
impl ChildCursor {
    pub fn new(range: Range<usize>) -> Self {
        Self {
            at: range.start,
            end: range.end,
        }
    }
    pub fn step(
        &mut self,
        raw: &[u8],
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<Option<Child>>> {
        check(cancel)?;
        if self.at > self.end || self.end > raw.len() {
            return Err(Error::Truncated);
        }
        if self.at == self.end {
            return Ok(ParseStep::Event(None));
        }
        if !work.try_charge(1) {
            return Ok(ParseStep::Yield);
        }
        // At most twelve VINT bytes: the same syntax helper as the frozen reader.
        let header = self.at;
        let (id, a, _) = ebml::vint(&raw[header..self.end], true)?;
        let (size, b, unknown) = ebml::vint(&raw[header + a..self.end], false)?;
        if unknown {
            return Err(Error::Invalid("unknown size inside metadata"));
        }
        let start = header.checked_add(a + b).ok_or(Error::Overflow)?;
        let end = start
            .checked_add(usize::try_from(size).map_err(|_| Error::Overflow)?)
            .ok_or(Error::Overflow)?;
        if end > self.end {
            return Err(Error::Truncated);
        }
        self.at = end;
        Ok(ParseStep::Event(Some(Child {
            id,
            header,
            payload: start..end,
        })))
    }
}

/// Required state has a success-only branch: no temporary Error is constructed
/// on Some. In particular, do not replace hot calls with eager ok_or(Error),
/// whose Io-containing error enum can generate cleanup/drop paths on success.
pub(crate) trait Required<T> {
    fn required(self, reason: &'static str) -> Result<T>;
}
impl<T> Required<T> for Option<T> {
    #[inline(always)]
    fn required(self, reason: &'static str) -> Result<T> {
        let Some(value) = self else {
            return Err(Error::Invalid(reason));
        };
        Ok(value)
    }
}

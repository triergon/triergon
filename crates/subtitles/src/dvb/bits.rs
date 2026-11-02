//! A tiny partial-field cursor. No owned input buffer or generic I/O framework.
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Error, Result, WorkBudget};

#[derive(Clone, Copy, Debug)]
pub(crate) enum Stop {
    NeedInput,
    Yield,
    Error(Error),
}

pub(crate) fn cancelled(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

/// Charge before byte/token/pixel work. The one shared pool is not duplicated.
pub(crate) fn take_work(
    work: &mut WorkBudget,
    total: &mut usize,
    limit: usize,
    maximum: usize,
    cancel: &AtomicBool,
) -> Result<usize> {
    cancelled(cancel)?;
    if maximum == 0 || work.remaining() == 0 {
        return Ok(0);
    }
    let available = limit.checked_sub(*total).ok_or(Error::Overflow)?;
    if available == 0 {
        return Err(Error::Limit {
            requested: total.checked_add(1).ok_or(Error::Overflow)?,
            maximum: limit,
        });
    }
    let bound = maximum.min(available);
    let amount = usize::try_from(work.take(u64::try_from(bound).map_err(|_| Error::Overflow)?))
        .map_err(|_| Error::Overflow)?;
    *total = total.checked_add(amount).ok_or(Error::Overflow)?;
    Ok(amount)
}

pub(crate) struct Feed<'a> {
    pub(crate) bytes: &'a [u8],
    pub(crate) consumed: usize,
    pub(crate) remaining: usize,
    pub(crate) final_input: bool,
    pub(crate) work: &'a mut WorkBudget,
    pub(crate) total: &'a mut usize,
    pub(crate) limit: usize,
    pub(crate) cancel: &'a AtomicBool,
}

impl Feed<'_> {
    pub(crate) fn byte(&mut self) -> std::result::Result<u8, Stop> {
        cancelled(self.cancel).map_err(Stop::Error)?;
        if self.remaining == 0 {
            return Err(Stop::Error(Error::Truncated));
        }
        let value = match self.bytes.get(self.consumed) {
            Some(&value) => value,
            None if self.final_input => return Err(Stop::Error(Error::Truncated)),
            None => return Err(Stop::NeedInput),
        };
        if take_work(self.work, self.total, self.limit, 1, self.cancel).map_err(Stop::Error)? == 0 {
            return Err(Stop::Yield);
        }
        self.consumed += 1;
        self.remaining -= 1;
        Ok(value)
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Bits {
    byte: u8,
    left: u8,
    goal: u8,
    have: u8,
    value: u16,
}

impl Bits {
    pub(crate) fn aligned_idle(self) -> bool {
        self.left == 0 && self.goal == 0
    }

    pub(crate) fn read(
        &mut self,
        width: u8,
        input: &mut Feed<'_>,
    ) -> std::result::Result<u8, Stop> {
        if width == 0 || width > 8 || (self.goal != 0 && self.goal != width) {
            return Err(Stop::Error(Error::Invalid("DVB bit-field state")));
        }
        if self.goal == 0 {
            self.goal = width;
        }
        while self.have < width {
            if self.left == 0 {
                self.byte = input.byte()?;
                self.left = 8;
            }
            let count = (width - self.have).min(self.left);
            let shift = self.left - count;
            let mask = (1_u16 << count) - 1;
            self.value = (self.value << count) | ((u16::from(self.byte) >> shift) & mask);
            self.left -= count;
            self.have += count;
        }
        let value = u8::try_from(self.value).map_err(|_| Stop::Error(Error::Overflow))?;
        self.goal = 0;
        self.have = 0;
        self.value = 0;
        Ok(value)
    }

    pub(crate) fn align_zero(&mut self) -> Result<()> {
        if self.goal != 0 {
            return Err(Error::Invalid("DVB partial field at alignment"));
        }
        if u16::from(self.byte) & ((1_u16 << self.left) - 1) != 0 {
            return Err(Error::Invalid("DVB nonzero string padding"));
        }
        self.left = 0;
        Ok(())
    }
}

//! Exact source-code coverage for the objects of one region composition.
//! EN300743 7.2.3/8.4.2 forbids overlapping current-display objects. Earlier
//! display-set pixels may be overwritten. A bounding-box approximation would
//! incorrectly reject some ragged-line objects, so each source pixel has one bit.
use super::Size;
use super::bits::take_work;
use safe_media_core::{Budget, BudgetVec, Error, Result, WorkBudget};
use std::sync::atomic::AtomicBool;

const CHUNK: usize = 1024;

#[derive(Debug)]
pub(super) struct Coverage {
    bits: BudgetVec<u8>,
    length: usize,
}

impl Coverage {
    pub fn new(budget: &Budget, size: Size) -> Result<Self> {
        let area = u64::from(size.width)
            .checked_mul(u64::from(size.height))
            .ok_or(Error::Overflow)?;
        let area = usize::try_from(area).map_err(|_| Error::Overflow)?;
        let length = area.checked_add(7).ok_or(Error::Overflow)? / 8;
        let mut bits = BudgetVec::new(budget);
        bits.try_reserve_exact(length)?;
        Ok(Self { bits, length })
    }

    pub fn prepare(
        &mut self,
        work: &mut WorkBudget,
        total: &mut usize,
        limit: usize,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        while self.bits.len() < self.length {
            let count = take_work(
                work,
                total,
                limit,
                (self.length - self.bits.len()).min(CHUNK),
                cancel,
            )?;
            if count == 0 {
                return Ok(false);
            }
            self.bits.try_extend_from_slice(&[0; CHUNK][..count])?;
        }
        Ok(true)
    }

    /// Pixel work is charged by the enclosing raster before claiming this span.
    /// Transparent/nonmodifying codes occupy source coordinates too; cropping
    /// cannot hide an overlap or turn this into visible-pixel-only validation.
    pub fn claim(&mut self, first: usize, count: usize) -> Result<()> {
        let end = first.checked_add(count).ok_or(Error::Overflow)?;
        for pixel in first..end {
            let byte = self
                .bits
                .as_mut_slice()
                .get_mut(pixel / 8)
                .ok_or(Error::Invalid("DVB coverage outside region"))?;
            let flag = 1_u8 << (pixel % 8);
            if *byte & flag != 0 {
                return Err(Error::Invalid("DVB current objects overlap"));
            }
            *byte |= flag;
        }
        Ok(())
    }
}

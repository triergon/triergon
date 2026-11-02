//! Wire tables retain compact source rows; never expand a run into per-sample storage.
//! Adobe F4V 10.1 §§2.7.6, 2.8. Unsupported current-ISO layouts stay explicit.
use crate::{BoxView, Limits, fields, policy};
use safe_media_core::{Error, Result};

#[derive(Clone, Debug)]
pub(crate) struct Words {
    pub raw: BoxView,
    pub count: u32,
    start: usize,
    stride: usize,
}
impl Words {
    pub fn parse(raw: BoxView, stride: usize, limits: Limits) -> Result<Self> {
        fields::full_zero(raw.payload())?;
        let count = fields::u32_at(raw.payload(), 4)?;
        fields::exact_rows(raw.payload(), 8, count, stride)?;
        policy(count, limits.max_records)?;
        Ok(Self {
            raw,
            count,
            start: 8,
            stride,
        })
    }
    pub fn word(&self, row: u32, column: usize) -> Result<u32> {
        fields::u32_at(self.raw.payload(), self.position(row, column, 4)?)
    }
    pub fn wide(&self, row: u32) -> Result<u64> {
        fields::u64_at(self.raw.payload(), self.position(row, 0, 8)?)
    }
    fn position(&self, row: u32, column: usize, width: usize) -> Result<usize> {
        if row >= self.count || column.checked_add(width).ok_or(Error::Overflow)? > self.stride {
            return Err(Error::Invalid("table row/column index"));
        }
        usize::try_from(row)
            .map_err(|_| Error::Overflow)?
            .checked_mul(self.stride)
            .and_then(|p| p.checked_add(self.start))
            .and_then(|p| p.checked_add(column))
            .ok_or(Error::Overflow)
    }
    /// Validated sorted 1-based sample numbers, including a present-empty table.
    pub fn contains(&self, sample: u64) -> Result<bool> {
        Ok(self.previous(sample)? == Some(sample))
    }
    pub fn previous(&self, sample: u64) -> Result<Option<u64>> {
        let mut lo = 0u32;
        let mut hi = self.count;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if u64::from(self.word(mid, 0)?) <= sample {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo == 0 {
            Ok(None)
        } else {
            Ok(Some(u64::from(self.word(lo - 1, 0)?)))
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Sizes {
    pub raw: BoxView,
    pub count: u32,
    pub constant: u32,
}
impl Sizes {
    pub fn parse(raw: BoxView) -> Result<Self> {
        fields::full_zero(raw.payload())?;
        let constant = fields::u32_at(raw.payload(), 4)?;
        let count = fields::u32_at(raw.payload(), 8)?;
        fields::exact_rows(raw.payload(), 12, if constant == 0 { count } else { 0 }, 4)?;
        Ok(Self {
            raw,
            count,
            constant,
        })
    }
    pub fn get(&self, sample: u64) -> Result<u32> {
        if sample >= u64::from(self.count) {
            return Err(Error::Invalid("sample size index"));
        }
        if self.constant != 0 {
            return Ok(self.constant);
        }
        let position = usize::try_from(sample)
            .map_err(|_| Error::Overflow)?
            .checked_mul(4)
            .and_then(|p| p.checked_add(12))
            .ok_or(Error::Overflow)?;
        fields::u32_at(self.raw.payload(), position)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Offsets {
    pub table: Words,
    pub wide: bool,
}
impl Offsets {
    pub fn parse(raw: BoxView, limits: Limits) -> Result<Self> {
        let wide = raw.header().kind == *b"co64";
        Ok(Self {
            table: Words::parse(raw, if wide { 8 } else { 4 }, limits)?,
            wide,
        })
    }
    pub fn get(&self, chunk: u32) -> Result<u64> {
        if self.wide {
            self.table.wide(chunk)
        } else {
            Ok(u64::from(self.table.word(chunk, 0)?))
        }
    }
}

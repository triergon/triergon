//! Wire tables retain compact source rows; never expand a run into per-sample storage.
//! Adobe F4V 10.1 §§2.7.6, 2.8. Unsupported current-ISO layouts stay explicit.
//! Signed ctts: published ISO 14496-12:2008/Amd1:2009 §§8.6.1.3.1-.3.
//! Experimental stz2: MPEG m76694 table9 Original and author SDL M2902.
//! Selected widths only; unused odd-count nibble is retained, never validated.
use crate::{BoxView, Limits, fields, policy};
use triergon_core::{Error, Result};

#[derive(Clone, Debug)]
pub(crate) struct Words {
    pub raw: BoxView,
    pub count: u32,
    start: usize,
    stride: usize,
}
impl Words {
    pub(crate) fn parse(raw: BoxView, stride: usize, limits: Limits) -> Result<Self> {
        fields::full_zero(raw.payload())?;
        Self::rows(raw, stride, limits)
    }
    /// Only ctts has the acquired version-1 row interpretation. Other tables
    /// keep their version/flags-zero constructor above.
    pub(crate) fn parse_ctts(raw: BoxView, limits: Limits) -> Result<Self> {
        if raw.header().kind != *b"ctts" {
            return Err(Error::Invalid("not ctts"));
        }
        let (version, flags) = fields::full(raw.payload())?;
        if version > 1 || flags != 0 {
            return Err(Error::Unsupported(
                "ctts version/flags outside acquired syntax",
            ));
        }
        Self::rows(raw, 8, limits)
    }
    fn rows(raw: BoxView, stride: usize, limits: Limits) -> Result<Self> {
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
    pub(crate) fn word(&self, row: u32, column: usize) -> Result<u32> {
        fields::u32_at(self.raw.payload(), self.position(row, column, 4)?)
    }
    pub(crate) fn wide(&self, row: u32) -> Result<u64> {
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
    pub(crate) fn contains(&self, sample: u64) -> Result<bool> {
        Ok(self.previous(sample)? == Some(sample))
    }
    pub(crate) fn previous(&self, sample: u64) -> Result<Option<u64>> {
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
    width: u8,
}
impl Sizes {
    pub(crate) fn parse(raw: BoxView) -> Result<Self> {
        if raw.header().kind == *b"stz2" {
            return Self::compact(raw);
        }
        fields::full_zero(raw.payload())?;
        let constant = fields::u32_at(raw.payload(), 4)?;
        let count = fields::u32_at(raw.payload(), 8)?;
        fields::exact_rows(raw.payload(), 12, if constant == 0 { count } else { 0 }, 4)?;
        Ok(Self {
            raw,
            count,
            constant,
            width: 32,
        })
    }
    fn compact(raw: BoxView) -> Result<Self> {
        let b = raw.payload();
        fields::slice(b, 0, 12)?;
        fields::full_zero(b)?;
        if b[4..7] != [0; 3] {
            return Err(Error::Unsupported("stz2 reserved-field semantics"));
        }
        let width = b[7];
        if !matches!(width, 4 | 8 | 16) {
            return Err(Error::Unsupported(
                "stz2 field width outside selected profile",
            ));
        }
        let count = fields::u32_at(b, 8)?;
        let bits = u64::from(count)
            .checked_mul(u64::from(width))
            .ok_or(Error::Overflow)?;
        let required = bits.div_ceil(8);
        let end = 12u64.checked_add(required).ok_or(Error::Overflow)?;
        let available = u64::try_from(b.len()).map_err(|_| Error::Overflow)?;
        if end > available {
            return Err(Error::Truncated);
        }
        if end != available {
            return Err(Error::Unsupported("stz2 opaque trailing full bytes"));
        }
        // The acquired primary array layout establishes MSB-first entries.
        // No acquired padding rule establishes a value for an unused low nibble.
        Ok(Self {
            raw,
            count,
            constant: 0,
            width,
        })
    }
    pub(crate) fn get(&self, sample: u64) -> Result<u32> {
        if sample >= u64::from(self.count) {
            return Err(Error::Invalid("sample size index"));
        }
        if self.constant != 0 {
            return Ok(self.constant);
        }
        let bit = sample
            .checked_mul(u64::from(self.width))
            .ok_or(Error::Overflow)?;
        let position = usize::try_from((bit / 8).checked_add(12).ok_or(Error::Overflow)?)
            .map_err(|_| Error::Overflow)?;
        let b = self.raw.payload();
        match self.width {
            4 => {
                let byte = *b.get(position).ok_or(Error::Truncated)?;
                Ok(u32::from(if sample & 1 == 0 {
                    byte >> 4
                } else {
                    byte & 15
                }))
            }
            8 => Ok(u32::from(*b.get(position).ok_or(Error::Truncated)?)),
            16 => Ok(u32::from(fields::u16_at(b, position)?)),
            32 => fields::u32_at(b, position),
            _ => Err(Error::Invalid("unvalidated sample size width")),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Offsets {
    pub table: Words,
    pub wide: bool,
}
impl Offsets {
    pub(crate) fn parse(raw: BoxView, limits: Limits) -> Result<Self> {
        let wide = raw.header().kind == *b"co64";
        Ok(Self {
            table: Words::parse(raw, if wide { 8 } else { 4 }, limits)?,
            wide,
        })
    }
    pub(crate) fn get(&self, chunk: u32) -> Result<u64> {
        if self.wide {
            self.table.wide(chunk)
        } else {
            Ok(u64::from(self.table.word(chunk, 0)?))
        }
    }
}

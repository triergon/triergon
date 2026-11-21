//! EBML primitives written from RFC 8794. No upstream implementation code.
use crate::step::{ParseStep, check};
use crate::{Error, Result};
use std::{ops::Range, sync::atomic::AtomicBool};
use triergon_core::WorkBudget;

pub(crate) fn vint(data: &[u8], keep_marker: bool) -> Result<(u64, usize, bool)> {
    let first = *data.first().ok_or(Error::Truncated)?;
    if first == 0 {
        return Err(Error::Invalid("zero VINT marker"));
    }
    let width = first.leading_zeros() as usize + 1;
    if keep_marker && width > 4 {
        return Err(Error::Invalid("element ID width"));
    }
    let bytes = data.get(..width).ok_or(Error::Truncated)?;
    let mask = (0xff_u64 >> width) as u8;
    let mut value = u64::from(if keep_marker { first } else { first & mask });
    for b in &bytes[1..] {
        value = (value << 8) | u64::from(*b);
    }
    Ok((
        value,
        width,
        !keep_marker && value == (1_u64 << (width * 7)) - 1,
    ))
}

pub(crate) fn uint(data: &[u8]) -> Result<u64> {
    if data.len() > 8 {
        return Err(Error::Invalid("integer width"));
    }
    Ok(data.iter().fold(0, |v, b| (v << 8) | u64::from(*b)))
}

pub(crate) fn sint(data: &[u8]) -> Result<i64> {
    if data.is_empty() {
        return Ok(0);
    }
    let u = uint(data)?;
    let shift = (8 - data.len()) * 8;
    Ok(((u << shift) as i64) >> shift)
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
    pub(crate) fn new(range: Range<usize>) -> Self {
        Self {
            at: range.start,
            end: range.end,
        }
    }
    pub(crate) fn step(
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
        // At most twelve VINT bytes.
        let header = self.at;
        let (id, a, _) = vint(&raw[header..self.end], true)?;
        let (size, b, unknown) = vint(&raw[header + a..self.end], false)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn all_vint_widths_unknown_sizes_and_truncation() {
        for w in 1..=8 {
            let mut b = vec![0xff; w];
            b[0] = 0xff >> (w - 1);
            assert_eq!(vint(&b, false).unwrap(), ((1_u64 << (7 * w)) - 1, w, true));
            for n in 0..w {
                assert!(vint(&b[..n], false).is_err());
            }
        }
        assert!(vint(&[0], false).is_err());
        assert_eq!(sint(&[0xff, 0xfe]).unwrap(), -2);
        assert_eq!(sint(&[0x80]).unwrap(), -128);
    }
}

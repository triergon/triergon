//! EBML primitives from RFC 8794: variable-size integers, element headers,
//! fixed-width integers and the children of a master element held in memory.
//! Every reader of this crate decodes an element header through this module.
use crate::step::{ParseStep, check};
use crate::{Error, Result};
use std::{ops::Range, sync::atomic::AtomicBool};
use triergon_core::WorkBudget;

/// Width in bytes, 1 to 8, of the variable-size integer that starts with
/// `first`. A zero byte carries no marker and reads as 9.
#[inline(always)]
pub(crate) const fn vint_width(first: u8) -> usize {
    first.leading_zeros() as usize + 1
}

/// One variable-size integer (RFC 8794 §4) at the start of `data`: its value,
/// its width in bytes, and whether it is the reserved all-ones value that marks
/// an unknown data size. With `keep_marker` the marker bit stays in the value
/// and the width is at most four, as an Element ID is read (§5).
pub(crate) fn vint(data: &[u8], keep_marker: bool) -> Result<(u64, usize, bool)> {
    let first = *data.first().ok_or(Error::Truncated)?;
    if first == 0 {
        return Err(Error::Invalid("zero VINT marker"));
    }
    let width = vint_width(first);
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

/// Width of the Element ID that starts with `first`, at most four bytes.
#[inline(always)]
pub(crate) fn id_width(first: u8) -> Result<usize> {
    if first == 0 {
        return Err(Error::Invalid("zero VINT marker"));
    }
    let width = vint_width(first);
    if width > 4 {
        return Err(Error::Invalid("element ID width"));
    }
    Ok(width)
}

/// Width of the element data size that starts with `first`.
#[inline(always)]
pub(crate) fn size_width(first: u8) -> Result<usize> {
    if first == 0 {
        return Err(Error::Invalid("zero VINT marker"));
    }
    Ok(vint_width(first))
}

/// Bytes the Element ID `id` occupies, from its value with the marker kept.
#[inline(always)]
pub(crate) const fn id_len(id: u32) -> usize {
    (32 - id.leading_zeros()).div_ceil(8) as usize
}

/// The data size held by the `width` bytes at `at` in `raw`: the size field of
/// a header that was decoded and bounds-checked before.
#[inline(always)]
pub(crate) fn size_at(raw: &[u8], at: usize, width: usize) -> usize {
    let mut size = usize::from(raw[at] & (0xff_u64 >> width) as u8);
    for byte in &raw[at + 1..at + width] {
        size = (size << 8) | usize::from(*byte);
    }
    size
}

/// Widths of the ID and of the data size of an element header that lies
/// wholly at the start of `bytes`; `None` when it is malformed or incomplete.
#[inline]
pub(crate) fn header_widths(bytes: &[u8]) -> Option<(usize, usize)> {
    let first = *bytes.first()?;
    if first == 0 {
        return None;
    }
    let id_width = vint_width(first);
    if id_width > 4 {
        return None;
    }
    let size = *bytes.get(id_width)?;
    if size == 0 {
        return None;
    }
    let size_width = vint_width(size);
    (bytes.len() >= id_width + size_width).then_some((id_width, size_width))
}

/// An element header decoded from bytes in memory.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Header {
    pub id: u64,
    /// Data size in bytes; `None` for the reserved unknown size.
    pub size: Option<u64>,
    /// Bytes the ID and the data size occupy together.
    pub len: usize,
}

/// The element header at the start of `data`.
#[inline(always)]
pub(crate) fn header(data: &[u8]) -> Result<Header> {
    let (id, id_width, _) = vint(data, true)?;
    let (size, size_width, unknown) = vint(&data[id_width..], false)?;
    Ok(Header {
        id,
        size: (!unknown).then_some(size),
        len: id_width + size_width,
    })
}

/// A big-endian unsigned integer of zero to eight bytes (RFC 8794 §7.2).
pub(crate) fn uint(data: &[u8]) -> Result<u64> {
    if data.len() > 8 {
        return Err(Error::Invalid("integer width"));
    }
    Ok(data.iter().fold(0, |v, b| (v << 8) | u64::from(*b)))
}

/// A big-endian two's-complement integer of zero to eight bytes (RFC 8794 §7.1).
pub(crate) fn sint(data: &[u8]) -> Result<i64> {
    if data.is_empty() {
        return Ok(0);
    }
    let u = uint(data)?;
    let shift = (8 - data.len()) * 8;
    Ok(((u << shift) as i64) >> shift)
}

/// One child element of a master element held in memory.
#[derive(Debug)]
pub(crate) struct Child {
    pub id: u64,
    /// Offset of the child's header in the source.
    pub header: usize,
    /// Range of the child's data in the source.
    pub payload: Range<usize>,
}
/// Walks the children of one master element whose bytes are in memory, one
/// child and one work unit per step. A child of unknown size is refused.
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
        let head = self::header(&raw[header..self.end])?;
        let Some(size) = head.size else {
            return Err(Error::Invalid("unknown size inside metadata"));
        };
        let start = header.checked_add(head.len).ok_or(Error::Overflow)?;
        let end = start
            .checked_add(usize::try_from(size).map_err(|_| Error::Overflow)?)
            .ok_or(Error::Overflow)?;
        if end > self.end {
            return Err(Error::Truncated);
        }
        self.at = end;
        Ok(ParseStep::Event(Some(Child {
            id: head.id,
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

    #[test]
    fn finite_child_cursor_rejects_even_an_empty_span_outside_its_owner() {
        let mut cursor = ChildCursor::new(4..4);
        assert!(matches!(
            cursor.step(b"abc", &mut WorkBudget::new(1), &AtomicBool::new(false)),
            Err(Error::Truncated)
        ));
    }
}

//! EBML primitives written from RFC 8794. No upstream implementation code.
use crate::storage::SharedBytes;
use crate::{Error, Result};
use safe_media_core::{Budget, BudgetVec};
use std::io::Read;

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

#[derive(Clone, Copy, Debug)]
pub(crate) struct Header {
    pub id: u64,
    pub size: Option<u64>,
    pub start: u64,
    pub data: u64,
}

pub(crate) struct PendingBytes {
    size: u64,
    buffer: BudgetVec<u8>,
}
pub(crate) struct Input<R> {
    pub pool: Budget,
    pub pending_bytes: Option<PendingBytes>,
    pub reader: R,
    pub position: u64,
    pub elements_left: u64,
    pub bytes_left: u64,
}

impl<R: Read> Input<R> {
    fn first(&mut self) -> Result<Option<u8>> {
        if self.bytes_left == 0 {
            return Err(Error::Limit("operation read bytes"));
        }
        let mut b = [0];
        loop {
            match self.reader.read(&mut b) {
                Ok(0) => return Ok(None),
                Ok(_) => {
                    self.bytes_left -= 1;
                    self.position = self.position.checked_add(1).ok_or(Error::Overflow)?;
                    return Ok(Some(b[0]));
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            }
        }
    }

    pub fn exact(&mut self, bytes: &mut [u8]) -> Result<()> {
        self.bytes_left = self
            .bytes_left
            .checked_sub(bytes.len() as u64)
            .ok_or(Error::Limit("operation read bytes"))?;
        self.reader.read_exact(bytes)?;
        self.position = self
            .position
            .checked_add(bytes.len() as u64)
            .ok_or(Error::Overflow)?;
        Ok(())
    }

    fn variable(&mut self, first: u8, id: bool) -> Result<(u64, bool)> {
        if first == 0 {
            return Err(Error::Invalid("zero VINT marker"));
        }
        let width = first.leading_zeros() as usize + 1;
        let mut bytes = [0; 8];
        bytes[0] = first;
        self.exact(&mut bytes[1..width])?;
        let (value, _, unknown) = vint(&bytes[..width], id)?;
        Ok((value, unknown))
    }

    pub fn header(&mut self) -> Result<Option<Header>> {
        let start = self.position;
        let Some(first) = self.first()? else {
            return Ok(None);
        };
        self.elements_left = self
            .elements_left
            .checked_sub(1)
            .ok_or(Error::Limit("element count"))?;
        let (id, _) = self.variable(first, true)?;
        let first = self.first()?.ok_or(Error::Truncated)?;
        let (size, unknown) = self.variable(first, false)?;
        Ok(Some(Header {
            id,
            size: (!unknown).then_some(size),
            start,
            data: self.position,
        }))
    }

    pub fn bytes(&mut self, size: u64, limit: usize) -> Result<SharedBytes> {
        let count = usize::try_from(size).map_err(|_| Error::Limit("address space"))?;
        if count > limit {
            return Err(Error::Limit("element bytes"));
        }
        if count == 0 && self.pending_bytes.is_none() {
            return Ok(SharedBytes::default());
        }
        let buffer = if let Some(pending) = self.pending_bytes.take() {
            if pending.size != size {
                return Err(Error::Invalid("pending element size changed"));
            }
            pending.buffer
        } else {
            let mut buffer = BudgetVec::new(&self.pool);
            // Initialize exactly the requested destination before borrowing its
            // slice. No fixed 8 KiB scratch zero/copy is needed for tiny elements.
            buffer.try_resize(count, 0)?;
            self.bytes_left = self
                .bytes_left
                .checked_sub(size)
                .ok_or(Error::Limit("operation read bytes"))?;
            let mut filled = 0;
            while filled < count {
                match self.reader.read(&mut buffer.as_mut_slice()[filled..]) {
                    Ok(0) => return Err(Error::Truncated),
                    Ok(read) if read <= count - filled => {
                        filled += read;
                        self.position = self
                            .position
                            .checked_add(read as u64)
                            .ok_or(Error::Overflow)?;
                    }
                    Ok(_) => return Err(Error::Invalid("reader returned excessive length")),
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error.into()),
                }
            }
            buffer
        };
        match buffer.freeze() {
            Ok(bytes) => Ok(SharedBytes::from_core(bytes)),
            Err(error) => {
                self.pending_bytes = Some(PendingBytes {
                    size,
                    buffer: error.buffer,
                });
                Err(error.error.into())
            }
        }
    }

    pub fn skip(&mut self, mut count: u64, limit: u64) -> Result<()> {
        if count > limit {
            return Err(Error::Limit("skip bytes"));
        }
        let mut scratch = [0; 8192];
        while count != 0 {
            let n = count.min(scratch.len() as u64) as usize;
            self.exact(&mut scratch[..n])?;
            count -= n as u64;
        }
        Ok(())
    }
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

pub(crate) fn children<'a>(
    data: &'a [u8],
    mut visit: impl FnMut(u64, &'a [u8]) -> Result<()>,
) -> Result<()> {
    child_ranges(data, |id, range| visit(id, &data[range]))
}

pub(crate) fn child_ranges(
    data: &[u8],
    mut visit: impl FnMut(u64, std::ops::Range<usize>) -> Result<()>,
) -> Result<()> {
    let mut offset = 0;
    while offset < data.len() {
        let (id, a, _) = vint(&data[offset..], true)?;
        let (size, b, unknown) = vint(&data[offset + a..], false)?;
        if unknown {
            return Err(Error::Invalid("unknown size inside metadata"));
        }
        let n = usize::try_from(size).map_err(|_| Error::Overflow)?;
        let start = offset.checked_add(a + b).ok_or(Error::Overflow)?;
        let end = start.checked_add(n).ok_or(Error::Overflow)?;
        if end > data.len() {
            return Err(Error::Truncated);
        }
        visit(id, start..end)?;
        offset = end;
    }
    Ok(())
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

//! Big-endian reads over font tables, and the work meter of one operation.
//!
//! Every read is bounds-checked. Data that ends early is malformed, so it is
//! `Error::Invalid`; `Error::Truncated` is kept for a transport that ends early.

use triergon_core::{Error, Result};

pub(crate) const TRUNCATED: Error = Error::Invalid("truncated font table");

/// A cursor over a byte slice.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Reader<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    pub(crate) const fn new(data: &'a [u8]) -> Self {
        Self { data, at: 0 }
    }

    /// A reader over `data[offset..]`.
    pub(crate) fn at(data: &'a [u8], offset: usize) -> Result<Self> {
        if offset > data.len() {
            return Err(TRUNCATED);
        }
        Ok(Self { data, at: offset })
    }

    /// A reader over the same data at `offset`.
    pub(crate) fn moved_to(&self, offset: usize) -> Result<Self> {
        Self::at(self.data, offset)
    }

    pub(crate) const fn offset(&self) -> usize {
        self.at
    }

    pub(crate) const fn at_end(&self) -> bool {
        self.at == self.data.len()
    }

    pub(crate) fn skip(&mut self, count: usize) -> Result<()> {
        self.take(count).map(|_| ())
    }

    pub(crate) fn take(&mut self, count: usize) -> Result<&'a [u8]> {
        let end = self.at.checked_add(count).ok_or(TRUNCATED)?;
        let bytes = self.data.get(self.at..end).ok_or(TRUNCATED)?;
        self.at = end;
        Ok(bytes)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        let bytes = self.data.get(self.at..).and_then(<[u8]>::first_chunk::<N>);
        let bytes = *bytes.ok_or(TRUNCATED)?;
        self.at += N;
        Ok(bytes)
    }

    pub(crate) fn u8(&mut self) -> Result<u8> {
        Ok(self.array::<1>()?[0])
    }

    pub(crate) fn i8(&mut self) -> Result<i8> {
        Ok(i8::from_be_bytes(self.array()?))
    }

    pub(crate) fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(self.array()?))
    }

    pub(crate) fn i16(&mut self) -> Result<i16> {
        Ok(i16::from_be_bytes(self.array()?))
    }

    pub(crate) fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.array()?))
    }

    pub(crate) fn i32(&mut self) -> Result<i32> {
        Ok(i32::from_be_bytes(self.array()?))
    }

    pub(crate) fn tag(&mut self) -> Result<[u8; 4]> {
        self.array()
    }

    /// An unsigned integer of `size` bytes, 1 to 4.
    pub(crate) fn uint(&mut self, size: u8) -> Result<u32> {
        let mut value = 0_u32;
        for _ in 0..size {
            value = value << 8 | u32::from(self.u8()?);
        }
        Ok(value)
    }

    /// A 2.14 fixed-point number.
    pub(crate) fn f2dot14(&mut self) -> Result<f64> {
        Ok(f64::from(self.i16()?) / 16384.0)
    }

    /// A 16.16 fixed-point number.
    pub(crate) fn fixed(&mut self) -> Result<f64> {
        Ok(f64::from(self.i32()?) / 65536.0)
    }
}

/// The big-endian `u16` at `at`, if `data` holds it.
pub(crate) fn u16_at(data: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(*data.get(at..)?.first_chunk()?))
}

/// The big-endian `u32` at `at`, if `data` holds it.
pub(crate) fn u32_at(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(*data.get(at..)?.first_chunk()?))
}

/// `data[range]`, or `Invalid` when the range lies outside it.
pub(crate) fn slice(data: &[u8], start: usize, len: usize) -> Result<&[u8]> {
    let end = start.checked_add(len).ok_or(TRUNCATED)?;
    data.get(start..end).ok_or(TRUNCATED)
}

/// The work units one parse or one outline may spend.
#[derive(Debug)]
pub(crate) struct Meter {
    remaining: u64,
}

impl Meter {
    pub(crate) const fn new(units: u64) -> Self {
        Self { remaining: units }
    }

    /// Charges `units` before the work is done.
    pub(crate) fn charge(&mut self, units: usize) -> Result<()> {
        let units = u64::try_from(units).map_err(|_| Error::Overflow)?;
        if units > self.remaining {
            return Err(Error::Policy("font work"));
        }
        self.remaining -= units;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{Meter, Reader, TRUNCATED, slice};
    use triergon_core::Error;

    #[test]
    fn reads_are_big_endian_and_bounded() {
        let data = [0x01, 0x02, 0xff, 0xfe, 0x40, 0x00, 0x80, 0x00, 0x00, 0x00];
        let mut r = Reader::new(&data);
        assert_eq!(r.u16(), Ok(0x0102));
        assert_eq!(r.i16(), Ok(-2));
        assert_eq!(r.f2dot14(), Ok(1.0));
        assert_eq!(r.fixed(), Ok(-32768.0));
        assert_eq!(r.u8(), Err(TRUNCATED));
        assert_eq!(r.offset(), 10);
        let mut r = Reader::at(&data, 1).unwrap();
        assert_eq!(r.uint(3), Ok(0x02fffe));
        assert!(Reader::at(&data, 11).is_err());
        assert_eq!(slice(&data, 8, 2), Ok(&data[8..]));
        assert_eq!(slice(&data, 9, 2), Err(TRUNCATED));
        assert_eq!(slice(&data, usize::MAX, 2), Err(TRUNCATED));
    }

    #[test]
    fn the_meter_refuses_work_beyond_its_units() {
        let mut meter = Meter::new(3);
        assert_eq!(meter.charge(3), Ok(()));
        assert_eq!(meter.charge(1), Err(Error::Policy("font work")));
    }
}

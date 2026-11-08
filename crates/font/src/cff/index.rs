//! CFF INDEX data (Adobe Technical Note 5176, section 5; CFF2 counts are 32-bit).
//!
//! Offsets are read on demand from the font bytes; parsing checks only that the
//! offset array and the last object lie inside the table.

use super::Version;
use crate::read::Reader;
use triergon_core::{Error, Result};

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Index {
    count: u32,
    offset_size: u8,
    /// The table offset of the offset array.
    offsets: usize,
    /// The table offset of the byte before the object data, which offsets count from.
    base: usize,
}

impl Index {
    /// Reads the INDEX at the reader and moves past it.
    pub(crate) fn parse(r: &mut Reader<'_>, version: Version) -> Result<Self> {
        let count = match version {
            Version::Cff1 => u32::from(r.u16()?),
            Version::Cff2 => r.u32()?,
        };
        if count == 0 {
            return Ok(Self::default());
        }
        let offset_size = r.u8()?;
        if !(1..=4).contains(&offset_size) {
            return Err(Error::Invalid("CFF INDEX offset size"));
        }
        let offsets = r.offset();
        let size = usize::from(offset_size);
        let entries = usize::try_from(count)
            .ok()
            .and_then(|n| n.checked_add(1))
            .ok_or(Error::Overflow)?;
        let array = entries.checked_mul(size).ok_or(Error::Overflow)?;
        r.skip(array)?;
        let mut last = r.moved_to(offsets + array - size)?;
        let end = usize::try_from(last.uint(offset_size)?).map_err(|_| Error::Overflow)?;
        if end == 0 {
            return Err(Error::Invalid("CFF INDEX offsets"));
        }
        let base = r.offset() - 1;
        r.skip(end - 1)?;
        Ok(Self {
            count,
            offset_size,
            offsets,
            base,
        })
    }

    pub(crate) const fn len(&self) -> u32 {
        self.count
    }

    /// The bytes of object `i` of `table`, the CFF table this INDEX was read from.
    pub(crate) fn get<'a>(&self, table: &'a [u8], i: u32) -> Result<&'a [u8]> {
        if i >= self.count {
            return Err(Error::Invalid("CFF INDEX object outside the INDEX"));
        }
        let size = usize::from(self.offset_size);
        // The offset array was found inside the table, so these products fit.
        let i = usize::try_from(i).map_err(|_| Error::Overflow)?;
        let mut r = Reader::at(table, self.offsets + i * size)?;
        let offset = |r: &mut Reader<'_>| -> Result<usize> {
            let value = usize::try_from(r.uint(self.offset_size)?).map_err(|_| Error::Overflow)?;
            self.base.checked_add(value).ok_or(Error::Overflow)
        };
        let (start, end) = (offset(&mut r)?, offset(&mut r)?);
        if start <= self.base || start > end {
            return Err(Error::Invalid("CFF INDEX offsets"));
        }
        table
            .get(start..end)
            .ok_or(Error::Invalid("CFF INDEX object outside the table"))
    }
}

#[cfg(test)]
mod tests {
    use super::{Index, Version};
    use crate::read::Reader;

    #[test]
    fn objects_are_read_on_demand() {
        let data = [0, 2, 1, 1, 3, 4, b'a', b'b', b'c', 0xee];
        let mut r = Reader::new(&data);
        let index = Index::parse(&mut r, Version::Cff1).unwrap();
        assert_eq!(r.offset(), 9);
        assert_eq!(index.len(), 2);
        assert_eq!(index.get(&data, 0), Ok(&b"ab"[..]));
        assert_eq!(index.get(&data, 1), Ok(&b"c"[..]));
        assert!(index.get(&data, 2).is_err());
        let cff2 = [0, 0, 0, 1, 1, 1, 2, b'x'];
        let index = Index::parse(&mut Reader::new(&cff2), Version::Cff2).unwrap();
        assert_eq!(index.get(&cff2, 0), Ok(&b"x"[..]));
        let empty = Index::parse(&mut Reader::new(&[0, 0]), Version::Cff1).unwrap();
        assert_eq!(empty.len(), 0);
    }

    #[test]
    fn malformed_offsets_are_invalid() {
        for bad in [&[0, 1, 5, 1, 2][..], &[0, 1, 1, 1, 9, 0], &[0, 1, 1, 0, 0]] {
            assert!(
                Index::parse(&mut Reader::new(bad), Version::Cff1).is_err(),
                "{bad:?}"
            );
        }
        // A middle offset out of order fails only for the objects it bounds.
        let data = [0, 2, 1, 1, 4, 2, b'a', b'b', b'c'];
        let index = Index::parse(&mut Reader::new(&data), Version::Cff1).unwrap();
        assert!(index.get(&data, 1).is_err());
        assert_eq!(index.get(&data, 0), Ok(&b"abc"[..]));
    }
}

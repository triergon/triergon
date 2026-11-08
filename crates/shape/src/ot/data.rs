//! Lenient big-endian views of OpenType layout data.
//!
//! Layout tables are read where a lookup needs them, without a separate parsing
//! pass. A read beyond the data yields zero and an offset beyond it an empty
//! view, so a damaged structure reads as the absent (all-zero) structure, the way
//! HarfBuzz substitutes its null objects for rejected subtables. Counts are never
//! trusted beyond the bytes that hold the records.

/// A view of layout data starting at one structure.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Data<'a>(pub(crate) &'a [u8]);

impl Data<'_> {
    pub(crate) const EMPTY: Self = Self(&[]);

    pub(crate) fn u8(self, at: usize) -> u8 {
        self.0.get(at).copied().unwrap_or(0)
    }

    pub(crate) fn u16(self, at: usize) -> u16 {
        self.0
            .get(at..)
            .and_then(<[u8]>::first_chunk::<2>)
            .map_or(0, |b| u16::from_be_bytes(*b))
    }

    pub(crate) fn i16(self, at: usize) -> i16 {
        self.u16(at).cast_signed()
    }

    pub(crate) fn u24(self, at: usize) -> u32 {
        self.0
            .get(at..)
            .and_then(<[u8]>::first_chunk::<3>)
            .map_or(0, |b| u32::from_be_bytes([0, b[0], b[1], b[2]]))
    }

    pub(crate) fn u32(self, at: usize) -> u32 {
        self.0
            .get(at..)
            .and_then(<[u8]>::first_chunk::<4>)
            .map_or(0, |b| u32::from_be_bytes(*b))
    }

    /// The view at `offset` from this structure; empty when the offset is zero
    /// (null) or lies outside the data.
    pub(crate) fn at(self, offset: usize) -> Self {
        if offset == 0 {
            return Self::EMPTY;
        }
        Self(self.0.get(offset..).unwrap_or_default())
    }

    /// The structure a 16-bit offset at `at` points to.
    pub(crate) fn offset16(self, at: usize) -> Self {
        self.at(usize::from(self.u16(at)))
    }

    /// The structure a 32-bit offset at `at` points to.
    pub(crate) fn offset32(self, at: usize) -> Self {
        self.at(self.u32(at) as usize)
    }

    pub(crate) const fn is_empty(self) -> bool {
        self.0.is_empty()
    }

    /// The number of `size`-byte records a count at `at` declares, cut to the
    /// records that start at `first` and fit in the data.
    pub(crate) fn count(self, at: usize, first: usize, size: usize) -> usize {
        let declared = usize::from(self.u16(at));
        let room = self.0.len().saturating_sub(first) / size.max(1);
        declared.min(room)
    }
}

/// HarfBuzz's binary search over `count` records: the index whose record
/// `compare` finds equal, if the probes meet one. Unsorted data gives the same
/// answer as in HarfBuzz because the probes are the same.
pub(crate) fn bsearch(
    count: usize,
    compare: impl Fn(usize) -> std::cmp::Ordering,
) -> Option<usize> {
    use std::cmp::Ordering;
    let (mut min, mut max) = (0_isize, count as isize - 1);
    while min <= max {
        let mid = ((min as usize + max as usize) / 2) as isize;
        match compare(mid as usize) {
            Ordering::Greater => max = mid - 1,
            Ordering::Less => min = mid + 1,
            Ordering::Equal => return Some(mid as usize),
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{Data, bsearch};

    #[test]
    fn reads_past_the_end_are_zero() {
        let d = Data(&[0x12, 0x34, 0x56]);
        assert_eq!(d.u16(0), 0x1234);
        assert_eq!(d.u16(2), 0);
        assert_eq!(d.u24(0), 0x123456);
        assert_eq!(d.u32(0), 0);
        assert!(d.at(9).is_empty());
        assert!(d.offset16(0).is_empty());
        assert_eq!(Data(&[0, 9, 1, 2]).count(0, 2, 2), 1);
    }

    #[test]
    fn binary_search_probes_like_harfbuzz() {
        let keys = [1, 3, 5, 7];
        assert_eq!(bsearch(4, |i| keys[i].cmp(&5)), Some(2));
        assert_eq!(bsearch(4, |i| keys[i].cmp(&4)), None);
        assert_eq!(bsearch(0, |_| unreachable!()), None);
    }
}

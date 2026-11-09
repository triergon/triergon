//! The NAL units of a whole access unit or byte stream in memory: Annex B start
//! codes (H.264 and H.265 Annex B) or big-endian length prefixes. The streaming,
//! budgeted reader is [`NalReader`](crate::NalReader).

/// The NAL units of an Annex B byte stream, each without its start code.
///
/// Bytes before the first start code are not a unit. Zero bytes before the next
/// start code (its `zero_byte`, or `trailing_zero_8bits`) are dropped, and so is
/// a unit left empty by that.
#[derive(Clone, Debug)]
pub struct AnnexBUnits<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> AnnexBUnits<'a> {
    #[must_use]
    pub fn new(data: &'a [u8]) -> Self {
        let at = start_code(data, 0).map_or(data.len(), |s| s + 3);
        Self { data, at }
    }
}

impl<'a> Iterator for AnnexBUnits<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        while self.at < self.data.len() {
            let start = self.at;
            let end = start_code(self.data, start).unwrap_or(self.data.len());
            self.at = (end + 3).min(self.data.len());
            let mut unit = &self.data[start..end];
            while let [head @ .., 0] = unit {
                unit = head;
            }
            if !unit.is_empty() {
                return Some(unit);
            }
        }
        None
    }
}

/// The index of the next `00 00 01` at or after `from`.
#[must_use]
pub fn start_code(data: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 2 < data.len() {
        // A start code ends in a byte of at most 1: skip past any other.
        let last = data[i + 2];
        if last > 1 {
            i += 3;
            continue;
        }
        if last == 1 && data[i] == 0 && data[i + 1] == 0 {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// The first NAL unit of `data` after its `width`-byte big-endian length, and
/// the bytes after it; `None` when the length or the unit runs past the end.
/// A length of zero gives an empty unit.
#[must_use]
pub fn split_length_prefixed(data: &[u8], width: usize) -> Option<(&[u8], &[u8])> {
    let (prefix, rest) = data.split_at_checked(width)?;
    let length = prefix
        .iter()
        .fold(0usize, |length, &byte| (length << 8) | usize::from(byte));
    rest.split_at_checked(length)
}

#[cfg(test)]
mod tests {
    use super::{AnnexBUnits, split_length_prefixed, start_code};

    #[test]
    fn annex_b_units_drop_start_codes_and_trailing_zeros() {
        let stream = [
            9, 9, 0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 0, 0, 3, 1, 0, 0, 0, 0, 1, 0x65, 9, 0,
        ];
        let units: Vec<&[u8]> = AnnexBUnits::new(&stream).collect();
        assert_eq!(units, [&[0x67, 1, 2][..], &[0x68, 0, 0, 3, 1], &[0x65, 9]]);
        assert_eq!(AnnexBUnits::new(&[1, 2, 3]).count(), 0);
        assert_eq!(AnnexBUnits::new(&[0, 0, 1, 0, 0, 0, 1]).count(), 0);
        // Every alignment of a start code is found.
        for lead in 0..8 {
            let mut data = vec![7; lead];
            data.extend_from_slice(&[0, 0, 1, 5]);
            assert_eq!(start_code(&data, 0), Some(lead));
        }
    }

    #[test]
    fn length_prefixes_split_or_report_overrun() {
        let sample = [0, 2, 0xaa, 0xbb, 0, 0, 0, 5, 1];
        let (first, rest) = split_length_prefixed(&sample, 2).unwrap();
        assert_eq!((first, rest), (&[0xaa, 0xbb][..], &sample[4..]));
        let (empty, rest) = split_length_prefixed(rest, 2).unwrap();
        assert!(empty.is_empty());
        assert_eq!(split_length_prefixed(rest, 2), None);
        assert_eq!(split_length_prefixed(&[0], 2), None);
    }
}

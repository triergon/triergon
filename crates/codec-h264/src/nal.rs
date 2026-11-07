//! Annex B byte stream splitting and NAL unit header / RBSP extraction (clauses 7.3.1, 7.4.1, B.1).

/// Zero bytes kept after an RBSP, for the CABAC engine's loads.
pub(crate) const RBSP_PAD: usize = 16;

/// Iterate over NAL unit payloads (without start code prefixes) in an Annex B byte stream chunk.
/// Trailing zero bytes (trailing_zero_8bits / next start code's leading zero) are stripped.
#[derive(Debug)]
pub struct NalIter<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> NalIter<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        let pos = find_start_code(data, 0)
            .map(|p| p + 3)
            .unwrap_or(data.len());
        NalIter { data, pos }
    }
}

/// Position of the next 0x000001 at or after `from`.
pub(crate) fn find_start_code(data: &[u8], from: usize) -> Option<usize> {
    let n = data.len();
    let mut i = from;
    while i + 2 < n {
        // Fast skip: a start code needs data[i+2] <= 1.
        let c = data[i + 2];
        if c > 1 {
            i += 3;
            continue;
        }
        if c == 1 && data[i] == 0 && data[i + 1] == 0 {
            return Some(i);
        }
        i += 1;
    }
    None
}

impl<'a> Iterator for NalIter<'a> {
    type Item = &'a [u8];
    fn next(&mut self) -> Option<&'a [u8]> {
        loop {
            if self.pos >= self.data.len() {
                return None;
            }
            let start = self.pos;
            let end = match find_start_code(self.data, start) {
                Some(p) => {
                    self.pos = p + 3;
                    p
                }
                None => {
                    self.pos = self.data.len();
                    self.data.len()
                }
            };
            let mut e = end;
            while e > start && self.data[e - 1] == 0 {
                e -= 1;
            }
            if e > start {
                return Some(&self.data[start..e]);
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NalHeader {
    pub nal_ref_idc: u8,
    pub nal_unit_type: u8,
}

pub(crate) fn parse_header(nal: &[u8]) -> Option<NalHeader> {
    let b = *nal.first()?;
    if b & 0x80 != 0 {
        return None; // forbidden_zero_bit
    }
    Some(NalHeader {
        nal_ref_idc: (b >> 5) & 3,
        nal_unit_type: b & 31,
    })
}

/// Remove emulation_prevention_three_byte (clause 7.4.1) from `src` into `dst`, append
/// zero padding, and return the RBSP length (trailing zero bytes trimmed).
pub(crate) fn unescape(src: &[u8], dst: &mut Vec<u8>) -> usize {
    dst.clear();
    dst.reserve(src.len() + RBSP_PAD);
    triergon_bitstream::unescape(src, dst);
    let mut len = dst.len();
    while len > 0 && dst[len - 1] == 0 {
        len -= 1;
    }
    dst.truncate(len);
    dst.resize(len + RBSP_PAD, 0);
    len
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_and_unescape() {
        let s = [
            0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 0, 0, 3, 1, 0, 0, 0, 0, 1, 0x65, 9,
        ];
        let v: Vec<&[u8]> = NalIter::new(&s).collect();
        assert_eq!(
            v,
            vec![&[0x67u8, 1, 2][..], &[0x68, 0, 0, 3, 1][..], &[0x65, 9][..]]
        );
        let mut d = Vec::new();
        let n = unescape(v[1], &mut d);
        assert_eq!(&d[..n], &[0x68, 0, 0, 1]);
        assert!(d.len() >= n + RBSP_PAD);
    }
}

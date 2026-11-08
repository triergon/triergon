//! NAL unit handling: Annex B byte stream splitting (Annex B.2), NAL unit header
//! (clause 7.3.1.2), emulation prevention removal (clause 7.3.1.1) and an access unit
//! splitter (clause 7.4.2.4.4) for elementary-stream input.

use triergon_bitstream::runs;
use triergon_codec::{BudgetVec, StorageError};

pub const RADL_N: u8 = 6;
pub const RADL_R: u8 = 7;
pub const RASL_N: u8 = 8;
pub const RASL_R: u8 = 9;
pub const RSV_VCL_N14: u8 = 14;
pub const BLA_W_LP: u8 = 16;
pub const BLA_N_LP: u8 = 18;
pub const IDR_W_RADL: u8 = 19;
pub const IDR_N_LP: u8 = 20;
pub const CRA_NUT: u8 = 21;
pub const RSV_IRAP_23: u8 = 23;
pub const VPS_NUT: u8 = 32;
pub const SPS_NUT: u8 = 33;
pub const PPS_NUT: u8 = 34;
pub const AUD_NUT: u8 = 35;
pub const EOS_NUT: u8 = 36;
pub const EOB_NUT: u8 = 37;
pub const PREFIX_SEI: u8 = 39;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NalHeader {
    pub nal_unit_type: u8,
    pub nuh_layer_id: u8,
    pub temporal_id: u8,
}

impl NalHeader {
    pub fn parse(b: &[u8]) -> Option<NalHeader> {
        if b.len() < 2 {
            return None;
        }
        if b[0] & 0x80 != 0 {
            return None;
        }
        let nal_unit_type = (b[0] >> 1) & 0x3f;
        let nuh_layer_id = ((b[0] & 1) << 5) | (b[1] >> 3);
        let tid1 = b[1] & 7;
        if tid1 == 0 {
            return None;
        }
        Some(NalHeader {
            nal_unit_type,
            nuh_layer_id,
            temporal_id: tid1 - 1,
        })
    }

    #[inline]
    pub fn is_vcl(&self) -> bool {
        self.nal_unit_type < 32
    }
}

#[inline]
pub fn is_irap(t: u8) -> bool {
    (BLA_W_LP..=RSV_IRAP_23).contains(&t)
}
#[inline]
pub fn is_idr(t: u8) -> bool {
    t == IDR_W_RADL || t == IDR_N_LP
}
#[inline]
pub fn is_bla(t: u8) -> bool {
    (BLA_W_LP..=BLA_N_LP).contains(&t)
}
#[inline]
pub fn is_rasl(t: u8) -> bool {
    t == RASL_N || t == RASL_R
}
#[inline]
pub fn is_radl(t: u8) -> bool {
    t == RADL_N || t == RADL_R
}
/// Sub-layer non-reference picture (clause 7.4.2.2): even types below 16 (excluding reserved).
#[inline]
pub fn is_sub_layer_non_ref(t: u8) -> bool {
    t <= RSV_VCL_N14 && t.is_multiple_of(2)
}

/// Iterate over NAL unit payloads (without start codes) in an Annex B byte stream.
#[derive(Debug)]
pub struct AnnexBIter<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> AnnexBIter<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        let pos = find_start_code(data, 0)
            .map(|(_, end)| end)
            .unwrap_or(data.len());
        AnnexBIter { data, pos }
    }
}

/// Find the next 0x000001 start code at or after `from`. Returns (start index of the
/// zero bytes, index right after the 0x01).
fn find_start_code(data: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut i = from;
    while i + 3 <= data.len() {
        if data[i + 2] > 1 {
            i += 3;
            continue;
        }
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            return Some((i, i + 3));
        }
        i += 1;
    }
    None
}

impl<'a> Iterator for AnnexBIter<'a> {
    type Item = &'a [u8];
    fn next(&mut self) -> Option<&'a [u8]> {
        while self.pos < self.data.len() {
            let start = self.pos;
            let (end, next) = match find_start_code(self.data, start) {
                Some((s, e)) => (s, e),
                None => (self.data.len(), self.data.len()),
            };
            self.pos = next;
            // Trailing zero bytes belong to the next start code (zero_byte) or trailing_zero_8bits.
            let mut e = end;
            while e > start && self.data[e - 1] == 0 {
                e -= 1;
            }
            if e > start {
                return Some(&self.data[start..e]);
            }
        }
        None
    }
}

/// Remove emulation prevention bytes (clause 7.3.1.1) from `src` (a NAL unit including its
/// two header bytes) into `dst` (header bytes are dropped). `dst` is cleared first and
/// charged for the whole payload before it grows.
pub fn unescape_rbsp(src: &[u8], dst: &mut BudgetVec<u8>) -> Result<(), StorageError> {
    let ebsp = src.get(2..).unwrap_or(&[]);
    dst.clear();
    dst.try_reserve_exact(ebsp.len())?;
    for run in runs(ebsp) {
        dst.try_extend_from_slice(run)?;
    }
    Ok(())
}

/// Splits an Annex B elementary stream into access units (clause 7.4.2.4.4 rules for the
/// first NAL unit of an access unit, base layer only). Each yielded slice includes start codes.
#[derive(Debug)]
pub struct AccessUnitSplitter<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> AccessUnitSplitter<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        AccessUnitSplitter { data, pos: 0 }
    }
}

fn starts_new_au(nal: &[u8]) -> Option<(bool, bool)> {
    // Returns (is_vcl_base_layer, begins_new_au_if_vcl_seen)
    let h = NalHeader::parse(nal)?;
    if h.nuh_layer_id > 0 {
        return Some((false, false));
    }
    let t = h.nal_unit_type;
    if t < 32 {
        if t > 21 {
            return Some((false, false));
        }
        let first_slice = nal.len() > 2 && nal[2] & 0x80 != 0;
        return Some((true, first_slice));
    }
    let begins =
        matches!(t, VPS_NUT | SPS_NUT | PPS_NUT | AUD_NUT | PREFIX_SEI | 41..=44 | 48..=55);
    Some((false, begins))
}

impl<'a> Iterator for AccessUnitSplitter<'a> {
    type Item = &'a [u8];
    fn next(&mut self) -> Option<&'a [u8]> {
        if self.pos >= self.data.len() {
            return None;
        }
        let au_start = self.pos;
        let mut seen_vcl = false;
        let mut i = match find_start_code(self.data, self.pos) {
            Some((s, _)) => s,
            None => {
                self.pos = self.data.len();
                return Some(&self.data[au_start..]);
            }
        };
        loop {
            let (sc_start, nal_start) = match find_start_code(self.data, i) {
                Some(v) => v,
                None => {
                    self.pos = self.data.len();
                    return Some(&self.data[au_start..]);
                }
            };
            // Include a preceding zero_byte in the boundary.
            let boundary = if sc_start > au_start && self.data[sc_start - 1] == 0 {
                sc_start - 1
            } else {
                sc_start
            };
            let nal = &self.data[nal_start..];
            if let Some((is_vcl, begins)) = starts_new_au(nal) {
                if seen_vcl && begins {
                    self.pos = boundary;
                    return Some(&self.data[au_start..boundary]);
                }
                if is_vcl {
                    seen_vcl = true;
                }
                if !seen_vcl && boundary > au_start && i == au_start {
                    // nothing
                }
                // EOS / EOB end the AU after themselves.
                let h = NalHeader::parse(nal);
                if let Some(h) = h
                    && seen_vcl
                    && h.nuh_layer_id == 0
                    && (h.nal_unit_type == EOS_NUT || h.nal_unit_type == EOB_NUT)
                {
                    // AU ends after this NAL.
                    let end = find_start_code(self.data, nal_start)
                        .map(|(s, _)| s)
                        .unwrap_or(self.data.len());
                    self.pos = end;
                    return Some(&self.data[au_start..end]);
                }
            }
            i = nal_start;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unescape() {
        let budget = triergon_codec::Budget::new(1 << 12).unwrap();
        let src = [0x40, 0x01, 0x00, 0x00, 0x03, 0x01, 0x00, 0x00, 0x03];
        let mut d = BudgetVec::new(&budget);
        unescape_rbsp(&src, &mut d).unwrap();
        assert_eq!(&d[..], &[0, 0, 1, 0, 0]);
    }

    #[test]
    fn annexb_split() {
        let s = [
            0, 0, 0, 1, 0x40, 0x01, 0xaa, 0, 0, 1, 0x42, 0x01, 0xbb, 0x00,
        ];
        let v: Vec<&[u8]> = AnnexBIter::new(&s).collect();
        assert_eq!(
            v,
            vec![&[0x40u8, 0x01, 0xaa][..], &[0x42u8, 0x01, 0xbb][..]]
        );
    }
}

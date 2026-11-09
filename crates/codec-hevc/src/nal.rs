//! NAL unit handling: Annex B byte stream splitting (Annex B.2), NAL unit header
//! (clause 7.3.1.2), emulation prevention removal (clause 7.3.1.1) and an access unit
//! splitter (clause 7.4.2.4.4) for elementary-stream input.

use triergon_bitstream::runs;
use triergon_codec::{BudgetVec, StorageError};
use triergon_framing::{self as framing, hevc, start_code};

/// The decoder's view of a NAL unit header (clause 7.3.1.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NalHeader {
    pub nal_unit_type: u8,
    pub nuh_layer_id: u8,
    pub temporal_id: u8,
}

impl NalHeader {
    /// `None` for a truncated header, a set forbidden_zero_bit or a zero
    /// temporal_id_plus1.
    pub fn parse(b: &[u8]) -> Option<NalHeader> {
        match framing::NalHeader::parse(framing::Codec::Hevc, b).ok()? {
            framing::NalHeader::Hevc {
                nal_unit_type,
                layer_id,
                temporal_id_plus_one,
            } => Some(NalHeader {
                nal_unit_type,
                nuh_layer_id: layer_id,
                temporal_id: temporal_id_plus_one - 1,
            }),
            framing::NalHeader::Avc { .. } => None,
        }
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
    if hevc::is_slice(t) {
        let first_slice = nal.len() > 2 && nal[2] & 0x80 != 0;
        return Some((true, first_slice));
    }
    Some((false, hevc::begins_access_unit(t)))
}

impl<'a> Iterator for AccessUnitSplitter<'a> {
    type Item = &'a [u8];
    fn next(&mut self) -> Option<&'a [u8]> {
        if self.pos >= self.data.len() {
            return None;
        }
        let au_start = self.pos;
        let mut seen_vcl = false;
        let mut i = match start_code(self.data, self.pos) {
            Some(s) => s,
            None => {
                self.pos = self.data.len();
                return Some(&self.data[au_start..]);
            }
        };
        loop {
            let (sc_start, nal_start) = match start_code(self.data, i) {
                Some(s) => (s, s + 3),
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
                    && (h.nal_unit_type == hevc::EOS || h.nal_unit_type == hevc::EOB)
                {
                    // AU ends after this NAL.
                    let end = start_code(self.data, nal_start).unwrap_or(self.data.len());
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

    /// A reserved VCL type (10 to 15) neither starts an access unit nor counts
    /// as its slice, as in FFmpeg's HEVC parser.
    #[test]
    fn reserved_vcl_types_do_not_split_access_units() {
        // TRAIL_R (type 1) with first_slice_segment_in_pic_flag set.
        let trail = [0, 0, 1, 0x02, 0x01, 0x80];
        for reserved in 10..=15 {
            let mut stream = trail.to_vec();
            stream.extend_from_slice(&[0, 0, 1, reserved << 1, 0x01, 0x80]);
            stream.extend_from_slice(&trail);
            let units: Vec<&[u8]> = AccessUnitSplitter::new(&stream).collect();
            assert_eq!(units, [&stream[..12], &stream[12..]], "type {reserved}");
            // Alone, the reserved unit is not a picture to close.
            let alone = [0, 0, 1, reserved << 1, 0x01, 0x80, 0, 0, 1, 0x40, 0x01, 0];
            assert_eq!(
                AccessUnitSplitter::new(&alone).count(),
                1,
                "type {reserved}"
            );
        }
    }
}

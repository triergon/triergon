//! NAL unit handling: Annex B byte stream splitting (Annex B.2), NAL unit header
//! (clause 7.3.1.2), emulation prevention removal (clause 7.3.1.1) and an access unit
//! splitter (clause 7.4.2.4.4) for elementary-stream input.

use triergon_bitstream::runs;
use triergon_codec::{BudgetVec, StorageError};
use triergon_framing::{
    hevc,
    nal::{self as framing, start_code},
};

/// The decoder's view of a NAL unit header (clause 7.3.1.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NalHeader {
    pub(crate) nal_unit_type: u8,
    pub(crate) nuh_layer_id: u8,
    pub(crate) temporal_id: u8,
}

impl NalHeader {
    /// `None` for a truncated header, a set forbidden_zero_bit or a zero
    /// temporal_id_plus1.
    pub(crate) fn parse(b: &[u8]) -> Option<NalHeader> {
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
pub(crate) fn unescape_rbsp(src: &[u8], dst: &mut BudgetVec<u8>) -> Result<(), StorageError> {
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
    /// A splitter over the whole stream `data`.
    pub fn new(data: &'a [u8]) -> Self {
        AccessUnitSplitter { data, pos: 0 }
    }
}

/// How the NAL unit at the start of `nal` bounds access units: whether it is a base-layer
/// slice, and whether it begins a new access unit once one has a slice. `None` for a
/// unit without a valid header, which bounds nothing.
fn starts_new_au(nal: &[u8]) -> Option<(bool, bool)> {
    let h = NalHeader::parse(nal)?;
    if h.nuh_layer_id > 0 {
        return Some((false, false));
    }
    let t = h.nal_unit_type;
    if hevc::is_slice(t) {
        let first_slice = nal.len() > 2 && nal[2] & 0x80 != 0;
        return Some((true, first_slice));
    }
    // FFmpeg's parser looks at a NAL unit once a byte after its two-byte
    // header is in, so a header-only unit that ends the stream begins nothing.
    Some((false, nal.len() > 2 && hevc::begins_access_unit(t)))
}

impl<'a> Iterator for AccessUnitSplitter<'a> {
    type Item = &'a [u8];
    fn next(&mut self) -> Option<&'a [u8]> {
        if self.pos >= self.data.len() {
            return None;
        }
        let start = self.pos;
        let mut seen_vcl = false;
        let mut from = start;
        while let Some(code) = start_code(self.data, from) {
            let nal_start = code + 3;
            if let Some((is_vcl, begins)) = starts_new_au(&self.data[nal_start..]) {
                if seen_vcl && begins {
                    // Include a preceding zero_byte in the boundary.
                    let boundary = if code > start && self.data[code - 1] == 0 {
                        code - 1
                    } else {
                        code
                    };
                    self.pos = boundary;
                    return Some(&self.data[start..boundary]);
                }
                seen_vcl |= is_vcl;
            }
            from = nal_start;
        }
        self.pos = self.data.len();
        Some(&self.data[start..])
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

    /// An end of sequence or bitstream starts the next access unit, as in
    /// FFmpeg's HEVC parser, which looks at a unit once a byte after its header
    /// is in: one ending the stream stays in the last access unit unless a
    /// trailing zero byte follows it.
    #[test]
    fn end_of_sequence_and_bitstream_start_access_units() {
        let trail = [0, 0, 1, 0x02, 0x01, 0x80];
        for end in [hevc::EOS, hevc::EOB] {
            let mut stream = trail.to_vec();
            stream.extend_from_slice(&[0, 0, 1, end << 1, 0x01]);
            stream.extend_from_slice(&trail);
            stream.extend_from_slice(&[0, 0, 1, end << 1, 0x01]);
            let units: Vec<&[u8]> = AccessUnitSplitter::new(&stream).collect();
            assert_eq!(units, [&stream[..6], &stream[6..]], "type {end}");
            stream.push(0);
            let units: Vec<&[u8]> = AccessUnitSplitter::new(&stream).collect();
            assert_eq!(
                units,
                [&stream[..6], &stream[6..17], &stream[17..]],
                "type {end}"
            );
        }
    }
}

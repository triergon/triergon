//! Fixed VVC header facts; no RBSP parsing or access-unit detection.

use crate::AnnexBUnits;
use triergon_core::{Error, Result};

/// The two-byte H.266 §7.3.1.2 header, including nonconforming values.
/// Inspecting it does not establish type-specific or payload conformance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VvcNalHeader {
    pub forbidden_zero_bit: bool,
    pub reserved_zero_bit: bool,
    pub layer_id: u8,
    pub nal_unit_type: u8,
    pub temporal_id_plus_one: u8,
}

impl VvcNalHeader {
    /// Reads only two bytes. All bit patterns are retained; a short header is
    /// `Truncated`. H.266 §7.4.2.2 requires the forbidden bit to be zero and
    /// the temporal identifier plus one to be nonzero.
    pub fn inspect(nal: &[u8]) -> Result<Self> {
        let [first, second, ..] = *nal else {
            return Err(Error::Truncated);
        };
        Ok(Self {
            forbidden_zero_bit: first & 0x80 != 0,
            reserved_zero_bit: first & 0x40 != 0,
            layer_id: first & 0x3f,
            nal_unit_type: second >> 3,
            temporal_id_plus_one: second & 7,
        })
    }

    /// H.266 (09/2023) §7.4.2.2 requires decoders to discard a NAL with
    /// the reserved bit set or a layer identifier above 55. No decoder runs here.
    #[must_use]
    pub const fn requires_discard(self) -> bool {
        self.reserved_zero_bit || self.layer_id > 55
    }
}

/// A borrowed NAL, with its original bytes and independently inspected header.
#[derive(Debug)]
pub struct VvcNal<'a> {
    pub bytes: &'a [u8],
    pub header: Result<VvcNalHeader>,
}

/// Allocation-free inspection of a complete, caller-bounded Annex B buffer.
///
/// Reuses `AnnexBUnits`: ignores bytes before the first prefix, empty units and
/// trailing zeros. Preserves emulation-prevention bytes. This is extraction,
/// not Annex B conformance validation (including mandatory `zero_byte` rules).
/// Never pass an arbitrary PES fragment as if it were a complete byte stream:
/// prefixes, headers and payloads may cross PES boundaries. No AU is inferred.
#[derive(Debug)]
pub struct VvcAnnexBUnits<'a>(AnnexBUnits<'a>);

impl<'a> VvcAnnexBUnits<'a> {
    /// Refuses an oversized buffer before scanning. Total work is linear in
    /// `data.len()`, bounded by `maximum_bytes`; no input-dependent allocation.
    pub fn new(data: &'a [u8], maximum_bytes: usize) -> Result<Self> {
        if data.len() > maximum_bytes {
            return Err(Error::Limit {
                requested: data.len(),
                maximum: maximum_bytes,
            });
        }
        Ok(Self(AnnexBUnits::new(data)))
    }
}

impl<'a> Iterator for VvcAnnexBUnits<'a> {
    type Item = VvcNal<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(|bytes| VvcNal {
            bytes,
            header: VvcNalHeader::inspect(bytes),
        })
    }
}

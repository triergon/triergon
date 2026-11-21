//! The core header in the first payload bits of a frame of any packing. Zero bits
//! that pad the payload to whole bytes are never read as header bits.
use crate::dts::core_frame::CORE_SYNC;
use crate::dts::inspect::{required_header_bits, validate_header};
use crate::dts::{
    DtsCoreField as F, DtsInspectionIssues, DtsInspectionSourceStatus, DtsInspectionViews,
    FieldObservation,
};
use triergon_bitstream::bits_at;
use triergon_core::{Error, Result};

/// The core header fields found in the first payload bits of a frame, as
/// [`inspect_header_bits`] reads them: what is present of each field, the
/// constraints the header breaks and the values its fields stand for. It keeps a
/// copy of the header bits, at most 120, and nothing else of the frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DtsPackedHeader {
    prefix: HeaderPrefix,
    issues: DtsInspectionIssues,
    views: DtsInspectionViews,
    required: Option<u16>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HeaderPrefix {
    bytes: [u8; 15],
    observed_bits: u8,
}

impl HeaderPrefix {
    fn field(&self, field: F) -> FieldObservation {
        // Table 5-1 offsets without HCRC. CPF at bit 38 controls its presence.
        const OFFSETS: [u8; 27] = [
            0, 32, 33, 38, 39, 46, 60, 66, 70, 75, 76, 77, 78, 79, 80, 83, 84, 85, 87, 88, 88, 89,
            93, 95, 98, 99, 100,
        ];
        let cpf = self.observed_bits >= 39 && self.bytes[4] & 2 != 0;
        if field == F::Hcrc && self.observed_bits >= 39 && !cpf {
            return FieldObservation::NotInSyntax;
        }
        let mut offset = OFFSETS[field as usize];
        if field as usize >= F::Filter as usize && cpf {
            offset += 16;
        }
        if offset >= self.observed_bits {
            return FieldObservation::NotReached;
        }
        let width = field.width();
        let count = width.min(self.observed_bits - offset);
        let Some(prefix) = bits_at(&self.bytes, usize::from(offset), u32::from(count)) else {
            return FieldObservation::NotReached;
        };
        FieldObservation::Bits {
            offset: u16::from(offset),
            width,
            observed_bits: count,
            prefix,
        }
    }
}

impl DtsPackedHeader {
    /// The source bits of a field, including any partial prefix.
    pub fn field(&self, field: F) -> FieldObservation {
        self.prefix.field(field)
    }
    /// The value of `field` when all of its bits are present; `None` when it is
    /// partial, not reached or not in the header.
    pub fn value(&self, field: F) -> Option<u32> {
        self.field(field).value()
    }
    /// The constraints the header breaks, as a mask of the
    /// [`DtsInspectionIssues`] flags.
    pub const fn issues(&self) -> u32 {
        self.issues.bits()
    }
    /// The values the complete fields stand for.
    pub const fn views(&self) -> DtsInspectionViews {
        self.views
    }
    /// Length of the header in bits, sync word included: 104, or 120 with
    /// `HCRC`. `None` unless the core sync word and `CPF` are both complete.
    pub const fn required_header_bits(&self) -> Option<u16> {
        self.required
    }
    /// Whether the bits begin with the core sync word and hold every header
    /// field completely.
    pub fn syntax_complete(&self) -> bool {
        self.value(F::Sync) == Some(CORE_SYNC) && self.value(F::DialogWord).is_some()
    }
    /// The header's class, derived from [`issues`](Self::issues).
    pub fn source_status(&self) -> DtsInspectionSourceStatus {
        self.issues.source_status()
    }
}

/// Inspects the core header in the first `valid_bits` bits of `storage`, most
/// significant bit first, as
/// [`DtsPackedPayload::storage_bytes`](crate::dts::DtsPackedPayload::storage_bytes)
/// holds a frame unpacked from any packing. Bits after `valid_bits` are ignored.
/// At most 120 bits are read, and only the header prefix and the values derived
/// from it are kept. A header that breaks the specification, or that the bits
/// end inside, is reported in the result.
///
/// Fails with `Invalid` when `valid_bits` exceeds the bits `storage` holds.
pub fn inspect_header_bits(storage: &[u8], valid_bits: usize) -> Result<DtsPackedHeader> {
    if valid_bits > storage.len().checked_mul(8).ok_or(Error::Overflow)? {
        return Err(Error::Invalid("DTS valid bit extent"));
    }
    let has_crc = valid_bits >= 39 && storage[4] & 2 != 0;
    let observed = valid_bits.min(if has_crc { 120 } else { 104 });
    let mut bytes = [0; 15];
    let len = observed.div_ceil(8);
    bytes[..len].copy_from_slice(&storage[..len]);
    if !observed.is_multiple_of(8) {
        bytes[len - 1] &= u8::MAX << (8 - observed % 8);
    }
    let prefix = HeaderPrefix {
        bytes,
        observed_bits: observed as u8,
    };
    let value = |field| prefix.field(field).value();
    let (issues, views) = validate_header(value)?;
    let required = required_header_bits(value(F::Sync), value(F::CrcPresent));
    Ok(DtsPackedHeader {
        prefix,
        issues,
        views,
        required,
    })
}

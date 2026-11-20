//! Original exact-bit header facts; output alignment zeros are never source bits.
use crate::dts_inspect::{CORE_SYNC, required_header_bits, validate_header};
use crate::{
    DtsCoreField as F, DtsInspectionIssues, DtsInspectionSourceStatus, DtsInspectionViews,
    FieldObservation,
};
use triergon_bitstream::bits_at;
use triergon_core::{Error, Result};

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
    pub fn value(&self, field: F) -> Option<u32> {
        self.field(field).value()
    }
    pub const fn issues(&self) -> u32 {
        self.issues.bits()
    }
    pub const fn views(&self) -> DtsInspectionViews {
        self.views
    }
    pub const fn required_header_bits(&self) -> Option<u16> {
        self.required
    }
    pub fn syntax_complete(&self) -> bool {
        self.value(F::Sync) == Some(CORE_SYNC) && self.value(F::DialogWord).is_some()
    }
    pub fn source_status(&self) -> DtsInspectionSourceStatus {
        self.issues.source_status()
    }
}

/// Inspect up to 120 source bits, keeping only the header prefix and its derived views.
pub fn inspect_dts_header_bits(storage: &[u8], valid_bits: usize) -> Result<DtsPackedHeader> {
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

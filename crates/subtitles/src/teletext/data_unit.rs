//! EN 300 472 PES data fields: the data identifier and its data units.

/// Bytes in one teletext data unit: identifier, length and the 44-byte field.
pub(crate) const DATA_UNIT_BYTES: usize = 46;

/// Length of the data field of an EBU teletext data unit.
const TELETEXT_UNIT_LENGTH: u8 = 0x2C;

/// What a data unit carries (EN 300 472 table 2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DataUnitKind {
    /// EBU teletext non-subtitle data (`0x02`).
    Teletext,
    /// EBU teletext subtitle data (`0x03`).
    Subtitle,
    /// Stuffing (`0xFF`).
    Stuffing,
    /// Any other identifier, reserved or for another service.
    Other(u8),
}

impl DataUnitKind {
    #[must_use]
    pub(crate) fn from_id(id: u8) -> Self {
        match id {
            0x02 => Self::Teletext,
            0x03 => Self::Subtitle,
            0xFF => Self::Stuffing,
            other => Self::Other(other),
        }
    }

    /// Whether the unit holds one EBU teletext packet.
    #[must_use]
    pub(crate) fn is_teletext(self) -> bool {
        matches!(self, Self::Teletext | Self::Subtitle)
    }
}

/// One EBU teletext data unit with its packet in transmission bit order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TeletextUnit {
    pub kind: DataUnitKind,
    /// `true` for the first field.
    pub field_parity: bool,
    /// VBI line within the field, `0` when undefined.
    pub line_offset: u8,
    pub framing_code: u8,
    /// Magazine and row address followed by the 40 data bytes, bit b1 in the
    /// least significant position of each byte.
    pub packet: [u8; 42],
}

/// One data unit as it is framed in the data field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DataUnitHeader {
    pub kind: DataUnitKind,
    pub length: u8,
}

impl DataUnitHeader {
    /// Whether this is an EBU teletext unit with the length EN 300 472 requires.
    #[must_use]
    pub(crate) fn is_well_formed_teletext(self) -> bool {
        self.kind.is_teletext() && self.length == TELETEXT_UNIT_LENGTH
    }
}

impl TeletextUnit {
    /// The teletext unit framed at the start of `unit`, if it is one.
    #[must_use]
    pub(crate) fn parse(unit: &[u8; DATA_UNIT_BYTES]) -> Option<Self> {
        let header = DataUnitHeader {
            kind: DataUnitKind::from_id(unit[0]),
            length: unit[1],
        };
        if !header.is_well_formed_teletext() {
            return None;
        }
        let mut packet = [0; 42];
        for (out, &byte) in packet.iter_mut().zip(&unit[4..]) {
            // EN 300 472 puts the first transmitted bit in the most significant position.
            *out = byte.reverse_bits();
        }
        Some(Self {
            kind: header.kind,
            field_parity: unit[2] & 0x20 != 0,
            line_offset: unit[2] & 0x1F,
            framing_code: unit[3],
            packet,
        })
    }
}

/// The data identifiers of EBU data (EN 300 472) and of EN 301 775 data that
/// carries teletext units.
#[must_use]
pub(crate) fn is_teletext_data_identifier(identifier: u8) -> bool {
    matches!(identifier, 0x10..=0x1F | 0x99..=0x9B)
}

/// A PES data field: its identifier and the data units after it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct DataField<'a> {
    pub data_identifier: u8,
    units: &'a [u8],
}

impl<'a> DataField<'a> {
    /// Splits a PES packet payload; `None` when it is empty.
    #[must_use]
    pub(crate) fn new(payload: &'a [u8]) -> Option<Self> {
        let (&data_identifier, units) = payload.split_first()?;
        Some(Self {
            data_identifier,
            units,
        })
    }

    /// The bytes after the data identifier.
    #[must_use]
    pub(crate) fn unit_bytes(&self) -> &'a [u8] {
        self.units
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_teletext_unit_in_transmission_order() {
        let mut unit = [0u8; DATA_UNIT_BYTES];
        unit[..4].copy_from_slice(&[0x03, 0x2C, 0xE7, 0xE4]);
        unit[4] = 0xA8;
        let parsed = TeletextUnit::parse(&unit).expect("teletext unit");
        assert_eq!(parsed.kind, DataUnitKind::Subtitle);
        assert!(parsed.field_parity);
        assert_eq!(parsed.line_offset, 7);
        assert_eq!(parsed.packet[0], 0x15);
        unit[1] = 0x2B;
        assert_eq!(TeletextUnit::parse(&unit), None);
    }
}

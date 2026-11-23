//! EN 300 472 data units, as a PES data field carries them.

/// Bytes in one teletext data unit: identifier, length and the 44-byte field.
pub(crate) const DATA_UNIT_BYTES: usize = 46;

/// Length of the data field of an EBU teletext data unit.
pub(crate) const TELETEXT_UNIT_LENGTH: u8 = 0x2C;

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

impl TeletextUnit {
    /// The teletext unit framed at the start of `unit`, if it is one with
    /// the length EN 300 472 requires.
    #[must_use]
    pub(crate) fn parse(unit: &[u8; DATA_UNIT_BYTES]) -> Option<Self> {
        let kind = DataUnitKind::from_id(unit[0]);
        if !kind.is_teletext() || unit[1] != TELETEXT_UNIT_LENGTH {
            return None;
        }
        let mut packet = [0; 42];
        for (out, &byte) in packet.iter_mut().zip(&unit[4..]) {
            // EN 300 472 puts the first transmitted bit in the most significant position.
            *out = byte.reverse_bits();
        }
        Some(Self {
            kind,
            field_parity: unit[2] & 0x20 != 0,
            line_offset: unit[2] & 0x1F,
            framing_code: unit[3],
            packet,
        })
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

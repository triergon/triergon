//! `cc_data()` constructs (CEA-708 §4.4, ATSC A/53 Part 4): a sequence of
//! three-byte constructs, each a flag byte and two data bytes.
//!
//! The flag byte carries five marker bits, `cc_valid` (bit 2) and `cc_type`
//! (bits 0 and 1): types 0 and 1 are CEA-608 byte pairs of field 1 and field 2,
//! types 2 and 3 carry DTVCC (CEA-708) packet data, 3 starting a packet.

/// What a construct carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CcType {
    /// A CEA-608 byte pair of field 1 (CC1, CC2, T1, T2).
    Field1,
    /// A CEA-608 byte pair of field 2 (CC3, CC4, T3, T4, XDS).
    Field2,
    /// Two bytes continuing a DTVCC packet.
    DtvccData,
    /// The first two bytes of a DTVCC packet.
    DtvccStart,
}

impl CcType {
    /// The type of a flag byte.
    #[must_use]
    pub const fn of(flags: u8) -> Self {
        match flags & 3 {
            0 => Self::Field1,
            1 => Self::Field2,
            2 => Self::DtvccData,
            _ => Self::DtvccStart,
        }
    }

    /// The type's `cc_type` value.
    #[must_use]
    pub const fn code(self) -> u8 {
        self as u8
    }
}

/// One construct.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Triplet {
    /// The flag byte as sent, marker bits included.
    pub flags: u8,
    pub data: [u8; 2],
}

impl Triplet {
    #[must_use]
    pub const fn valid(self) -> bool {
        self.flags & 4 != 0
    }

    #[must_use]
    pub const fn kind(self) -> CcType {
        CcType::of(self.flags)
    }

    /// The construct's three bytes.
    #[must_use]
    pub const fn bytes(self) -> [u8; 3] {
        [self.flags, self.data[0], self.data[1]]
    }
}

/// The constructs of `cc_data`, in order; a trailing partial construct is not one.
pub fn triplets(cc_data: &[u8]) -> impl ExactSizeIterator<Item = Triplet> + Clone + '_ {
    cc_data.chunks_exact(3).map(|c| Triplet {
        flags: c[0],
        data: [c[1], c[2]],
    })
}

//! `cc_data()` constructs (CEA-708 §4.4, ATSC A/53 Part 4): a sequence of
//! three-byte constructs, each a flag byte and two data bytes.
//!
//! The flag byte carries five marker bits, `cc_valid` (bit 2) and `cc_type`
//! (bits 0 and 1): types 0 and 1 are CEA-608 byte pairs of field 1 and field 2,
//! types 2 and 3 carry DTVCC (CEA-708) packet data, 3 starting a packet.

/// The bits of a flag byte that say whether a construct is a CEA-608 byte pair
/// of a field: `cc_valid` and `cc_type`.
const SELECTION: u8 = 0b111;

/// Whether a construct with flag byte `flags` is a valid CEA-608 byte pair of
/// `field` (0 or 1): `cc_valid` set and `cc_type` equal to the field.
#[must_use]
pub(crate) const fn is_field_pair(flags: u8, field: u8) -> bool {
    flags & SELECTION == 0b100 | field
}

/// The constructs of `cc_data`, in order: flag byte and two data bytes each. A
/// trailing partial construct is not one.
#[must_use]
pub(crate) fn constructs(cc_data: &[u8]) -> &[[u8; 3]] {
    cc_data.as_chunks().0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_field_pair_is_valid_and_of_that_field() {
        for flags in 0..=u8::MAX {
            for field in [0, 1] {
                let valid = flags & 4 != 0;
                let cc_type = flags & 3;
                assert_eq!(
                    is_field_pair(flags, field),
                    valid && cc_type == field,
                    "{flags:#x}"
                );
            }
        }
    }

    #[test]
    fn a_trailing_partial_construct_is_dropped() {
        assert_eq!(constructs(&[0xfc, 1, 2, 0xfd, 3]), &[[0xfc, 1, 2]]);
        assert!(constructs(&[0xfc, 1]).is_empty());
    }
}

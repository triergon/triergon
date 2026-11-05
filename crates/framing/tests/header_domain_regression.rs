use triergon_core::Error;
use triergon_framing::{Codec, NalHeader};
#[test]
fn every_hevc_fixed_header_preserves_error_priority_and_fields() {
    assert_eq!(NalHeader::parse(Codec::Hevc, &[]), Err(Error::Truncated));
    for first in 0u8..=255 {
        assert_eq!(
            NalHeader::parse(Codec::Hevc, &[first]),
            Err(Error::Truncated)
        );
        for second in 0u8..=255 {
            let actual = NalHeader::parse(Codec::Hevc, &[first, second]);
            let expected = if first & 128 != 0 {
                Err(Error::Invalid("NAL forbidden_zero_bit"))
            } else if second & 7 == 0 {
                Err(Error::Invalid("HEVC temporal_id_plus1 is zero"))
            } else {
                Ok(NalHeader::Hevc {
                    nal_unit_type: (first >> 1) & 63,
                    layer_id: ((first & 1) << 5) | (second >> 3),
                    temporal_id_plus_one: second & 7,
                })
            };
            assert_eq!(actual, expected, "fixed header {first:02x}{second:02x}");
        }
    }
}
#[test]
fn every_avc_fixed_header_preserves_the_public_result() {
    assert_eq!(NalHeader::parse(Codec::Avc, &[]), Err(Error::Truncated));
    for first in 0u8..=255 {
        let actual = NalHeader::parse(Codec::Avc, &[first]);
        let expected = if first & 128 != 0 {
            Err(Error::Invalid("NAL forbidden_zero_bit"))
        } else {
            Ok(NalHeader::Avc {
                nal_ref_idc: (first >> 5) & 3,
                nal_unit_type: first & 31,
            })
        };
        assert_eq!(actual, expected);
    }
}

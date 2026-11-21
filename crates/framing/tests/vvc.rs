//! Specification controls, independent of FFmpeg's parser/decoder policy.
//! H.266 (09/2023) §§7.3.1.2, 7.4.2.2; (04/2022) Annex B.2.
use triergon_core::Error;
use triergon_framing::{VvcAnnexBUnits, VvcNalHeader};

#[test]
fn every_fixed_header_bit_pattern_matches_the_specification_fields() {
    // Independent MSB-first bit walk, rather than the candidate's masks.
    for word in 0..=u16::MAX {
        let bytes = word.to_be_bytes();
        let bits: Vec<u8> = bytes
            .iter()
            .flat_map(|b| (0..8).rev().map(move |n| (b >> n) % 2))
            .collect();
        let field = |start: usize, end: usize| bits[start..end].iter().fold(0, |v, b| 2 * v + b);
        let h = VvcNalHeader::inspect(&bytes).unwrap();
        assert_eq!(h.forbidden_zero_bit, bits[0] == 1);
        assert_eq!(h.reserved_zero_bit, bits[1] == 1);
        assert_eq!(h.layer_id, field(2, 8));
        assert_eq!(h.nal_unit_type, field(8, 13));
        assert_eq!(h.temporal_id_plus_one, field(13, 16));
        assert_eq!(h.requires_discard(), bits[1] == 1 || field(2, 8) >= 56);
    }
    for short in [&[][..], &[0x80][..]] {
        assert!(matches!(
            VvcNalHeader::inspect(short),
            Err(Error::Truncated)
        ));
    }
    // Two bytes suffice; payload contents do not change fixed-header facts.
    assert_eq!(
        VvcNalHeader::inspect(&[0x37, 0xa1]).unwrap(),
        VvcNalHeader::inspect(&[0x37, 0xa1, 0xff, 0, 0, 3, 1]).unwrap()
    );
}

#[test]
fn annex_b_preserves_payload_and_does_not_hide_a_short_header() {
    let data = [
        0, 0, 0, 0, 1, 0, 0x79, 0x80, // SPS (15), layer 0, temporal +1 = 1
        0, 0, 0, 0, 1, 0x37, 0xa1, 0, 0, 3, 1, 0x80, // AUD (20), layer 55
        0, 0, 1, 0x40, // truncated reserved-bit header
        0, 0, 1, 0x3f, 0xff, 0x80, 0, 0, // unspecified type 31, layer 63
    ];
    let units: Vec<_> = VvcAnnexBUnits::new(&data, data.len()).unwrap().collect();
    assert_eq!(units.len(), 4);
    assert_eq!(units[0].bytes, &[0, 0x79, 0x80]);
    assert_eq!(units[0].header.as_ref().unwrap().nal_unit_type, 15);
    assert_eq!(units[1].bytes, &[0x37, 0xa1, 0, 0, 3, 1, 0x80]);
    assert_eq!(units[1].header.as_ref().unwrap().nal_unit_type, 20);
    assert_eq!(units[1].header.as_ref().unwrap().layer_id, 55);
    assert!(matches!(units[2].header, Err(Error::Truncated)));
    assert_eq!(units[2].bytes, &[0x40]);
    assert_eq!(units[3].bytes, &[0x3f, 0xff, 0x80]);
    assert!(units[3].header.as_ref().unwrap().requires_discard());
    assert!(matches!(
        VvcAnnexBUnits::new(&data, data.len() - 1),
        Err(Error::Limit { requested, maximum }) if requested == data.len() && maximum == data.len() - 1
    ));
    assert_eq!(VvcAnnexBUnits::new(&[], 0).unwrap().count(), 0);
    assert_eq!(
        VvcAnnexBUnits::new(&[0, 0, 1, 0, 0, 1], 6).unwrap().count(),
        0
    );
    assert_eq!(VvcAnnexBUnits::new(&[9, 9, 9], 3).unwrap().count(), 0);
    // Every truncation is a complete buffer for this API, never a request to
    // infer whether an external transport has more bytes.
    for end in 0..=data.len() {
        for unit in VvcAnnexBUnits::new(&data[..end], end).unwrap() {
            assert!(!unit.bytes.is_empty());
            assert!(unit.bytes.len() <= end);
        }
    }
}

#[test]
fn original_pes_controls_are_not_conforming_picture_or_au_vectors() {
    // Preserve the existing opaque PES bytes. The first deliberately has a
    // zero temporal_id_plus1: carriage success did not validate VVC syntax.
    for n in 0..3u8 {
        let body = [0, 0, 1, 0, n, 0x0f, 0xff, 0xf8];
        let units: Vec<_> = VvcAnnexBUnits::new(&body, 8).unwrap().collect();
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].bytes, &body[3..]);
        let h = units[0].header.as_ref().unwrap();
        assert_eq!((h.nal_unit_type, h.temporal_id_plus_one), (0, n));
    }
}

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

#[test]
fn delimited_access_units_keep_exact_source_ranges() {
    use triergon_framing::VvcAccessUnits;
    let data = [
        0, 0, 0, 1, 0, 0xa1, 0x08, // AUD
        0, 0, 0, 1, 0, 0x79, 0x80, // SPS
        0, 0, 1, 0, 0x01, 0x80, 0, 0, // slice and trailing zeros
        0, 0, 0, 1, 0, 0xa1, 0x08, // AUD
        0, 0, 1, 0, 0x01, 0x80, 0, 0,
    ];
    let units: Vec<_> = VvcAccessUnits::with_delimiters(&data, data.len())
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(units.len(), 2);
    assert_eq!(units[0].range, 0..22);
    assert_eq!(units[1].range, 22..data.len());
    assert_eq!(units[0].bytes, &data[..22]);
    assert_eq!(units[1].bytes, &data[22..]);
    assert!(matches!(
        VvcAccessUnits::with_delimiters(&data, data.len() - 1),
        Err(Error::Limit { .. })
    ));
}

#[test]
fn delimited_access_units_refuse_unavailable_or_ambiguous_boundaries() {
    use triergon_framing::VvcAccessUnits;
    let aud = [0, 0, 0, 1, 0, 0xa1, 8];
    let slice = [0, 0, 1, 0, 1, 0x80];
    let rejects: Vec<Vec<u8>> = vec![
        slice.to_vec(),
        aud.to_vec(),
        [aud.as_slice(), aud.as_slice()].concat(),
        [aud.as_slice(), slice.as_slice(), slice.as_slice()].concat(),
        [aud.as_slice(), slice.as_slice(), &[0, 0, 1, 0, 0x79, 0x80]].concat(),
        [aud.as_slice(), &[0, 0, 1, 1, 1, 0x80]].concat(),
        [aud.as_slice(), &[0, 0, 1, 0, 0, 0x80]].concat(),
        [aud.as_slice(), &[0, 0, 1, 0x80]].concat(),
        [aud.as_slice(), &[0, 0, 1, 0, 1, 0, 0, 0, 4]].concat(),
        [aud.as_slice(), &[0, 0, 1, 0, 0, 1], slice.as_slice()].concat(),
        [&[0, 0, 0, 1, 0, 0xa1, 0], slice.as_slice()].concat(),
    ];
    for data in rejects {
        assert!(
            VvcAccessUnits::with_delimiters(&data, data.len())
                .unwrap()
                .any(|unit| unit.is_err()),
            "{data:?}"
        );
    }
    assert_eq!(VvcAccessUnits::with_delimiters(&[], 0).unwrap().count(), 0);
}

#[test]
fn delimited_access_units_check_aud_fields_and_vcl_types() {
    use triergon_framing::VvcAccessUnits;
    let mut data = [0, 0, 0, 1, 0, 0xa1, 8, 0, 0, 1, 0, 1, 0x80];
    for pic_type in 0..8 {
        for irap_flag in [0, 0x80] {
            data[6] = irap_flag | (pic_type << 4) | 8;
            for nal_type in 0..=11 {
                data[11] = (nal_type << 3) | 1;
                let mut units = VvcAccessUnits::with_delimiters(&data, data.len()).unwrap();
                let result = units.next().unwrap();
                assert_eq!(
                    result.is_ok(),
                    pic_type <= 2 && matches!(nal_type, 0..=3 | 7..=10),
                    "{pic_type} {irap_flag} {nal_type}"
                );
                assert!(units.next().is_none());
            }
        }
    }
}

#[test]
fn delimited_access_units_require_eob_to_finish_the_stream() {
    use triergon_framing::VvcAccessUnits;
    let audit = [0, 0, 0, 1, 0, 0xa1, 8];
    let slice = [0, 0, 1, 0, 1, 0x80];
    let eob = [0, 0, 1, 0, 0xb1, 0x80, 0, 0];
    let complete = [&audit[..], &slice, &eob].concat();
    let mut units = VvcAccessUnits::with_delimiters(&complete, complete.len()).unwrap();
    assert_eq!(units.next().unwrap().unwrap().bytes, complete);
    assert!(units.next().is_none());
    for suffix in [&slice[..], &[&audit[..], &slice].concat()] {
        let malformed = [complete.as_slice(), suffix].concat();
        let mut units = VvcAccessUnits::with_delimiters(&malformed, malformed.len()).unwrap();
        assert!(matches!(
            units.next(),
            Some(Err(Error::Invalid("NAL after VVC end marker")))
        ));
        assert!(units.next().is_none());
    }
}

#[test]
fn picture_header_framing_preserves_boundaries_and_refuses_ambiguity() {
    use triergon_framing::VvcAccessUnits;
    let sps = [0, 0, 0, 1, 0, 0x79, 0x80];
    let ph = [0, 0, 0, 1, 0, 0x99, 0x80];
    let embedded = [0, 0, 0, 1, 0, 1, 0x80];
    let separate = [0, 0, 0, 1, 0, 1, 0x40];
    let data = [&sps[..], &embedded, &ph, &separate, &embedded].concat();
    let units: Vec<_> = VvcAccessUnits::with_picture_headers(&data, data.len())
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        units.iter().map(|u| u.range.clone()).collect::<Vec<_>>(),
        [0..14, 14..28, 28..35]
    );
    assert_eq!(
        units
            .iter()
            .flat_map(|u| u.bytes.iter().copied())
            .collect::<Vec<_>>(),
        data
    );
    let consecutive = [&ph[..], &separate, &separate].concat();
    let units = VvcAccessUnits::with_picture_headers(&consecutive, consecutive.len())
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(units.len(), 1);
    assert_eq!(units[0].bytes, consecutive);
    for bad in [
        separate.to_vec(),
        [&ph[..], &embedded].concat(),
        [&ph[..], &ph, &separate].concat(),
    ] {
        assert!(
            VvcAccessUnits::with_picture_headers(&bad, bad.len())
                .unwrap()
                .any(|u| u.is_err()),
            "{bad:?}"
        );
    }
}

#[test]
fn picture_header_groups_consecutive_slices_into_one_unit() {
    use triergon_framing::VvcAccessUnits;

    let header = [0, 0, 0, 1, 0, 0x99, 0x80];
    let slice = [0, 0, 1, 0, 0x01, 0x08];
    let later = [0, 0, 1, 0, 0x01, 0x28];
    let suffix = [0, 0, 1, 0, 0xc1, 0x80];
    let good = [&header[..], &slice, &later, &suffix, &header, &slice].concat();
    let units = VvcAccessUnits::with_picture_headers(&good, good.len())
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let boundary = header.len() + slice.len() + later.len() + suffix.len();
    assert_eq!(units.len(), 2);
    assert_eq!(units[0].range, 0..boundary);
    assert_eq!(units[1].range, boundary..good.len());
    assert_eq!(units[0].bytes, &good[..boundary]);
    assert_eq!(units[1].bytes, &good[boundary..]);

    let bad = [&header[..], &slice, &suffix, &later].concat();
    let mut units = VvcAccessUnits::with_picture_headers(&bad, bad.len()).unwrap();
    assert!(matches!(
        units.next(),
        Some(Err(Error::Invalid("ambiguous VVC picture boundary")))
    ));
    assert!(units.next().is_none());
}

#[test]
fn compatibility_keeps_the_short_prefix_at_an_embedded_picture_header() {
    use triergon_framing::{VvcAccessUnits, compat};

    let data = [0, 0, 0, 1, 0, 1, 0x80, 0, 0, 1, 0, 1, 0x80];
    let strict = VvcAccessUnits::with_picture_headers(&data, data.len())
        .unwrap()
        .next();
    assert!(matches!(
        strict,
        Some(Err(Error::Invalid("ambiguous VVC access-unit boundary")))
    ));
    let units = compat::vvc::access_units(&data, data.len())
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(units.len(), 2);
    assert_eq!((units[0].bytes, units[1].bytes), (&data[..7], &data[7..]));

    let standalone = [0, 0, 0, 1, 0, 1, 0x80, 0, 0, 1, 0, 0x99, 0x80];
    assert!(matches!(
        compat::vvc::access_units(&standalone, standalone.len())
            .unwrap()
            .next(),
        Some(Err(Error::Invalid("ambiguous VVC access-unit boundary")))
    ));
}

#[test]
fn parameter_units_require_a_zero_byte_in_both_policies() {
    use triergon_framing::{VvcAccessUnits, compat};

    for kind in 12..=18 {
        let data = [0, 0, 0, 1, 0, 0xa1, 8, 0, 0, 1, 0, (kind << 3) | 1, 0x80];
        for mut units in [
            VvcAccessUnits::with_delimiters(&data, data.len()).unwrap(),
            compat::vvc::access_units(&data, data.len()).unwrap(),
        ] {
            assert!(matches!(
                units.next(),
                Some(Err(Error::Invalid(
                    "VVC NAL without the required zero byte"
                )))
            ));
            assert!(units.next().is_none());
        }
    }
}

#[allow(dead_code)]
#[path = "../../../xtask/src/sha256.rs"]
mod sha256;

#[test]
#[ignore = "requires the thirteen original FATE inputs and FFprobe 9.0.2"]
fn original_fate_vvc_packet_comparison() {
    use std::{path::PathBuf, process::Command};
    use triergon_framing::{VvcAccessUnits, compat};

    let root = PathBuf::from(std::env::var_os("TRIERGON_VVC_FATE").unwrap());
    let ffprobe = std::env::var_os("TRIERGON_VVC_FFPROBE").unwrap();
    let version = Command::new(&ffprobe).arg("-version").output().unwrap();
    assert!(version.status.success());
    assert!(
        String::from_utf8(version.stdout)
            .unwrap()
            .starts_with("ffprobe version 9.0.2 ")
    );
    let mut differences = Vec::new();
    for (name, digest, packets) in [
        (
            "Hierarchical.bit",
            "c5c5016395e99219c153538b03f3cd4ea11d0b4f5d05bc3ab20f46253582e7b9",
            30,
        ),
        (
            "curr-ltrp-alias.vvc",
            "224a58ce7f2b690f4f26bac7af2d137c8047ab04dff640f5f0b48e70af2ee91d",
            30,
        ),
        (
            "frame-packing-nested.vvc",
            "b42ad5e790ea4fa2cefc03dd4ab1191630b7bd0db99e4284ff7fa55ef1867023",
            10,
        ),
        (
            "frame-packing-not-output.vvc",
            "1b2b3aa2f8f1f7b74faef937e2b688bf2ab042ee7ed92d51b7f003e0a5f014e6",
            16,
        ),
        (
            "frame-packing-reorder.vvc",
            "1b4943b4600cb44bb084249b5f331bd785c830e2e3fa57086aebf403065a0050",
            10,
        ),
        (
            "frame-packing-sequence.vvc",
            "7903df2a4f32fecdcd415207c3e7b0fba38e6784f65273a75c394af8c19c0933",
            2,
        ),
        (
            "frame-packing-temporal.vvc",
            "744a9aaf0be6d008199e565f53558532d8931d387d7e2a364f48f0a2ca76f2da",
            10,
        ),
        (
            "frame-packing-upsampled.vvc",
            "329b9ec4bf52c0304bed96a74aaae5521fecbf440c9637040e58e5401c1d53c7",
            10,
        ),
        (
            "mv-clip-wrap.vvc",
            "757766c97e854f5108fa9129b6899c7355a3d2ae2d0a3a3d3b1f59ffbc5a4597",
            2,
        ),
        (
            "rpr-weighted-pred.vvc",
            "58a2cc27d10793a954a1b00715825ac9ce8749d7a8079cd8198b3758a741de32",
            3,
        ),
        (
            "vvc-16bit-mc.vvc",
            "aa874c3d69d77b780bd734cba4d50cb9528beb5531c609e30c962beb515d9a17",
            7,
        ),
        (
            "vvc_frames_with_ltr.vvc",
            "2a31b362d606946f31b7418d4e6372b8728e46ccca34e4af4416ee94bf04799a",
            20,
        ),
        (
            "wpp-single-slice-pic.vvc",
            "3e84f434f351d6b6a38be6d398b15fa5f6ab184efab6e26b29e869e71612c4bb",
            1,
        ),
    ] {
        let source = root.join(name);
        let data = std::fs::read(&source).unwrap();
        assert_eq!(sha256::hex(&data), digest, "{name}");
        let reference = Command::new(&ffprobe)
            .args([
                "-v",
                "error",
                "-show_entries",
                "packet=pos,size,data_hash",
                "-show_data_hash",
                "sha256",
                "-of",
                "compact=p=0",
            ])
            .arg(&source)
            .output()
            .unwrap();
        assert!(reference.status.success(), "{name}");
        let reference = String::from_utf8(reference.stdout).unwrap();
        assert_eq!(reference.lines().count(), packets, "{name}");
        for (label, units) in [
            (
                "strict",
                VvcAccessUnits::with_picture_headers(&data, data.len()),
            ),
            ("compatible", compat::vvc::access_units(&data, data.len())),
        ] {
            let units: Result<Vec<_>, _> = units.unwrap().collect();
            if name == "curr-ltrp-alias.vvc" && label == "strict" {
                assert!(matches!(
                    units,
                    Err(Error::Invalid("ambiguous VVC access-unit boundary"))
                ));
                println!("{name} strict: refuses a boundary without the required zero byte");
                continue;
            }
            match units {
                Ok(units) => {
                    let actual: Vec<_> = units
                        .iter()
                        .map(|unit| {
                            format!(
                                "size={}|pos={}|data_hash=SHA256:{}",
                                unit.bytes.len(),
                                unit.range.start,
                                sha256::hex(unit.bytes)
                            )
                        })
                        .collect();
                    if actual.iter().map(String::as_str).eq(reference.lines()) {
                        println!("{name} {label}: {packets} complete packets match");
                    } else {
                        differences.push(format!(
                            "{name} {label}: packet positions, extents or bytes differ"
                        ));
                    }
                }
                Err(error) => differences.push(format!("{name} {label}: {error}")),
            }
        }
    }
    assert!(differences.is_empty(), "open comparisons: {differences:#?}");
}

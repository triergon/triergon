//! Original spec-derived signalling regressions, authored before typed views.
//! Parent owns execution. These tests do not claim codec/AU conformance.
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, SourceId, WorkBudget};
use triergon_mpegts::{
    AssemblyStep, ParseLimits, SectionAssembler,
    psi::{Descriptor, Descriptors, PmtReader, TableStep},
};

const HEVC: [u8; 13] = [
    0xa2, 0x80, 0, 0, 1, 0xda, 0xbc, 0x12, 0x34, 0x56, 0x78, 120, 0x7e,
];

fn descriptor(tag: u8, data: &[u8]) -> Descriptor<'_> {
    // Typed views operate on the declared body; production Descriptors supplies
    // the exact enclosing raw TLV. This helper does not generate parser output.
    Descriptor {
        tag,
        data,
        source: data,
    }
}

#[test]
fn registration_keeps_four_identifier_bytes_and_opaque_additional_identity() {
    let raw = [5, 7, b'H', b'D', b'M', b'V', 0, 0x47, 0xff];
    let original = Descriptors::new(&raw).next().unwrap().unwrap();
    let value = original.registration().unwrap();
    assert_eq!(value.format_identifier, *b"HDMV");
    assert_eq!(value.additional_identification_info, &[0, 0x47, 0xff]);
    assert_eq!(
        value.additional_identification_info.as_ptr(),
        raw[6..].as_ptr()
    );
    assert_eq!(original.source, &raw);
    assert!(descriptor(5, b"abc").registration().is_err());
    assert!(descriptor(0xee, b"HDMV").registration().is_err());
}

#[test]
fn language_entries_keep_order_non_ascii_bytes_and_complete_audio_types() {
    let raw = [
        b'e', b'n', b'g', 0x80, b'f', b'r', b'a', 2, 0xff, 0, 0x47, 0x85,
    ];
    let mut entries = descriptor(10, &raw).languages().unwrap();
    let first = entries.next().unwrap();
    assert_eq!(first.language_code, *b"eng");
    assert_eq!(first.audio_type, 0x80);
    let second = entries.next().unwrap();
    assert_eq!(second.language_code, *b"fra");
    assert_eq!(second.audio_type, 2);
    let last = entries.next().unwrap();
    assert_eq!(last.language_code, [0xff, 0, 0x47]);
    assert_eq!(last.audio_type, 0x85);
    assert_eq!(entries.next(), None);
    assert!(descriptor(10, &raw[..11]).languages().is_err());
    assert_eq!(descriptor(10, &[]).languages().unwrap().next(), None);
}

#[test]
fn avc_fixed_body_retains_compatible_bits_and_rejects_length_or_reserved_damage() {
    let raw = [100, 0xa5, 42, 0xbf];
    let view = descriptor(0x28, &raw).avc_video().unwrap();
    assert_eq!(view.profile_idc, 100);
    assert_eq!(
        view.constraint_set_flags,
        [true, false, true, false, false, true]
    );
    assert_eq!(view.compatible_flags, 1);
    assert_eq!(view.level_idc, 42);
    assert!(view.still_present);
    assert!(!view.picture_24h);
    assert!(view.frame_packing_sei_not_present);
    assert!(descriptor(0x28, &raw[..3]).avc_video().is_err());
    assert!(
        descriptor(0x28, &[100, 0xa5, 42, 0xbf, 0])
            .avc_video()
            .is_err()
    );
    assert!(
        descriptor(0x28, &[100, 0xa5, 42, 0xbe])
            .avc_video()
            .is_err()
    );
}

#[test]
fn hevc_copied_profile_bits_are_not_misclassified_as_reserved_zero() {
    let view = descriptor(0x38, &HEVC).hevc_video().unwrap();
    assert_eq!(view.profile_space, 2);
    assert!(view.high_tier);
    assert_eq!(view.profile_idc, 2);
    assert_eq!(view.profile_compatibility_indication, 0x8000_0001);
    assert!(view.progressive_source);
    assert!(view.interlaced_source);
    assert!(!view.non_packed_constraint);
    assert!(view.frame_only_constraint);
    assert_eq!(view.copied_44bits, 0x0abc_1234_5678);
    assert_eq!(view.level_idc, 120);
    assert!(view.still_present && view.picture_24h && view.sub_pic_hrd_params_not_present);
    assert_eq!(view.hdr_wcg_idc, 2);
    assert_eq!(view.temporal_ids, None);
}

#[test]
fn hevc_temporal_ids_use_the_high_three_bits_and_do_not_swallow_trailing_bytes() {
    let mut raw = HEVC.to_vec();
    raw[12] |= 0x80;
    raw.extend_from_slice(&[0x3f, 0xbf]);
    assert_eq!(
        descriptor(0x38, &raw).hevc_video().unwrap().temporal_ids,
        Some((1, 5))
    );
    assert!(descriptor(0x38, &raw[..13]).hevc_video().is_err());
    assert!(descriptor(0x38, &raw[..14]).hevc_video().is_err());
    let mut extra = raw.clone();
    extra.push(0);
    assert!(descriptor(0x38, &extra).hevc_video().is_err());
    let mut bad = raw.clone();
    bad[13] &= !1;
    assert!(descriptor(0x38, &bad).hevc_video().is_err());
    bad = raw.clone();
    bad[12] &= !4;
    assert!(descriptor(0x38, &bad).hevc_video().is_err());
    bad[12] = raw[12];
    bad[13] = 0xdf;
    bad[14] = 0x3f;
    assert!(descriptor(0x38, &bad).hevc_video().is_err());
}

#[test]
fn dvb_ac3_optional_fields_and_additional_bytes_need_an_explicit_namespace_call() {
    let raw = [0xf0, 0x81, 8, 3, 0x40, 0xaa, 0];
    let value = descriptor(0x6a, &raw).dvb_ac3().unwrap();
    assert_eq!(value.component_type, Some(0x81));
    assert_eq!(value.bsid, Some(8));
    assert_eq!(value.mainid, Some(3));
    assert_eq!(value.asvc, Some(0x40));
    assert_eq!(value.reserved_flags, 0);
    assert_eq!(value.additional_info, &[0xaa, 0]);
    assert_eq!(value.additional_info.as_ptr(), raw[5..].as_ptr());
    assert!(descriptor(0x6a, &[0xf0, 0x81, 8, 3]).dvb_ac3().is_err());
    assert!(descriptor(0x6a, &[]).dvb_ac3().is_err());
    // An unscoped private tag is retained rather than universally treated as DVB.
    assert!(descriptor(0x6a, &[]).validate_selected_syntax().is_ok());
}

#[test]
fn dvb_eac3_mix_flag_consumes_no_byte_and_three_substreams_keep_their_order() {
    let raw = [0xff, 0x44, 16, 2, 0x80, 0x11, 0x22, 0x33, 0x47];
    let value = descriptor(0x7a, &raw).dvb_eac3().unwrap();
    assert!(value.mix_info_exists);
    assert_eq!(value.component_type, Some(0x44));
    assert_eq!(value.bsid, Some(16));
    assert_eq!(value.mainid, Some(2));
    assert_eq!(value.asvc, Some(0x80));
    assert_eq!(value.substreams, [Some(0x11), Some(0x22), Some(0x33)]);
    assert_eq!(value.additional_info, &[0x47]);
    let mix_only = descriptor(0x7a, &[8]).dvb_eac3().unwrap();
    assert!(mix_only.mix_info_exists);
    assert_eq!(mix_only.substreams, [None; 3]);
    assert!(mix_only.additional_info.is_empty());
    assert!(
        descriptor(0x7a, &[0xff, 0x44, 16, 2, 0x80, 0x11, 0x22])
            .dvb_eac3()
            .is_err()
    );
}

// Mathematical long division, independent of the production CRC updater.
fn crc(data: &[u8]) -> u32 {
    let mut value = 0xffff_ffff_u64;
    for byte in data {
        value = (value << 8) ^ (u64::from(*byte) << 32);
        for shift in (0..8).rev() {
            if value & (1_u64 << (32 + shift)) != 0 {
                value ^= 0x1_04c1_1db7_u64 << shift;
            }
        }
    }
    value as u32
}

fn map_section(
    program: &[u8],
    stream_type: u8,
    elementary: &[u8],
    budget: &Budget,
) -> triergon_mpegts::psi::Section {
    let length = 18 + program.len() + elementary.len();
    let mut source = vec![
        2,
        0xb0 | (length >> 8) as u8,
        length as u8,
        0,
        1,
        0xc1,
        0,
        0,
        0xe1,
        0,
        0xf0,
        program.len() as u8,
    ];
    source.extend_from_slice(program);
    source.extend_from_slice(&[stream_type, 0xe1, 0, 0xf0, elementary.len() as u8]);
    source.extend_from_slice(elementary);
    source.extend_from_slice(&crc(&source).to_be_bytes());
    let mut payload = vec![0];
    payload.extend_from_slice(&source);
    let mut assembler =
        SectionAssembler::new(4096, budget.clone(), ParseLimits::default()).unwrap();
    assembler
        .push(
            Bytes::copy_from_slice(budget, &payload).unwrap(),
            true,
            SourceId(1),
            0,
        )
        .unwrap();
    loop {
        match assembler
            .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            AssemblyStep::Output(value) => return value,
            AssemblyStep::Yield => {}
            other => panic!("section did not complete: {other:?}"),
        }
    }
}

fn table_fault(program: &[u8], stream_type: u8, elementary: &[u8]) -> Error {
    let budget = Budget::new(1 << 20).unwrap();
    let source = map_section(program, stream_type, elementary, &budget);
    let mut reader = PmtReader::new(&source, &budget, ParseLimits::default()).unwrap();
    loop {
        match reader.poll(&mut WorkBudget::new(1), &AtomicBool::new(false)) {
            Err(error) => return error,
            Ok(TableStep::Yield) => {}
            other => panic!("invalid signalling became a table: {other:?}"),
        }
    }
}

#[test]
fn pmt_validates_selected_video_descriptors_before_output_and_checks_hevc_context() {
    assert_eq!(
        table_fault(&[], 0x1b, &[0x28, 3, 100, 0, 42]),
        Error::Invalid("AVC descriptor length")
    );
    let mut raw = vec![0x38, 13];
    raw.extend_from_slice(&HEVC);
    assert_eq!(
        table_fault(&[], 0x1b, &raw),
        Error::Invalid("HEVC descriptor stream type")
    );
    assert_eq!(
        table_fault(&raw, 0x24, &[]),
        Error::Invalid("HEVC descriptor stream type")
    );
}

#[test]
fn pmt_video_descriptor_parsing_yields_cancels_and_keeps_owned_raw_source() {
    let budget = Budget::new(1 << 20).unwrap();
    let mut raw = vec![0x38, 13];
    raw.extend_from_slice(&HEVC);
    let source = map_section(&[], 0x24, &raw, &budget);
    let mut reader = PmtReader::new(&source, &budget, ParseLimits::default()).unwrap();
    let cancel = AtomicBool::new(false);
    assert!(matches!(
        reader.poll(&mut WorkBudget::new(0), &cancel).unwrap(),
        TableStep::Yield
    ));
    cancel.store(true, Ordering::Relaxed);
    assert!(matches!(
        reader.poll(&mut WorkBudget::new(1), &cancel),
        Err(Error::Cancelled)
    ));
    cancel.store(false, Ordering::Relaxed);
    let map = loop {
        match reader.poll(&mut WorkBudget::new(1), &cancel).unwrap() {
            TableStep::Output(value) => break value,
            TableStep::Yield => {}
            TableStep::Done => panic!("cancelled map disappeared"),
        }
    };
    drop(source);
    drop(reader);
    let stream = &map.streams.as_slice()[0];
    assert_eq!(stream.stream_type, 0x24);
    assert_eq!(stream.descriptor_bytes.as_ref(), raw);
    let descriptor = stream.descriptors().next().unwrap().unwrap();
    assert_eq!(
        descriptor.hevc_video().unwrap().copied_44bits,
        0x0abc_1234_5678
    );
    assert_eq!(map.source_ranges.as_slice()[0].entity, SourceId(1));
}

use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, SourceId, WorkBudget};
use triergon_subtitles::text::{Encoding, Event, Extractor, Format, Input, Limits, Status, Timing};
fn input(pool: &Budget, bytes: &[u8]) -> Input {
    Input {
        format: Format::Tx3g,
        encoding: Encoding::Tx3gInBand,
        payload: Bytes::copy_from_slice(pool, bytes).unwrap(),
        codec_id: None,
        codec_private: None,
        charset_label: None,
        addition: None,
        timing: Timing::unknown(),
        source_id: SourceId(91),
        generation: 0,
        sequence: 1,
        configuration_epoch: 0,
        container_position: None,
    }
}
fn extract(bytes: &[u8], q: u64) -> triergon_subtitles::text::Cue {
    let pool = Budget::new(1024 * 1024).unwrap();
    let mut parser = Extractor::tx3g_iso_boxes(&pool, Limits::default(), 0);
    parser.provide(input(&pool, bytes)).unwrap();
    for _ in 0..10000 {
        match parser
            .step(&mut WorkBudget::new(q), &AtomicBool::new(false))
            .unwrap()
        {
            Status::Event(Event::Cue(cue)) => return cue,
            Status::Yield => {}
            x => panic!("unexpected {x:?}"),
        }
    }
    panic!("bounded extraction did not finish")
}
#[test]
fn tx3g_extended_length_box_preserves_complete_physical_ranges() {
    let mut bytes = vec![0, 1, b'X'];
    bytes.extend_from_slice(&1u32.to_be_bytes());
    bytes.extend_from_slice(b"styl");
    bytes.extend_from_slice(&18u64.to_be_bytes());
    bytes.extend_from_slice(&[0, 0]);
    for q in [1, 7, 4096] {
        let cue = extract(&bytes, q);
        assert_eq!(cue.decoded_utf8.unwrap().as_ref(), b"X");
        assert_eq!(cue.modifiers.as_slice()[0].range, 3..21);
        assert_eq!(cue.input.payload.as_ref(), bytes);
    }
}
#[test]
fn tx3g_size_zero_box_ends_at_actual_sample_boundary() {
    let bytes = [0, 1, b'X', 0, 0, 0, 0, b'z', b'z', b'z', b'z', 0x11, 0x22];
    for q in [1, 7, 4096] {
        let cue = extract(&bytes, q);
        assert_eq!(cue.modifiers.as_slice()[0].range, 3..13);
        assert_eq!(cue.input.payload.as_ref(), bytes);
    }
}

use std::sync::atomic::Ordering;
use triergon_core::Error;
use triergon_subtitles::text::charset::{Charset, CharsetJob, CharsetStatus};
use triergon_subtitles::text::tx3g_config::{ConfigJob, ConfigLayout, ConfigLimits, ConfigStatus};

fn converted(raw: &[u8], charset: Charset, q: u64) -> Vec<u8> {
    let pool = Budget::new(1024 * 1024).unwrap();
    let bytes = Bytes::copy_from_slice(&pool, raw).unwrap();
    let mut job = CharsetJob::new(&bytes, None, charset, &pool, Limits::default()).unwrap();
    let c = AtomicBool::new(false);
    for _ in 0..100000 {
        if job.poll(&mut WorkBudget::new(q), &c).unwrap() == CharsetStatus::Ready {
            let out = job.take_output(&c).unwrap();
            assert_eq!(out.source.as_ref(), raw);
            return out.utf8.as_ref().to_vec();
        }
    }
    panic!("bounded charset work");
}
fn config_bytes(layout: ConfigLayout) -> Vec<u8> {
    let mut bytes = Vec::new();
    if layout == ConfigLayout::SampleEntryBody {
        bytes.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 2]);
    }
    bytes.extend_from_slice(&0x0004_0020u32.to_be_bytes());
    bytes.extend_from_slice(&[0xff, 1, 12, 34, 56, 78]);
    for value in [-4i16, 3, 32, 128] {
        bytes.extend_from_slice(&value.to_be_bytes());
    }
    bytes.extend_from_slice(&[0, 0, 0, 0, 0, 7, 3, 24, 1, 2, 3, 4]);
    let body = [
        0, 2, 0, 7, 5, b'S', b'e', b'r', b'i', b'f', 0, 12, 4, b'N', b'o', b't', b'o',
    ];
    bytes.extend_from_slice(&(8u32 + body.len() as u32).to_be_bytes());
    bytes.extend_from_slice(b"ftab");
    bytes.extend_from_slice(&body);
    bytes
}
fn parsed_config(
    raw: &[u8],
    layout: ConfigLayout,
    q: u64,
) -> triergon_subtitles::text::tx3g_config::Configuration {
    let pool = Budget::new(1024 * 1024).unwrap();
    let bytes = Bytes::copy_from_slice(&pool, raw).unwrap();
    let mut job = ConfigJob::new(&bytes, layout, &pool, ConfigLimits::default()).unwrap();
    for _ in 0..100000 {
        if job
            .poll(&mut WorkBudget::new(q), &AtomicBool::new(false))
            .unwrap()
            == ConfigStatus::Ready
        {
            return job.take_output(&AtomicBool::new(false)).unwrap();
        }
    }
    panic!("bounded config work");
}
#[test]
fn charset_unicode_modes_preserve_explicit_bom_rules_and_nuls() {
    assert_eq!(
        converted(b"\xef\xbb\xbfX\0", Charset::Utf8, 1),
        b"\xef\xbb\xbfX\0"
    );
    assert_eq!(
        converted(
            &[0xff, 0xfe, 0x3d, 0xd8, 0, 0xde, 0, 0],
            Charset::Utf16Bom,
            1
        ),
        "😀\0".as_bytes()
    );
    assert_eq!(
        converted(&[0xfe, 0xff, 0, 0x58], Charset::Utf16Be, 7),
        "\u{feff}X".as_bytes()
    );
    assert_eq!(converted(&[0, 0x41, 0, 0], Charset::Utf16Be, 4096), b"A\0");
}
#[test]
fn explicit_charset_label_retained_without_truth_or_detection_assertion() {
    let pool = Budget::new(4096).unwrap();
    let raw = Bytes::copy_from_slice(&pool, &[0xcf, 0xf0, 0xe8, 0xe2, 0xe5, 0xf2]).unwrap();
    let label = Bytes::copy_from_slice(&pool, b"X-UNKNOWN").unwrap();
    let c = AtomicBool::new(false);
    let mut job = CharsetJob::new(
        &raw,
        Some(&label),
        Charset::Windows1251,
        &pool,
        Limits::default(),
    )
    .unwrap();
    while job.poll(&mut WorkBudget::new(1), &c).unwrap() != CharsetStatus::Ready {}
    let out = job.take_output(&c).unwrap();
    assert_eq!(out.utf8.as_ref(), "Привет".as_bytes());
    assert_eq!(out.declared_label.unwrap().as_ref(), b"X-UNKNOWN");
    assert!(out.source.shares_storage_with(&raw));
}
#[test]
fn charset_unknown_labels_and_undefined_bytes_remain_explicit_errors() {
    assert!(matches!(
        Charset::from_label(b"shift_jis"),
        Err(Error::Unsupported(_))
    ));
    assert_eq!(
        Charset::from_label(b"WINDOWS-1251").unwrap(),
        Charset::Windows1251
    );
    for (charset, raw) in [
        (Charset::Windows1251, &[0x98][..]),
        (Charset::Windows1252, &[0x81][..]),
        (Charset::Ascii, &[0x80][..]),
        (Charset::Utf8, &[0xc0, 0x80][..]),
        (Charset::Utf16Bom, &[0, 65][..]),
        (Charset::Utf16Le, &[0][..]),
    ] {
        let pool = Budget::new(4096).unwrap();
        let bytes = Bytes::copy_from_slice(&pool, raw).unwrap();
        let mut job = CharsetJob::new(&bytes, None, charset, &pool, Limits::default()).unwrap();
        assert!(
            job.poll(&mut WorkBudget::new(4096), &AtomicBool::new(false))
                .is_err()
        );
        assert!(job.take_output(&AtomicBool::new(false)).is_err());
    }
}
#[test]
fn charset_zero_work_and_cancellation_are_resumable() {
    let pool = Budget::new(4096).unwrap();
    let raw = Bytes::copy_from_slice(&pool, &[0xcf, 0xf0, 0xe8, 0xe2, 0xe5, 0xf2]).unwrap();
    let mut job =
        CharsetJob::new(&raw, None, Charset::Windows1251, &pool, Limits::default()).unwrap();
    let c = AtomicBool::new(false);
    let before = pool.used();
    assert_eq!(
        job.poll(&mut WorkBudget::new(0), &c).unwrap(),
        CharsetStatus::Pending
    );
    assert_eq!(pool.used(), before);
    job.poll(&mut WorkBudget::new(3), &c).unwrap();
    c.store(true, Ordering::Relaxed);
    assert_eq!(job.poll(&mut WorkBudget::new(0), &c), Err(Error::Cancelled));
    c.store(false, Ordering::Relaxed);
    while job.poll(&mut WorkBudget::new(1), &c).unwrap() != CharsetStatus::Ready {}
    assert_eq!(
        job.take_output(&c).unwrap().utf8.as_ref(),
        "Привет".as_bytes()
    );
}
#[test]
fn charset_output_retention_and_foreign_owners_are_charged() {
    let pool = Budget::new(4096).unwrap();
    let other = Budget::new(4096).unwrap();
    let raw = Bytes::copy_from_slice(&pool, &[0xe9]).unwrap();
    let label = Bytes::copy_from_slice(&other, b"windows-1252").unwrap();
    let before = pool.used();
    assert!(matches!(
        CharsetJob::new(
            &raw,
            Some(&label),
            Charset::Windows1252,
            &pool,
            Limits::default()
        ),
        Err(Error::ForeignPool)
    ));
    let c = AtomicBool::new(false);
    let mut job =
        CharsetJob::new(&raw, None, Charset::Windows1252, &pool, Limits::default()).unwrap();
    while job.poll(&mut WorkBudget::new(7), &c).unwrap() != CharsetStatus::Ready {}
    let out = job.take_output(&c).unwrap();
    drop(job);
    assert!(pool.used() > before);
    assert_eq!(out.utf8.as_ref(), "é".as_bytes());
    drop(out);
    assert_eq!(pool.used(), before);
}
#[test]
fn charset_decoded_and_source_limits_precede_publication() {
    let pool = Budget::new(4096).unwrap();
    let raw = Bytes::copy_from_slice(&pool, &[0x80]).unwrap();
    let mut job = CharsetJob::new(
        &raw,
        None,
        Charset::Windows1252,
        &pool,
        Limits {
            max_decoded_bytes: 2,
            ..Limits::default()
        },
    )
    .unwrap();
    assert!(matches!(
        job.poll(&mut WorkBudget::new(4096), &AtomicBool::new(false)),
        Err(Error::Limit { .. })
    ));
    assert!(matches!(
        CharsetJob::new(
            &raw,
            None,
            Charset::Windows1252,
            &pool,
            Limits {
                max_input_bytes: 0,
                ..Limits::default()
            }
        ),
        Err(Error::Limit { .. })
    ));
}
#[test]
fn charset_reserve_and_freeze_pressure_preserve_scalar_state() {
    let pool = Budget::new(8192).unwrap();
    let raw = Bytes::copy_from_slice(&pool, &[0x80; 16]).unwrap();
    let hold = pool.reserve(pool.available() - 8).unwrap();
    let c = AtomicBool::new(false);
    let mut job =
        CharsetJob::new(&raw, None, Charset::Windows1252, &pool, Limits::default()).unwrap();
    assert!(matches!(
        job.poll(&mut WorkBudget::new(4096), &c),
        Err(Error::Backpressure { .. })
    ));
    drop(hold);
    while job.poll(&mut WorkBudget::new(1), &c).unwrap() != CharsetStatus::Ready {}
    assert_eq!(
        job.take_output(&c).unwrap().utf8.as_ref(),
        "€".repeat(16).as_bytes()
    );
}
#[test]
fn charset_utf8_output_aliases_validated_original_storage() {
    let pool = Budget::new(4096).unwrap();
    let raw = Bytes::copy_from_slice(&pool, b"ASCII and UTF8 \xc3\x85").unwrap();
    let before = pool.used();
    let c = AtomicBool::new(false);
    let mut job = CharsetJob::new(&raw, None, Charset::Utf8, &pool, Limits::default()).unwrap();
    while job.poll(&mut WorkBudget::new(1), &c).unwrap() != CharsetStatus::Ready {}
    let out = job.take_output(&c).unwrap();
    assert!(out.utf8.shares_storage_with(&raw));
    assert_eq!(pool.used(), before);
}
#[test]
fn tx3g_configuration_layouts_expose_all_fixed_fields_and_font_ranges() {
    for layout in [ConfigLayout::SampleEntryBody, ConfigLayout::DefaultsRecord] {
        let bytes = config_bytes(layout);
        for q in [1, 7, 4096] {
            let out = parsed_config(&bytes, layout, q);
            assert_eq!(out.raw.as_ref(), bytes);
            assert_eq!(
                out.defaults.data_reference_index,
                if layout == ConfigLayout::SampleEntryBody {
                    Some(2)
                } else {
                    None
                }
            );
            assert_eq!(out.defaults.display_flags, 0x40020);
            assert_eq!(out.defaults.horizontal, -1);
            assert_eq!(out.defaults.vertical, 1);
            assert_eq!(out.defaults.background, [12, 34, 56, 78]);
            assert_eq!(out.defaults.text_box, [-4, 3, 32, 128]);
            assert_eq!(out.defaults.style.font_id, 7);
            assert_eq!(out.defaults.style.face_flags, 3);
            assert_eq!(out.defaults.style.font_size, 24);
            assert_eq!(out.defaults.style.rgba, [1, 2, 3, 4]);
            assert_eq!(out.fonts.len(), 2);
            assert_eq!(out.fonts.as_slice()[1].id, 12);
            assert_eq!(&bytes[out.fonts.as_slice()[1].name_range.clone()], b"Noto");
            assert_eq!(out.boxes.as_slice()[0].kind, *b"ftab");
        }
    }
}
#[test]
fn tx3g_configuration_extended_and_zero_size_font_tables() {
    let base = config_bytes(ConfigLayout::DefaultsRecord);
    let mut extended = base[..30].to_vec();
    extended.extend_from_slice(&1u32.to_be_bytes());
    extended.extend_from_slice(b"ftab");
    extended.extend_from_slice(&33u64.to_be_bytes());
    extended.extend_from_slice(&base[38..]);
    assert_eq!(
        parsed_config(&extended, ConfigLayout::DefaultsRecord, 1)
            .fonts
            .len(),
        2
    );
    let mut zero = base;
    zero[30..34].copy_from_slice(&[0; 4]);
    assert_eq!(
        parsed_config(&zero, ConfigLayout::DefaultsRecord, 7)
            .fonts
            .len(),
        2
    );
}
#[test]
fn tx3g_config_all_truncated_advertised_prefixes_fail() {
    let bytes = config_bytes(ConfigLayout::SampleEntryBody);
    for cut in 0..bytes.len() {
        let pool = Budget::new(4096).unwrap();
        let raw = Bytes::copy_from_slice(&pool, &bytes[..cut]).unwrap();
        let mut job = ConfigJob::new(
            &raw,
            ConfigLayout::SampleEntryBody,
            &pool,
            ConfigLimits::default(),
        )
        .unwrap();
        assert!(
            job.poll(&mut WorkBudget::new(4096), &AtomicBool::new(false))
                .is_err(),
            "cut {cut}"
        );
    }
}
#[test]
fn tx3g_config_invalid_utf8_defaults_reserved_and_counts_refused() {
    let original = config_bytes(ConfigLayout::SampleEntryBody);
    for at in [0, 8 + 18, 8 + 4, 38 + 8 + 2 + 3] {
        let mut bytes = original.clone();
        bytes[at] = if at == 8 + 4 { 5 } else { 0xff };
        let pool = Budget::new(4096).unwrap();
        let raw = Bytes::copy_from_slice(&pool, &bytes).unwrap();
        let mut job = ConfigJob::new(
            &raw,
            ConfigLayout::SampleEntryBody,
            &pool,
            ConfigLimits::default(),
        )
        .unwrap();
        assert!(
            job.poll(&mut WorkBudget::new(4096), &AtomicBool::new(false))
                .is_err(),
            "at {at}"
        );
    }
}
#[test]
fn tx3g_config_zero_work_cancel_and_output_lifetime() {
    let pool = Budget::new(8192).unwrap();
    let raw = Bytes::copy_from_slice(&pool, &config_bytes(ConfigLayout::SampleEntryBody)).unwrap();
    let before = pool.used();
    let c = AtomicBool::new(false);
    let mut job = ConfigJob::new(
        &raw,
        ConfigLayout::SampleEntryBody,
        &pool,
        ConfigLimits::default(),
    )
    .unwrap();
    assert_eq!(
        job.poll(&mut WorkBudget::new(0), &c).unwrap(),
        ConfigStatus::Pending
    );
    assert_eq!(pool.used(), before);
    job.poll(&mut WorkBudget::new(11), &c).unwrap();
    c.store(true, Ordering::Relaxed);
    assert_eq!(job.poll(&mut WorkBudget::new(0), &c), Err(Error::Cancelled));
    c.store(false, Ordering::Relaxed);
    while job.poll(&mut WorkBudget::new(1), &c).unwrap() != ConfigStatus::Ready {}
    let out = job.take_output(&c).unwrap();
    drop(job);
    assert!(pool.used() > before);
    assert!(out.raw.shares_storage_with(&raw));
    drop(out);
    assert_eq!(pool.used(), before);
}
#[test]
fn tx3g_config_foreign_and_finite_limits_preserve_input() {
    let pool = Budget::new(8192).unwrap();
    let other = Budget::new(8192).unwrap();
    let raw = Bytes::copy_from_slice(&pool, &config_bytes(ConfigLayout::SampleEntryBody)).unwrap();
    assert!(matches!(
        ConfigJob::new(
            &raw,
            ConfigLayout::SampleEntryBody,
            &other,
            ConfigLimits::default()
        ),
        Err(Error::ForeignPool)
    ));
    for limits in [
        ConfigLimits {
            maximum_fonts: 1,
            ..ConfigLimits::default()
        },
        ConfigLimits {
            maximum_boxes: 0,
            ..ConfigLimits::default()
        },
    ] {
        let mut job = ConfigJob::new(&raw, ConfigLayout::SampleEntryBody, &pool, limits).unwrap();
        assert!(matches!(
            job.poll(&mut WorkBudget::new(4096), &AtomicBool::new(false)),
            Err(Error::Limit { .. })
        ));
    }
}
#[test]
fn tx3g_original_profile_keeps_original_size_zero_refusal() {
    let pool = Budget::new(4096).unwrap();
    let mut parser = Extractor::new(&pool, Limits::default(), 0);
    parser
        .provide(input(
            &pool,
            &[0, 1, b'X', 0, 0, 0, 0, b'z', b'z', b'z', b'z'],
        ))
        .unwrap();
    assert_eq!(
        parser
            .step(&mut WorkBudget::new(4096), &AtomicBool::new(false))
            .unwrap_err(),
        Error::Unsupported("tx3g box-to-end size")
    );
}
#[test]
fn tx3g_extended_header_negative_geometry_and_prefixes() {
    for size in [0u64, 8, 15, 18, u64::MAX] {
        let mut bytes = vec![0, 1, b'X'];
        bytes.extend_from_slice(&1u32.to_be_bytes());
        bytes.extend_from_slice(b"free");
        bytes.extend_from_slice(&size.to_be_bytes());
        let pool = Budget::new(4096).unwrap();
        let mut parser = Extractor::tx3g_iso_boxes(&pool, Limits::default(), 0);
        parser.provide(input(&pool, &bytes)).unwrap();
        assert!(
            parser
                .step(&mut WorkBudget::new(4096), &AtomicBool::new(false))
                .is_err()
        );
    }
}

const EXPECTED_CP1251: [i32; 256] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25,
    26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49,
    50, 51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 63, 64, 65, 66, 67, 68, 69, 70, 71, 72, 73,
    74, 75, 76, 77, 78, 79, 80, 81, 82, 83, 84, 85, 86, 87, 88, 89, 90, 91, 92, 93, 94, 95, 96, 97,
    98, 99, 100, 101, 102, 103, 104, 105, 106, 107, 108, 109, 110, 111, 112, 113, 114, 115, 116,
    117, 118, 119, 120, 121, 122, 123, 124, 125, 126, 127, 1026, 1027, 8218, 1107, 8222, 8230,
    8224, 8225, 8364, 8240, 1033, 8249, 1034, 1036, 1035, 1039, 1106, 8216, 8217, 8220, 8221, 8226,
    8211, 8212, -1, 8482, 1113, 8250, 1114, 1116, 1115, 1119, 160, 1038, 1118, 1032, 164, 1168,
    166, 167, 1025, 169, 1028, 171, 172, 173, 174, 1031, 176, 177, 1030, 1110, 1169, 181, 182, 183,
    1105, 8470, 1108, 187, 1112, 1029, 1109, 1111, 1040, 1041, 1042, 1043, 1044, 1045, 1046, 1047,
    1048, 1049, 1050, 1051, 1052, 1053, 1054, 1055, 1056, 1057, 1058, 1059, 1060, 1061, 1062, 1063,
    1064, 1065, 1066, 1067, 1068, 1069, 1070, 1071, 1072, 1073, 1074, 1075, 1076, 1077, 1078, 1079,
    1080, 1081, 1082, 1083, 1084, 1085, 1086, 1087, 1088, 1089, 1090, 1091, 1092, 1093, 1094, 1095,
    1096, 1097, 1098, 1099, 1100, 1101, 1102, 1103,
];
const EXPECTED_CP1252: [i32; 256] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25,
    26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49,
    50, 51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 63, 64, 65, 66, 67, 68, 69, 70, 71, 72, 73,
    74, 75, 76, 77, 78, 79, 80, 81, 82, 83, 84, 85, 86, 87, 88, 89, 90, 91, 92, 93, 94, 95, 96, 97,
    98, 99, 100, 101, 102, 103, 104, 105, 106, 107, 108, 109, 110, 111, 112, 113, 114, 115, 116,
    117, 118, 119, 120, 121, 122, 123, 124, 125, 126, 127, 8364, -1, 8218, 402, 8222, 8230, 8224,
    8225, 710, 8240, 352, 8249, 338, -1, 381, -1, -1, 8216, 8217, 8220, 8221, 8226, 8211, 8212,
    732, 8482, 353, 8250, 339, -1, 382, 376, 160, 161, 162, 163, 164, 165, 166, 167, 168, 169, 170,
    171, 172, 173, 174, 175, 176, 177, 178, 179, 180, 181, 182, 183, 184, 185, 186, 187, 188, 189,
    190, 191, 192, 193, 194, 195, 196, 197, 198, 199, 200, 201, 202, 203, 204, 205, 206, 207, 208,
    209, 210, 211, 212, 213, 214, 215, 216, 217, 218, 219, 220, 221, 222, 223, 224, 225, 226, 227,
    228, 229, 230, 231, 232, 233, 234, 235, 236, 237, 238, 239, 240, 241, 242, 243, 244, 245, 246,
    247, 248, 249, 250, 251, 252, 253, 254, 255,
];
#[test]
fn all_single_byte_mappings_match_independent_python_codecs() {
    for (charset, values) in [
        (Charset::Windows1251, &EXPECTED_CP1251),
        (Charset::Windows1252, &EXPECTED_CP1252),
    ] {
        for (byte, &scalar) in values.iter().enumerate() {
            for q in [1, 7, 4096] {
                if scalar >= 0 {
                    let expected = char::from_u32(scalar as u32).unwrap().to_string();
                    assert_eq!(converted(&[byte as u8], charset, q), expected.as_bytes());
                } else {
                    let pool = Budget::new(4096).unwrap();
                    let raw = Bytes::copy_from_slice(&pool, &[byte as u8]).unwrap();
                    let mut job =
                        CharsetJob::new(&raw, None, charset, &pool, Limits::default()).unwrap();
                    let c = AtomicBool::new(false);
                    loop {
                        match job.poll(&mut WorkBudget::new(q), &c) {
                            Ok(CharsetStatus::Pending) => {}
                            Err(Error::Invalid(_)) => break,
                            _ => panic!("undefined code point admitted"),
                        }
                    }
                    assert!(job.take_output(&c).is_err());
                }
            }
        }
    }
}

#[test]
fn explicit_cp1251_matches_complete_native_ass_bytes_while_native_stop_stays_preserved() {
    let raw = b"2,0,Default,,0,0,0,,\xcf\xf0\xe8\xe2\xe5\xf2";
    let expected = b"2,0,Default,,0,0,0,,\xd0\x9f\xd1\x80\xd0\xb8\xd0\xb2\xd0\xb5\xd1\x82";
    for q in [1, 7, 4096] {
        assert_eq!(converted(raw, Charset::Windows1251, q), expected);
    }
    // Original observer ends with -22 after preserving this subtitle. That
    // process outcome remains in the frozen177 audit, not converted to success.
}
#[test]
fn tx3g_config_backpressure_retry_keeps_exact_font_record_and_raw_owner() {
    let pool = Budget::new(8192).unwrap();
    let raw = Bytes::copy_from_slice(&pool, &config_bytes(ConfigLayout::SampleEntryBody)).unwrap();
    let held = pool.reserve(pool.available() - 8).unwrap();
    let c = AtomicBool::new(false);
    let mut job = ConfigJob::new(
        &raw,
        ConfigLayout::SampleEntryBody,
        &pool,
        ConfigLimits::default(),
    )
    .unwrap();
    assert!(matches!(
        job.poll(&mut WorkBudget::new(4096), &c),
        Err(Error::Backpressure { .. })
    ));
    let before = pool.used();
    assert_eq!(
        job.poll(&mut WorkBudget::new(0), &c).unwrap(),
        ConfigStatus::Pending
    );
    assert_eq!(pool.used(), before);
    drop(held);
    while job.poll(&mut WorkBudget::new(1), &c).unwrap() != ConfigStatus::Ready {}
    let out = job.take_output(&c).unwrap();
    assert_eq!(out.fonts.len(), 2);
    assert_eq!(out.boxes.len(), 1);
    assert!(out.raw.shares_storage_with(&raw));
}

fn legacy_srt(raw_text: &[u8], encoding: Encoding, q: u64) -> triergon_subtitles::text::Cue {
    let pool = Budget::new(16384).unwrap();
    let prefix = b"1\r\n00:00:01,000 --> 00:00:02,500\r\n";
    let mut raw = prefix.to_vec();
    raw.extend_from_slice(raw_text);
    raw.extend_from_slice(b"\r\n");
    let mut source = input(&pool, &raw);
    source.format = Format::SubRip;
    source.encoding = encoding;
    source.configuration_epoch = 601;
    source.charset_label = Some(Bytes::copy_from_slice(&pool, b"original-declared-label").unwrap());
    let mut extractor = Extractor::new(&pool, Limits::default(), 0);
    extractor.provide(source).unwrap();
    for _ in 0..10000 {
        match extractor
            .step(&mut WorkBudget::new(q), &AtomicBool::new(false))
            .unwrap()
        {
            Status::Yield => {}
            Status::Event(Event::Cue(cue)) => {
                assert_eq!(cue.input.payload.as_ref(), raw);
                assert_eq!(cue.text_range, prefix.len()..prefix.len() + raw_text.len());
                assert_eq!(cue.input.configuration_epoch, 601);
                assert_eq!(
                    cue.input.charset_label.as_ref().unwrap().as_ref(),
                    b"original-declared-label"
                );
                return cue;
            }
            x => panic!("unexpected legacy source status {x:?}"),
        }
    }
    panic!("bounded legacy SRT extraction");
}
#[test]
fn declared_windows1251_srt_extracts_utf8_and_preserves_physical_source_ranges() {
    for q in [1, 7, 4096] {
        let cue = legacy_srt(
            &[0xcf, 0xf0, 0xe8, 0xe2, 0xe5, 0xf2],
            Encoding::Windows1251,
            q,
        );
        assert_eq!(
            cue.decoded_utf8.as_ref().unwrap().as_ref(),
            "Привет".as_bytes()
        );
        assert_eq!(cue.effective_encoding, Encoding::Windows1251);
        assert_eq!(cue.start.unwrap(), triergon_core::Time::new(1, 1).unwrap());
        assert_eq!(
            cue.duration.unwrap(),
            triergon_core::Time::new(3, 2).unwrap()
        );
    }
}
#[test]
fn declared_windows1252_and_latin1_srt_do_not_normalize_codepoints() {
    for q in [1, 7, 4096] {
        assert_eq!(
            legacy_srt(&[0x80, b' ', 0xe9], Encoding::Windows1252, q)
                .decoded_utf8
                .unwrap()
                .as_ref(),
            "€ é".as_bytes()
        );
        assert_eq!(
            legacy_srt(&[0xa0, b' ', 0xe9], Encoding::Latin1, q)
                .decoded_utf8
                .unwrap()
                .as_ref(),
            "\u{a0} é".as_bytes()
        );
    }
}
#[test]
fn undefined_legacy_srt_byte_never_publishes_a_valid_looking_prefix() {
    let pool = Budget::new(8192).unwrap();
    let mut source = input(
        &pool,
        b"1\n00:00:00,000 --> 00:00:01,000\nvalid\n\n2\n00:00:02,000 --> 00:00:03,000\n\x98\n",
    );
    source.format = Format::SubRip;
    source.encoding = Encoding::Windows1251;
    let mut parser = Extractor::new(&pool, Limits::default(), 0);
    parser.provide(source).unwrap();
    for _ in 0..10000 {
        match parser.step(&mut WorkBudget::new(1), &AtomicBool::new(false)) {
            Ok(Status::Yield) => {}
            Err(Error::Invalid(_)) => return,
            x => panic!("invalid charset emitted source event {x:?}"),
        }
    }
    panic!("undefined charset byte was not checked");
}

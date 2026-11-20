use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, WorkBudget};
use triergon_subtitles::text::{
    Limits,
    charset::{
        Charset, CharsetJob, CharsetStatus,
        multibyte::{DecodeStep, MultibyteCharset as M, MultibyteDecoder},
    },
};

fn decode(raw: &[u8], profile: M, quantum: u64) -> Result<Vec<u8>, Error> {
    let pool = Budget::new(1024 * 1024).unwrap();
    let source = Bytes::copy_from_slice(&pool, raw).unwrap();
    let mut job = CharsetJob::new(
        &source,
        None,
        Charset::Multibyte(profile),
        &pool,
        Limits::default(),
    )?;
    let cancel = AtomicBool::new(false);
    for _ in 0..1000000 {
        if job.poll(&mut WorkBudget::new(quantum), &cancel)? == CharsetStatus::Ready {
            let out = job.take_output(&cancel)?;
            assert!(out.source.shares_storage_with(&source));
            assert_eq!(out.source.as_ref(), raw);
            return Ok(out.utf8.as_ref().to_vec());
        }
    }
    panic!("bounded conversion did not terminate")
}
#[test]
fn multibyte_missing_api_and_four_selected_scripts() {
    for (profile, raw, text) in [
        (M::GbkCp936, &[0xd6, 0xd0, 0xce, 0xc4][..], "中文"),
        (M::Big5Unicode, &[0xa4, 0xa4, 0xa4, 0xe5][..], "中文"),
        (M::ShiftJisUnicode, &[0x93, 0xfa, 0x96, 0x7b][..], "日本"),
        (M::EucKr, &[0xc7, 0xd1, 0xb1, 0xdb][..], "한글"),
    ] {
        for q in [1, 7, 4096] {
            assert_eq!(decode(raw, profile, q).unwrap(), text.as_bytes());
        }
    }
}
#[test]
fn windows_variants_are_observably_distinct() {
    assert_eq!(
        decode(&[0x81, 0x60], M::ShiftJisUnicode, 1).unwrap(),
        "〜".as_bytes()
    );
    assert_eq!(
        decode(&[0x81, 0x60], M::Windows932, 1).unwrap(),
        "～".as_bytes()
    );
    assert_eq!(
        decode(&[0xa1, 0x45], M::Big5Unicode, 1).unwrap(),
        "•".as_bytes()
    );
    assert_eq!(
        decode(&[0xa1, 0x45], M::Windows950, 1).unwrap(),
        "‧".as_bytes()
    );
    assert!(decode(&[0x81, 0x41], M::EucKr, 1).is_err());
    assert_eq!(
        decode(&[0x81, 0x41], M::Windows949, 1).unwrap(),
        "갂".as_bytes()
    );
}
#[test]
fn old_resolver_refusals_are_preserved_and_new_aliases_are_explicit() {
    for name in [
        b"shift_jis".as_slice(),
        b"big5",
        b"gbk",
        b"euc-kr",
        b"cp932",
        b"cp950",
        b"cp949",
    ] {
        assert!(matches!(
            Charset::from_label(name),
            Err(Error::Unsupported(_))
        ));
        assert!(M::from_label(name).is_ok());
    }
    for name in [
        b"GB18030".as_slice(),
        b"big5-hkscs",
        b"ms_kanji",
        b"ks_c_5601-1987",
        b" gbk",
        b"gbk\0",
    ] {
        assert!(matches!(M::from_label(name), Err(Error::Unsupported(_))));
    }
    assert_eq!(M::from_label(b"WINDOWS-932").unwrap(), M::Windows932);
    assert!(matches!(
        M::from_label(&[b'a'; 257]),
        Err(Error::Limit { .. })
    ));
}

const PROFILES: [(M, &str, usize); 7] = [
    (
        M::GbkCp936,
        include_str!("multibyte-mapping-data/CP936.TXT"),
        21920,
    ),
    (
        M::Big5Unicode,
        include_str!("multibyte-mapping-data/BIG5.TXT"),
        13831,
    ),
    (
        M::ShiftJisUnicode,
        include_str!("multibyte-mapping-data/SHIFTJIS.TXT"),
        7070,
    ),
    (
        M::EucKr,
        include_str!("multibyte-mapping-data/CP949.TXT"),
        8354,
    ),
    (
        M::Windows932,
        include_str!("multibyte-mapping-data/CP932.TXT"),
        7915,
    ),
    (
        M::Windows949,
        include_str!("multibyte-mapping-data/CP949.TXT"),
        17176,
    ),
    (
        M::Windows950,
        include_str!("multibyte-mapping-data/CP950.TXT"),
        13631,
    ),
];

// Independent test oracle reads original licensed text records, not packed DATA.
fn mapping(profile: M, original: &str) -> std::collections::BTreeMap<u16, char> {
    let mut map = std::collections::BTreeMap::new();
    for line in original.lines() {
        let mut words = line.split('#').next().unwrap().split_whitespace();
        let Some(code) = words.next() else { continue };
        let Some(value) = words.next() else { continue };
        let Some(value) = value.strip_prefix("0x") else {
            continue;
        };
        let code = u16::from_str_radix(code.strip_prefix("0x").unwrap(), 16).unwrap();
        let value = char::from_u32(u32::from_str_radix(value, 16).unwrap()).unwrap();
        if profile == M::Big5Unicode && value == '\u{fffd}' {
            continue;
        }
        if profile == M::EucKr && !(0xa1..=0xfe).contains(&(code >> 8)) {
            continue;
        }
        if profile == M::EucKr && !(0xa1..=0xfe).contains(&(code & 255)) {
            continue;
        }
        assert!(map.insert(code, value).is_none());
    }
    if matches!(profile, M::Big5Unicode | M::EucKr) {
        for byte in 0..128 {
            assert!(
                map.insert(byte, char::from_u32(u32::from(byte)).unwrap())
                    .is_none()
            );
        }
    }
    if profile == M::ShiftJisUnicode {
        for byte in (0..32).chain(std::iter::once(127)) {
            assert!(
                map.insert(byte, char::from_u32(u32::from(byte)).unwrap())
                    .is_none()
            );
        }
    }
    map
}

fn whole_mapping(index: usize) {
    let (profile, original, count) = PROFILES[index];
    let table = mapping(profile, original);
    assert_eq!(table.len(), count);
    let mut bytes = Vec::new();
    let mut expected = String::new();
    for (&code, &value) in &table {
        if code <= 255 {
            bytes.push(code as u8);
        } else {
            bytes.extend_from_slice(&code.to_be_bytes());
        }
        expected.push(value);
    }
    for q in [1, 7, 4096] {
        assert_eq!(decode(&bytes, profile, q).unwrap(), expected.as_bytes());
    }
}

fn low_level(raw: &[u8], profile: M) -> Result<String, Error> {
    let mut decoder = MultibyteDecoder::new(profile);
    let cancel = AtomicBool::new(false);
    let mut output = String::new();
    for (offset, &byte) in raw.iter().enumerate() {
        match decoder.push(byte, &mut WorkBudget::new(1), &cancel)? {
            DecodeStep::Scalar(scalar) => {
                assert_eq!(scalar.range.end, offset + 1);
                assert!(scalar.range.len() == 1 || scalar.range.len() == 2);
                output.push(scalar.value);
            }
            DecodeStep::NeedInput => {}
            DecodeStep::Yield => panic!("one byte must fit one work unit"),
        }
    }
    decoder.finish(&cancel)?;
    Ok(output)
}

fn all_short_inputs(index: usize) {
    let (profile, original, _) = PROFILES[index];
    let table = mapping(profile, original);
    for first in 0u16..256 {
        match table.get(&first) {
            Some(value) => assert_eq!(
                low_level(&[first as u8], profile).unwrap(),
                value.to_string()
            ),
            None => assert!(low_level(&[first as u8], profile).is_err()),
        }
        for second in 0u16..256 {
            let raw = [first as u8, second as u8];
            let expected = if let Some(a) = table.get(&first) {
                table.get(&second).map(|b| format!("{a}{b}"))
            } else {
                table.get(&u16::from_be_bytes(raw)).map(char::to_string)
            };
            let actual = low_level(&raw, profile);
            if let Some(expected) = expected {
                assert_eq!(actual.unwrap(), expected, "{profile:?} {raw:02x?}");
            } else {
                assert!(
                    actual.is_err(),
                    "unexpected acceptance {profile:?} {raw:02x?}"
                );
            }
        }
    }
}

macro_rules! table_tests {
    ($whole:ident,$short:ident,$index:expr) => {
        #[test]
        fn $whole() {
            whole_mapping($index);
        }
        #[test]
        fn $short() {
            all_short_inputs($index);
        }
    };
}
table_tests!(whole_gbk_mapping, every_gbk_one_and_two_byte_input, 0);
table_tests!(whole_big5_mapping, every_big5_one_and_two_byte_input, 1);
table_tests!(
    whole_shift_jis_mapping,
    every_shift_jis_one_and_two_byte_input,
    2
);
table_tests!(whole_euc_kr_mapping, every_euc_kr_one_and_two_byte_input, 3);
table_tests!(
    whole_windows932_mapping,
    every_windows932_one_and_two_byte_input,
    4
);
table_tests!(
    whole_windows949_mapping,
    every_windows949_one_and_two_byte_input,
    5
);
table_tests!(
    whole_windows950_mapping,
    every_windows950_one_and_two_byte_input,
    6
);

#[test]
fn every_physical_partition_retains_lead_and_exact_ranges() {
    let cancel = AtomicBool::new(false);
    for (profile, raw) in [
        (M::GbkCp936, &[0, 0xd6, 0xd0, 0x41, 0xce, 0xc4][..]),
        (M::Big5Unicode, &[0, 0xa4, 0xa4, 0x41, 0xa4, 0xe5][..]),
        (M::ShiftJisUnicode, &[0, 0x93, 0xfa, 0x41, 0x96, 0x7b][..]),
        (M::EucKr, &[0, 0xc7, 0xd1, 0x41, 0xb1, 0xdb][..]),
    ] {
        for mask in 0..(1usize << (raw.len() - 1)) {
            let mut decoder = MultibyteDecoder::at_offset(profile, 19);
            let mut actual = String::new();
            let mut ranges = Vec::new();
            let mut start = 0;
            for end in 1..=raw.len() {
                if end == raw.len() || mask & (1 << (end - 1)) != 0 {
                    // Each physical chunk is supplied separately; no joined buffer.
                    for &byte in &raw[start..end] {
                        if let DecodeStep::Scalar(s) = decoder
                            .push(byte, &mut WorkBudget::new(1), &cancel)
                            .unwrap()
                        {
                            actual.push(s.value);
                            ranges.push(s.range);
                        }
                    }
                    start = end;
                }
            }
            decoder.finish(&cancel).unwrap();
            assert_eq!(actual.as_bytes(), decode(raw, profile, 4096).unwrap());
            assert_eq!(ranges, [19..20, 20..22, 22..23, 23..25]);
        }
    }
}

#[test]
fn lead_starvation_is_not_eof_and_finish_is_sticky_truncation() {
    let cancel = AtomicBool::new(false);
    for profile in [
        M::GbkCp936,
        M::Big5Unicode,
        M::ShiftJisUnicode,
        M::EucKr,
        M::Windows932,
        M::Windows949,
        M::Windows950,
    ] {
        let lead = if matches!(profile, M::ShiftJisUnicode | M::Windows932) {
            0x81
        } else {
            0xa1
        };
        let mut decoder = MultibyteDecoder::new(profile);
        assert_eq!(
            decoder
                .push(lead, &mut WorkBudget::new(1), &cancel)
                .unwrap(),
            DecodeStep::NeedInput
        );
        for _ in 0..3 {
            assert_eq!(
                decoder
                    .push(0xa1, &mut WorkBudget::new(0), &cancel)
                    .unwrap(),
                DecodeStep::Yield
            );
            assert_eq!(decoder.position(), 1);
        }
        assert_eq!(decoder.finish(&cancel), Err(Error::Truncated));
        assert_eq!(
            decoder.push(0xa1, &mut WorkBudget::new(1), &cancel),
            Err(Error::Truncated)
        );
        assert_eq!(decoder.finish(&cancel), Err(Error::Truncated));
    }
}

#[test]
fn cancellation_between_lead_and_trail_preserves_pending_source() {
    let cancel = AtomicBool::new(false);
    let mut decoder = MultibyteDecoder::new(M::GbkCp936);
    assert_eq!(
        decoder
            .push(0xd6, &mut WorkBudget::new(1), &cancel)
            .unwrap(),
        DecodeStep::NeedInput
    );
    cancel.store(true, Ordering::Relaxed);
    assert_eq!(
        decoder.push(0xd0, &mut WorkBudget::new(1), &cancel),
        Err(Error::Cancelled)
    );
    assert_eq!(decoder.finish(&cancel), Err(Error::Cancelled));
    assert_eq!(decoder.position(), 1);
    cancel.store(false, Ordering::Relaxed);
    let DecodeStep::Scalar(s) = decoder
        .push(0xd0, &mut WorkBudget::new(1), &cancel)
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(s.value, '中');
    assert_eq!(s.range, 0..2);
    decoder.finish(&cancel).unwrap();
}

#[test]
fn invalid_trail_never_replays_as_ascii_or_publishes_replacement() {
    let cancel = AtomicBool::new(false);
    let mut decoder = MultibyteDecoder::new(M::GbkCp936);
    decoder
        .push(0xd6, &mut WorkBudget::new(1), &cancel)
        .unwrap();
    let error = decoder
        .push(b' ', &mut WorkBudget::new(1), &cancel)
        .unwrap_err();
    assert!(matches!(error, Error::Invalid(_)));
    assert_eq!(decoder.position(), 1);
    assert_eq!(
        decoder.push(b'A', &mut WorkBudget::new(1), &cancel),
        Err(error)
    );
    assert_eq!(decoder.finish(&cancel), Err(error));
}

#[test]
fn big5_seven_unmapped_records_are_all_required_refusals() {
    for code in [0xa15au16, 0xa1c3, 0xa1c5, 0xa1fe, 0xa240, 0xa2cc, 0xa2ce] {
        assert!(matches!(
            decode(&code.to_be_bytes(), M::Big5Unicode, 1),
            Err(Error::Invalid(_))
        ));
        assert!(decode(&code.to_be_bytes(), M::Windows950, 1).is_ok());
    }
}

#[test]
fn no_private_use_or_gb18030_four_byte_inference() {
    assert!(decode(&[0x81, 0x30, 0x81, 0x30], M::GbkCp936, 1).is_err());
    assert!(decode(&[0xf0, 0x40], M::Windows932, 1).is_err());
    assert!(decode(&[0xfa, 0x40], M::Windows950, 1).is_err());
    assert!(decode(&[0x87, 0x40], M::Big5Unicode, 1).is_err());
}

#[test]
fn byte_origin_overflow_is_checked_before_source_progress() {
    let cancel = AtomicBool::new(false);
    let mut decoder = MultibyteDecoder::at_offset(M::GbkCp936, usize::MAX);
    assert_eq!(
        decoder.push(b'A', &mut WorkBudget::new(1), &cancel),
        Err(Error::Overflow)
    );
    assert_eq!(decoder.position(), usize::MAX);
    assert_eq!(decoder.finish(&cancel), Err(Error::Overflow));
}

#[test]
fn finished_decoder_rejects_more_bytes_and_empty_input_is_real_empty_text() {
    let cancel = AtomicBool::new(false);
    let mut decoder = MultibyteDecoder::new(M::Big5Unicode);
    decoder.finish(&cancel).unwrap();
    decoder.finish(&cancel).unwrap();
    assert!(matches!(
        decoder.push(0, &mut WorkBudget::new(1), &cancel),
        Err(Error::Invalid(_))
    ));
    for (profile, _, _) in PROFILES {
        assert!(decode(&[], profile, 1).unwrap().is_empty());
    }
}

#[test]
fn complete_validation_refuses_malformed_tail_before_any_output() {
    let pool = Budget::new(8192).unwrap();
    let raw = Bytes::copy_from_slice(&pool, &[b'A', 0xd6, 0xd0, 0xff]).unwrap();
    let mut job = CharsetJob::new(
        &raw,
        None,
        Charset::Multibyte(M::GbkCp936),
        &pool,
        Limits::default(),
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    let baseline = pool.used();
    let error = loop {
        match job.poll(&mut WorkBudget::new(1), &cancel) {
            Ok(CharsetStatus::Pending) => assert!(job.take_output(&cancel).is_err()),
            Ok(CharsetStatus::Ready) => panic!("malformed tail published"),
            Err(error) => break error,
        }
    };
    assert!(matches!(error, Error::Invalid(_)));
    assert_eq!(pool.used(), baseline);
    assert_eq!(job.poll(&mut WorkBudget::new(4096), &cancel), Err(error));
    assert!(job.take_output(&cancel).is_err());
}

#[test]
fn declared_label_pool_and_backing_source_owner_survive_conversion() {
    let pool = Budget::new(8192).unwrap();
    let backing = Bytes::copy_from_slice(&pool, &[0x55, 0xd6, 0xd0, 0x66]).unwrap();
    let raw = backing.slice(1..3).unwrap();
    let label = Bytes::copy_from_slice(&pool, b"contradictory/windows-1251").unwrap();
    let cancel = AtomicBool::new(false);
    let mut job = CharsetJob::new(
        &raw,
        Some(&label),
        Charset::Multibyte(M::GbkCp936),
        &pool,
        Limits::default(),
    )
    .unwrap();
    while job.poll(&mut WorkBudget::new(1), &cancel).unwrap() != CharsetStatus::Ready {}
    let out = job.take_output(&cancel).unwrap();
    assert!(out.source.shares_storage_with(&backing));
    assert!(
        out.declared_label
            .as_ref()
            .unwrap()
            .shares_storage_with(&label)
    );
    assert_eq!(out.source.as_ref(), &[0xd6, 0xd0]);
    assert_eq!(out.utf8.as_ref(), "中".as_bytes());
    drop(job);
    drop(backing);
    drop(raw);
    drop(label);
    assert!(pool.used() > 0);
    drop(out);
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}

#[test]
fn foreign_source_and_label_rejected_without_consuming_caller_owners() {
    let pool = Budget::new(4096).unwrap();
    let other = Budget::new(4096).unwrap();
    let source = Bytes::copy_from_slice(&pool, b"A").unwrap();
    let foreign = Bytes::copy_from_slice(&other, b"B").unwrap();
    assert!(matches!(
        CharsetJob::new(
            &foreign,
            None,
            Charset::Multibyte(M::GbkCp936),
            &pool,
            Limits::default()
        ),
        Err(Error::ForeignPool)
    ));
    assert!(matches!(
        CharsetJob::new(
            &source,
            Some(&foreign),
            Charset::Multibyte(M::GbkCp936),
            &pool,
            Limits::default()
        ),
        Err(Error::ForeignPool)
    ));
    assert_eq!(foreign.as_ref(), b"B");
    assert_eq!(source.as_ref(), b"A");
}

#[test]
fn decoded_byte_cap_checks_real_expansion_without_truncation() {
    let pool = Budget::new(4096).unwrap();
    let raw = Bytes::copy_from_slice(&pool, &[0xd6, 0xd0]).unwrap();
    let limits = Limits {
        max_decoded_bytes: 2,
        ..Limits::default()
    };
    let mut job =
        CharsetJob::new(&raw, None, Charset::Multibyte(M::GbkCp936), &pool, limits).unwrap();
    assert_eq!(
        job.poll(&mut WorkBudget::new(4096), &AtomicBool::new(false)),
        Err(Error::Limit {
            requested: 3,
            maximum: 2
        })
    );
    assert!(job.take_output(&AtomicBool::new(false)).is_err());
}

#[test]
fn zero_work_job_has_no_progress_or_allocation() {
    let pool = Budget::new(4096).unwrap();
    let raw = Bytes::copy_from_slice(&pool, &[0xd6, 0xd0]).unwrap();
    let mut job = CharsetJob::new(
        &raw,
        None,
        Charset::Multibyte(M::GbkCp936),
        &pool,
        Limits::default(),
    )
    .unwrap();
    let used = pool.used();
    for _ in 0..8 {
        assert_eq!(
            job.poll(&mut WorkBudget::new(0), &AtomicBool::new(false))
                .unwrap(),
            CharsetStatus::Pending
        );
        assert_eq!(pool.used(), used);
    }
    assert_eq!(
        job.poll(&mut WorkBudget::new(4096), &AtomicBool::new(false))
            .unwrap(),
        CharsetStatus::Ready
    );
}

#[test]
fn job_cancellation_at_each_single_unit_boundary_is_resumable() {
    let raw = [b'A', 0xd6, 0xd0, 0xce, 0xc4, 0];
    for pause in 0..40 {
        let pool = Budget::new(8192).unwrap();
        let source = Bytes::copy_from_slice(&pool, &raw).unwrap();
        let mut job = CharsetJob::new(
            &source,
            None,
            Charset::Multibyte(M::GbkCp936),
            &pool,
            Limits::default(),
        )
        .unwrap();
        let cancel = AtomicBool::new(false);
        for _ in 0..pause {
            if job.poll(&mut WorkBudget::new(1), &cancel).unwrap() == CharsetStatus::Ready {
                break;
            }
        }
        let used = pool.used();
        cancel.store(true, Ordering::Relaxed);
        assert_eq!(
            job.poll(&mut WorkBudget::new(1), &cancel),
            Err(Error::Cancelled)
        );
        assert_eq!(pool.used(), used);
        cancel.store(false, Ordering::Relaxed);
        while job.poll(&mut WorkBudget::new(1), &cancel).unwrap() != CharsetStatus::Ready {}
        assert_eq!(
            job.take_output(&cancel).unwrap().utf8.as_ref(),
            "A中文\0".as_bytes()
        );
    }
}

#[test]
fn retained_output_pressure_retries_without_replaying_or_losing_source() {
    let pool = Budget::new(4096).unwrap();
    let raw = Bytes::copy_from_slice(&pool, &[0xd6, 0xd0]).unwrap();
    let pressure = pool.reserve(pool.limit() - pool.used()).unwrap();
    let cancel = AtomicBool::new(false);
    let mut job = CharsetJob::new(
        &raw,
        None,
        Charset::Multibyte(M::GbkCp936),
        &pool,
        Limits::default(),
    )
    .unwrap();
    assert!(matches!(
        job.poll(&mut WorkBudget::new(4096), &cancel),
        Err(Error::Backpressure { .. })
    ));
    let used = pool.used();
    for _ in 0..3 {
        let mut work = WorkBudget::new(0);
        assert_eq!(
            job.poll(&mut work, &cancel).unwrap(),
            CharsetStatus::Pending
        );
        assert_eq!(pool.used(), used);
    }
    drop(pressure);
    while job.poll(&mut WorkBudget::new(1), &cancel).unwrap() != CharsetStatus::Ready {}
    assert_eq!(
        job.take_output(&cancel).unwrap().utf8.as_ref(),
        "中".as_bytes()
    );
}

use triergon_core::SourceId;
use triergon_subtitles::text::{Encoding, Extractor, Format, Input, Timing};

#[test]
fn utf8_packets_do_not_adopt_multibyte_selection() {
    for format in [Format::Utf8Packet, Format::MatroskaWebVtt] {
        let pool = Budget::new(4096).unwrap();
        let input = Input {
            format,
            encoding: Encoding::Multibyte(M::GbkCp936),
            payload: Bytes::copy_from_slice(&pool, b"A").unwrap(),
            codec_id: None,
            codec_private: None,
            charset_label: None,
            addition: None,
            timing: Timing::unknown(),
            source_id: SourceId(91),
            generation: 0,
            sequence: 0,
            configuration_epoch: 0,
            container_position: None,
        };
        let mut extractor = Extractor::new(&pool, Limits::default(), 0);
        extractor.provide(input).unwrap();
        assert!(matches!(
            extractor.step(&mut WorkBudget::new(4096), &AtomicBool::new(false)),
            Err(Error::Invalid(_))
        ));
    }
}

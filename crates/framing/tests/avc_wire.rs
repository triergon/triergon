//! Independent primary-field-authored records; no encoder or native-derived expectations.
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, SourceId, WorkBudget};
use triergon_framing::{
    AvcConfigArrayKind, AvcConfigLimits, AvcConfigParser, AvcConfigSource, AvcConfiguration,
    BuildStep,
};
use triergon_harness_protocol::unhex;

fn limits() -> AvcConfigLimits {
    AvcConfigLimits {
        maximum_record_bytes: 100_000,
        maximum_nals: 600,
        maximum_nal_bytes: 65535,
    }
}
fn source() -> AvcConfigSource {
    AvcConfigSource {
        source: SourceId(291),
        generation: 17,
        offset: 91,
    }
}
fn raw() -> Vec<u8> {
    unhex("0142a528ffe1000267800100026840")
}
fn kind(kind: AvcConfigArrayKind) -> usize {
    match kind {
        AvcConfigArrayKind::Sequence => 0,
        AvcConfigArrayKind::Picture => 1,
        AvcConfigArrayKind::SequenceExtension => 2,
    }
}
fn drive(parser: &mut AvcConfigParser, work: u64) -> Result<AvcConfiguration, Error> {
    for _ in 0..20_000 {
        match parser.step(&mut WorkBudget::new(work), &AtomicBool::new(false))? {
            BuildStep::Ready(v) => return Ok(v),
            BuildStep::Yield => {}
            BuildStep::Complete => panic!("missing output"),
        }
    }
    panic!("bounded AVC walk did not progress")
}
fn parser(pool: &Budget, bytes: &[u8]) -> Result<AvcConfigParser, Error> {
    AvcConfigParser::new(
        pool,
        source(),
        Bytes::copy_from_slice(pool, bytes)?,
        limits(),
    )
}
fn parse(pool: &Budget, bytes: &[u8]) -> Result<AvcConfiguration, Error> {
    drive(&mut parser(pool, bytes)?, 1)
}

#[test]
fn wire_grammar_is_implemented_instead_of_declared_layout_only() {
    // Input comes from one fixture source record; every expected fact
    // below is literal ISO length/count/header arithmetic, not another parser.
    let line = include_str!("../fixtures/avc-wire-v1.tsv")
        .lines()
        .find(|line| line.starts_with("plain-control\t"))
        .expect("frozen plain-control record");
    let input = unhex(line.split('\t').nth(1).expect("frozen record bytes"));
    let expected = [
        0x01, 0x42, 0xa5, 0x28, 0xff, 0xe1, 0x00, 0x02, 0x67, 0x80, 0x01, 0x00, 0x02, 0x68, 0x40,
    ];
    assert_eq!(input, expected);
    let pool = Budget::new(65536).unwrap();
    let value =
        parse(&pool, &input).expect("wire avcC parser must parse the frozen primary-backed record");
    assert_eq!(value.source_bytes().as_ref(), expected);
    assert_eq!(value.source(), source());
    assert_eq!(value.fields().raw_prefix(), [1, 66, 165, 40, 255, 225]);
    assert_eq!(value.fields().version(), 1);
    assert_eq!(value.fields().profile_indication(), 66);
    assert_eq!(value.fields().profile_compatibility(), 165);
    assert_eq!(value.fields().level_indication(), 40);
    assert_eq!(value.sample_length_size().bytes(), 4);
    assert_eq!(value.fields().raw_extension(), None);
    assert_eq!(value.fields().syntax_issues(), 0);
    assert_eq!(value.arrays().len(), 2);
    assert_eq!(value.array(0).unwrap().kind(), AvcConfigArrayKind::Sequence);
    assert_eq!(value.array(0).unwrap().source_range(), 5..10);
    assert_eq!(value.array(0).unwrap().count_offset(), 5);
    assert_eq!(value.array(0).unwrap().nal_indices(), 0..1);
    assert_eq!(value.array(1).unwrap().kind(), AvcConfigArrayKind::Picture);
    assert_eq!(value.array(1).unwrap().source_range(), 10..15);
    assert_eq!(value.array(1).unwrap().count_offset(), 10);
    assert_eq!(value.array(1).unwrap().nal_indices(), 1..2);
    assert_eq!(value.nals().len(), 2);
    assert_eq!(value.nal(0).unwrap().source_range(), 8..10);
    assert_eq!(value.nal(0).unwrap().length_field_bytes(), [0, 2]);
    assert_eq!(value.nal(0).unwrap().source_bytes(), [0x67, 0x80]);
    assert_eq!(value.nal(0).unwrap().header().nal_unit_type(), 7);
    assert_eq!(value.nal(1).unwrap().source_range(), 13..15);
    assert_eq!(value.nal(1).unwrap().length_field_bytes(), [0, 2]);
    assert_eq!(value.nal(1).unwrap().source_bytes(), [0x68, 0x40]);
    assert_eq!(value.nal(1).unwrap().header().nal_unit_type(), 8);
    assert_eq!(value.understood_end(), 15);
    assert!(value.trailing_bytes().is_empty());
}

#[test]
fn all82_independent_records_and_spans_agree_at_each_work_quantum() {
    let mut count = 0;
    for line in include_str!("../fixtures/avc-wire-v1.tsv")
        .lines()
        .filter(|v| !v.starts_with('#') && !v.is_empty())
    {
        let cells: Vec<_> = line.split('\t').collect();
        assert_eq!(cells.len(), 11);
        let bytes = unhex(cells[1]);
        for work in [1, 2, 7, 64] {
            let pool = Budget::new(2_000_000).unwrap();
            let result = parser(&pool, &bytes).and_then(|mut p| drive(&mut p, work));
            match cells[2] {
                "ok" => {
                    let v = result.unwrap_or_else(|e| panic!("{} work{}: {:?}", cells[0], work, e));
                    assert_eq!(v.source_bytes().as_ref(), bytes, "{}", cells[0]);
                    assert_eq!(v.source(), source());
                    assert_eq!(v.fields().profile_indication().to_string(), cells[3]);
                    assert_eq!(v.fields().profile_compatibility(), 0xa5);
                    assert_eq!(v.fields().level_indication(), 40);
                    assert_eq!(v.sample_length_size().bytes().to_string(), cells[4]);
                    assert_eq!(v.fields().syntax_issues().to_string(), cells[5]);
                    assert_eq!(
                        v.fields()
                            .raw_extension()
                            .map(|x| x.to_vec())
                            .unwrap_or_default(),
                        unhex(cells[6])
                    );
                    assert_eq!(v.understood_end().to_string(), cells[7]);
                    assert_eq!(v.trailing_bytes(), unhex(cells[8]));
                    let arrays: Vec<_> = cells[9].split('|').collect();
                    assert_eq!(arrays.len(), v.arrays().len());
                    for (expected, a) in arrays.iter().zip(v.arrays()) {
                        let n: Vec<usize> =
                            expected.split(',').map(|x| x.parse().unwrap()).collect();
                        assert_eq!(
                            &[
                                kind(a.kind()),
                                a.count_offset(),
                                a.source_range().start,
                                a.source_range().end,
                                a.nal_indices().start,
                                a.nal_indices().end
                            ][..],
                            n.as_slice()
                        );
                    }
                    let nals: Vec<_> = if cells[10] == "-" {
                        Vec::new()
                    } else {
                        cells[10].split('|').collect()
                    };
                    assert_eq!(nals.len(), v.nals().len());
                    for (expected, n) in nals.iter().zip(v.nals()) {
                        let c: Vec<_> = expected.split(',').collect();
                        let val = |i: usize| c[i].parse::<usize>().unwrap();
                        assert_eq!(kind(n.array_kind()), val(0));
                        assert_eq!(n.source_range(), val(2)..val(3));
                        assert_eq!(n.source_bytes(), unhex(c[4]));
                        assert_eq!(n.length_field_bytes(), &bytes[val(1)..val(2)]);
                        assert_eq!(
                            usize::from(u16::from_be_bytes(
                                n.length_field_bytes().try_into().unwrap()
                            )),
                            n.source_bytes().len()
                        );
                        assert_eq!(
                            n.source_bytes().as_ptr_range(),
                            v.source_bytes()[n.source_range()].as_ptr_range()
                        );
                    }
                }
                "truncated" => assert!(matches!(result, Err(Error::Truncated)), "{}", cells[0]),
                "invalid" => assert!(matches!(result, Err(Error::Invalid(_))), "{}", cells[0]),
                "unsupported" => {
                    assert!(matches!(result, Err(Error::Unsupported(_))), "{}", cells[0]);
                }
                _ => panic!("unknown oracle status"),
            }
        }
        count += 1;
    }
    assert_eq!(count, 82);
}

#[test]
fn empty_arrays_and_high_extension_presence_are_physical_facts() {
    let pool = Budget::new(65536).unwrap();
    let base = parse(&pool, &unhex("0142a528ffe000")).unwrap();
    assert_eq!(base.arrays().len(), 2);
    assert_eq!(base.nal_count(), 0);
    assert_eq!(base.fields().raw_extension(), None);
    let high = parse(&pool, &unhex("0164a528ffe000fdf8f800")).unwrap();
    assert_eq!(high.arrays().len(), 3);
    assert_eq!(high.fields().raw_extension(), Some([0xfd, 0xf8, 0xf8, 0]));
    assert_eq!(high.array(2).unwrap().source_range(), 7..11);
}

#[test]
fn reserved_and_out_of_range_depth_facts_are_retained_without_repair() {
    let pool = Budget::new(65536).unwrap();
    let bytes = unhex("0164a52803000003070500");
    let v = parse(&pool, &bytes).unwrap();
    assert_eq!(v.fields().syntax_issues(), 127);
    assert_eq!(v.fields().raw_prefix(), [1, 100, 0xa5, 40, 3, 0]);
    assert_eq!(v.fields().chroma_format(), Some(3));
    assert_eq!(v.fields().bit_depth_luma_minus8(), Some(7));
    assert_eq!(v.fields().bit_depth_chroma_minus8(), Some(5));
    assert_eq!(v.source_bytes().as_ref(), bytes);
}

#[test]
fn selected2010_profile_branch_does_not_guess_newer_profile_extensions() {
    let pool = Budget::new(65536).unwrap();
    let v = parse(&pool, &unhex("01f4a528ffe000fdf8f800")).unwrap();
    assert_eq!(v.fields().profile_indication(), 244);
    assert_eq!(v.fields().raw_extension(), None);
    assert_eq!(v.trailing_bytes(), [0xfd, 0xf8, 0xf8, 0]);
    assert!(matches!(
        parse(&pool, &unhex("0164a528ffe000")),
        Err(Error::Truncated)
    ));
}

#[test]
fn source_epoch_and_checked_absolute_extent_are_preserved() {
    let pool = Budget::new(65536).unwrap();
    let bytes = raw();
    let mut p = AvcConfigParser::new(
        &pool,
        source(),
        Bytes::copy_from_slice(&pool, &bytes).unwrap(),
        limits(),
    )
    .unwrap();
    assert_eq!(drive(&mut p, 1).unwrap().source(), source());
    let overflow = AvcConfigSource {
        offset: u64::MAX,
        ..source()
    };
    assert!(matches!(
        AvcConfigParser::new(
            &pool,
            overflow,
            Bytes::copy_from_slice(&pool, &bytes).unwrap(),
            limits()
        ),
        Err(Error::Overflow)
    ));
}

#[test]
fn foreign_pool_record_is_rejected_at_entry() {
    let a = Budget::new(65536).unwrap();
    let b = Budget::new(65536).unwrap();
    assert!(matches!(
        AvcConfigParser::new(
            &a,
            source(),
            Bytes::copy_from_slice(&b, &raw()).unwrap(),
            limits()
        ),
        Err(Error::ForeignPool)
    ));
}

#[test]
fn zero_work_makes_no_descriptor_allocation_and_retains_source() {
    let pool = Budget::new(65536).unwrap();
    let mut p = parser(&pool, &raw()).unwrap();
    let used = pool.used();
    for _ in 0..3 {
        assert!(matches!(
            p.step(&mut WorkBudget::new(0), &AtomicBool::new(false))
                .unwrap(),
            BuildStep::Yield
        ));
        assert_eq!(pool.used(), used);
    }
    assert_eq!(drive(&mut p, 1).unwrap().nals().len(), 2);
}

#[test]
fn cancellation_releases_private_owners_and_stays_sticky() {
    let pool = Budget::new(65536).unwrap();
    let baseline = pool.used();
    let mut p = parser(&pool, &raw()).unwrap();
    let cancel = AtomicBool::new(false);
    assert!(matches!(
        p.step(&mut WorkBudget::new(2), &cancel).unwrap(),
        BuildStep::Yield
    ));
    cancel.store(true, Ordering::Relaxed);
    assert!(matches!(
        p.step(&mut WorkBudget::new(1), &cancel),
        Err(Error::Cancelled)
    ));
    assert_eq!(pool.used(), baseline);
    cancel.store(false, Ordering::Relaxed);
    assert!(matches!(
        p.step(&mut WorkBudget::new(100), &cancel),
        Err(Error::Cancelled)
    ));
}

#[test]
fn exact_descriptor_reservation_retries_without_skipping_a_nal() {
    let pool = Budget::new(65536).unwrap();
    let mut p = parser(&pool, &raw()).unwrap();
    let block = pool.reserve(pool.available()).unwrap();
    let err = p.step(&mut WorkBudget::new(100), &AtomicBool::new(false));
    assert!(matches!(err, Err(Error::Backpressure { .. })));
    drop(block);
    let v = drive(&mut p, 1).unwrap();
    assert_eq!(v.nals().len(), 2);
    assert_eq!(v.nal(0).unwrap().source_bytes(), [0x67, 0x80]);
    assert_eq!(v.nal(1).unwrap().source_bytes(), [0x68, 0x40]);
}

#[test]
fn configuration_holds_one_record_owner_and_releases_on_another_thread() {
    let pool = Budget::new(65536).unwrap();
    let baseline = pool.used();
    let v = parse(&pool, &raw()).unwrap();
    let owner = v.source_bytes().retained_storage_bytes();
    let table = v.descriptor_storage_bytes().unwrap();
    assert_eq!(pool.used(), baseline + owner + table);
    assert_eq!(v.nal(1).unwrap().source_bytes(), [0x68, 0x40]);
    std::thread::spawn(move || drop(v)).join().unwrap();
    assert_eq!(pool.used(), baseline);
}

#[test]
fn caller_size_count_and_nal_limits_remain_hard() {
    let pool = Budget::new(65536).unwrap();
    let bytes = raw();
    let small = AvcConfigLimits {
        maximum_record_bytes: bytes.len() - 1,
        ..limits()
    };
    assert!(matches!(
        AvcConfigParser::new(
            &pool,
            source(),
            Bytes::copy_from_slice(&pool, &bytes).unwrap(),
            small
        ),
        Err(Error::Limit { .. })
    ));
    for small in [
        AvcConfigLimits {
            maximum_nals: 1,
            ..limits()
        },
        AvcConfigLimits {
            maximum_nal_bytes: 1,
            ..limits()
        },
    ] {
        let mut p = AvcConfigParser::new(
            &pool,
            source(),
            Bytes::copy_from_slice(&pool, &bytes).unwrap(),
            small,
        )
        .unwrap();
        assert!(matches!(drive(&mut p, 1), Err(Error::Limit { .. })));
    }
}

#[test]
fn publication_occurs_once_and_source_failure_is_sticky() {
    let pool = Budget::new(65536).unwrap();
    let mut p = parser(&pool, &raw()).unwrap();
    let v = drive(&mut p, 1).unwrap();
    assert_eq!(v.nals().len(), 2);
    assert!(matches!(
        p.step(&mut WorkBudget::new(0), &AtomicBool::new(false))
            .unwrap(),
        BuildStep::Complete
    ));
    let mut bytes = raw();
    bytes[4] = 0xfe;
    let mut p = parser(&pool, &bytes).unwrap();
    assert!(matches!(drive(&mut p, 1), Err(Error::Invalid(_))));
    assert!(matches!(
        p.step(&mut WorkBudget::new(100), &AtomicBool::new(false)),
        Err(Error::Invalid(_))
    ));
    for unknown in [0, 2, 255] {
        assert!(matches!(
            parse(&pool, &[unknown]),
            Err(Error::Unsupported(_))
        ));
    }
}

#[test]
fn original_length_fields_and_opaque_nal_body_bytes_are_not_rewritten() {
    let pool = Budget::new(65536).unwrap();
    let bytes = unhex("0142a528ffe20005670000030100021f800100036800ffbb00");
    let v = parse(&pool, &bytes).unwrap();
    assert_eq!(v.nals().len(), 3);
    assert_eq!(v.nal(0).unwrap().source_bytes(), [0x67, 0, 0, 3, 1]);
    assert_eq!(v.nal(1).unwrap().header().nal_unit_type(), 31);
    assert_eq!(v.trailing_bytes(), [0xbb, 0]);
    assert_eq!(v.source_bytes().as_ref(), bytes);
}

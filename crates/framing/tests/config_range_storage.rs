#![forbid(unsafe_code)]
use std::mem::size_of;
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, Error, SourceId, WorkBudget};
use triergon_framing::*;

fn avcc() -> Vec<u8> {
    let mut raw = vec![1, 66, 0, 10, 255, 225, 255, 255];
    raw.push(0x67);
    raw.resize(8 + 65_535, 0);
    raw.extend_from_slice(&[1, 0, 3, 0x68, 1, 99]);
    raw
}
fn hvcc() -> Vec<u8> {
    let mut raw = vec![
        1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 120, 240, 0, 252, 253, 248, 248, 0, 0, 15, 2,
    ];
    raw.extend_from_slice(&[160, 0, 1, 255, 255, 64, 1]);
    raw.resize(28 + 65_535, 0);
    raw.extend_from_slice(&[161, 0, 1, 0, 3, 66, 1, 99]);
    raw
}
fn avc(pool: &Budget, raw: Bytes, q: u64) -> AvcConfiguration {
    let mut parser = AvcConfigParser::new(
        pool,
        AvcConfigSource {
            source: SourceId(7),
            generation: u64::MAX,
            offset: u64::MAX - raw.len() as u64,
        },
        raw,
        AvcConfigLimits {
            maximum_record_bytes: 262_144,
            maximum_nals: 1024,
            maximum_nal_bytes: 65_535,
        },
    )
    .unwrap();
    for _ in 0..200_000 {
        match parser
            .step(&mut WorkBudget::new(q), &AtomicBool::new(false))
            .unwrap()
        {
            BuildStep::Ready(value) => return value,
            BuildStep::Yield => (),
            BuildStep::Complete => panic!("configuration missing"),
        }
    }
    panic!("AVC configuration stalled")
}
fn hevc(pool: &Budget, raw: Bytes, q: u64) -> HvcConfiguration {
    let mut parser = HvcConfigParser::new(
        pool,
        HvcConfigSource {
            source: SourceId(9),
            generation: u64::MAX,
            offset: u64::MAX - raw.len() as u64,
        },
        raw,
        HvcConfigLimits {
            maximum_record_bytes: 262_144,
            maximum_arrays: 255,
            maximum_nals: 1024,
            maximum_nal_bytes: 65_535,
        },
    )
    .unwrap();
    for _ in 0..200_000 {
        match parser
            .step(&mut WorkBudget::new(q), &AtomicBool::new(false))
            .unwrap()
        {
            BuildStep::Ready(value) => return value,
            BuildStep::Yield => (),
            BuildStep::Complete => panic!("configuration missing"),
        }
    }
    panic!("HEVC configuration stalled")
}

#[test]
fn avc_nal_does_not_store_redundant_range_end() {
    let ceiling = 2 * size_of::<Bytes>() + size_of::<usize>() + 8;
    println!(
        "CONFIG_RANGE_LAYOUT avc={} ceiling={ceiling}",
        size_of::<AvcConfigNal>()
    );
    assert!(
        size_of::<AvcConfigNal>() <= ceiling,
        "AVC NAL retains an end already determined by its immutable payload"
    );
}
#[test]
fn hevc_nal_does_not_store_redundant_range_end() {
    let ceiling = 2 * size_of::<Bytes>() + size_of::<usize>() + 8;
    println!(
        "CONFIG_RANGE_LAYOUT hevc={} ceiling={ceiling}",
        size_of::<HvcConfigNal>()
    );
    assert!(
        size_of::<HvcConfigNal>() <= ceiling,
        "HEVC NAL retains an end already determined by its immutable payload"
    );
}
#[test]
fn avc_source_ranges_above_u16_and_headers_survive_all_quanta() {
    for q in [1, 7, 4096] {
        let pool = Budget::new(512 * 1024).unwrap();
        let baseline = pool.used();
        let raw = avcc();
        let config = avc(&pool, Bytes::copy_from_slice(&pool, &raw).unwrap(), q);
        assert_eq!(config.source().offset + raw.len() as u64, u64::MAX);
        assert_eq!(config.source().generation, u64::MAX);
        assert_eq!(config.nals()[0].source_range(), 8..65_543);
        assert_eq!(config.nals()[1].source_range(), 65_546..65_549);
        assert_eq!(config.nals()[1].length_field_bytes().as_ref(), &[0, 3]);
        assert_eq!(config.nals()[1].source_bytes().as_ref(), &[0x68, 1, 99]);
        assert_eq!(config.nals()[1].array_kind(), AvcConfigArrayKind::Picture);
        assert_eq!(config.arrays()[0].nal_indices(), 0..1);
        assert_eq!(config.arrays()[1].nal_indices(), 1..2);
        assert_eq!(config.nals()[1].header().nal_unit_type(), 8);
        assert!(format!("{:?}", config.nals()[1]).contains("range: 65546..65549"));
        drop(config);
        assert_eq!(pool.used(), baseline);
    }
}
#[test]
fn hevc_source_ranges_above_u16_and_array_facts_survive_all_quanta() {
    for q in [1, 7, 4096] {
        let pool = Budget::new(512 * 1024).unwrap();
        let baseline = pool.used();
        let raw = hvcc();
        let config = hevc(&pool, Bytes::copy_from_slice(&pool, &raw).unwrap(), q);
        assert_eq!(config.source().offset + raw.len() as u64, u64::MAX);
        assert_eq!(config.nals()[0].source_range(), 28..65_563);
        assert_eq!(config.nals()[1].source_range(), 65_568..65_571);
        assert_eq!(config.nals()[1].array_index(), 1);
        assert_eq!(config.arrays()[0].nal_indices(), 0..1);
        assert_eq!(config.arrays()[1].nal_indices(), 1..2);
        assert_eq!(config.nals()[1].declared_nal_type(), 33);
        assert_eq!(config.nals()[1].length_field_bytes().as_ref(), &[0, 3]);
        assert_eq!(config.nals()[1].source_bytes().as_ref(), &[66, 1, 99]);
        assert!(config.nals()[1].type_matches_declaration());
        assert!(format!("{:?}", config.nals()[1]).contains("range: 65568..65571"));
        drop(config);
        assert_eq!(pool.used(), baseline);
    }
}
#[test]
fn cloned_descriptor_keeps_full_hidden_owner_after_record_closes() {
    let pool = Budget::new(512 * 1024).unwrap();
    let baseline = pool.used();
    let raw = hvcc();
    let mut parent = vec![0xa5; raw.len() + 2048];
    parent[127..127 + raw.len()].copy_from_slice(&raw);
    let root = Bytes::try_from_vec(&pool, parent)
        .map_err(|e| e.error())
        .unwrap();
    let retained = root.retained_storage_bytes();
    let record = hevc(&pool, root.slice(127..127 + raw.len()).unwrap(), 1);
    let nal = record.nals()[1].clone();
    drop(record);
    drop(root);
    assert_eq!(pool.used(), baseline + retained);
    assert_eq!(nal.source_range(), 65_568..65_571);
    assert_eq!(nal.source_bytes().retained_storage_bytes(), retained);
    assert_eq!(nal.length_field_bytes().retained_storage_bytes(), retained);
    drop(nal);
    assert_eq!(pool.used(), baseline);
}
#[test]
fn cancellation_precedes_work0_and_truncation_stays_physical() {
    let pool = Budget::new(512 * 1024).unwrap();
    let baseline = pool.used();
    let mut raw = hvcc();
    raw.pop();
    let bytes = Bytes::copy_from_slice(&pool, &raw).unwrap();
    let source = HvcConfigSource {
        source: SourceId(11),
        generation: 13,
        offset: 17,
    };
    let limits = HvcConfigLimits {
        maximum_record_bytes: 262_144,
        maximum_arrays: 255,
        maximum_nals: 1024,
        maximum_nal_bytes: 65_535,
    };
    let mut parser = HvcConfigParser::new(&pool, source, bytes.clone(), limits).unwrap();
    let mut work = WorkBudget::new(0);
    assert!(matches!(
        parser.step(&mut work, &AtomicBool::new(true)),
        Err(Error::Cancelled)
    ));
    assert_eq!(work.remaining(), 0);
    drop(parser);
    let mut parser = HvcConfigParser::new(&pool, source, bytes, limits).unwrap();
    let mut error = None;
    for _ in 0..200_000 {
        match parser.step(&mut WorkBudget::new(1), &AtomicBool::new(false)) {
            Ok(BuildStep::Yield) => (),
            Err(e) => {
                error = Some(e);
                break;
            }
            _ => panic!("truncated record published"),
        }
    }
    assert_eq!(error, Some(Error::Truncated));
    drop(parser);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn avc_array_uses_four_machine_words() {
    println!(
        "CONFIG_RANGE_LAYOUT avc_array={}",
        size_of::<AvcConfigArray>()
    );
    assert!(
        size_of::<AvcConfigArray>() <= 4 * size_of::<usize>(),
        "AVC array membership retains two full machine words"
    );
}
#[test]
fn hevc_array_membership_uses_at_most_eight_bytes() {
    let ceiling = size_of::<Bytes>() + size_of::<std::ops::Range<usize>>() + 8;
    println!(
        "CONFIG_RANGE_LAYOUT hevc_array={}",
        size_of::<HvcConfigArray>()
    );
    assert!(
        size_of::<HvcConfigArray>() <= ceiling,
        "HEVC array membership retains two full machine words"
    );
}
#[test]
fn maximum_avc_group_counts_preserve_all541_memberships() {
    let mut raw = vec![1, 100, 0, 10, 255, 255];
    for _ in 0..31 {
        raw.extend_from_slice(&[0, 1, 0x67]);
    }
    raw.push(255);
    for _ in 0..255 {
        raw.extend_from_slice(&[0, 1, 0x68]);
    }
    raw.extend_from_slice(&[253, 248, 248, 255]);
    for _ in 0..255 {
        raw.extend_from_slice(&[0, 1, 0x6d]);
    }
    for q in [1, 7, 4096] {
        let pool = Budget::new(512 * 1024).unwrap();
        let baseline = pool.used();
        let config = avc(&pool, Bytes::copy_from_slice(&pool, &raw).unwrap(), q);
        assert_eq!(config.nals().len(), 541);
        assert_eq!(config.arrays()[0].nal_indices(), 0..31);
        assert_eq!(config.arrays()[1].nal_indices(), 31..286);
        assert_eq!(config.arrays()[2].nal_indices(), 286..541);
        assert_eq!(config.arrays()[2].source_range().end, raw.len());
        assert_eq!(
            config.nals()[540].array_kind(),
            AvcConfigArrayKind::SequenceExtension
        );
        assert_eq!(config.nals()[540].source_bytes().as_ref(), &[0x6d]);
        assert!(format!("{:?}", config.arrays()[2]).contains("nals: 286..541"));
        drop(config);
        assert_eq!(pool.used(), baseline);
    }
}

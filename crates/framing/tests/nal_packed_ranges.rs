#![forbid(unsafe_code)]
use std::mem::size_of;
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, Error, SourceId, WorkBudget};
use triergon_framing::*;

fn hvcc(arrays: u8) -> Vec<u8> {
    let mut raw = vec![
        1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 120, 240, 0, 252, 253, 248, 248, 0, 0, 15, arrays,
    ];
    for index in 0..arrays {
        let declared = 32 + index % 3;
        // One header-valid opaque NAL per array; the last byte remains source.
        raw.extend_from_slice(&[128 | declared, 0, 1, 0, 3, declared << 1, 1, index]);
    }
    raw
}

fn parsed(pool: &Budget, raw: Bytes, quantum: u64) -> HvcConfiguration {
    let mut parser = HvcConfigParser::new(
        pool,
        HvcConfigSource {
            source: SourceId(u64::MAX),
            generation: u64::MAX,
            offset: 17,
        },
        raw,
        HvcConfigLimits {
            maximum_record_bytes: 65_536,
            maximum_arrays: 255,
            maximum_nals: 65_535,
            maximum_nal_bytes: 65_535,
        },
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    for _ in 0..20_000 {
        match parser.step(&mut WorkBudget::new(quantum), &cancel).unwrap() {
            BuildStep::Ready(value) => return value,
            BuildStep::Yield => (),
            BuildStep::Complete => panic!("configuration missing"),
        }
    }
    panic!("bounded configuration stalled")
}

#[test]
fn packet_segment_keeps_two_full_offsets_and_two_wire_fields() {
    assert_eq!(
        size_of::<NalPacketSegment>(),
        2 * size_of::<usize>() + 2 * size_of::<u32>(),
        "source-range segment descriptor retains a redundant end offset"
    );
    assert!(!std::mem::needs_drop::<NalPacketSegment>());
}

#[test]
fn all255_array_indices_and_source_fields_survive_hidden_parent_and_close() {
    for quantum in [1, 7, 4096] {
        let pool = Budget::new(256 * 1024).unwrap();
        let baseline = pool.used();
        let bytes = hvcc(255);
        let mut storage = vec![0xa5; bytes.len() + 511];
        storage[31..31 + bytes.len()].copy_from_slice(&bytes);
        let root = Bytes::try_from_vec(&pool, storage)
            .map_err(|e| e.error())
            .unwrap();
        let retained = root.retained_storage_bytes();
        let config = parsed(&pool, root.slice(31..31 + bytes.len()).unwrap(), quantum);
        assert_eq!(config.source_bytes().as_ref(), bytes);
        assert_eq!(config.source().source, SourceId(u64::MAX));
        assert_eq!(config.source().generation, u64::MAX);
        assert_eq!(config.nals().len(), 255);
        assert_eq!(config.source_bytes().retained_storage_bytes(), retained);
        for (index, nal) in config.nals().enumerate() {
            assert_eq!(nal.array_index(), index);
            assert_eq!(nal.declared_nal_type(), 32 + (index % 3) as u8);
            assert!(nal.type_matches_declaration());
            assert_eq!(nal.source_range(), 28 + 8 * index..31 + 8 * index);
            assert_eq!(nal.length_field_bytes(), &[0, 3]);
            assert_eq!(nal.source_bytes(), &bytes[28 + 8 * index..31 + 8 * index]);
        }
        drop(root);
        assert_eq!(
            pool.used(),
            baseline + config.retained_storage_bytes().unwrap()
        );
        let last = config.nal(254).unwrap();
        assert_eq!(last.array_index(), 254);
        assert_eq!(last.source_bytes(), &[68, 1, 254]);
        drop(config);
        assert_eq!(pool.used(), baseline);
    }
}

#[test]
fn zero_array_configuration_and_cancel_preserve_old_semantics() {
    let pool = Budget::new(16_384).unwrap();
    let baseline = pool.used();
    let raw = Bytes::copy_from_slice(&pool, &hvcc(0)).unwrap();
    let configuration = parsed(&pool, raw.clone(), 1);
    assert_eq!(configuration.nal_count(), 0);
    assert_eq!(configuration.array_count(), 0);
    drop(configuration);
    let before = pool.used();
    let mut parser = HvcConfigParser::new(
        &pool,
        HvcConfigSource {
            source: SourceId(0),
            generation: 0,
            offset: 0,
        },
        raw.clone(),
        HvcConfigLimits {
            maximum_record_bytes: 4096,
            maximum_arrays: 255,
            maximum_nals: 4096,
            maximum_nal_bytes: 4096,
        },
    )
    .unwrap();
    let cancel = AtomicBool::new(true);
    assert!(matches!(
        parser.step(&mut WorkBudget::new(0), &cancel),
        Err(Error::Cancelled)
    ));
    drop(parser);
    assert_eq!(pool.used(), before);
    drop(raw);
    assert_eq!(pool.used(), baseline);
}

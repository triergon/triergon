//! Parent-executed packet-layout guards; authored before the private changes.
use super::*;
use crate::{Error, Limits};
use std::mem::size_of;

#[test]
fn packed_lace_workspace_keeps_full_width_offsets_within_one_word_per_frame() {
    let ceiling = (256 + 40) * size_of::<usize>();
    assert!(
        size_of::<lace::LaceJob>() <= ceiling,
        "lace workspace={} ceiling={ceiling}; retain each frame end once",
        size_of::<lace::LaceJob>()
    );
}

#[test]
fn packet_batch_dormant_workspace_stays_within_four_kibibytes() {
    assert!(
        size_of::<packet::PacketBatchJob>() <= 4096,
        "ordinary packets cannot carry all exclusive metadata jobs: {} bytes",
        size_of::<packet::PacketBatchJob>()
    );
}

#[test]
fn all_lace_modes_keep_empty_frames_and_last_frame_bounds_under_work_one() {
    for quantum in [1, 7, 8192] {
        // Xiph: 256 frames, alternating sizes 0/1; final size is inferred.
        let lengths: Vec<usize> = (0..256).map(|i| i % 2).collect();
        let mut xiph = vec![255];
        xiph.extend(lengths[..255].iter().map(|n| *n as u8));
        xiph.extend((0..128).map(|n| n as u8));

        // EBML: sizes 0,1,0 and inferred 2. Signed deltas use bias 63.
        let ebml = vec![3, 0x80, 0xc0, 0xbe, b'A', b'B', b'C'];
        let fixed = vec![255]; // 256 valid empty frames.
        for (mode, bytes, sizes, data_at) in [
            (1, xiph, lengths, 256),
            (2, fixed, vec![0; 256], 1),
            (3, ebml, vec![0, 1, 0, 2], 4),
        ] {
            let pool = Budget::new(64 * 1024).unwrap();
            let baseline = pool.used();
            let raw = [b"prefix".as_slice(), bytes.as_slice(), b"suffix".as_slice()].concat();
            let source = SharedBytes::copy_from_slice(&pool, &raw).unwrap();
            let before = pool.used();
            let range = 6..6 + bytes.len();
            let mut job = lace::LaceJob::new(source.clone(), range, mode).unwrap();
            assert!(job.frames().is_none());
            assert!(matches!(
                job.step_descriptors(&mut WorkBudget::new(0), &AtomicBool::new(false))
                    .unwrap(),
                ParseStep::Yield
            ));
            let mut done = false;
            for _ in 0..4096 {
                let mut work = WorkBudget::new(quantum);
                let step = job
                    .step_descriptors(&mut work, &AtomicBool::new(false))
                    .unwrap();
                assert!(work.spent() <= quantum);
                assert_eq!(
                    pool.used(),
                    before,
                    "descriptors need no separate allocation"
                );
                match step {
                    ParseStep::Event(()) => {
                        done = true;
                        break;
                    }
                    ParseStep::Yield => assert!(job.frames().is_none()),
                    other => panic!("bounded lace: {other:?}"),
                }
            }
            assert!(done);
            let frames = job.frames().unwrap();
            assert_eq!(frames.len(), sizes.len());
            let mut at = 6 + data_at;
            for (index, size) in sizes.into_iter().enumerate() {
                assert_eq!(frames.get(index), Some(at..at + size));
                at += size;
            }
            assert_eq!(at, 6 + bytes.len());
            assert!(frames.get(frames.len()).is_none());
            drop(source);
            assert_eq!(
                pool.used(),
                before,
                "the job retains the whole source owner"
            );
            drop(job);
            assert_eq!(pool.used(), baseline);
        }
    }
}

#[test]
fn malformed_lace_and_cancellation_publish_no_partial_descriptor_plan() {
    for (mode, raw) in [
        (1, vec![1, 255]),
        (2, vec![2, b'A']),
        (3, vec![2, 0x81, 0x80]),
    ] {
        let pool = Budget::new(8192).unwrap();
        let source = SharedBytes::copy_from_slice(&pool, &raw).unwrap();
        let mut job = lace::LaceJob::new(source, 0..raw.len(), mode).unwrap();
        let mut rejected = false;
        for _ in 0..1024 {
            match job.step_descriptors(&mut WorkBudget::new(1), &AtomicBool::new(false)) {
                Ok(ParseStep::Yield) => assert!(job.frames().is_none()),
                Err(Error::Invalid(_) | Error::Truncated | Error::Overflow) => {
                    rejected = true;
                    break;
                }
                other => panic!("malformed lace cannot publish: {other:?}"),
            }
        }
        assert!(rejected);
        assert!(job.frames().is_none());
    }
    let pool = Budget::new(8192).unwrap();
    let baseline = pool.used();
    let source = SharedBytes::copy_from_slice(&pool, &[1, b'A', b'B']).unwrap();
    let mut job = lace::LaceJob::new(source, 0..3, 2).unwrap();
    assert!(matches!(
        job.step_descriptors(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap(),
        ParseStep::Yield
    ));
    assert!(matches!(
        job.step_descriptors(&mut WorkBudget::new(0), &AtomicBool::new(true)),
        Err(Error::Cancelled)
    ));
    assert!(job.frames().is_none());
    drop(job);
    assert_eq!(pool.used(), baseline);
}

fn assert_all_packet_owners_visible(job: &packet::PacketBatchJob, pool: &Budget) {
    // Every live allocation in this fixture belongs to the operation. One byte
    // beyond current free storage is permanently impossible only if the graph
    // includes the exact fixed slot and all its nested owners, deduplicated.
    let mut proof = usage::Pressure::new(pool.available() + 1);
    for _ in 0..100_000 {
        let mut work = WorkBudget::new(1);
        let step = proof.step(job, pool, &mut work, &AtomicBool::new(false));
        assert!(work.spent() <= 1);
        match step {
            Ok(ParseStep::Yield) => (),
            Err(Error::Policy("operation retained storage")) => return,
            other => panic!("an owned allocation escaped the graph: {other:?}"),
        }
    }
    panic!("bounded owner proof did not complete");
}

#[test]
fn group_retry_keeps_owned_source_and_needs_work_before_parse() {
    let pool = Budget::new(128 * 1024).unwrap();
    let baseline = pool.used();
    let config = seed_configuration(&pool, &[]);
    let raw = element(0xa1, &[0x81, 0, 0, 0, b'V']);
    let source = SharedBytes::copy_from_slice(&pool, &raw).unwrap();
    let before = pool.used();
    let mut job = packet::PacketBatchJob::new(
        source,
        false,
        0,
        1_000_000,
        config,
        Limits::default(),
        &pool,
    )
    .unwrap();
    assert_eq!(
        pool.used(),
        before,
        "construction alone cannot allocate workspace"
    );
    assert!(matches!(
        job.step(&mut WorkBudget::new(0), &AtomicBool::new(false))
            .unwrap(),
        ParseStep::Yield
    ));
    assert_eq!(pool.used(), before);
    let blocker = pool.reserve(pool.available()).unwrap();
    let mut requested = None;
    for _ in 0..2048 {
        let mut work = WorkBudget::new(1);
        let step = job.step(&mut work, &AtomicBool::new(false)).unwrap();
        assert!(work.spent() <= 1);
        match step {
            ParseStep::Yield => (),
            ParseStep::Backpressure {
                requested: bytes, ..
            } => {
                requested = Some(bytes);
                break;
            }
            other => panic!("retained caller pressure must prevent admission: {other:?}"),
        }
    }
    assert!(
        requested.unwrap() > 0,
        "a real retained output allocation must wait"
    );
    drop(blocker);
    let mut result = None;
    for _ in 0..10_000 {
        let mut work = WorkBudget::new(1);
        let step = job.step(&mut work, &AtomicBool::new(false)).unwrap();
        assert!(work.spent() <= 1);
        match step {
            ParseStep::Yield => assert_all_packet_owners_visible(&job, &pool),
            ParseStep::Event(packet) => {
                result = Some(packet);
                break;
            }
            other => panic!("workspace retry must preserve the group: {other:?}"),
        }
    }
    let packet = result.unwrap();
    assert_eq!(packet.packet.data.as_ref(), b"V");
    assert_eq!(packet.group.as_ref().unwrap().raw_body(), raw.as_slice());
    drop(job);
    assert_eq!(packet.packet.data.as_ref(), b"V");
    assert!(pool.used() > baseline);
    drop(packet);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn exclusive_metadata_transitions_keep_owner_graph_and_configuration_atomic() {
    let pool = Budget::new(256 * 1024).unwrap();
    let baseline = pool.used();
    let config = seed_configuration(&pool, &uint(0x55ee, 1));
    let original_revision = config.revision();
    let raw = [
        element(0xa1, &[0x81, 0, 0, 0, b'V']),
        element(0x75a1, &element(0xa6, &element(0xa5, b"opaque addition"))),
        element(0xa4, b"new initialization"),
    ]
    .concat();
    let source = SharedBytes::copy_from_slice(&pool, &raw).unwrap();
    let mut job = packet::PacketBatchJob::new(
        source,
        false,
        0,
        1_000_000,
        config,
        Limits::default(),
        &pool,
    )
    .unwrap();
    let mut changed = None;
    let mut result = None;
    for _ in 0..20_000 {
        if let Some(configuration) = job.take_configuration_change() {
            assert!(result.is_none());
            assert!(changed.replace(configuration).is_none());
        }
        let mut work = WorkBudget::new(1);
        let step = job.step(&mut work, &AtomicBool::new(false)).unwrap();
        assert!(work.spent() <= 1);
        match step {
            ParseStep::Yield => assert_all_packet_owners_visible(&job, &pool),
            ParseStep::Event(packet) => {
                result = Some(packet);
                break;
            }
            other => panic!("source metadata transaction: {other:?}"),
        }
    }
    let packet = result.unwrap();
    let changed = changed.unwrap();
    assert_eq!(changed.revision(), original_revision + 1);
    assert_eq!(packet.configuration.revision(), changed.revision());
    assert_eq!(
        packet.configuration.initialization(0).unwrap().data,
        b"new initialization"
    );
    assert_eq!(packet.packet.data.as_ref(), b"V");
    assert_eq!(packet.group.as_ref().unwrap().raw_body(), raw.as_slice());
    drop((job, changed));
    assert!(pool.used() > baseline);
    drop(packet);
    assert_eq!(pool.used(), baseline);
}

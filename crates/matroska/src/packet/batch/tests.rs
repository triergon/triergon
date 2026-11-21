//! Packet batch jobs: laces, restoration, configuration lifetimes, pressure,
//! cancellation and exact work.
use super::*;
use crate::ebml_kit::{ABC, abc_laces, element, uint, zlib_encoding};
use crate::storage::usage::Pressure;
use crate::test_kit::seed_configuration;

mod handoff;
mod layout;
mod publication;
mod ready_slot;

fn assert_owners_visible(job: &PacketBatchJob, pool: &Budget) {
    // Every live allocation in this fixture belongs to the operation. One byte
    // beyond current free storage is permanently impossible only if the graph
    // includes the exact fixed slot and all its nested owners, deduplicated.
    let mut proof = Pressure::new(pool.available() + 1);
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
fn recovery_uses_the_exact_end_of_a_fractional_audio_lace() {
    let timing = BlockTiming {
        timestamp_ticks: 0,
        scale_ns: 1,
        codec_delay_ns: 0,
        default_duration_ns: None,
        duration_ticks: Some(40_000_001),
        frame: 1,
        frames: 2,
    };
    let packet = PacketFields::new(0, timing, true, 0)
        .unwrap()
        .finish(PacketData {
            storage: SharedBytes::default(),
            range: 0..0,
        });
    assert_eq!(packet.pts_ns, Some(20_000_001));
    assert_eq!(packet.duration_ns, Some(20_000_001));
    let cutoff = |lower_ns| RecoveryCutoff {
        lower_ns,
        block_start_ticks: None,
    };
    assert!(!keep_recovery_packet(2, &packet, cutoff(40_000_001)).unwrap());
    assert!(keep_recovery_packet(2, &packet, cutoff(40_000_000)).unwrap());
}

#[test]
fn packet_batch_rejects_a_bad_last_crc_without_publishing_any_lace() {
    let pool = Budget::new(64 * 1024).unwrap();
    let config = seed_configuration(&pool, &zlib_encoding(1));
    let source = SharedBytes::copy_from_slice(&pool, &abc_laces(true)).unwrap();
    let mut job =
        PacketBatchJob::new(source, true, 0, 1_000_000, config, Limits::default(), &pool).unwrap();
    let mut failed = false;
    for _ in 0..10_000 {
        let mut work = WorkBudget::new(1);
        match job.step(&mut work, &AtomicBool::new(false)) {
            Ok(ParseStep::Yield) => (),
            Err(Error::Invalid("Adler-32 mismatch")) => {
                failed = true;
                break;
            }
            other => panic!("no lace may escape rejected batch: {other:?}"),
        }
        assert!(work.spent() <= 1);
    }
    assert!(failed);
    assert!(matches!(
        job.step(&mut WorkBudget::new(1000), &AtomicBool::new(false)),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn packet_batch_preserves_exact_laced_capacity_and_old_configuration_lifetime() {
    let pool = Budget::new(64 * 1024).unwrap();
    let baseline = pool.used();
    let config = seed_configuration(&pool, &zlib_encoding(1));
    let next = config.rebind(8, 4);
    let limits = Limits {
        block_bytes: 33,
        ..Limits::default()
    };
    let source = SharedBytes::copy_from_slice(&pool, &abc_laces(false)).unwrap();
    let mut job = PacketBatchJob::new(source, true, 2, 1_000_000, config, limits, &pool).unwrap();
    let mut packets = Vec::new();
    for _ in 0..10_000 {
        let mut work = WorkBudget::new(1);
        match job.step(&mut work, &AtomicBool::new(false)).unwrap() {
            ParseStep::Yield => (),
            ParseStep::Event(value) => packets.push(value),
            ParseStep::Finished => break,
            other => panic!("valid batch: {other:?}"),
        }
        assert!(work.spent() <= 1);
    }
    assert_eq!(packets.len(), 2);
    for packet in &packets {
        assert_eq!(packet.packet.data.as_ref(), b"abc");
        assert_eq!(packet.packet.data.storage.retained_capacity(), 3);
        assert_eq!(packet.configuration.generation, 7);
        assert_eq!(packet.configuration.revision, 3);
    }
    assert_eq!(packets[0].packet.pts_ns, Some(2_000_000));
    assert_eq!(packets[1].packet.pts_ns, None);
    assert_eq!(next.generation, 8);
    drop((next, job));
    assert!(pool.used() > baseline);
    assert_eq!(packets[1].packet.data.as_ref(), b"abc");
    drop(packets);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn nonlaced_stored_packet_keeps_borrowed_block_owner_and_its_capacity() {
    let pool = Budget::new(64 * 1024).unwrap();
    let config = seed_configuration(&pool, &zlib_encoding(1));
    let source =
        SharedBytes::copy_from_slice(&pool, &[&[0x81, 0, 0, 0x80][..], &ABC].concat()).unwrap();
    let original = source.clone();
    let mut job =
        PacketBatchJob::new(source, true, 0, 1_000_000, config, Limits::default(), &pool).unwrap();
    let mut packet = None;
    for _ in 0..10_000 {
        match job
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            ParseStep::Yield => (),
            ParseStep::Event(value) => {
                packet = Some(value);
                break;
            }
            other => panic!("stored frame: {other:?}"),
        }
    }
    let packet = packet.unwrap();
    assert_eq!(packet.packet.data.as_ref(), b"abc");
    assert!(packet.packet.data.storage.shares_storage_with(&original));
    assert_eq!(
        packet.packet.data.storage.retained_capacity(),
        original.retained_capacity()
    );
}

#[test]
fn packet_batch_keeps_pending_output_across_pressure_and_checks_cancel_before_publish() {
    let pool = Budget::new(64 * 1024).unwrap();
    let config = seed_configuration(&pool, &zlib_encoding(1));
    let source = SharedBytes::copy_from_slice(&pool, &abc_laces(false)).unwrap();
    let mut job =
        PacketBatchJob::new(source, true, 0, 1_000_000, config, Limits::default(), &pool).unwrap();
    let blocker = pool.reserve(pool.available()).unwrap();
    let mut saw_pressure = false;
    for _ in 0..1000 {
        match job
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            ParseStep::Yield => (),
            ParseStep::Backpressure { .. } => {
                saw_pressure = true;
                break;
            }
            other => panic!("reserved pool: {other:?}"),
        }
    }
    assert!(saw_pressure);
    drop(blocker);
    assert!(matches!(
        job.step(&mut WorkBudget::new(0), &AtomicBool::new(false))
            .unwrap(),
        ParseStep::Yield
    ));
    assert!(matches!(
        job.step(&mut WorkBudget::new(1000), &AtomicBool::new(true)),
        Err(Error::Cancelled)
    ));
    assert!(matches!(
        job.step(&mut WorkBudget::new(1000), &AtomicBool::new(false)),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn packet_batch_retries_pending_allocation_without_losing_either_lace() {
    let pool = Budget::new(64 * 1024).unwrap();
    let config = seed_configuration(&pool, &zlib_encoding(1));
    let source = SharedBytes::copy_from_slice(&pool, &abc_laces(false)).unwrap();
    let mut job =
        PacketBatchJob::new(source, true, 0, 1_000_000, config, Limits::default(), &pool).unwrap();
    let blocker = pool.reserve(pool.available()).unwrap();
    let mut blocked = false;
    for _ in 0..1000 {
        match job
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            ParseStep::Yield => (),
            ParseStep::Backpressure { .. } => {
                blocked = true;
                break;
            }
            other => panic!("retained caller pressure: {other:?}"),
        }
    }
    assert!(blocked);
    drop(blocker);
    let mut packets = Vec::new();
    let mut finished = false;
    for _ in 0..10_000 {
        match job
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            ParseStep::Yield => (),
            ParseStep::Event(value) => packets.push(value),
            ParseStep::Finished => {
                finished = true;
                break;
            }
            other => panic!("resumed batch: {other:?}"),
        }
    }
    assert!(finished);
    assert_eq!(packets.len(), 2);
    assert_eq!(packets[0].packet.data.as_ref(), b"abc");
    assert_eq!(packets[1].packet.data.as_ref(), b"abc");
}

#[test]
fn block_group_waits_for_trailing_timing_and_accepts_empty_codec_state_before_publication() {
    let block = element(0xa1, &[0x81, 0, 5, 0x04, 1, b'A', b'B']);
    for has_state in [false, true] {
        let pool = Budget::new(64 * 1024).unwrap();
        let configuration = seed_configuration(&pool, &[]);
        let initial_revision = configuration.revision();
        let initial_generation = configuration.generation();
        let mut raw = [
            block.clone(),
            uint(0x9b, 4),
            element(0xfb, &[0xff]),
            element(0x75a2, &[0xff]),
        ]
        .concat();
        if has_state {
            raw.extend(element(0xa4, &[]));
        }
        let source = SharedBytes::copy_from_slice(&pool, &raw).unwrap();
        let mut job = PacketBatchJob::new(
            source,
            false,
            10,
            1_000_000,
            configuration.clone(),
            Limits::default(),
            &pool,
        )
        .unwrap();
        let mut packets = Vec::new();
        let mut changed = None;
        let mut finished = false;
        for _ in 0..10_000 {
            if let Some(value) = job.take_configuration_change() {
                assert!(
                    packets.is_empty(),
                    "state configuration must precede every lace"
                );
                assert!(
                    changed.is_none(),
                    "one written state changes the batch revision once"
                );
                changed = Some(value);
            }
            match job.step(&mut WorkBudget::new(1), &AtomicBool::new(false)) {
                Ok(ParseStep::Yield) => (),
                Ok(ParseStep::Event(packet)) => packets.push(packet),
                Ok(ParseStep::Finished) => {
                    finished = true;
                    break;
                }
                other => panic!("trailing group fields: {other:?}"),
            }
        }
        assert!(finished);
        // Every original timing/duration/key/padding assertion now applies to
        // BOTH the ordinary group and the accepted empty-state transaction.
        assert_eq!(packets.len(), 2);
        assert_eq!(packets[0].packet.pts_ns, Some(15_000_000));
        assert_eq!(packets[1].packet.pts_ns, Some(17_000_000));
        assert_eq!(packets[0].packet.duration_ns, Some(2_000_000));
        assert!(
            packets
                .iter()
                .all(|p| !p.packet.keyframe && p.packet.discard_padding_ns == -1)
        );
        assert_eq!(changed.is_some(), has_state);
        assert!(
            configuration.codec_state(0).is_none(),
            "the initial snapshot stays immutable"
        );
        for packet in &packets {
            assert_eq!(packet.configuration.generation(), initial_generation);
            assert_eq!(
                packet.configuration.revision(),
                initial_revision + u64::from(has_state)
            );
            let group = packet.group.as_ref().unwrap();
            assert_eq!(group.raw_body(), raw.as_slice());
            assert_eq!(group.codec_state().is_some(), has_state);
            assert_eq!(packet.configuration.codec_state(0).is_some(), has_state);
            let initialization = packet.configuration.initialization(0).unwrap();
            if has_state {
                let state = group.codec_state().unwrap();
                assert!(state.data().is_empty());
                assert_eq!(state.raw(), element(0xa4, &[]).as_slice());
                assert_eq!(
                    state.position().unwrap(),
                    None,
                    "a low-level job has no invented absolute origin"
                );
                assert!(
                    packet
                        .configuration
                        .codec_state(0)
                        .unwrap()
                        .data()
                        .is_empty()
                );
                assert!(initialization.data.is_empty());
                assert_eq!(
                    initialization.origin,
                    crate::InitializationOrigin::CodecState(None)
                );
                assert_eq!(
                    changed.as_ref().unwrap().revision(),
                    packet.configuration.revision()
                );
            } else {
                assert_eq!(
                    initialization.origin,
                    crate::InitializationOrigin::InitialTrackEntry
                );
            }
        }
    }
}

#[test]
fn unlaced_batch_needs_no_heap_pointer_arena() {
    let pool = Budget::new(128 * 1024).unwrap();
    let config = seed_configuration(&pool, &[]);
    let source = SharedBytes::copy_from_slice(&pool, &[0x81, 0, 0, 0x80, b'V']).unwrap();
    let mut job =
        PacketBatchJob::new(source, true, 0, 1_000_000, config, Limits::default(), &pool).unwrap();
    let before = pool.used();
    println!(
        "Packet={} PacketBatch={}",
        size_of::<Packet>(),
        size_of::<PacketBatchJob>()
    );
    let cancel = AtomicBool::new(false);
    assert!(matches!(
        job.step(&mut WorkBudget::new(0), &cancel).unwrap(),
        ParseStep::Yield
    ));
    let mut packet = None;
    for _ in 0..1000 {
        let mut work = WorkBudget::new(1);
        let result = job.step(&mut work, &cancel).unwrap();
        assert!(work.spent() <= 1);
        assert_eq!(
            pool.used(),
            before,
            "one validated frame needs no pointer-vector allocation"
        );
        match result {
            ParseStep::Yield => (),
            ParseStep::Event(value) => {
                packet = Some(value);
                break;
            }
            other => panic!("one authored packet: {other:?}"),
        }
    }
    let packet = packet.unwrap();
    drop(job);
    assert_eq!(packet.packet.data.as_ref(), b"V");
    assert_eq!(packet.configuration.tracks()[0].number, 1);
}

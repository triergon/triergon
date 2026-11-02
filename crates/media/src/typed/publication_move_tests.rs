//! Last-publication lifetime, foreign-pool and exact-work compatibility guards.
use super::*;
use crate::{Error, Limits};

#[test]
fn only_publication_finishes_without_work_and_retains_configuration_after_job_drop() {
    let pool = Budget::new(128 * 1024).unwrap();
    let baseline = pool.used();
    let config = seed_configuration(&pool, &[]);
    let generation = config.generation();
    let revision = config.revision();
    let source = SharedBytes::copy_from_slice(&pool, &[0x81, 0, 0, 0x80, b'V']).unwrap();
    let mut job =
        packet::PacketBatchJob::new(source, true, 0, 1_000_000, config, Limits::default(), &pool)
            .unwrap();
    let blocker = pool.reserve(pool.available()).unwrap();
    let mut work = WorkBudget::new(7);
    let packet = match job.step(&mut work, &AtomicBool::new(false)).unwrap() {
        ParseStep::Event(packet) => packet,
        other => panic!("one complete identity frame: {other:?}"),
    };
    assert_eq!(work.spent(), 7);
    assert_eq!(packet.configuration.generation(), generation);
    assert_eq!(packet.configuration.revision(), revision);
    assert!(packet.configuration.belongs_to(&pool));
    assert!(packet.group.is_none());
    for _ in 0..2 {
        assert!(matches!(
            job.step(&mut WorkBudget::new(0), &AtomicBool::new(false))
                .unwrap(),
            ParseStep::Finished
        ));
    }
    drop((job, blocker));
    assert_eq!(packet.packet.data.as_ref(), b"V");
    let config = packet.configuration.clone();
    drop(packet);
    assert_eq!(config.tracks()[0].number, 1);
    assert_eq!(config.tracks()[0].codec.as_str(), "V_AUTHORED/OPAQUE");
    assert!(pool.used() > baseline);
    drop(config);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn final_laced_group_keeps_configuration_state_and_raw_source_after_earlier_packet_drop() {
    let pool = Budget::new(128 * 1024).unwrap();
    let baseline = pool.used();
    let config = seed_configuration(&pool, &[]);
    let revision = config.revision();
    let raw = [
        element(0xa1, &[0x81, 0, 0, 0x04, 1, b'A', b'B']),
        element(0xa4, b"state"),
        element(0xec, b"opaque source extension"),
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
    let (mut packets, mut changed) = (Vec::new(), None);
    let mut done = false;
    for _ in 0..20_000 {
        if let Some(configuration) = job.take_configuration_change() {
            assert!(
                packets.is_empty(),
                "configuration precedes every affected lace"
            );
            assert!(changed.replace(configuration).is_none());
        }
        let mut work = WorkBudget::new(1);
        let step = job.step(&mut work, &AtomicBool::new(false)).unwrap();
        assert!(work.spent() <= 1);
        match step {
            ParseStep::Yield => (),
            ParseStep::Event(packet) => packets.push(packet),
            ParseStep::Finished => {
                done = true;
                break;
            }
            other => panic!("complete source/state transaction: {other:?}"),
        }
    }
    assert!(done);
    assert_eq!(packets.len(), 2);
    assert_eq!(changed.as_ref().unwrap().revision(), revision + 1);
    for (packet, bytes) in packets.iter().zip([b"A", b"B"]) {
        assert_eq!(packet.packet.data.as_ref(), bytes);
        assert_eq!(packet.packet.data.retained_capacity(), 1);
        assert_eq!(packet.configuration.revision(), revision + 1);
        assert_eq!(
            packet.configuration.initialization(0).unwrap().data,
            b"state"
        );
        assert_eq!(packet.group.as_ref().unwrap().raw_body(), raw.as_slice());
    }
    assert!(matches!(
        job.step(&mut WorkBudget::new(0), &AtomicBool::new(false))
            .unwrap(),
        ParseStep::Finished
    ));
    let last = packets.pop().unwrap();
    drop((job, packets, changed));
    assert_eq!(last.packet.data.as_ref(), b"B");
    assert_eq!(last.configuration.initialization(0).unwrap().data, b"state");
    let group = last.group.as_ref().unwrap().clone();
    drop(last);
    assert_eq!(group.raw_body(), raw.as_slice());
    assert_eq!(group.codec_state().unwrap().data(), b"state");
    assert!(pool.used() > baseline);
    drop(group);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn constructor_keeps_foreign_pool_rejection_and_pending_publication_cancel_ownership() {
    let pool = Budget::new(128 * 1024).unwrap();
    let foreign = Budget::new(128 * 1024).unwrap();
    let baseline = pool.used();
    let config = seed_configuration(&pool, &[]);
    let source = SharedBytes::copy_from_slice(&foreign, &[0x81, 0, 0, 0x80, b'V']).unwrap();
    assert!(matches!(
        packet::PacketBatchJob::new(source, true, 0, 1_000_000, config, Limits::default(), &pool),
        Err(Error::Core(triergon_core::Error::ForeignPool))
    ));
    assert_eq!(pool.used(), baseline);
    let config = seed_configuration(&pool, &[]);
    let source = SharedBytes::copy_from_slice(&pool, &[0x81, 0, 0, 0x80, b'V']).unwrap();
    let before = pool.used();
    let mut job =
        packet::PacketBatchJob::new(source, true, 0, 1_000_000, config, Limits::default(), &pool)
            .unwrap();
    for _ in 0..6 {
        assert!(matches!(
            job.step(&mut WorkBudget::new(1), &AtomicBool::new(false))
                .unwrap(),
            ParseStep::Yield
        ));
    }
    assert_eq!(pool.used(), before);
    assert!(matches!(
        job.step(&mut WorkBudget::new(0), &AtomicBool::new(true)),
        Err(Error::Cancelled)
    ));
    assert_eq!(pool.used(), before);
    drop(job);
    assert_eq!(pool.used(), baseline);
}

//! Ownership, pending-publication and exact-work controls for packet moves.
use super::*;
use crate::{Error, Limits};

fn assert_owned_graph(job: &packet::PacketBatchJob, pool: &Budget) {
    let mut proof = usage::Pressure::new(pool.available() + 1);
    for _ in 0..100_000 {
        let mut work = WorkBudget::new(1);
        match proof.step(job, pool, &mut work, &AtomicBool::new(false)) {
            Ok(ParseStep::Yield) => (),
            Err(Error::Policy("operation retained storage")) => return,
            other => panic!("pending operation lost a full owner: {other:?}"),
        }
        assert!(work.spent() <= 1);
    }
    panic!("bounded owner proof did not finish");
}

#[test]
fn hidden_parent_identity_packet_keeps_exact_seven_work_units_and_full_owner() {
    for quantum in [1, 2, 7, 8192] {
        let pool = Budget::new(128 * 1024).unwrap();
        let baseline = pool.used();
        let config = seed_configuration(&pool, &[]);
        let mut raw = Vec::with_capacity(2048);
        raw.extend([0xa5; 32]);
        raw.extend([0x81, 0xff, 0xfe, 0x80, b'V', b'P']);
        raw.extend([0x5a; 64]);
        let owner = Bytes::try_from_vec(&pool, raw).unwrap();
        let source = SharedBytes::from_core(owner.slice(32..38).unwrap());
        let pointer = source[4..].as_ptr();
        let capacity = source.retained_capacity();
        drop(owner);
        let before = pool.used();
        let mut job = packet::PacketBatchJob::new(
            source,
            true,
            5,
            1_000_000,
            config,
            Limits::default(),
            &pool,
        )
        .unwrap();
        assert!(matches!(
            job.step(&mut WorkBudget::new(0), &AtomicBool::new(false))
                .unwrap(),
            ParseStep::Yield
        ));
        let mut spent = 0;
        let mut result = None;
        for _ in 0..100 {
            let mut work = WorkBudget::new(quantum);
            let step = job.step(&mut work, &AtomicBool::new(false)).unwrap();
            spent += work.spent();
            assert!(work.spent() <= quantum);
            assert_eq!(
                pool.used(),
                before,
                "identity handoff cannot allocate or release its hidden owner"
            );
            match step {
                ParseStep::Yield => assert_owned_graph(&job, &pool),
                ParseStep::Event(packet) => {
                    result = Some(packet);
                    break;
                }
                other => panic!("identity packet: {other:?}"),
            }
        }
        assert_eq!(
            spent, 7,
            "all original header/track/lace/reserve/frame/store/publish admissions remain"
        );
        let packet = result.unwrap();
        assert_eq!(packet.packet.data.as_ref(), b"VP");
        assert_eq!(packet.packet.data.as_ptr(), pointer);
        assert_eq!(packet.packet.data.storage.retained_capacity(), capacity);
        assert_eq!(packet.packet.pts_ns, Some(3_000_000));
        assert!(matches!(
            job.step(&mut WorkBudget::new(0), &AtomicBool::new(false))
                .unwrap(),
            ParseStep::Finished
        ));
        drop(job);
        assert_eq!(packet.packet.data.as_ref(), b"VP");
        assert!(pool.used() > baseline);
        drop(packet);
        assert_eq!(pool.used(), baseline);
    }
}

#[test]
fn identity_publication_waits_for_its_work_unit_and_cancel_keeps_the_pending_owner() {
    let pool = Budget::new(128 * 1024).unwrap();
    let baseline = pool.used();
    let config = seed_configuration(&pool, &[]);
    let source = SharedBytes::copy_from_slice(&pool, &[0x81, 0, 0, 0x80, b'V']).unwrap();
    let before = pool.used();
    let mut job =
        packet::PacketBatchJob::new(source, true, 0, 1_000_000, config, Limits::default(), &pool)
            .unwrap();
    for _ in 0..6 {
        let mut work = WorkBudget::new(1);
        assert!(matches!(
            job.step(&mut work, &AtomicBool::new(false)).unwrap(),
            ParseStep::Yield
        ));
        assert_eq!(work.spent(), 1);
        assert_eq!(pool.used(), before);
        assert_owned_graph(&job, &pool);
    }
    assert!(matches!(
        job.step(&mut WorkBudget::new(0), &AtomicBool::new(false))
            .unwrap(),
        ParseStep::Yield
    ));
    assert!(matches!(
        job.step(&mut WorkBudget::new(0), &AtomicBool::new(true)),
        Err(Error::Cancelled)
    ));
    assert_eq!(pool.used(), before);
    assert!(matches!(
        job.step(&mut WorkBudget::new(1), &AtomicBool::new(false)),
        Err(Error::Invalid(
            "packet batch requires replacement after error"
        ))
    ));
    drop(job);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn identity_handoff_under_external_pressure_needs_no_new_output_allocation() {
    let pool = Budget::new(128 * 1024).unwrap();
    let baseline = pool.used();
    let config = seed_configuration(&pool, &[]);
    let source = SharedBytes::copy_from_slice(&pool, &[0x81, 0, 0, 0x80, b'V']).unwrap();
    let mut job =
        packet::PacketBatchJob::new(source, true, 0, 1_000_000, config, Limits::default(), &pool)
            .unwrap();
    let blocker = pool.reserve(pool.available()).unwrap();
    let before = pool.used();
    let packet = match job
        .step(&mut WorkBudget::new(7), &AtomicBool::new(false))
        .unwrap()
    {
        ParseStep::Event(packet) => packet,
        other => panic!("identity needs no free storage: {other:?}"),
    };
    assert_eq!(packet.packet.data.as_ref(), b"V");
    assert_eq!(pool.used(), before);
    drop((blocker, job, packet));
    assert_eq!(pool.used(), baseline);
}

#[test]
fn packet_timing_failure_retains_owned_frame_until_failed_job_is_replaced() {
    let pool = Budget::new(128 * 1024).unwrap();
    let baseline = pool.used();
    let config = seed_configuration(&pool, &[]);
    let source = SharedBytes::copy_from_slice(&pool, &[0x81, 0, 0, 0x80, b'V']).unwrap();
    let before = pool.used();
    let mut job = packet::PacketBatchJob::new(
        source,
        true,
        u64::MAX,
        u64::MAX,
        config,
        Limits::default(),
        &pool,
    )
    .unwrap();
    assert!(matches!(
        job.step(&mut WorkBudget::new(8192), &AtomicBool::new(false)),
        Err(Error::Overflow)
    ));
    assert_eq!(
        pool.used(),
        before,
        "fallible timestamp preparation precedes taking ready storage"
    );
    assert_owned_graph(&job, &pool);
    drop(job);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn restored_laces_retry_real_pressure_and_keep_independent_decoded_capacity() {
    let pool = Budget::new(128 * 1024).unwrap();
    let baseline = pool.used();
    let config = seed_configuration(&pool, &encoding(1));
    let source = SharedBytes::copy_from_slice(&pool, &fixed_stored_block(false)).unwrap();
    let mut job =
        packet::PacketBatchJob::new(source, true, 0, 1_000_000, config, Limits::default(), &pool)
            .unwrap();
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
            other => panic!("real output admission must wait: {other:?}"),
        }
    }
    assert!(blocked);
    drop(blocker);
    let mut packets = Vec::new();
    let mut finished = false;
    for _ in 0..20_000 {
        let mut work = WorkBudget::new(1);
        let step = job.step(&mut work, &AtomicBool::new(false)).unwrap();
        assert!(work.spent() <= 1);
        match step {
            ParseStep::Yield if packets.is_empty() => assert_owned_graph(&job, &pool),
            ParseStep::Yield => (),
            ParseStep::Event(packet) => packets.push(packet),
            ParseStep::Finished => {
                finished = true;
                break;
            }
            other => panic!("restored batch retry: {other:?}"),
        }
    }
    assert!(finished);
    assert_eq!(packets.len(), 2);
    for packet in &packets {
        assert_eq!(packet.packet.data.as_ref(), b"abc");
        assert_eq!(packet.packet.data.storage.retained_capacity(), 3);
    }
    assert!(
        !packets[0]
            .packet
            .data
            .storage
            .shares_storage_with(&packets[1].packet.data.storage)
    );
    drop(job);
    assert!(pool.used() > baseline);
    drop(packets);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn plain_lace_keeps_exact_work_and_independent_frames_across_quanta() {
    for quantum in [1, 8192] {
        let pool = Budget::new(128 * 1024).unwrap();
        let baseline = pool.used();
        let config = seed_configuration(&pool, &[]);
        let source =
            SharedBytes::copy_from_slice(&pool, &[0x81, 0, 0, 0x84, 1, b'A', b'B', b'C', b'D'])
                .unwrap();
        let original = source.clone();
        let mut job = packet::PacketBatchJob::new(
            source,
            true,
            5,
            1_000_000,
            config,
            Limits::default(),
            &pool,
        )
        .unwrap();
        let mut packets = Vec::new();
        let mut spent = 0;
        let cancel = AtomicBool::new(false);
        for _ in 0..100 {
            let mut work = WorkBudget::new(quantum);
            let step = job.step(&mut work, &cancel).unwrap();
            spent += work.spent();
            match step {
                ParseStep::Yield => (),
                ParseStep::Event(packet) => packets.push(packet),
                ParseStep::Finished => break,
                other => panic!("plain lace: {other:?}"),
            }
        }
        assert_eq!(spent, 24);
        assert_eq!(packets.len(), 2);
        assert_eq!(packets[0].packet.pts_ns, Some(5_000_000));
        assert_eq!(packets[1].packet.pts_ns, None);
        for (packet, payload) in packets.iter().zip([b"AB", b"CD"]) {
            assert_eq!(packet.packet.data.as_ref(), payload);
            assert_eq!(packet.packet.data.retained_capacity(), 2);
            assert!(!packet.packet.data.storage.shares_storage_with(&original));
        }
        assert!(
            !packets[0]
                .packet
                .data
                .shares_storage_with(&packets[1].packet.data)
        );
        drop((job, original, packets));
        assert_eq!(pool.used(), baseline);
    }
}

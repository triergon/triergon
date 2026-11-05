//! Publication-boundary ownership and progress guards for the compact result.
use super::*;

#[test]
fn packet_ready_layout() {
    println!(
        "PACKET_READY_LAYOUT reader={} batch={} event={} batch_result={} compact_result={}",
        size_of::<crate::typed::Reader>(),
        size_of::<packet::PacketBatchJob>(),
        size_of::<crate::typed::Event>(),
        size_of::<crate::Result<ParseStep<packet::TypedPacket>>>(),
        size_of::<crate::Result<ParseStep<()>>>()
    );
}

fn assert_owner_visible(job: &packet::PacketBatchJob, pool: &Budget) {
    let mut proof = usage::Pressure::new(pool.available() + 1);
    for _ in 0..100_000 {
        let mut work = WorkBudget::new(1);
        match proof.step(job, pool, &mut work, &AtomicBool::new(false)) {
            Ok(ParseStep::Yield) => (),
            Err(crate::Error::Limit("operation retained storage")) => return,
            other => panic!("ready slot lost its retained owner: {other:?}"),
        }
        assert!(work.spent() <= 1);
    }
    panic!("owner proof failed to terminate");
}

#[test]
fn ready_slot_keeps_hidden_owner_and_exact_seven_units_until_synchronous_take() {
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
        let mut job = packet::PacketBatchJob::new(
            source,
            true,
            5,
            1_000_000,
            config,
            crate::Limits::default(),
            &pool,
        )
        .unwrap();
        let blocker = pool.reserve(pool.available()).unwrap();
        let used = pool.used();
        let cancel = AtomicBool::new(false);
        assert!(matches!(
            job.step_ready(None, &mut WorkBudget::new(0), &cancel)
                .unwrap(),
            ParseStep::Yield
        ));
        let mut spent = 0;
        let mut ready = false;
        for _ in 0..100 {
            let mut work = WorkBudget::new(quantum);
            let step = job.step_ready(None, &mut work, &cancel).unwrap();
            spent += work.spent();
            assert!(work.spent() <= quantum);
            assert_eq!(pool.used(), used);
            match step {
                ParseStep::Event(()) => {
                    ready = true;
                    break;
                }
                ParseStep::Yield => (),
                other => panic!("identity readiness: {other:?}"),
            }
        }
        assert!(ready);
        assert_eq!(spent, 7);
        // Permanent own-storage proofs exclude external reservations. Validate
        // the graph without that caller owner, then restore the exact pressure.
        drop(blocker);
        assert_owner_visible(&job, &pool);
        let blocker = pool.reserve(pool.available()).unwrap();
        let used = pool.used();
        let mut zero = WorkBudget::new(0);
        assert!(matches!(
            job.step_ready(None, &mut zero, &cancel).unwrap(),
            ParseStep::Event(())
        ));
        assert_eq!(zero.spent(), 0);
        // No new cancellation boundary exists between ready and its synchronous
        // take: the old API already returned the packet at this point.
        cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        let packet = job.take_ready().unwrap();
        assert_eq!(packet.packet.data.as_ref(), b"VP");
        assert_eq!(packet.packet.data.as_ptr(), pointer);
        assert_eq!(packet.packet.data.retained_capacity(), capacity);
        assert_eq!(packet.packet.pts_ns, Some(3_000_000));
        assert_eq!(pool.used(), used);
        drop((job, blocker));
        assert!(pool.used() > baseline);
        drop(packet);
        assert_eq!(pool.used(), baseline);
    }
}

#[test]
fn cancellation_observed_by_ready_poll_poisoning_cannot_be_bypassed_by_take() {
    let pool = Budget::new(128 * 1024).unwrap();
    let baseline = pool.used();
    let config = seed_configuration(&pool, &[]);
    let source = SharedBytes::copy_from_slice(&pool, &[0x81, 0, 0, 0x80, b'V']).unwrap();
    let mut job = packet::PacketBatchJob::new(
        source,
        true,
        0,
        1_000_000,
        config,
        crate::Limits::default(),
        &pool,
    )
    .unwrap();
    let used = pool.used();
    assert!(matches!(
        job.step_ready(None, &mut WorkBudget::new(7), &AtomicBool::new(false))
            .unwrap(),
        ParseStep::Event(())
    ));
    assert!(matches!(
        job.step_ready(None, &mut WorkBudget::new(0), &AtomicBool::new(true)),
        Err(crate::Error::Cancelled)
    ));
    assert!(matches!(
        job.take_ready(),
        Err(crate::Error::Invalid(
            "packet batch requires replacement after error"
        ))
    ));
    assert_eq!(pool.used(), used);
    assert_owner_visible(&job, &pool);
    drop(job);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn premature_take_poisoning_preserves_the_exact_source_until_drop() {
    let pool = Budget::new(128 * 1024).unwrap();
    let baseline = pool.used();
    let config = seed_configuration(&pool, &[]);
    let source = SharedBytes::copy_from_slice(&pool, &[0x81, 0, 0, 0x80, b'V']).unwrap();
    let mut job = packet::PacketBatchJob::new(
        source,
        true,
        0,
        1_000_000,
        config,
        crate::Limits::default(),
        &pool,
    )
    .unwrap();
    let used = pool.used();
    assert!(matches!(
        job.take_ready(),
        Err(crate::Error::Invalid("packet is not ready"))
    ));
    assert_eq!(pool.used(), used);
    assert!(matches!(
        job.step_ready(None, &mut WorkBudget::new(7), &AtomicBool::new(false)),
        Err(crate::Error::Invalid(
            "packet batch requires replacement after error"
        ))
    ));
    assert_owner_visible(&job, &pool);
    drop(job);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn ready_laces_observe_configuration_change_before_any_publication_and_keep_last_group() {
    let pool = Budget::new(128 * 1024).unwrap();
    let baseline = pool.used();
    let config = seed_configuration(&pool, &[]);
    let revision = config.revision();
    let raw = [
        element(0xa1, &[0x81, 0, 0, 0x04, 1, b'A', b'B']),
        element(0xa4, b"state"),
        element(0xec, b"opaque"),
    ]
    .concat();
    let source = SharedBytes::copy_from_slice(&pool, &raw).unwrap();
    let mut job = packet::PacketBatchJob::new(
        source,
        false,
        0,
        1_000_000,
        config,
        crate::Limits::default(),
        &pool,
    )
    .unwrap();
    let mut changed = None;
    let mut packets = Vec::new();
    let mut finished = false;
    for _ in 0..20_000 {
        if let Some(configuration) = job.take_configuration_change() {
            assert!(packets.is_empty());
            assert!(changed.replace(configuration).is_none());
        }
        let mut work = WorkBudget::new(1);
        match job
            .step_ready(None, &mut work, &AtomicBool::new(false))
            .unwrap()
        {
            ParseStep::Yield => (),
            ParseStep::Event(()) => {
                assert!(changed.is_some());
                // After the first take, packets contains real external owners;
                // the last-packet lifetime is checked after dropping the job.
                if packets.is_empty() {
                    assert_owner_visible(&job, &pool);
                }
                packets.push(job.take_ready().unwrap());
            }
            ParseStep::Finished => {
                finished = true;
                break;
            }
            other => panic!("complete state batch: {other:?}"),
        }
        assert!(work.spent() <= 1);
    }
    assert!(finished);
    assert_eq!(packets.len(), 2);
    assert_eq!(changed.as_ref().unwrap().revision(), revision + 1);
    for (packet, bytes) in packets.iter().zip([b"A", b"B"]) {
        assert_eq!(packet.packet.data.as_ref(), bytes);
        assert_eq!(packet.configuration.revision(), revision + 1);
        assert_eq!(
            packet.configuration.initialization(0).unwrap().data,
            b"state"
        );
        assert_eq!(packet.group.as_ref().unwrap().raw_body(), raw.as_slice());
    }
    let last = packets.pop().unwrap();
    drop((job, packets, changed));
    assert_eq!(last.packet.data.as_ref(), b"B");
    assert_eq!(
        last.group.as_ref().unwrap().codec_state().unwrap().data(),
        b"state"
    );
    drop(last);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn recovery_prefix_still_drops_early_video_and_keeps_unknown_second_lace() {
    let pool = Budget::new(128 * 1024).unwrap();
    let baseline = pool.used();
    let config = seed_configuration(&pool, &[]);
    let source = SharedBytes::copy_from_slice(&pool, &[0x81, 0, 0, 0x84, 1, b'A', b'B']).unwrap();
    let mut job = packet::PacketBatchJob::new(
        source,
        true,
        0,
        1_000_000,
        config,
        crate::Limits::default(),
        &pool,
    )
    .unwrap();
    let mut packets = Vec::new();
    let mut finished = false;
    for _ in 0..1000 {
        let mut work = WorkBudget::new(1);
        match job
            .step_ready(Some(1), &mut work, &AtomicBool::new(false))
            .unwrap()
        {
            ParseStep::Yield => (),
            ParseStep::Event(()) => packets.push(job.take_ready().unwrap()),
            ParseStep::Finished => {
                finished = true;
                break;
            }
            other => panic!("recovery readiness: {other:?}"),
        }
        assert!(work.spent() <= 1);
    }
    assert!(finished);
    assert_eq!(packets.len(), 1);
    assert_eq!(packets[0].packet.data.as_ref(), b"B");
    assert_eq!(packets[0].packet.pts_ns, None);
    assert_eq!(packets[0].packet.timing.frame, 1);
    drop((job, packets));
    assert_eq!(pool.used(), baseline);
}

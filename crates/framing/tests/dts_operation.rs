//! Operation guards.
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, InputWindow, SourceCapabilities, SourceId, WorkBudget};
use triergon_framing::compat::dts::{
    FactAssociationJob, ParserClockDisposition, ParserOperation, ParserOperationEvent,
    ParserOperationLane, ParserOperationLimits, ParserOperationStep, ParserOperationTerminal,
};
use triergon_framing::{Dts14CarrierLimits, Dts14SyncLimits, DtsWordOrder};

const VARIABLE: &[u8] = include_bytes!("../fixtures/dts-native-v1/variable-big.bin");
fn limits() -> ParserOperationLimits {
    ParserOperationLimits {
        source: Dts14SyncLimits {
            maximum_physical_frame_bytes: 8192,
            maximum_canonical_frame_bytes: 7168,
            maximum_frames: 32,
            maximum_input_window: 4096,
        },
        parser: Dts14CarrierLimits {
            maximum_packet_bytes: 65536,
            maximum_packets: 32,
            maximum_input_window: 4096,
        },
    }
}
fn new(pool: &Budget) -> ParserOperation {
    ParserOperation::new(
        pool,
        SourceId(35248),
        SourceCapabilities {
            length: None,
            random_access: true,
        },
        DtsWordOrder::BigEndian,
        limits(),
    )
    .unwrap()
}
fn collect(
    pool: &Budget,
    bytes: &[u8],
    chunk: usize,
    quantum: u64,
) -> (Vec<ParserOperationEvent>, ParserOperation) {
    let mut job = new(pool);
    let mut events = Vec::new();
    for _ in 0..1_000_000 {
        match job
            .step(&mut WorkBudget::new(quantum), &AtomicBool::new(false))
            .unwrap()
        {
            ParserOperationStep::Yield => {}
            ParserOperationStep::NeedInput { lane, request } => {
                let at = request.offset as usize;
                assert!(at <= bytes.len());
                if at == bytes.len() {
                    job.finish(lane, request.source, request.token).unwrap();
                } else {
                    let end = bytes.len().min(at + chunk.min(request.maximum));
                    let window = InputWindow::new(
                        request.source,
                        request.token,
                        request.offset,
                        Bytes::copy_from_slice(pool, &bytes[at..end]).unwrap(),
                    )
                    .unwrap();
                    job.provide(lane, &window).unwrap();
                }
            }
            ParserOperationStep::Event => events.push(job.take_event().unwrap()),
            ParserOperationStep::End { source, parser } => {
                assert_eq!(
                    source,
                    ParserOperationTerminal::End(triergon_core::SourceLocation {
                        source: SourceId(35248),
                        offset: bytes.len() as u64
                    })
                );
                assert_eq!(parser, source);
                return (events, job);
            }
        }
    }
    panic!("bounded original stream did not terminate")
}

#[test]
fn full_source_facts_are_not_discarded_when_native_packet_is_opaque() {
    for quantum in [1, 7, 4096] {
        for chunk in [1, 31, 4096] {
            let pool = Budget::new(2_000_000).unwrap();
            let (events, job) = collect(&pool, VARIABLE, chunk, quantum);
            let frames: Vec<_> = events
                .iter()
                .filter_map(|e| {
                    if let ParserOperationEvent::SourceFrame(f) = e {
                        Some(f)
                    } else {
                        None
                    }
                })
                .collect();
            let packets: Vec<_> = events
                .iter()
                .filter_map(|e| {
                    if let ParserOperationEvent::ParserPacket(p) = e {
                        Some(p)
                    } else {
                        None
                    }
                })
                .collect();
            assert_eq!(frames.len(), 3);
            assert_eq!(packets.len(), 1);
            assert!(packets[0].source_header().is_none());
            assert_eq!(packets[0].source_bytes().as_ref(), VARIABLE);
            assert_eq!(packets[0].physical_range(), 0..400);
            for (frame, size) in frames.iter().zip([98, 112, 140]) {
                assert_eq!(frame.facts().header.frame_bytes, size);
                assert_eq!(
                    frame.facts().header.window,
                    triergon_core::Time::new(2, 375).unwrap()
                );
            }
            let context = job.context();
            assert_ne!(context.source_generation(), context.parser_generation());
            drop(job);
            assert_eq!(packets[0].source_bytes().as_ref(), VARIABLE);
            assert_eq!(frames[0].physical_bytes().as_ref(), &VARIABLE[..112]);
            drop(frames);
            drop(packets);
            drop(events);
            assert_eq!(pool.used(), Budget::control_storage_bytes());
        }
    }
}

#[test]
fn zero_work_has_no_request_progress_or_allocation() {
    let pool = Budget::new(2_000_000).unwrap();
    let mut job = new(&pool);
    let a = job.request(ParserOperationLane::Source).unwrap();
    let b = job.request(ParserOperationLane::ParserProjection).unwrap();
    let used = pool.used();
    for _ in 0..8 {
        assert!(matches!(
            job.step(&mut WorkBudget::new(0), &AtomicBool::new(false))
                .unwrap(),
            ParserOperationStep::Yield
        ));
    }
    assert_eq!(
        job.request(ParserOperationLane::Source).unwrap().offset,
        a.offset
    );
    assert_eq!(
        job.request(ParserOperationLane::ParserProjection)
            .unwrap()
            .token,
        b.token
    );
    assert_eq!(pool.used(), used);
}

#[test]
fn cancel_drops_both_admitted_windows_and_is_sticky() {
    let pool = Budget::new(2_000_000).unwrap();
    let mut job = new(&pool);
    for lane in [
        ParserOperationLane::Source,
        ParserOperationLane::ParserProjection,
    ] {
        let q = job.request(lane).unwrap();
        let bytes = Bytes::copy_from_slice(&pool, &VARIABLE[..31]).unwrap();
        job.provide(
            lane,
            &InputWindow::new(q.source, q.token, q.offset, bytes).unwrap(),
        )
        .unwrap();
    }
    let cancel = AtomicBool::new(true);
    assert!(matches!(
        job.step(&mut WorkBudget::new(1), &cancel),
        Err(Error::Cancelled)
    ));
    cancel.store(false, Ordering::Relaxed);
    assert!(matches!(
        job.step(&mut WorkBudget::new(4096), &cancel),
        Err(Error::Cancelled)
    ));
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}

#[test]
fn foreign_response_and_wrong_lane_token_do_not_mutate_requests() {
    let pool = Budget::new(2_000_000).unwrap();
    let foreign = Budget::new(1024).unwrap();
    let mut job = new(&pool);
    let q = job.request(ParserOperationLane::Source).unwrap();
    let bad = InputWindow::new(
        q.source,
        q.token.clone(),
        q.offset,
        Bytes::copy_from_slice(&foreign, b"x").unwrap(),
    )
    .unwrap();
    assert_eq!(
        job.provide(ParserOperationLane::Source, &bad),
        Err(Error::ForeignPool)
    );
    let wrong = InputWindow::new(
        q.source,
        q.token.clone(),
        q.offset,
        Bytes::copy_from_slice(&pool, b"x").unwrap(),
    )
    .unwrap();
    assert!(
        job.provide(ParserOperationLane::ParserProjection, &wrong)
            .is_err()
    );
    assert_eq!(
        job.request(ParserOperationLane::Source).unwrap().token,
        q.token
    );
}

#[test]
fn every_source_frame_has_a_byte_proved_association_without_filling_native_header() {
    let pool = Budget::new(2_000_000).unwrap();
    let (events, job) = collect(&pool, VARIABLE, 31, 1);
    let packet = events
        .iter()
        .find_map(|e| {
            if let ParserOperationEvent::ParserPacket(p) = e {
                Some(p)
            } else {
                None
            }
        })
        .unwrap();
    for frame in events.iter().filter_map(|e| {
        if let ParserOperationEvent::SourceFrame(f) = e {
            Some(f)
        } else {
            None
        }
    }) {
        let mut proof = FactAssociationJob::new(&pool, job.context(), frame, packet).unwrap();
        assert!(matches!(
            proof
                .step(&mut WorkBudget::new(0), &AtomicBool::new(false))
                .unwrap(),
            triergon_core::Step::Yield
        ));
        let association = loop {
            match proof
                .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
                .unwrap()
            {
                triergon_core::Step::Yield => {}
                triergon_core::Step::Ready(v) => break v,
                _ => panic!(),
            }
        };
        assert_eq!(
            association.source_range().start,
            frame.facts().identity.offset
        );
        assert_eq!(association.packet_range(), 0..400);
        assert!(association.parser_packet().source_header().is_none());
        let clock = association.parser_packet().clock().unwrap();
        assert_eq!(
            (clock.duration, clock.codec_sample_rate, clock.profile),
            (0, 0, -99)
        );
        assert_eq!(clock.disposition, ParserClockDisposition::OpaqueOrShort);
        drop(proof);
        assert_eq!(
            association.source_frame().physical_bytes().as_ref(),
            frame.physical_bytes().as_ref()
        );
    }
}

#[test]
fn association_rejects_cross_generation_and_foreign_pool() {
    let pool = Budget::new(2_000_000).unwrap();
    let other = Budget::new(1024).unwrap();
    let (events, job) = collect(&pool, VARIABLE, 31, 7);
    let later = new(&pool);
    let source = events
        .iter()
        .find_map(|e| {
            if let ParserOperationEvent::SourceFrame(f) = e {
                Some(f)
            } else {
                None
            }
        })
        .unwrap();
    let packet = events
        .iter()
        .find_map(|e| {
            if let ParserOperationEvent::ParserPacket(p) = e {
                Some(p)
            } else {
                None
            }
        })
        .unwrap();
    assert!(matches!(
        FactAssociationJob::new(&other, job.context(), source, packet),
        Err(Error::ForeignPool)
    ));
    assert!(matches!(
        FactAssociationJob::new(&pool, later.context(), source, packet),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn association_cancellation_is_sticky_and_keeps_external_owners() {
    let pool = Budget::new(2_000_000).unwrap();
    let (events, job) = collect(&pool, VARIABLE, 31, 7);
    let source = events
        .iter()
        .find_map(|e| {
            if let ParserOperationEvent::SourceFrame(f) = e {
                Some(f)
            } else {
                None
            }
        })
        .unwrap();
    let packet = events
        .iter()
        .find_map(|e| {
            if let ParserOperationEvent::ParserPacket(p) = e {
                Some(p)
            } else {
                None
            }
        })
        .unwrap();
    let baseline = pool.used();
    let mut proof = FactAssociationJob::new(&pool, job.context(), source, packet).unwrap();
    let cancel = AtomicBool::new(true);
    assert!(matches!(
        proof.step(&mut WorkBudget::new(1), &cancel),
        Err(Error::Cancelled)
    ));
    cancel.store(false, Ordering::Relaxed);
    assert!(matches!(
        proof.step(&mut WorkBudget::new(1), &cancel),
        Err(Error::Cancelled)
    ));
    assert_eq!(pool.used(), baseline);
    assert_eq!(packet.source_bytes().as_ref(), VARIABLE);
}

#[test]
fn transport_failure_never_flushes_the_other_lane_as_eof() {
    let pool = Budget::new(2_000_000).unwrap();
    let mut job = new(&pool);
    let q = job.request(ParserOperationLane::Source).unwrap();
    job.fail(
        ParserOperationLane::Source,
        q.source,
        q.token,
        triergon_core::SourceFailure::Transport,
    )
    .unwrap();
    assert!(matches!(
        job.step(&mut WorkBudget::new(4096), &AtomicBool::new(false)),
        Err(Error::Transport)
    ));
    assert!(job.request(ParserOperationLane::ParserProjection).is_err());
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}

#[test]
fn known_length_is_not_an_explicit_eof() {
    let pool = Budget::new(2_000_000).unwrap();
    let mut job = ParserOperation::new(
        &pool,
        SourceId(35248),
        SourceCapabilities {
            length: Some(VARIABLE.len() as u64),
            random_access: true,
        },
        DtsWordOrder::BigEndian,
        limits(),
    )
    .unwrap();
    let mut waiting = [false; 2];
    for _ in 0..100000 {
        match job
            .step(&mut WorkBudget::new(4096), &AtomicBool::new(false))
            .unwrap()
        {
            ParserOperationStep::NeedInput { lane, request } => {
                let at = request.offset as usize;
                if at == VARIABLE.len() {
                    waiting[usize::from(lane == ParserOperationLane::ParserProjection)] = true;
                    if waiting == [true, true] {
                        return;
                    }
                } else {
                    let end = VARIABLE.len().min(at + request.maximum);
                    job.provide(
                        lane,
                        &InputWindow::new(
                            request.source,
                            request.token,
                            request.offset,
                            Bytes::copy_from_slice(&pool, &VARIABLE[at..end]).unwrap(),
                        )
                        .unwrap(),
                    )
                    .unwrap();
                }
            }
            ParserOperationStep::Event => {
                let _ = job.take_event().unwrap();
            }
            ParserOperationStep::Yield => {}
            ParserOperationStep::End { .. } => panic!("length silently completed a source"),
        }
    }
    panic!("bounded source did not request explicit EOF on both views")
}

#[test]
fn malformed_source_refusal_is_an_event_and_other_view_is_not_removed() {
    let pool = Budget::new(2_000_000).unwrap();
    let mut job = new(&pool);
    let mut refused = false;
    let mut opaque = false;
    for _ in 0..10000 {
        match job
            .step(&mut WorkBudget::new(7), &AtomicBool::new(false))
            .unwrap()
        {
            ParserOperationStep::NeedInput { lane, request } => {
                if request.offset == 0 {
                    job.provide(
                        lane,
                        &InputWindow::new(
                            request.source,
                            request.token,
                            0,
                            Bytes::copy_from_slice(&pool, b"bad").unwrap(),
                        )
                        .unwrap(),
                    )
                    .unwrap();
                } else {
                    job.finish(lane, request.source, request.token).unwrap();
                }
            }
            ParserOperationStep::Event => match job.take_event().unwrap() {
                ParserOperationEvent::ViewRefusal {
                    lane: ParserOperationLane::Source,
                    error: Error::Truncated,
                    ..
                } => refused = true,
                ParserOperationEvent::ParserPacket(p) => {
                    assert_eq!(p.source_bytes().as_ref(), b"bad");
                    opaque = true;
                }
                other => panic!("unexpected real event {other:?}"),
            },
            ParserOperationStep::End {
                source:
                    ParserOperationTerminal::Refused {
                        error: Error::Truncated,
                        ..
                    },
                parser: ParserOperationTerminal::End(_),
            } => {
                assert!(refused && opaque);
                return;
            }
            ParserOperationStep::Yield => {}
            other @ ParserOperationStep::End { .. } => {
                panic!("unexpected real failure outcome {other:?}")
            }
        }
    }
    panic!("bounded malformed input did not terminate")
}

#[test]
fn parser_clock_is_an_explicit_prefix_fact_not_source_completeness() {
    let original =
        include_bytes!("../fixtures/dts-native-v1/primary-sync-example-variable-big.bin");
    for length in [6, 8, 16, 17, 18, 19, 20, 21, 100, 110, 111, 112] {
        let pool = Budget::new(2_000_000).unwrap();
        let mut job = new(&pool);
        let mut result = None;
        for _ in 0..100_000 {
            match job
                .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
                .unwrap()
            {
                ParserOperationStep::NeedInput { lane, request } => {
                    let at = request.offset as usize;
                    if at == length {
                        job.finish(lane, request.source, request.token).unwrap();
                    } else {
                        let end = length.min(at + 7.min(request.maximum));
                        job.provide(
                            lane,
                            &InputWindow::new(
                                request.source,
                                request.token,
                                request.offset,
                                Bytes::copy_from_slice(&pool, &original[at..end]).unwrap(),
                            )
                            .unwrap(),
                        )
                        .unwrap();
                    }
                }
                ParserOperationStep::Event => {
                    if let ParserOperationEvent::ParserPacket(p) = job.take_event().unwrap() {
                        result = Some(p.clock().unwrap());
                    }
                }
                ParserOperationStep::End { .. } => break,
                ParserOperationStep::Yield => {}
            }
        }
        let clock = result.unwrap();
        assert_eq!(
            (clock.duration, clock.codec_sample_rate),
            if length < 18 { (0, 0) } else { (256, 48000) },
            "physical prefix {length}"
        );
    }
}

#[test]
fn native_clock_and_source_window_are_distinct_and_owners_outlive_operation() {
    let pool = Budget::new(2_000_000).unwrap();
    let input = include_bytes!("../fixtures/dts-native-v1/real-prefix-be.bin");
    let (events, job) = collect(&pool, input, 31, 1);
    let packet = events
        .iter()
        .find_map(|e| {
            if let ParserOperationEvent::ParserPacket(p) = e {
                Some(p)
            } else {
                None
            }
        })
        .unwrap()
        .clone();
    let clock = packet.clock().unwrap();
    assert_eq!(
        (clock.duration, clock.codec_sample_rate, clock.profile),
        (1024, 44100, 20)
    );
    drop(job);
    drop(events);
    assert_eq!(packet.source_bytes().len(), 4096);
    assert_eq!(packet.clock().unwrap(), clock);
    drop(packet);
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}

#[test]
fn native_clock_latches_initial_rate_with_nearest_rounding_and_keeps_source_time() {
    let input = include_bytes!("../fixtures/dts-operation-v1/latch48-round441.bin");
    let pool = Budget::new(2_000_000).unwrap();
    let (events, job) = collect(&pool, input, 1, 1);
    let clocks: Vec<_> = events
        .iter()
        .filter_map(|e| {
            if let ParserOperationEvent::ParserPacket(p) = e {
                Some(p.clock().unwrap())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        clocks
            .iter()
            .map(|c| (c.duration, c.codec_sample_rate))
            .collect::<Vec<_>>(),
        vec![(256, 48000), (1115, 48000)]
    );
    let facts: Vec<_> = events
        .iter()
        .filter_map(|e| {
            if let ParserOperationEvent::SourceFrame(f) = e {
                Some(f.facts())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(facts[1].header.core_rate_hz, 44100);
    assert_eq!(facts[1].header.coding_window_samples, 1024);
    assert_eq!(
        facts[1].header.window,
        triergon_core::Time::new(256, 11025).unwrap()
    );
    assert_ne!(
        facts[1].header.window,
        triergon_core::Time::new(1115, 48000).unwrap()
    );
    drop(job);
    drop(events);
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}

#[test]
fn a_later_clock_never_rewrites_the_first_source_or_projection() {
    for input in [
        include_bytes!("../fixtures/dts-operation-v1/latch441-round48.bin").as_slice(),
        include_bytes!("../fixtures/dts-operation-v1/return-to-initial.bin").as_slice(),
    ] {
        let pool = Budget::new(2_000_000).unwrap();
        let (events, job) = collect(&pool, input, 31, 7);
        let packets: Vec<_> = events
            .iter()
            .filter_map(|e| {
                if let ParserOperationEvent::ParserPacket(p) = e {
                    Some(p)
                } else {
                    None
                }
            })
            .collect();
        let initial = packets[0].clock().unwrap();
        assert!(
            packets
                .iter()
                .all(|p| p.clock().unwrap().codec_sample_rate == initial.codec_sample_rate)
        );
        if packets.len() == 4 {
            assert_eq!(
                packets
                    .iter()
                    .map(|p| p.clock().unwrap().duration)
                    .collect::<Vec<_>>(),
                vec![256, 1115, 768, 256]
            );
        } else {
            assert_eq!(
                (
                    initial.codec_sample_rate,
                    packets[1].clock().unwrap().duration
                ),
                (44100, 470)
            );
        }
        drop(job);
        assert_eq!(packets[0].clock().unwrap(), initial);
    }
}

#[test]
fn association_requires_literal_source_bytes_not_header_or_gold_agreement() {
    let pool = Budget::new(2_000_000).unwrap();
    let mut op = new(&pool);
    let mut frames = Vec::new();
    let mut packets = Vec::new();
    let mut altered = VARIABLE.to_vec();
    altered[30] ^= 1;
    for _ in 0..100_000 {
        match op
            .step(&mut WorkBudget::new(4096), &AtomicBool::new(false))
            .unwrap()
        {
            ParserOperationStep::NeedInput { lane, request } => {
                let data = if lane == ParserOperationLane::Source {
                    VARIABLE
                } else {
                    altered.as_slice()
                };
                let at = request.offset as usize;
                if at == data.len() {
                    op.finish(lane, request.source, request.token).unwrap();
                } else {
                    let end = data.len().min(at + 31.min(request.maximum));
                    op.provide(
                        lane,
                        &InputWindow::new(
                            request.source,
                            request.token,
                            request.offset,
                            Bytes::copy_from_slice(&pool, &data[at..end]).unwrap(),
                        )
                        .unwrap(),
                    )
                    .unwrap();
                }
            }
            ParserOperationStep::Event => match op.take_event().unwrap() {
                ParserOperationEvent::SourceFrame(f) => frames.push(f),
                ParserOperationEvent::ParserPacket(p) => packets.push(p),
                ParserOperationEvent::ViewRefusal { .. } => {}
            },
            ParserOperationStep::End { .. } => break,
            ParserOperationStep::Yield => {}
        }
    }
    let mut association =
        FactAssociationJob::new(&pool, op.context(), &frames[0], &packets[0]).unwrap();
    assert!(matches!(
        association.step(&mut WorkBudget::new(4096), &AtomicBool::new(false)),
        Err(Error::Invalid("DTS associated source bytes differ"))
    ));
    assert!(matches!(
        association.step(&mut WorkBudget::new(4096), &AtomicBool::new(false)),
        Err(Error::Invalid("DTS associated source bytes differ"))
    ));
    assert_ne!(
        frames[0].physical_bytes().as_ref()[30],
        packets[0].source_bytes().as_ref()[30]
    );
}

fn reach_ready(pool: &Budget, op: &mut ParserOperation) {
    for _ in 0..100_000 {
        match op
            .step(&mut WorkBudget::new(4096), &AtomicBool::new(false))
            .unwrap()
        {
            ParserOperationStep::Event => return,
            ParserOperationStep::NeedInput { lane, request } => {
                let at = request.offset as usize;
                if at == VARIABLE.len() {
                    op.finish(lane, request.source, request.token).unwrap();
                } else {
                    let end = VARIABLE.len().min(at + 31.min(request.maximum));
                    op.provide(
                        lane,
                        &InputWindow::new(
                            request.source,
                            request.token,
                            request.offset,
                            Bytes::copy_from_slice(pool, &VARIABLE[at..end]).unwrap(),
                        )
                        .unwrap(),
                    )
                    .unwrap();
                }
            }
            ParserOperationStep::Yield => {}
            other @ ParserOperationStep::End { .. } => {
                panic!("unexpected readiness outcome {other:?}")
            }
        }
    }
    panic!("bounded ready slot did not fill")
}

#[test]
fn ready_slot_repeated_poll_is_idempotent_without_work_or_allocation() {
    let pool = Budget::new(2_000_000).unwrap();
    let mut op = new(&pool);
    assert!(matches!(
        op.take_event(),
        Err(Error::Invalid("DTS event slot empty"))
    ));
    reach_ready(&pool, &mut op);
    let before = pool.used();
    let context = op.context();
    let a = op.request(ParserOperationLane::Source).unwrap();
    let b = op.request(ParserOperationLane::ParserProjection).unwrap();
    for quantum in [0, 1, 4096, 0] {
        let mut work = WorkBudget::new(quantum);
        assert!(matches!(
            op.step(&mut work, &AtomicBool::new(false)).unwrap(),
            ParserOperationStep::Event
        ));
        assert_eq!(work.remaining(), quantum);
        assert_eq!(pool.used(), before);
        assert_eq!(op.context(), context);
        assert_eq!(
            op.request(ParserOperationLane::Source).unwrap().offset,
            a.offset
        );
        assert_eq!(
            op.request(ParserOperationLane::ParserProjection)
                .unwrap()
                .token,
            b.token
        );
    }
    let event = op.take_event().unwrap();
    assert!(matches!(
        op.take_event(),
        Err(Error::Invalid("DTS event slot empty"))
    ));
    drop(op);
    if let ParserOperationEvent::SourceFrame(f) = event {
        assert_eq!(f.physical_bytes().as_ref(), &VARIABLE[..112]);
    } else {
        panic!("first complete Source frame must remain owned");
    }
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}

#[test]
fn cancellation_drops_ready_slot_without_publishing_its_owner() {
    let pool = Budget::new(2_000_000).unwrap();
    let mut op = new(&pool);
    reach_ready(&pool, &mut op);
    assert!(pool.used() > Budget::control_storage_bytes());
    assert!(matches!(
        op.step(&mut WorkBudget::new(0), &AtomicBool::new(true)),
        Err(Error::Cancelled)
    ));
    assert!(matches!(op.take_event(), Err(Error::Cancelled)));
    assert!(matches!(
        op.step(&mut WorkBudget::new(1), &AtomicBool::new(false)),
        Err(Error::Cancelled)
    ));
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}

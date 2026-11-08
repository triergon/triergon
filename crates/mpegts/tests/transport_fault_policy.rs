//! The consumer policy uses actual source diagnostics without reference-trace.
use std::sync::atomic::AtomicBool;
use triergon_core::{
    Budget, Bytes, Error, InputCursor, InputWindow, SourceCapabilities, SourceId, Step, WorkBudget,
};
use triergon_mpegts::compat::pes::{
    DemuxClock, DemuxObserver, DemuxProfile, DemuxScope, SourceContext, TransportFaultKind,
    TransportFaultPacket, TransportFaultRecovery, TransportObservation,
};
use triergon_mpegts::{
    AssemblyStep, PacketFormat, ParseLimits, SourceEpochs, TraceEvent, TraceFault, TransportPacket,
    TransportStream,
};

const TEI: &[u8] = include_bytes!("data/transport-tei.ts");
const SCRAMBLED: &[u8] = include_bytes!("data/transport-scrambled.ts");
const EPOCHS: SourceEpochs = SourceEpochs {
    clock: 0,
    configuration: 2,
};
const ORIGINAL: &[u8] = b"lane-ts-original-payload\0\x47\xff";

fn context(ordinal: u64) -> SourceContext {
    SourceContext {
        source_event_ordinal: ordinal,
        epochs: EPOCHS,
        byte_run_epoch: None,
    }
}
fn observer(budget: &Budget, limits: ParseLimits) -> DemuxObserver {
    DemuxObserver::new(
        DemuxProfile::PesPackets,
        DemuxScope {
            program: 1,
            pid: 256,
            stream_type: 0x1b,
            stream_id: 0xe0,
            entity: SourceId(1),
            epochs: EPOCHS,
        },
        budget.clone(),
        limits,
    )
    .unwrap()
}
fn source(data: &[u8], budget: &Budget) -> Vec<(TraceEvent, SourceContext)> {
    let backing = Bytes::copy_from_slice(budget, data).unwrap();
    let mut input = InputCursor::new(
        budget,
        SourceId(1),
        SourceCapabilities {
            length: Some(data.len() as u64),
            random_access: false,
        },
        4096,
    )
    .unwrap();
    let mut source = TransportStream::new(
        budget.clone(),
        PacketFormat::Ts188,
        ParseLimits::default(),
        1,
    )
    .unwrap();
    let mut events = Vec::new();
    for _ in 0..100_000 {
        match source
            .poll(&mut input, &mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            Step::Ready(event) => {
                let finished = matches!(event, TraceEvent::Finished);
                events.push((
                    event,
                    SourceContext {
                        source_event_ordinal: events.len() as u64,
                        epochs: source.source_epochs(),
                        byte_run_epoch: None,
                    },
                ));
                if finished {
                    return events;
                }
            }
            Step::Yield => {}
            Step::NeedInput(request) => {
                let start = request.offset as usize;
                if start == data.len() {
                    input.finish(request.source, request.token).unwrap();
                } else {
                    let end = (start + request.maximum).min(data.len());
                    input
                        .provide_owned(
                            InputWindow::new(
                                request.source,
                                request.token,
                                request.offset,
                                backing.slice(start..end).unwrap(),
                            )
                            .unwrap(),
                        )
                        .unwrap();
                }
            }
            Step::End(_) => source.end_of_source().unwrap(),
        }
    }
    panic!("bounded source did not finish");
}
fn original(budget: &Budget) -> (TransportPacket, TraceFault) {
    let mut packet = None;
    let mut fault = None;
    for (event, _) in source(TEI, budget) {
        match event {
            TraceEvent::Transport(p) if p.pid == 256 => packet = Some(p),
            TraceEvent::Diagnostic(f) if f.pid == Some(256) => fault = Some(f),
            _ => {}
        }
    }
    (packet.unwrap(), fault.unwrap())
}
fn staged(budget: &Budget) -> (TransportFaultRecovery, TransportPacket, TraceFault) {
    let (packet, fault) = original(budget);
    let mut policy = TransportFaultRecovery::new(observer(budget, ParseLimits::default())).unwrap();
    policy.push(packet.clone(), context(8)).unwrap();
    (policy, packet, fault)
}
fn idle_after_valid_packet(
    budget: &Budget,
) -> (DemuxObserver, triergon_mpegts::compat::pes::DemuxPacket) {
    let mut packet = triergon_mpegts::parse_transport(
        Bytes::copy_from_slice(budget, &include_bytes!("data/ts188-basic.ts")[376..564]).unwrap(),
        PacketFormat::Ts188,
        376,
    )
    .unwrap();
    packet.source_id = Some(SourceId(1));
    let mut inner = observer(budget, ParseLimits::default());
    inner.push(packet, context(8)).unwrap();
    let AssemblyStep::Output(packet) = inner
        .poll(&mut WorkBudget::new(4096), &AtomicBool::new(false))
        .unwrap()
    else {
        panic!("valid packet did not finish");
    };
    (inner, packet)
}
fn following_fault(budget: &Budget, capacity: usize) -> TransportPacket {
    let mut data = Vec::with_capacity(capacity);
    data.extend_from_slice(&TEI[376..564]);
    data[3] = (data[3] & 0xf0) | 1;
    let mut packet = triergon_mpegts::parse_transport(
        Bytes::try_from_vec(budget, data).unwrap(),
        PacketFormat::Ts188,
        564,
    )
    .unwrap();
    packet.source_id = Some(SourceId(1));
    packet
}
fn drain(policy: &mut TransportFaultRecovery) -> triergon_core::Result<Vec<TransportObservation>> {
    let mut packets = Vec::new();
    for _ in 0..4096 {
        let mut work = WorkBudget::new(1);
        let step = policy.poll(&mut work, &AtomicBool::new(false))?;
        assert!(work.spent() <= 1);
        match step {
            AssemblyStep::Output(packet) => packets.push(packet),
            AssemblyStep::Yield => {}
            AssemblyStep::NeedPayload | AssemblyStep::Finished => return Ok(packets),
        }
    }
    panic!("bounded policy did not progress");
}
fn one(policy: &mut TransportFaultRecovery) -> TransportFaultPacket {
    let mut packets = drain(policy).unwrap();
    assert_eq!(packets.len(), 1);
    let TransportObservation::Faulted(packet) = packets.pop().unwrap() else {
        panic!("faulted result must stay distinct");
    };
    packet
}

#[test]
fn wire_flags_keep_raw_bytes_faults_and_unknown_source_dts() {
    for (data, kind, expected, pts) in [
        (TEI, TransportFaultKind::TransportError, ORIGINAL, 90_000),
        (SCRAMBLED, TransportFaultKind::Scrambling, ORIGINAL, 90_000),
        (
            include_bytes!("data/selected-faults-v1/scrambling-1.ts").as_slice(),
            TransportFaultKind::Scrambling,
            b"fault-controls\0\x47\xff".as_slice(),
            180_000,
        ),
        (
            include_bytes!("data/selected-faults-v1/scrambling-3.ts").as_slice(),
            TransportFaultKind::Scrambling,
            b"fault-controls\0\x47\xff".as_slice(),
            180_000,
        ),
    ] {
        let budget = Budget::new(1 << 20).unwrap();
        let mut policy =
            TransportFaultRecovery::new(observer(&budget, ParseLimits::default())).unwrap();
        let events = source(data, &budget);
        assert!(
            !events
                .iter()
                .any(|(event, _)| matches!(event, TraceEvent::Pes(_)))
        );
        let mut outputs = Vec::new();
        for (event, c) in &events {
            match event {
                TraceEvent::Transport(p) if p.pid == 256 => policy.push(p.clone(), *c).unwrap(),
                TraceEvent::Diagnostic(f) if policy.awaiting_source_fault() => {
                    policy.confirm_source_fault(f, *c).unwrap();
                }
                _ => {}
            }
            outputs.extend(drain(&mut policy).unwrap());
        }
        assert_eq!(outputs.len(), 1);
        let TransportObservation::Faulted(packet) = outputs.pop().unwrap() else {
            panic!();
        };
        assert_eq!(packet.kind, kind);
        assert_eq!(packet.carrier.source.as_ref(), &data[376..564]);
        assert_eq!(
            packet.payload().flatten().copied().collect::<Vec<_>>(),
            expected
        );
        assert_eq!(packet.payload_len, expected.len());
        assert_eq!(packet.encoded_header.encoded_pts_raw33(), pts);
        assert_eq!(packet.encoded_header.encoded_dts_raw33(), None);
        assert_eq!(packet.source_epochs, EPOCHS);
        assert_eq!(packet.position, 376);
        assert_eq!(packet.flags, 0);
        assert_eq!(packet.duration90_raw, 0);
        assert_eq!(packet.stream_id, 0xe0);
        assert_eq!(packet.input_context, context(8));
        assert_eq!(packet.diagnostic_context, context(10));
        assert_eq!(packet.source_fault.location, packet.source_origin);
        assert!(
            packet
                .encoded_header
                .backing_bytes()
                .shares_storage_with(&packet.carrier.source)
        );
        assert!(
            packet.spans.as_slice()[0]
                .backing_bytes()
                .shares_storage_with(&packet.carrier.source)
        );
        let mut clock = DemuxClock::new(1).unwrap();
        let time = clock
            .project_transport_fault(&packet, &mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
            .unwrap();
        assert_eq!(time.encoded_pts_raw33, pts);
        assert_eq!(time.encoded_dts_raw33, None);
        assert_eq!(time.pts90, i128::from(pts));
        assert_eq!(time.dts90, time.pts90);
    }
}

#[test]
fn actual_confirmation_order_and_eof_are_required() {
    let budget = Budget::new(1 << 20).unwrap();
    let (mut policy, packet, fault) = staged(&budget);
    let used = budget.used();
    assert!(drain(&mut policy).unwrap().is_empty());
    assert!(policy.awaiting_source_fault());
    assert!(policy.end_of_source().is_err());
    assert!(policy.push(packet.clone(), context(11)).is_err());
    let mut wrong = fault.clone();
    wrong.pid = Some(257);
    assert!(policy.confirm_source_fault(&wrong, context(10)).is_err());
    wrong = fault.clone();
    wrong.location.offset += 188;
    assert!(policy.confirm_source_fault(&wrong, context(10)).is_err());
    wrong = fault.clone();
    wrong.error = Error::Unsupported("scrambled TS payload");
    assert!(policy.confirm_source_fault(&wrong, context(10)).is_err());
    for c in [
        context(8),
        SourceContext {
            epochs: SourceEpochs { clock: 1, ..EPOCHS },
            ..context(10)
        },
        SourceContext {
            byte_run_epoch: Some(0),
            ..context(10)
        },
    ] {
        assert!(policy.confirm_source_fault(&fault, c).is_err());
    }
    assert_eq!(budget.used(), used);
    policy.confirm_source_fault(&fault, context(10)).unwrap();
    assert!(policy.confirm_source_fault(&fault, context(11)).is_err());
    assert!(policy.end_of_source().is_err());
    let output = one(&mut policy);
    assert!(output.carrier.source.shares_storage_with(&packet.source));
    assert!(!policy.awaiting_source_fault());
    let replay = policy.push(packet, context(11)).unwrap_err();
    assert_eq!(
        replay.error,
        Error::Invalid("native PES source event order")
    );
    policy.end_of_source().unwrap();
    assert!(matches!(
        policy.poll(&mut WorkBudget::new(1), &AtomicBool::new(false)),
        Ok(AssemblyStep::Finished)
    ));
}

#[test]
fn every_phase_yields_and_cancellation_is_retryable() {
    for phase in 0..10 {
        let budget = Budget::new(1 << 20).unwrap();
        let (mut policy, _, fault) = staged(&budget);
        policy.confirm_source_fault(&fault, context(10)).unwrap();
        let mut work = WorkBudget::new(phase);
        assert!(matches!(
            policy.poll(&mut work, &AtomicBool::new(false)),
            Ok(AssemblyStep::Yield)
        ));
        assert_eq!(work.spent(), phase);
        let used = budget.used();
        assert!(matches!(
            policy.poll(&mut WorkBudget::new(0), &AtomicBool::new(false)),
            Ok(AssemblyStep::Yield)
        ));
        assert!(matches!(
            policy.poll(&mut WorkBudget::new(0), &AtomicBool::new(true)),
            Err(Error::Cancelled)
        ));
        assert_eq!(budget.used(), used);
        assert_eq!(one(&mut policy).payload_len, ORIGINAL.len());
        assert!(drain(&mut policy).unwrap().is_empty());
    }
}

#[test]
fn each_charged_reserve_retries_without_losing_input_or_duplicating_spans() {
    for phase in [5, 7] {
        let budget = Budget::new(1 << 20).unwrap();
        let baseline = budget.used();
        let (mut policy, original, fault) = staged(&budget);
        policy.confirm_source_fault(&fault, context(10)).unwrap();
        assert!(matches!(
            policy.poll(&mut WorkBudget::new(phase), &AtomicBool::new(false)),
            Ok(AssemblyStep::Yield)
        ));
        let blocker = budget.reserve(budget.available()).unwrap();
        let used = budget.used();
        for _ in 0..2 {
            assert!(matches!(
                policy.poll(&mut WorkBudget::new(1), &AtomicBool::new(false)),
                Err(Error::Backpressure { .. })
            ));
            assert_eq!(budget.used(), used);
        }
        drop(blocker);
        let packet = one(&mut policy);
        assert_eq!(packet.header_source_ranges.len(), 1);
        assert_eq!(packet.spans.len(), 1);
        assert!(packet.carrier.source.shares_storage_with(&original.source));
        assert_eq!(packet.source_fault.error, fault.error);
        assert!(drain(&mut policy).unwrap().is_empty());
        drop(packet);
        drop(original);
        drop(fault);
        drop(policy);
        assert_eq!(budget.used(), baseline);
    }
}

#[test]
fn unavoidable_carrier_and_descriptor_storage_returns_limit_when_one_byte_short() {
    let proof_budget = Budget::new(1 << 20).unwrap();
    let (_, fault) = original(&proof_budget);
    for capacity in [188, 4096] {
        for short in [true, false] {
            let mut data = Vec::with_capacity(capacity);
            data.extend_from_slice(&TEI[376..564]);
            let minimum = Budget::control_storage_bytes()
                + Bytes::owner_overhead_bytes()
                + data.capacity()
                + size_of::<triergon_mpegts::SourceRange>()
                + size_of::<triergon_mpegts::compat::pes::DemuxSpan>();
            let budget = Budget::new(minimum - usize::from(short)).unwrap();
            let backing = Bytes::try_from_vec(&budget, data).unwrap();
            let mut packet =
                triergon_mpegts::parse_transport(backing, PacketFormat::Ts188, 376).unwrap();
            packet.source_id = Some(SourceId(1));
            let mut policy =
                TransportFaultRecovery::new(observer(&budget, ParseLimits::default())).unwrap();
            policy.push(packet, context(8)).unwrap();
            policy.confirm_source_fault(&fault, context(10)).unwrap();
            assert!(matches!(
                policy.poll(&mut WorkBudget::new(0), &AtomicBool::new(false)),
                Ok(AssemblyStep::Yield)
            ));
            assert_eq!(
                policy
                    .poll(&mut WorkBudget::new(0), &AtomicBool::new(true))
                    .unwrap_err(),
                Error::Cancelled
            );
            if short {
                let mut work = WorkBudget::new(10);
                let error = policy.poll(&mut work, &AtomicBool::new(false)).unwrap_err();
                assert_eq!(work.spent(), 8);
                assert_eq!(
                    error,
                    Error::Limit {
                        requested: minimum,
                        maximum: minimum - 1
                    }
                );
                assert_eq!(
                    policy
                        .poll(&mut WorkBudget::new(0), &AtomicBool::new(true))
                        .unwrap_err(),
                    error
                );
            } else {
                let AssemblyStep::Output(TransportObservation::Faulted(packet)) = policy
                    .poll(&mut WorkBudget::new(10), &AtomicBool::new(false))
                    .unwrap()
                else {
                    panic!("exact storage must fit");
                };
                assert_eq!(packet.payload_len, ORIGINAL.len());
                assert_eq!(packet.spans.len(), 1);
                assert_eq!(packet.header_source_ranges.len(), 1);
                assert_eq!(budget.used(), minimum);
            }
        }
    }
}

#[test]
fn permanent_shortfall_includes_retained_idle_arena_capacity() {
    let probe = Budget::new(1 << 20).unwrap();
    let (_, mut fault) = original(&probe);
    fault.location.offset = 564;
    let (probe_inner, packet) = idle_after_valid_packet(&probe);
    drop(packet);
    let arena = probe.used() - Budget::control_storage_bytes();
    assert!(arena > 0);
    drop(probe_inner);
    let minimum = Budget::control_storage_bytes()
        + Bytes::owner_overhead_bytes()
        + 4096
        + arena
        + size_of::<triergon_mpegts::SourceRange>()
        + size_of::<triergon_mpegts::compat::pes::DemuxSpan>();
    let budget = Budget::new(minimum - 1).unwrap();
    let (inner, packet) = idle_after_valid_packet(&budget);
    drop(packet);
    assert_eq!(budget.used(), Budget::control_storage_bytes() + arena);
    let mut policy = TransportFaultRecovery::new(inner).unwrap();
    policy
        .push(following_fault(&budget, 4096), context(11))
        .unwrap();
    policy.confirm_source_fault(&fault, context(13)).unwrap();
    assert_eq!(
        policy
            .poll(&mut WorkBudget::new(8), &AtomicBool::new(false))
            .unwrap_err(),
        Error::Limit {
            requested: minimum,
            maximum: minimum - 1
        }
    );
}

#[test]
fn releasing_a_prior_output_resolves_pressure_without_releasing_other_reservations() {
    let budget = Budget::new(1 << 20).unwrap();
    let (_, mut fault) = original(&budget);
    fault.location.offset = 564;
    let (inner, retained) = idle_after_valid_packet(&budget);
    let mut policy = TransportFaultRecovery::new(inner).unwrap();
    policy
        .push(following_fault(&budget, 4096), context(11))
        .unwrap();
    policy.confirm_source_fault(&fault, context(13)).unwrap();
    let blocker = budget.reserve(budget.available()).unwrap();
    assert!(matches!(
        policy.poll(&mut WorkBudget::new(10), &AtomicBool::new(false)),
        Err(Error::Backpressure { .. })
    ));
    drop(retained);
    let packet = one(&mut policy);
    assert_eq!(packet.payload_len, ORIGINAL.len());
    assert_eq!(packet.position, 564);
    drop(blocker);
}

#[test]
fn descriptor_charges_and_retention_do_not_copy_payload() {
    let budget = Budget::new(1 << 20).unwrap();
    let baseline = budget.used();
    let (mut policy, original, fault) = staged(&budget);
    let input_charge = original.source.retained_storage_bytes();
    assert_eq!(budget.used(), baseline + input_charge);
    policy.confirm_source_fault(&fault, context(10)).unwrap();
    let mut used = budget.used();
    assert!(matches!(
        policy.poll(&mut WorkBudget::new(5), &AtomicBool::new(false)),
        Ok(AssemblyStep::Yield)
    ));
    assert_eq!(budget.used(), used);
    assert!(matches!(
        policy.poll(&mut WorkBudget::new(1), &AtomicBool::new(false)),
        Ok(AssemblyStep::Yield)
    ));
    used += size_of::<triergon_mpegts::SourceRange>();
    assert_eq!(budget.used(), used);
    assert!(matches!(
        policy.poll(&mut WorkBudget::new(1), &AtomicBool::new(false)),
        Ok(AssemblyStep::Yield)
    ));
    assert_eq!(budget.used(), used);
    assert!(matches!(
        policy.poll(&mut WorkBudget::new(1), &AtomicBool::new(false)),
        Ok(AssemblyStep::Yield)
    ));
    used += size_of::<triergon_mpegts::compat::pes::DemuxSpan>();
    assert_eq!(budget.used(), used);
    let packet = one(&mut policy);
    assert_eq!(budget.used(), used);
    policy.end_of_source().unwrap();
    drop(policy);
    drop(original);
    drop(fault);
    assert_eq!(budget.used(), used);
    assert_eq!(
        packet.payload().flatten().copied().collect::<Vec<_>>(),
        ORIGINAL
    );
    drop(packet);
    assert_eq!(budget.used(), baseline);
}

#[test]
fn encoded_clock_yield_cancellation_and_program_mismatch_leave_no_anchor() {
    let budget = Budget::new(1 << 20).unwrap();
    let (mut policy, _, fault) = staged(&budget);
    policy.confirm_source_fault(&fault, context(10)).unwrap();
    let packet = one(&mut policy);
    let mut clock = DemuxClock::new(2).unwrap();
    assert!(
        clock
            .project_transport_fault(&packet, &mut WorkBudget::new(0), &AtomicBool::new(false))
            .unwrap()
            .is_none()
    );
    assert_eq!(
        clock
            .project_transport_fault(&packet, &mut WorkBudget::new(0), &AtomicBool::new(true))
            .unwrap_err(),
        Error::Cancelled
    );
    assert_eq!(
        clock
            .project_transport_fault(&packet, &mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap_err(),
        Error::Invalid("transport fault clock program mismatch")
    );
    assert!(
        clock
            .project_header(
                256,
                EPOCHS,
                None,
                &mut WorkBudget::new(1),
                &AtomicBool::new(false)
            )
            .unwrap()
            .unwrap()
            .origin
            .is_none()
    );
    let mut clock = DemuxClock::new(1).unwrap();
    assert!(
        clock
            .project_transport_fault(&packet, &mut WorkBudget::new(1), &AtomicBool::new(true))
            .is_err()
    );
    assert!(
        clock
            .project_header(
                256,
                EPOCHS,
                None,
                &mut WorkBudget::new(1),
                &AtomicBool::new(false)
            )
            .unwrap()
            .unwrap()
            .origin
            .is_none()
    );
    assert_eq!(
        clock
            .project_transport_fault(&packet, &mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
            .unwrap()
            .encoded_dts_raw33,
        None
    );
}

#[test]
fn malformed_and_combined_faults_never_become_empty_success() {
    for data in [
        include_bytes!("data/selected-faults-v1/tei-and-marker.ts").as_slice(),
        include_bytes!("data/selected-faults-v1/tei-and-scrambling.ts").as_slice(),
        include_bytes!("data/selected-faults-v1/tei-incomplete-finite.ts").as_slice(),
        include_bytes!("data/transport-afc-reserved.ts").as_slice(),
        include_bytes!("data/transport-adaptation-overrun.ts").as_slice(),
        include_bytes!("data/pes-optional-header-overrun.ts").as_slice(),
        include_bytes!("data/pes-start-prefix-invalid.ts").as_slice(),
    ] {
        let budget = Budget::new(1 << 20).unwrap();
        let mut policy =
            TransportFaultRecovery::new(observer(&budget, ParseLimits::default())).unwrap();
        let mut refused = false;
        for (event, c) in source(data, &budget) {
            let admission = match event {
                TraceEvent::Transport(p) if p.pid == 256 => {
                    policy.push(p, c).map_err(|rejected| rejected.error)
                }
                TraceEvent::Diagnostic(f) => {
                    if policy.awaiting_source_fault() {
                        policy.confirm_source_fault(&f, c)
                    } else {
                        Err(f.error)
                    }
                }
                _ => Ok(()),
            };
            if admission.is_err() || drain(&mut policy).is_err() {
                refused = true;
                break;
            }
        }
        assert!(refused, "no malformed input may silently complete");
    }
}

#[test]
fn faulted_then_valid_preserves_counter_order_and_shared_clock_interval() {
    let budget = Budget::new(1 << 20).unwrap();
    let mut policy =
        TransportFaultRecovery::new(observer(&budget, ParseLimits::default())).unwrap();
    let mut clock = DemuxClock::new(1).unwrap();
    let mut times = Vec::new();
    for (event, c) in source(
        include_bytes!("data/selected-faults-v1/tei-then-valid.ts"),
        &budget,
    ) {
        match event {
            TraceEvent::Transport(p) if p.pid == 256 => policy.push(p, c).unwrap(),
            TraceEvent::Diagnostic(f) if policy.awaiting_source_fault() => {
                policy.confirm_source_fault(&f, c).unwrap();
            }
            _ => {}
        }
        for packet in drain(&mut policy).unwrap() {
            let time = match packet {
                TransportObservation::Faulted(packet) => {
                    let time = clock
                        .project_transport_fault(
                            &packet,
                            &mut WorkBudget::new(1),
                            &AtomicBool::new(false),
                        )
                        .unwrap()
                        .unwrap();
                    (time.pts90, time.origin)
                }
                TransportObservation::Accepted(packet) => {
                    assert_eq!(packet.source_header.dts_raw33, None);
                    let time = clock
                        .project_header(
                            packet.pid,
                            packet.source_epochs,
                            packet.time_header(),
                            &mut WorkBudget::new(1),
                            &AtomicBool::new(false),
                        )
                        .unwrap()
                        .unwrap();
                    (time.pts90.unwrap(), time.origin.unwrap())
                }
            };
            times.push(time);
        }
    }
    assert_eq!(times.len(), 2);
    assert_eq!(times[0].1, times[1].1);
    assert!(times[1].0 > times[0].0);
}

#[test]
fn plain_policy_pool_limits_and_full_width_locations_remain_explicit() {
    let budget = Budget::new(1 << 20).unwrap();
    let (packet, mut fault) = original(&budget);
    assert!(
        observer(&budget, ParseLimits::default())
            .push(packet.clone(), context(8))
            .is_err()
    );
    let valid = source(include_bytes!("data/ts188-basic.ts"), &budget)
        .into_iter()
        .find_map(|(event, _)| match event {
            TraceEvent::Transport(packet) if packet.pid == 256 => Some(packet),
            _ => None,
        })
        .unwrap();
    let mut queued = observer(&budget, ParseLimits::default());
    queued.push(valid, context(8)).unwrap();
    let mut rejected = TransportFaultRecovery::new(queued).unwrap_err();
    assert_eq!(
        rejected.error,
        Error::Invalid("transport recovery requires drained observer")
    );
    let AssemblyStep::Output(retained) = rejected
        .value
        .poll(&mut WorkBudget::new(4096), &AtomicBool::new(false))
        .unwrap()
    else {
        panic!("failed construction must retain pending assembly");
    };
    assert_eq!(retained.payload_len, ORIGINAL.len());
    let foreign = Budget::new(1 << 20).unwrap();
    let mut policy =
        TransportFaultRecovery::new(observer(&foreign, ParseLimits::default())).unwrap();
    let rejected = policy.push(packet.clone(), context(8)).unwrap_err();
    assert!(rejected.value.source.shares_storage_with(&packet.source));
    let mut policy = TransportFaultRecovery::new(observer(
        &budget,
        ParseLimits {
            pes_bytes: 14,
            ..ParseLimits::default()
        },
    ))
    .unwrap();
    policy.push(packet.clone(), context(8)).unwrap();
    policy.confirm_source_fault(&fault, context(10)).unwrap();
    let error = drain(&mut policy).unwrap_err();
    assert!(matches!(error, Error::Limit { .. }));
    assert_eq!(
        policy
            .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap_err(),
        error
    );
    let mut relocated = packet.clone();
    let delta = 1_u64 << 40;
    relocated.source_offset += delta;
    relocated.payload_source_offset += delta;
    fault.location.offset += delta;
    let mut policy =
        TransportFaultRecovery::new(observer(&budget, ParseLimits::default())).unwrap();
    policy.push(relocated, context(8)).unwrap();
    policy.confirm_source_fault(&fault, context(10)).unwrap();
    let output = one(&mut policy);
    assert_eq!(output.position, i64::try_from(delta + 376).unwrap());
    assert_eq!(output.source_origin.offset, delta + 376);
    assert_eq!(
        output.header_source_ranges.as_slice()[0].offset,
        delta + packet.payload_source_offset
    );
    let mut overflowing = packet;
    overflowing.source_offset = u64::MAX - 100;
    assert_eq!(
        TransportFaultRecovery::new(observer(&budget, ParseLimits::default()))
            .unwrap()
            .push(overflowing, context(8))
            .unwrap_err()
            .error,
        Error::Overflow
    );
}

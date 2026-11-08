//! Source-boundary policy without comparison-only APIs.
use std::sync::atomic::AtomicBool;
use triergon_core::{
    Budget, Bytes, Error, InputCursor, InputWindow, SourceCapabilities, SourceFailure, SourceId,
    Step, WorkBudget,
};
use triergon_mpegts::compat::pes::{
    DemuxClock, DemuxObserver, DemuxPacket, DemuxProfile, DemuxScope, PesFaultPolicy,
    SourceContext, TransportFaultRecovery, TransportObservation,
};
use triergon_mpegts::{
    AssemblyStep, MediaEntityEnd, PacketFormat, ParseLimits, SourceEpochs, TraceEvent, TraceFault,
    TransportStream,
};

const TIMELINE: &[u8] = include_bytes!("data/pcr-timeline-discontinuity.ts");
const THREE: &[u8] = include_bytes!("data/selected-faults-v1/pcr-discontinuity-three.ts");
const PARTIAL: &[u8] = include_bytes!("data/transport-partial-final-packet.ts");
const SYNC: &[u8] = include_bytes!("data/transport-sync-loss.ts");
const ORIGINAL: &[u8] = b"lane-ts-original-payload\0\x47\xff";
const EPOCHS: SourceEpochs = SourceEpochs {
    clock: 0,
    configuration: 2,
};
static RUN: AtomicBool = AtomicBool::new(false);
static CANCEL: AtomicBool = AtomicBool::new(true);

fn policy(pool: &Budget, options: PesFaultPolicy) -> TransportFaultRecovery {
    TransportFaultRecovery::with_policy(
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
            pool.clone(),
            ParseLimits::default(),
        )
        .unwrap(),
        options,
    )
    .unwrap()
}
fn options() -> PesFaultPolicy {
    PesFaultPolicy {
        source_pcr_pid: Some(256),
        terminal_transport: true,
        ..PesFaultPolicy::default()
    }
}
struct Source {
    bytes: Bytes,
    input: InputCursor,
    stream: TransportStream,
    ordinal: u64,
    grant: u64,
}
impl Source {
    fn new(data: &[u8], pool: &Budget, window: usize, grant: u64) -> Self {
        Self {
            bytes: Bytes::copy_from_slice(pool, data).unwrap(),
            input: InputCursor::new(
                pool,
                SourceId(1),
                SourceCapabilities {
                    // The proof must work without an advertised length.
                    length: None,
                    random_access: false,
                },
                window,
            )
            .unwrap(),
            stream: TransportStream::new(
                pool.clone(),
                PacketFormat::Ts188,
                ParseLimits::default(),
                1,
            )
            .unwrap(),
            ordinal: 0,
            grant,
        }
    }
    fn next(&mut self) -> triergon_core::Result<Step<TraceEvent>> {
        for _ in 0..100_000 {
            match self
                .stream
                .poll(&mut self.input, &mut WorkBudget::new(self.grant), &RUN)?
            {
                Step::NeedInput(r) => {
                    let start = r.offset as usize;
                    if start == self.bytes.len() {
                        self.input.finish(r.source, r.token)?;
                    } else {
                        let end = (start + r.maximum).min(self.bytes.len());
                        self.input.provide_owned(InputWindow::new(
                            r.source,
                            r.token,
                            r.offset,
                            self.bytes.slice(start..end)?,
                        )?)?;
                    }
                }
                Step::Yield => {}
                Step::Ready(event) => {
                    self.ordinal += 1;
                    return Ok(Step::Ready(event));
                }
                end => return Ok(end),
            }
        }
        panic!("bounded source did not advance");
    }
    fn context(&self) -> SourceContext {
        SourceContext {
            source_event_ordinal: self.ordinal - 1,
            epochs: self.stream.source_epochs(),
            byte_run_epoch: None,
        }
    }
    fn end(&mut self) -> MediaEntityEnd {
        assert!(matches!(self.next().unwrap(), Step::End(_)));
        self.stream
            .media_entity_end(&mut self.input, &mut WorkBudget::new(4096), &RUN)
            .unwrap()
            .unwrap()
    }
    fn finished(&mut self) -> (TraceEvent, SourceContext) {
        self.stream.end_of_source().unwrap();
        let Step::Ready(event) = self.next().unwrap() else {
            panic!("no source Finished")
        };
        assert!(matches!(event, TraceEvent::Finished));
        (event, self.context())
    }
}
fn drain(
    observer: &mut TransportFaultRecovery,
    grant: u64,
) -> triergon_core::Result<Vec<DemuxPacket>> {
    let mut packets = Vec::new();
    for _ in 0..100_000 {
        match observer.poll(&mut WorkBudget::new(grant), &RUN)? {
            AssemblyStep::Output(TransportObservation::Accepted(packet)) => packets.push(packet),
            AssemblyStep::Yield => {}
            AssemblyStep::NeedPayload | AssemblyStep::Finished => return Ok(packets),
            output @ AssemblyStep::Output(_) => panic!("unexpected fault extraction: {output:?}"),
        }
    }
    panic!("bounded observer did not drain");
}
fn tail(
    pool: &Budget,
    data: &[u8],
) -> (
    Source,
    TransportFaultRecovery,
    Vec<DemuxPacket>,
    TraceFault,
    SourceContext,
) {
    tail_from_source(pool, Source::new(data, pool, 1, 1))
}
fn tail_from_source(
    pool: &Budget,
    mut source: Source,
) -> (
    Source,
    TransportFaultRecovery,
    Vec<DemuxPacket>,
    TraceFault,
    SourceContext,
) {
    let mut observer = policy(pool, options());
    let mut packets = Vec::new();
    loop {
        let Step::Ready(event) = source.next().unwrap() else {
            panic!("no source fault")
        };
        let context = source.context();
        match event {
            TraceEvent::Transport(packet) if packet.pid == 256 => {
                observer.push(packet, context).unwrap();
                packets.extend(drain(&mut observer, 1).unwrap());
            }
            TraceEvent::Diagnostic(fault) => return (source, observer, packets, fault, context),
            _ => {}
        }
    }
}

#[test]
fn real_timeline_advances_only_future_source_epochs_and_keeps_the_demux_clock() {
    for data in [TIMELINE, THREE] {
        for (window, grant) in [(1, 1), (4096, 4096)] {
            let pool = Budget::new(1 << 20).unwrap();
            let idle = pool.used();
            {
                let mut source = Source::new(data, &pool, window, grant);
                let mut observer = policy(&pool, options());
                let mut packets = Vec::new();
                let mut strict_epochs = Vec::new();
                loop {
                    match source.next().unwrap() {
                        Step::Ready(event) => {
                            let context = source.context();
                            match event {
                                TraceEvent::Transport(packet) if packet.pid == 256 => {
                                    observer.push(packet, context).unwrap();
                                    packets.extend(drain(&mut observer, grant).unwrap());
                                }
                                TraceEvent::Pes(packet) => strict_epochs.push(packet.epochs.clock),
                                TraceEvent::Timeline { .. } => {
                                    observer.confirm_source_timeline(&event, context).unwrap();
                                }
                                TraceEvent::Finished => break,
                                TraceEvent::Diagnostic(fault) => {
                                    panic!("unexpected source fault {fault:?}")
                                }
                                _ => {}
                            }
                        }
                        Step::End(_) => source.stream.end_of_source().unwrap(),
                        other => panic!("unexpected source state {other:?}"),
                    }
                }
                observer.end_of_source().unwrap();
                assert!(drain(&mut observer, grant).unwrap().is_empty());
                assert_eq!(observer.scope().epochs.clock, 1);
                assert_eq!(
                    packets
                        .iter()
                        .map(|packet| packet.source_epochs.clock)
                        .collect::<Vec<_>>(),
                    if data.len() == TIMELINE.len() {
                        vec![0, 0]
                    } else {
                        vec![0, 0, 1]
                    }
                );
                assert_eq!(
                    strict_epochs,
                    if data.len() == TIMELINE.len() {
                        vec![0, 1]
                    } else {
                        vec![0, 1, 1]
                    }
                );
                let mut clock = DemuxClock::new(1).unwrap();
                let times: Vec<_> = packets
                    .iter()
                    .map(|packet| {
                        assert_eq!(packet.source_header.dts_raw33, None);
                        clock
                            .project_header(
                                packet.pid,
                                packet.source_epochs,
                                packet.time_header(),
                                &mut WorkBudget::new(1),
                                &RUN,
                            )
                            .unwrap()
                            .unwrap()
                            .pts90
                            .unwrap()
                    })
                    .collect();
                assert_eq!(
                    times,
                    if packets.len() == 2 {
                        vec![90000, 10]
                    } else {
                        vec![90000, 10, 90010]
                    }
                );
            }
            assert_eq!(pool.used(), idle);
        }
    }
}

#[test]
fn timeline_requires_the_actual_event_exact_epoch_and_fully_drained_boundary() {
    let pool = Budget::new(1 << 20).unwrap();
    let mut source = Source::new(TIMELINE, &pool, 4096, 4096);
    let mut observer = policy(&pool, options());
    loop {
        let Step::Ready(event) = source.next().unwrap() else {
            panic!("no PCR boundary")
        };
        let context = source.context();
        match event {
            TraceEvent::Transport(packet) if packet.pid == 256 => {
                let boundary = packet.discontinuity_indicator();
                observer.push(packet, context).unwrap();
                if boundary {
                    let next = SourceContext {
                        source_event_ordinal: context.source_event_ordinal + 1,
                        epochs: SourceEpochs { clock: 1, ..EPOCHS },
                        ..context
                    };
                    let event = TraceEvent::Timeline {
                        program: 1,
                        clock_epoch: 1,
                    };
                    assert!(observer.confirm_source_timeline(&event, next).is_err());
                    assert_eq!(
                        observer.poll(&mut WorkBudget::new(0), &CANCEL).unwrap_err(),
                        Error::Cancelled
                    );
                    assert!(matches!(
                        observer.poll(&mut WorkBudget::new(0), &RUN).unwrap(),
                        AssemblyStep::Yield
                    ));
                    drain(&mut observer, 1).unwrap();
                    assert!(observer.end_of_source().is_err());
                } else {
                    drain(&mut observer, 1).unwrap();
                }
            }
            TraceEvent::Timeline { .. } => {
                let before = observer.scope();
                for invalid in [
                    SourceContext {
                        source_event_ordinal: 0,
                        ..context
                    },
                    SourceContext {
                        epochs: EPOCHS,
                        ..context
                    },
                    SourceContext {
                        epochs: SourceEpochs { clock: 2, ..EPOCHS },
                        ..context
                    },
                    SourceContext {
                        epochs: SourceEpochs {
                            configuration: 3,
                            ..context.epochs
                        },
                        ..context
                    },
                    SourceContext {
                        byte_run_epoch: Some(1),
                        ..context
                    },
                ] {
                    assert!(observer.confirm_source_timeline(&event, invalid).is_err());
                    assert_eq!(observer.scope(), before);
                }
                for invalid in [
                    TraceEvent::Finished,
                    TraceEvent::Timeline {
                        program: 2,
                        clock_epoch: 1,
                    },
                    TraceEvent::Timeline {
                        program: 1,
                        clock_epoch: 2,
                    },
                ] {
                    assert!(observer.confirm_source_timeline(&invalid, context).is_err());
                }
                observer.confirm_source_timeline(&event, context).unwrap();
                assert!(observer.confirm_source_timeline(&event, context).is_err());
                return;
            }
            _ => {}
        }
    }
}

#[test]
fn terminal_fault_needs_real_eof_and_finished_and_retries_without_allocating() {
    for data in [PARTIAL, SYNC] {
        let pool = Budget::new(1 << 20).unwrap();
        let idle = pool.used();
        {
            let (mut source, mut observer, packets, fault, context) = tail(&pool, data);
            assert_eq!(packets.len(), 1);
            assert_eq!(
                packets[0].payload().flatten().copied().collect::<Vec<_>>(),
                ORIGINAL
            );
            observer.hold_terminal_transport(&fault, context).unwrap();
            assert!(observer.terminal_confirmation().is_none());
            assert!(observer.end_of_source().is_err());
            assert!(observer.hold_terminal_transport(&fault, context).is_err());
            assert!(
                observer
                    .confirm_terminal_finished(
                        &TraceEvent::Finished,
                        context,
                        &mut source.input,
                        &mut WorkBudget::new(1),
                        &RUN
                    )
                    .is_err()
            );
            let end = source.end();
            let used = pool.used();
            assert_eq!(
                observer
                    .seal_terminal_end(&end, &mut source.input, &mut WorkBudget::new(0), &CANCEL)
                    .unwrap_err(),
                Error::Cancelled
            );
            assert!(
                !observer
                    .seal_terminal_end(&end, &mut source.input, &mut WorkBudget::new(0), &RUN)
                    .unwrap()
            );
            assert!(
                observer
                    .seal_terminal_end(&end, &mut source.input, &mut WorkBudget::new(1), &RUN)
                    .unwrap()
            );
            assert_eq!(pool.used(), used);
            assert!(observer.terminal_confirmation().is_none());
            assert!(observer.end_of_source().is_err());
            let (finished, context) = source.finished();
            assert!(
                !observer
                    .confirm_terminal_finished(
                        &finished,
                        context,
                        &mut source.input,
                        &mut WorkBudget::new(0),
                        &RUN
                    )
                    .unwrap()
            );
            assert_eq!(
                observer
                    .confirm_terminal_finished(
                        &finished,
                        context,
                        &mut source.input,
                        &mut WorkBudget::new(0),
                        &CANCEL
                    )
                    .unwrap_err(),
                Error::Cancelled
            );
            assert!(
                observer
                    .confirm_terminal_finished(
                        &finished,
                        context,
                        &mut source.input,
                        &mut WorkBudget::new(1),
                        &RUN
                    )
                    .unwrap()
            );
            assert_eq!(pool.used(), used);
            let confirmation = observer.terminal_confirmation().unwrap();
            assert_eq!(confirmation.source_fault().error, fault.error);
            assert_eq!(confirmation.source_fault().location, fault.location);
            assert_eq!(confirmation.physical_end(), end.location());
            assert_eq!(confirmation.source_finished(), context);
            observer.end_of_source().unwrap();
            assert!(drain(&mut observer, 1).unwrap().is_empty());
            assert!(matches!(
                observer.poll(&mut WorkBudget::new(1), &RUN).unwrap(),
                AssemblyStep::Finished
            ));
            // Output storage survives completion and policy drop.
            drop(observer);
            assert_eq!(packets[0].payload_len, ORIGINAL.len());
        }
        assert_eq!(pool.used(), idle);
    }
}

#[test]
fn wrong_context_and_transport_faults_cannot_select_terminal_completion() {
    let pool = Budget::new(1 << 20).unwrap();
    let (_, mut observer, _, fault, context) = tail(&pool, PARTIAL);
    for invalid in [
        SourceContext {
            source_event_ordinal: 0,
            ..context
        },
        SourceContext {
            epochs: SourceEpochs { clock: 1, ..EPOCHS },
            ..context
        },
        SourceContext {
            byte_run_epoch: Some(1),
            ..context
        },
    ] {
        assert!(observer.hold_terminal_transport(&fault, invalid).is_err());
    }
    for error in [
        Error::Invalid("reserved adaptation control"),
        Error::Invalid("PES start prefix"),
        Error::Cancelled,
        Error::Allocation,
        Error::Backpressure {
            requested: 1,
            available: 0,
        },
    ] {
        let mut other = fault.clone();
        other.error = error;
        assert!(observer.hold_terminal_transport(&other, context).is_err());
    }
    let mut other = fault.clone();
    other.location.offset += 1;
    assert!(observer.hold_terminal_transport(&other, context).is_err());
    other = fault.clone();
    other.location.source = SourceId(2);
    assert!(observer.hold_terminal_transport(&other, context).is_err());
    let mut empty = policy(&pool, options());
    assert!(empty.hold_terminal_transport(&fault, context).is_err());
    let mut disabled = policy(&pool, PesFaultPolicy::default());
    assert!(disabled.hold_terminal_transport(&fault, context).is_err());
    observer.hold_terminal_transport(&fault, context).unwrap();
}

#[test]
fn a_valid_carrier_after_the_fault_and_a_longer_tail_are_not_completion() {
    for original in [PARTIAL, SYNC] {
        let pool = Budget::new(1 << 20).unwrap();
        let mut data = original.to_vec();
        data.extend_from_slice(&TIMELINE[376..564]);
        let (mut source, mut observer, _, fault, context) = tail(&pool, &data);
        if observer.hold_terminal_transport(&fault, context).is_err() {
            // More bytes can complete a formerly partial carrier, whose actual
            // malformed adaptation/PES must still be refused before EOF.
            assert!(observer.terminal_confirmation().is_none());
            continue;
        }
        loop {
            match source.next().unwrap() {
                Step::Ready(TraceEvent::Transport(packet)) if packet.pid == 256 => {
                    assert!(observer.push(packet, source.context()).is_err());
                    break;
                }
                Step::Ready(TraceEvent::Diagnostic(_)) => {}
                Step::End(_) => {
                    let end = source
                        .stream
                        .media_entity_end(&mut source.input, &mut WorkBudget::new(4096), &RUN)
                        .unwrap()
                        .unwrap();
                    assert!(
                        observer
                            .seal_terminal_end(
                                &end,
                                &mut source.input,
                                &mut WorkBudget::new(1),
                                &RUN
                            )
                            .is_err()
                    );
                    break;
                }
                other => panic!("unexpected source state {other:?}"),
            }
        }
        assert!(observer.end_of_source().is_err());
        assert!(observer.terminal_confirmation().is_none());
    }
}

#[test]
fn terminal_confirmation_rejects_reset_tokens_foreign_pool_and_real_input_failure() {
    for reset in [false, true] {
        let pool = Budget::new(1 << 20).unwrap();
        let mut source = Source::new(SYNC, &pool, 1, 1);
        if reset {
            source.input = InputCursor::new(
                &pool,
                SourceId(1),
                SourceCapabilities {
                    length: None,
                    random_access: true,
                },
                1,
            )
            .unwrap();
        }
        let (mut source, mut observer, _, fault, context) = tail_from_source(&pool, source);
        observer.hold_terminal_transport(&fault, context).unwrap();
        let end = source.end();
        let foreign = Budget::new(1 << 20).unwrap();
        let mut foreign_input = InputCursor::new(
            &foreign,
            SourceId(1),
            SourceCapabilities {
                length: None,
                random_access: false,
            },
            4096,
        )
        .unwrap();
        assert_eq!(
            observer
                .seal_terminal_end(&end, &mut foreign_input, &mut WorkBudget::new(1), &RUN)
                .unwrap_err(),
            Error::ForeignPool
        );
        assert!(
            observer
                .seal_terminal_end(&end, &mut source.input, &mut WorkBudget::new(1), &RUN)
                .unwrap()
        );
        let (finished, context) = source.finished();
        if reset {
            source.input.reset(end.location().offset).unwrap();
            assert!(
                observer
                    .confirm_terminal_finished(
                        &finished,
                        context,
                        &mut source.input,
                        &mut WorkBudget::new(1),
                        &RUN
                    )
                    .is_err()
            );
        } else {
            source
                .input
                .fail(SourceId(1), source.input.token(), SourceFailure::Transport)
                .unwrap();
            assert_eq!(
                observer
                    .confirm_terminal_finished(
                        &finished,
                        context,
                        &mut source.input,
                        &mut WorkBudget::new(1),
                        &RUN
                    )
                    .unwrap_err(),
                Error::Transport
            );
        }
        assert!(observer.terminal_confirmation().is_none());
        assert!(observer.end_of_source().is_err());
    }
}

#[test]
fn terminal_proof_uses_no_new_storage_under_unrelated_pressure() {
    let pool = Budget::new(1 << 20).unwrap();
    let (mut source, mut observer, _, fault, context) = tail(&pool, SYNC);
    let used = pool.used();
    let pressure = pool.reserve(pool.available()).unwrap();
    observer.hold_terminal_transport(&fault, context).unwrap();
    assert_eq!(pool.available(), 0);
    drop(pressure);
    assert_eq!(pool.used(), used);
    let end = source.end();
    let used = pool.used();
    let pressure = pool.reserve(pool.available()).unwrap();
    assert!(
        observer
            .seal_terminal_end(&end, &mut source.input, &mut WorkBudget::new(1), &RUN)
            .unwrap()
    );
    drop(pressure);
    assert_eq!(pool.used(), used);
    let (finished, context) = source.finished();
    let used = pool.used();
    let pressure = pool.reserve(pool.available()).unwrap();
    assert!(
        observer
            .confirm_terminal_finished(
                &finished,
                context,
                &mut source.input,
                &mut WorkBudget::new(1),
                &RUN
            )
            .unwrap()
    );
    drop(pressure);
    assert_eq!(pool.used(), used);
}

#[test]
fn unfinished_pes_and_unbound_pcr_pid_preserve_strict_refusals() {
    let pool = Budget::new(1 << 20).unwrap();
    let mut data = PARTIAL.to_vec();
    let payload = 376 + 5 + usize::from(data[380]);
    data[payload + 5] = 100;
    let (_, mut observer, packets, fault, context) = tail(&pool, &data);
    assert!(packets.is_empty());
    assert!(observer.hold_terminal_transport(&fault, context).is_err());
    for pcr_pid in [None, Some(257)] {
        let mut source = Source::new(TIMELINE, &pool, 4096, 4096);
        let mut observer = policy(
            &pool,
            PesFaultPolicy {
                source_pcr_pid: pcr_pid,
                ..PesFaultPolicy::default()
            },
        );
        loop {
            let Step::Ready(event) = source.next().unwrap() else {
                panic!("no PCR boundary")
            };
            if let TraceEvent::Transport(packet) = event
                && packet.pid == 256
            {
                let boundary = packet.discontinuity_indicator();
                let pushed = observer.push(packet, source.context());
                if boundary {
                    assert_eq!(
                        pushed.unwrap_err().error,
                        Error::Unsupported("native PCR clock boundary context unobserved")
                    );
                    break;
                }
                pushed.unwrap();
                drain(&mut observer, 1).unwrap();
            }
        }
    }
}

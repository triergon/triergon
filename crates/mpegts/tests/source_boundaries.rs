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
    AssemblyStep, MediaEntityEnd, ObservedTrace, PacketFormat, ParseLimits, SourceEpochs,
    TraceEvent, TransportStream,
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
    fn next_observed(&mut self) -> triergon_core::Result<Step<ObservedTrace<TraceEvent>>> {
        for _ in 0..100_000 {
            match self.stream.poll_observed(
                &mut self.input,
                &mut WorkBudget::new(self.grant),
                &RUN,
            )? {
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
    fn next(&mut self) -> triergon_core::Result<Step<TraceEvent>> {
        Ok(match self.next_observed()? {
            Step::Ready(observed) => Step::Ready(observed.into_event()),
            Step::End(location) => Step::End(location),
            Step::NeedInput(request) => Step::NeedInput(request),
            Step::Yield => Step::Yield,
            Step::Backpressure(never) => match never {},
        })
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
    fn finished(&mut self) -> (ObservedTrace<()>, SourceContext) {
        self.stream.end_of_source().unwrap();
        let Step::Ready(event) = self.next().unwrap() else {
            panic!("no source Finished")
        };
        assert!(matches!(event, TraceEvent::Finished));
        (
            self.stream.source_finished(&self.input).unwrap(),
            self.context(),
        )
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

#[test]
fn recovery_continuation_preserves_active_pes_and_checks_the_original_cursor() {
    let pool = Budget::new(1 << 20).unwrap();
    let idle = pool.used();
    let mut source = Source::new(include_bytes!("data/hls-contiguous-part-a.ts"), &pool, 7, 1);
    let mut observer = policy(&pool, options());
    observer.bind_source_input(&source.input).unwrap();
    let drive_entity = |source: &mut Source, observer: &mut TransportFaultRecovery| {
        let mut packets = Vec::new();
        while let Step::Ready(event) = source.next().unwrap() {
            if let TraceEvent::Transport(packet) = event
                && packet.pid == observer.scope().pid
            {
                observer.push(packet, source.context()).unwrap();
                packets.extend(drain(observer, 1).unwrap());
            }
        }
        packets
    };
    assert!(drive_entity(&mut source, &mut observer).is_empty());
    assert!(!observer.discard_idle());
    let end = source.end();
    let original_token = source.input.token();
    let capabilities = SourceCapabilities {
        length: None,
        random_access: false,
    };
    let mut replacement = InputCursor::new(&pool, SourceId(2), capabilities, 7).unwrap();
    assert!(matches!(
        observer.continue_media_entity(&end, &mut replacement, &mut WorkBudget::new(1), &RUN),
        Err(Error::StaleGeneration { .. })
    ));
    assert!(matches!(
        observer.continue_media_entity(&end, &mut source.input, &mut WorkBudget::new(1), &RUN),
        Err(Error::StaleRequest { .. })
    ));
    source
        .input
        .continue_source(SourceId(2), capabilities)
        .unwrap();
    let used = pool.used();
    let mut short = WorkBudget::new(0);
    assert_eq!(
        observer.continue_media_entity(&end, &mut source.input, &mut short, &CANCEL),
        Err(Error::Cancelled)
    );
    assert_eq!(
        observer.continue_media_entity(&end, &mut source.input, &mut short, &RUN),
        Ok(false)
    );
    assert_eq!(observer.scope().entity, SourceId(1));
    assert_eq!(pool.used(), used);
    assert_eq!(
        observer.continue_media_entity(&end, &mut source.input, &mut WorkBudget::new(1), &RUN),
        Ok(true)
    );
    assert_eq!(observer.scope().entity, SourceId(2));
    assert_eq!(
        source.input.token().generation(),
        original_token.generation()
    );
    assert_eq!(source.input.position(), 0);
    assert!(matches!(
        observer.continue_media_entity(&end, &mut source.input, &mut WorkBudget::new(1), &RUN),
        Err(Error::StaleRequest { .. })
    ));
    source.bytes =
        Bytes::copy_from_slice(&pool, include_bytes!("data/hls-contiguous-part-b.ts")).unwrap();
    let packets = drive_entity(&mut source, &mut observer);
    let [packet] = packets.as_slice() else {
        panic!("expected one complete carried PES");
    };
    assert_eq!(packet.source_origin.source, end.location().source);
    assert_eq!(packet.source_origin.offset, 376);
    assert_eq!(packet.source_epochs, EPOCHS);
    assert_eq!(packet.source_header.pts_raw33, Some(90_000));
    assert_eq!(packet.position, 376);
    assert_eq!(
        packet
            .spans
            .as_slice()
            .iter()
            .map(|span| span.source.entity)
            .collect::<Vec<_>>(),
        [SourceId(1), SourceId(2), SourceId(2)]
    );
    drop((source, observer, end, replacement));
    assert!(
        packet
            .payload()
            .flatten()
            .copied()
            .eq((0_u8..=255).cycle().take(512))
    );
    drop(packets);
    assert_eq!(pool.used(), idle);
}

fn tail(
    pool: &Budget,
    data: &[u8],
) -> (
    Source,
    TransportFaultRecovery,
    Vec<DemuxPacket>,
    ObservedTrace<TraceEvent>,
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
    ObservedTrace<TraceEvent>,
    SourceContext,
) {
    let mut observer = policy(pool, options());
    observer.bind_source_input(&source.input).unwrap();
    let mut packets = Vec::new();
    loop {
        let Step::Ready(observed) = source.next_observed().unwrap() else {
            panic!("no source fault")
        };
        let context = source.context();
        match observed.event() {
            TraceEvent::Transport(packet) if packet.pid == 256 => {
                observer.push(packet.clone(), context).unwrap();
                packets.extend(drain(&mut observer, 1).unwrap());
            }
            TraceEvent::Diagnostic(_) => return (source, observer, packets, observed, context),
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
            assert!(source.stream.source_finished(&source.input).is_err());
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
            let (mut alternate, ..) = tail(&pool, data);
            alternate.end();
            let (foreign_finished, _) = alternate.finished();
            assert!(
                observer
                    .confirm_terminal_finished(
                        &foreign_finished,
                        context,
                        &mut source.input,
                        &mut WorkBudget::new(1),
                        &RUN
                    )
                    .is_err()
            );
            assert!(observer.terminal_confirmation().is_none());
            drop(alternate);
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
            let TraceEvent::Diagnostic(diagnostic) = fault.event() else {
                panic!("expected the staged diagnostic");
            };
            assert_eq!(confirmation.source_fault().error, diagnostic.error);
            assert_eq!(confirmation.source_fault().location, diagnostic.location);
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
fn wrong_context_or_foreign_diagnostic_cannot_select_terminal_completion() {
    let pool = Budget::new(1 << 20).unwrap();
    let (source, mut observer, _, fault, context) = tail(&pool, PARTIAL);
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
    let (_, _, _, other, other_context) = tail(&pool, PARTIAL);
    assert_eq!(
        observer.hold_terminal_transport(&other, other_context),
        Err(Error::Invalid(
            "terminal transport diagnostic source mismatch"
        ))
    );
    let mut empty = policy(&pool, options());
    empty.bind_source_input(&source.input).unwrap();
    assert!(empty.hold_terminal_transport(&fault, context).is_err());
    let mut disabled = policy(&pool, PesFaultPolicy::default());
    disabled.bind_source_input(&source.input).unwrap();
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

#[test]
fn confirmed_timeline_fences_later_carriers_without_consuming_rejected_input() {
    let pool = Budget::new(1 << 20).unwrap();
    let mut source = Source::new(THREE, &pool, 4096, 4096);
    let mut observer = policy(&pool, options());
    let mut confirmed = None;
    loop {
        let Step::Ready(event) = source.next().unwrap() else {
            panic!("no carrier after Timeline");
        };
        let context = source.context();
        match event {
            TraceEvent::Transport(packet) if packet.pid == 256 => {
                if let Some(ordinal) = confirmed {
                    let used = pool.used();
                    for stale in [ordinal - 1, ordinal] {
                        let rejected = observer
                            .push(
                                packet.clone(),
                                SourceContext {
                                    source_event_ordinal: stale,
                                    ..context
                                },
                            )
                            .expect_err("a confirmed source ordinal cannot be reused");
                        assert_eq!(
                            rejected.error,
                            Error::Invalid("native PES source event order")
                        );
                        assert!(rejected.value.source.shares_storage_with(&packet.source));
                        assert_eq!(rejected.value.source_offset, packet.source_offset);
                        assert_eq!(pool.used(), used);
                    }
                    observer.push(packet, context).unwrap();
                    let packets = drain(&mut observer, 1).unwrap();
                    assert_eq!(packets.len(), 1);
                    assert_eq!(packets[0].source_epochs.clock, 1);
                    return;
                }
                observer.push(packet, context).unwrap();
                drain(&mut observer, 1).unwrap();
            }
            TraceEvent::Timeline { .. } => {
                observer.confirm_source_timeline(&event, context).unwrap();
                confirmed = Some(context.source_event_ordinal);
            }
            _ => {}
        }
    }
}

#[test]
fn replacement_cursor_and_end_cannot_hide_the_original_transport_failure() {
    let pool = Budget::new(1 << 20).unwrap();
    let (mut original, mut observer, _, fault, context) = tail(&pool, SYNC);
    observer.hold_terminal_transport(&fault, context).unwrap();
    let original_token = original.input.token();
    original
        .input
        .fail(
            SourceId(1),
            original_token.clone(),
            SourceFailure::Transport,
        )
        .unwrap();
    let (mut replacement, ..) = tail(&pool, SYNC);
    let end = replacement.end();
    let used = pool.used();
    let mut work = WorkBudget::new(1);
    let expected = Error::StaleGeneration {
        expected: original_token.generation(),
        received: replacement.input.generation(),
    };
    assert_eq!(
        observer.seal_terminal_end(&end, &mut replacement.input, &mut work, &RUN),
        Err(expected)
    );
    assert_eq!(work.remaining(), 1);
    assert_eq!(pool.used(), used);
    assert!(observer.terminal_confirmation().is_none());
    let (finished, context) = replacement.finished();
    assert_eq!(
        observer.confirm_terminal_finished(
            &finished,
            context,
            &mut replacement.input,
            &mut WorkBudget::new(1),
            &RUN
        ),
        Err(expected)
    );
    assert!(observer.end_of_source().is_err());
    assert_eq!(
        original
            .input
            .read_into(&mut [0], &mut WorkBudget::new(1), &RUN)
            .unwrap_err(),
        Error::Transport
    );
}

#[test]
fn terminal_input_binding_cannot_be_added_after_carriers_or_replaced() {
    let pool = Budget::new(1 << 20).unwrap();
    let mut source = Source::new(SYNC, &pool, 4096, 4096);
    let mut unbound = policy(&pool, options());
    loop {
        let Step::Ready(observed) = source.next_observed().unwrap() else {
            panic!("no terminal diagnostic");
        };
        let context = source.context();
        match observed.event() {
            TraceEvent::Transport(packet) if packet.pid == 256 => {
                unbound.push(packet.clone(), context).unwrap();
                drain(&mut unbound, 1).unwrap();
                assert_eq!(
                    unbound.bind_source_input(&source.input),
                    Err(Error::Invalid(
                        "source input binding must precede admitted carriers"
                    ))
                );
            }
            TraceEvent::Diagnostic(_) => {
                assert_eq!(
                    unbound.hold_terminal_transport(&observed, context),
                    Err(Error::Unsupported(
                        "terminal transport source input not bound"
                    ))
                );
                break;
            }
            _ => {}
        }
    }
    let (mut original, mut bound, _, fault, context) = tail(&pool, SYNC);
    let (mut replacement, ..) = tail(&pool, SYNC);
    assert!(bound.bind_source_input(&replacement.input).is_err());
    bound.hold_terminal_transport(&fault, context).unwrap();
    let end = original.end();
    assert!(
        bound
            .seal_terminal_end(&end, &mut original.input, &mut WorkBudget::new(1), &RUN)
            .unwrap()
    );
    // Both cursor and witness match each other, but not the bound operation.
    let replacement_end = replacement.end();
    assert!(
        bound
            .seal_terminal_end(
                &replacement_end,
                &mut replacement.input,
                &mut WorkBudget::new(1),
                &RUN
            )
            .is_err()
    );
    assert!(bound.terminal_confirmation().is_none());
}

#[test]
fn terminal_end_rejects_same_cursor_request_changes_and_keeps_cancel_priority() {
    let pool = Budget::new(1 << 20).unwrap();
    let (mut source, mut observer, _, fault, context) = tail(&pool, SYNC);
    observer.hold_terminal_transport(&fault, context).unwrap();
    let old_token = source.input.token();
    let _ = source.end();
    // continue_source keeps the generation but starts a different entity request.
    source
        .input
        .continue_source(
            SourceId(1),
            SourceCapabilities {
                length: None,
                random_access: false,
            },
        )
        .unwrap();
    let (mut replacement, ..) = tail(&pool, SYNC);
    let end = replacement.end();
    let mut work = WorkBudget::new(0);
    assert_eq!(
        observer.seal_terminal_end(&end, &mut source.input, &mut work, &CANCEL),
        Err(Error::Cancelled)
    );
    assert_eq!(
        observer.seal_terminal_end(&end, &mut source.input, &mut work, &RUN),
        Err(Error::StaleRequest {
            expected: old_token.request(),
            received: source.input.token().request(),
        })
    );
    assert!(observer.terminal_confirmation().is_none());
}

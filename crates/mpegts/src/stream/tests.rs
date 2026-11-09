use super::*;
use crate::compat::pes::{DemuxProfile, DemuxScope, DemuxSpan, RegisteredCarriage};
use triergon_core::{InputWindow, SourceCapabilities};

const PID: u16 = 256;
const SOURCE: SourceId = SourceId(1);
const WINDOW: usize = 4096;

fn carrier(counter: u8, start: bool) -> [u8; TS188_BYTES] {
    let mut ts = [counter; TS188_BYTES];
    ts[..4].copy_from_slice(&[0x47, 1 | (u8::from(start) << 6), 0, 0x10 | counter]);
    if start {
        ts[4..13].copy_from_slice(&[0, 0, 1, 0xe0, 0, 0, 0x80, 0, 0]);
    }
    ts
}

fn context(ordinal: u64, epochs: SourceEpochs) -> SourceContext {
    SourceContext {
        source_event_ordinal: ordinal,
        epochs,
        byte_run_epoch: None,
    }
}

fn provide(input: &mut InputCursor, bytes: &[u8]) {
    let request = input.request();
    let bytes = Bytes::copy_from_slice(input.budget(), bytes).unwrap();
    input
        .provide_owned(
            InputWindow::new(request.source, request.token, request.offset, bytes).unwrap(),
        )
        .unwrap();
}

#[test]
fn idle_psi_preserves_cancellation_and_zero_work_fault_delivery() {
    let pool = Budget::new(8192).unwrap();
    let mut input = InputCursor::new(
        &pool,
        SOURCE,
        SourceCapabilities {
            length: None,
            random_access: false,
        },
        WINDOW,
    )
    .unwrap();
    let mut stream =
        TransportStream::new(pool, PacketFormat::Ts188, ParseLimits::default(), 1).unwrap();
    assert!(!stream.collector.has_pending());
    let cancel = AtomicBool::new(true);
    let mut work = WorkBudget::new(0);
    assert!(matches!(
        stream.poll(&mut input, &mut work, &cancel),
        Err(Error::Cancelled)
    ));
    cancel.store(false, Ordering::Relaxed);
    assert!(matches!(
        stream.poll(&mut input, &mut work, &cancel),
        Ok(Step::Yield)
    ));
    stream.fault = Some(TraceFault {
        kind: FaultKind::Transport,
        pid: Some(PID),
        location: SourceLocation {
            source: SOURCE,
            offset: 188,
        },
        error: Error::Truncated,
        rejected: None,
    });
    assert!(matches!(
        stream.poll(&mut input, &mut work, &cancel),
        Ok(Step::Ready(TraceEvent::Diagnostic(TraceFault {
            pid: Some(PID),
            error: Error::Truncated,
            ..
        })))
    ));
    assert_eq!(work.spent(), 0);
    assert_eq!(input.position(), 0);
}

#[test]
fn output_modes_preserve_null_carriers_faults_and_cancelled_partial_input() {
    fn collect(compact: bool, quantum: u64, window: usize) -> (Vec<(u64, u16, u8)>, usize, u64) {
        let pool = Budget::new(64 * 1024).unwrap();
        let mut wire = Vec::new();
        for counter in 0..4 {
            let mut packet = carrier(counter, false);
            packet[1..3].copy_from_slice(&[0x1f, 0xff]);
            if counter == 3 {
                packet[0] = 0;
            }
            wire.extend_from_slice(&packet);
        }
        let mut input = InputCursor::new(
            &pool,
            SOURCE,
            SourceCapabilities {
                length: Some(wire.len() as u64),
                random_access: false,
            },
            WINDOW,
        )
        .unwrap();
        let mut stream =
            TransportStream::new(pool.clone(), PacketFormat::Ts188, ParseLimits::default(), 1)
                .unwrap();
        let mut packets = Vec::new();
        let mut runs = 0;
        let mut spent = 0;
        for _ in 0..10000 {
            // Cancellation must not consume a partially read carrier or a ready run.
            let before = (input.position(), pool.used());
            let mut zero = WorkBudget::new(0);
            let cancelled = if compact {
                stream
                    .poll_compact(&mut input, &mut zero, &AtomicBool::new(true))
                    .map(|_| ())
            } else {
                stream
                    .poll(&mut input, &mut zero, &AtomicBool::new(true))
                    .map(|_| ())
            };
            assert_eq!(cancelled.unwrap_err(), Error::Cancelled);
            assert_eq!((input.position(), pool.used()), before);
            assert_eq!(zero.spent(), 0);
            let mut work = WorkBudget::new(quantum);
            let step = if compact {
                stream.poll_compact(&mut input, &mut work, &AtomicBool::new(false))
            } else {
                stream
                    .poll(&mut input, &mut work, &AtomicBool::new(false))
                    .map(|step| match step {
                        Step::Ready(event) => Step::Ready(CompactTraceEvent::Event(event)),
                        Step::NeedInput(request) => Step::NeedInput(request),
                        Step::Yield => Step::Yield,
                        Step::End(location) => Step::End(location),
                    })
            }
            .unwrap();
            assert!(work.spent() <= quantum);
            spent += work.spent();
            let mut record = |packet: TransportPacket| {
                let offset = packet.source_offset as usize;
                assert_eq!(packet.source.as_ref(), &wire[offset..offset + TS188_BYTES]);
                assert_eq!(packet.source_id, Some(SOURCE));
                assert_eq!(
                    packet.payload.as_ref().unwrap().as_ref(),
                    &wire[offset + 4..offset + TS188_BYTES]
                );
                assert_eq!(packet.payload_source_offset, packet.source_offset + 4);
                assert!(packet.adaptation.is_none());
                assert!(
                    !packet.payload_unit_start && !packet.transport_error && packet.scrambling == 0
                );
                packets.push((packet.source_offset, packet.pid, packet.continuity_counter));
            };
            match step {
                Step::Ready(CompactTraceEvent::Event(TraceEvent::Transport(packet))) => {
                    record(packet);
                }
                Step::Ready(CompactTraceEvent::NullRun(run)) => {
                    runs += 1;
                    for index in 0..run.len() {
                        record(run.packet(index).unwrap());
                    }
                }
                Step::Ready(CompactTraceEvent::Event(TraceEvent::Diagnostic(fault))) => {
                    assert_eq!(fault.kind, FaultKind::Transport);
                    assert_eq!(
                        fault.location,
                        SourceLocation {
                            source: SOURCE,
                            offset: 3 * TS188_BYTES as u64
                        }
                    );
                    assert_eq!(fault.error, Error::Invalid("TS sync byte"));
                    assert_eq!(
                        fault.rejected.unwrap().bytes().as_ref(),
                        &wire[3 * TS188_BYTES..]
                    );
                    return (packets, runs, spent);
                }
                Step::NeedInput(request) => {
                    let offset = request.offset as usize;
                    provide(&mut input, &wire[offset..wire.len().min(offset + window)]);
                }
                Step::Yield => {}
                _ => panic!("unexpected source event before the malformed carrier"),
            }
        }
        panic!("bounded source polling did not reach the malformed carrier")
    }
    let mut counts = Vec::new();
    for (quantum, window) in [(1, 31), (17, 187), (4096, WINDOW)] {
        let (source, runs, source_work) = collect(false, quantum, window);
        assert_eq!(runs, 0);
        let (compact, runs, compact_work) = collect(true, quantum, window);
        assert_eq!(source, [(0, 0x1fff, 0), (188, 0x1fff, 1), (376, 0x1fff, 2)]);
        assert_eq!(compact, source);
        assert_eq!(runs, usize::from(quantum == 4096));
        counts.push((source_work, compact_work));
    }
    // Preserve the prior work charges across short reads and small polls. Only
    // the compact bulk poll can use its existing two-carrier null run.
    assert_eq!(counts, [(771, 771), (771, 771), (771, 761)]);
}

struct PayloadFeed {
    stream: TransportStream,
    input: InputCursor,
    observer: DemuxObserver,
}

impl PayloadFeed {
    fn new(bytes: &[u8]) -> Self {
        Self::with_compact_payloads(bytes, false)
    }

    fn with_compact_payloads(bytes: &[u8], compact: bool) -> Self {
        let pool = Budget::new(64 * 1024).unwrap();
        let limits = ParseLimits::default();
        let mut stream = TransportStream::new_single_program_progressive_feed(
            pool.clone(),
            limits,
            DescriptorNamespace::Unspecified,
        )
        .unwrap();
        // Enter the selected AVC route after program discovery.
        stream.program = 1;
        stream.pcr_pid = Some(PID);
        let epochs = stream.source_epochs();
        let mut input = InputCursor::new(
            &pool,
            SOURCE,
            SourceCapabilities {
                length: None,
                random_access: false,
            },
            WINDOW,
        )
        .unwrap();
        provide(&mut input, bytes);
        let cancel = AtomicBool::new(false);
        let mut work = WorkBudget::new(4096);
        let Step::Ready(FramedCarrier::Packet(start)) = stream
            .framer
            .poll_carrier::<true>(&mut input, &mut work, &cancel)
            .unwrap()
        else {
            panic!("expected a PES start");
        };
        let mut continuity = Continuity::new(PID).unwrap();
        assert_eq!(
            continuity.observe(&start).unwrap(),
            ContinuityOutcome::First
        );
        let mut assembler = PesAssembler::new(PID, true, pool.clone(), limits, epochs).unwrap();
        assembler
            .push(
                start.payload.as_ref().unwrap().clone(),
                true,
                SOURCE,
                start.payload_source_offset,
            )
            .unwrap();
        assert!(matches!(
            assembler.poll_source::<false>(&mut work, &cancel).unwrap(),
            AssemblyStep::NeedPayload
        ));
        let mut observer = DemuxObserver::new_registered(
            DemuxProfile::PesPackets,
            DemuxScope {
                program: 1,
                pid: PID,
                stream_type: 0x1b,
                stream_id: 0xe0,
                entity: SOURCE,
                epochs,
            },
            RegisteredCarriage::Avc,
            pool.clone(),
            limits,
        )
        .unwrap();
        if compact {
            observer.enable_compact_payloads().unwrap();
        }
        observer.push(start, context(0, epochs)).unwrap();
        let needs_payload = if compact {
            matches!(
                observer.poll_compact_payload(&mut work, &cancel).unwrap(),
                AssemblyStep::NeedPayload
            )
        } else {
            matches!(
                observer.poll(&mut work, &cancel).unwrap(),
                AssemblyStep::NeedPayload
            )
        };
        assert!(needs_payload);
        stream
            .routes
            .try_push(PesRoute {
                pid: PID,
                stream_type: 0x1b,
                descriptors: Bytes::copy_from_slice(&pool, &[]).unwrap(),
                epochs,
                continuity: Some(continuity),
                assembler: Some(assembler),
                reuse: None,
            })
            .unwrap();
        stream.route = Route::Pes(0);
        Self {
            stream,
            input,
            observer,
        }
    }

    fn try_run(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<bool> {
        let observer = &mut self.observer;
        self.stream.try_feed_payload_run(
            &mut self.input,
            work,
            cancel,
            &mut |item, omitted, epochs, work| {
                let FeedItem::PayloadRun(run) = item else {
                    panic!("expected a run probe");
                };
                assert_eq!(run.pid(), PID);
                Ok(
                    if run.append_ready(observer, context(2 + omitted, epochs), usize::MAX, work)? {
                        FeedContinuation::Buffered
                    } else {
                        FeedContinuation::Emit
                    },
                )
            },
        )
    }

    fn ordinary(&mut self, work: &mut WorkBudget) -> Step<TraceEvent> {
        self.stream
            .poll_feed_carriers(
                &mut self.input,
                work,
                &AtomicBool::new(false),
                &mut |_, _, _, _| Ok(FeedContinuation::Emit),
            )
            .unwrap()
    }

    fn finish_native(&mut self) -> crate::compat::pes::DemuxPacket {
        self.observer.end_of_source().unwrap();
        let AssemblyStep::Output(packet) = self
            .observer
            .poll(&mut WorkBudget::new(4096), &AtomicBool::new(false))
            .unwrap()
        else {
            panic!("expected the unbounded PES");
        };
        packet
    }

    fn finish_compact(&mut self) -> crate::compat::pes::CompactDemuxPacket {
        self.observer.end_of_source().unwrap();
        let AssemblyStep::Output(packet) = self
            .observer
            .poll_compact_payload(&mut WorkBudget::new(4096), &AtomicBool::new(false))
            .unwrap()
        else {
            panic!("expected compact PES output");
        };
        packet
    }
}

#[test]
fn mixed_payload_polling_rejects_before_work_or_state_changes() {
    let wire = [carrier(14, true), carrier(15, false), carrier(0, false)].concat();
    let cancel = AtomicBool::new(false);
    for compact in [false, true] {
        let mut feed = PayloadFeed::with_compact_payloads(&wire, compact);
        let before = format!("{:?}", feed.observer);
        let used = feed.input.budget().used();
        let mut work = WorkBudget::new(1000);
        for bound in [false, true] {
            let error = if compact {
                if bound {
                    feed.observer
                        .poll_with_input(&feed.input, &mut work, &cancel)
                } else {
                    feed.observer.poll(&mut work, &cancel)
                }
                .unwrap_err()
            } else {
                if bound {
                    feed.observer
                        .poll_compact_payload_with_input(&feed.input, &mut work, &cancel)
                } else {
                    feed.observer.poll_compact_payload(&mut work, &cancel)
                }
                .unwrap_err()
            };
            assert_eq!(error, Error::Invalid("PES payload polling representation"));
            assert_eq!(work.remaining(), 1000);
            assert_eq!(feed.input.budget().used(), used);
            assert_eq!(format!("{:?}", feed.observer), before);
        }
        assert!(feed.try_run(&mut work, &cancel).unwrap());
        if compact {
            assert_eq!(feed.finish_compact().payload.len(), 3);
        } else {
            assert_eq!(feed.finish_native().spans.len(), 3);
        }
    }
}

#[test]
fn compact_run_keeps_owned_records_work_and_charges_without_carrier_handles() {
    let wire = [carrier(14, true), carrier(15, false), carrier(0, false)].concat();
    let mut owned = PayloadFeed::new(&wire);
    let mut compact = PayloadFeed::with_compact_payloads(&wire, true);
    let mut first = WorkBudget::new(2 * PAYLOAD_RUN_WORK + 1);
    let mut second = WorkBudget::new(2 * PAYLOAD_RUN_WORK + 1);
    assert!(owned.try_run(&mut first, &AtomicBool::new(false)).unwrap());
    assert!(
        compact
            .try_run(&mut second, &AtomicBool::new(false))
            .unwrap()
    );
    assert_eq!(first.remaining(), second.remaining());
    assert_eq!(owned.stream.feed_omissions, compact.stream.feed_omissions);
    assert_eq!(owned.input.position(), compact.input.position());
    assert_eq!(owned.input.budget().used(), compact.input.budget().used());
    let ordinary = owned.finish_native();
    let packet = compact.finish_compact();
    assert_eq!(ordinary.spans.len(), 3);
    assert_eq!(
        packet.payload.retained_handles(),
        if cfg!(target_pointer_width = "64") {
            2
        } else {
            3
        }
    );
    assert_eq!(ordinary.payload_len, packet.payload_len);
    assert_eq!(ordinary.source_origin, packet.source_origin);
    assert_eq!(ordinary.source_epochs, packet.source_epochs);
    assert_eq!(ordinary.flags, packet.flags);
    assert_eq!(ordinary.boundary, packet.boundary);
    assert_eq!(ordinary.payload_kind, packet.payload_kind);
    assert_eq!(
        format!("{:?}", ordinary.source_header),
        format!("{:?}", packet.source_header)
    );
    assert_eq!(
        ordinary.header_source_ranges.as_slice(),
        packet.header_source_ranges.as_slice()
    );
    assert_eq!(ordinary.spans.len(), packet.payload.len());
    assert_eq!(ordinary.spans.capacity(), packet.payload.capacity());
    for (expected, span) in ordinary.spans.as_slice().iter().zip(packet.payload.spans()) {
        assert_eq!(expected.as_ref(), span.as_ref());
        assert_eq!(expected.source, span.source);
        assert_eq!(expected.transport, span.transport);
        assert_eq!(expected.context, span.context);
    }
    let pool = compact.input.budget().clone();
    let source_charge = packet
        .payload
        .spans()
        .next()
        .unwrap()
        .bytes
        .backing_bytes()
        .retained_storage_bytes();
    let capacity = packet.payload.capacity();
    let header_capacity = packet.header_source_ranges.capacity();
    drop(compact);
    assert_eq!(
        pool.used(),
        Budget::control_storage_bytes()
            + source_charge
            + capacity * size_of::<DemuxSpan>()
            + header_capacity * size_of::<crate::SourceRange>()
    );
    drop(packet);
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}

#[test]
fn retained_continuation_keeps_the_grant_and_original_storage() {
    let pool = Budget::new(8192).unwrap();
    let limits = ParseLimits::default();
    let cancel = AtomicBool::new(false);
    let mut assembler =
        PesAssembler::new(PID, true, pool.clone(), limits, SourceEpochs::default()).unwrap();
    let start = Bytes::copy_from_slice(&pool, &[0, 0, 1, 0xe0, 0, 6, 0x80, 0, 0, 42]).unwrap();
    assembler.push(start, true, SOURCE, 4).unwrap();
    assert!(matches!(
        assembler.poll(&mut WorkBudget::new(64), &cancel).unwrap(),
        AssemblyStep::NeedPayload
    ));

    let mut body = Some(Bytes::copy_from_slice(&pool, &[43]).unwrap());
    let address = body.as_ref().unwrap().storage_address();
    let location = SourceLocation {
        source: SOURCE,
        offset: 200,
    };
    let before = pool.used();
    let mut work = WorkBudget::new(0);
    assert!(!assembler.retain_body_carrier(&mut body, location, &mut work));
    assert_eq!(body.as_ref().unwrap().storage_address(), address);
    let mut work = WorkBudget::new(1);
    assert!(assembler.retain_body_carrier(&mut body, location, &mut work));
    assert_eq!(work.spent(), 1);
    assert_eq!(pool.used(), before);
    assert!(body.is_none());

    let mut last = Some(Bytes::copy_from_slice(&pool, &[44]).unwrap());
    let location = SourceLocation {
        offset: 392,
        ..location
    };
    let mut work = WorkBudget::new(1);
    assert!(!assembler.retain_body_carrier(&mut last, location, &mut work));
    assert_eq!(work.spent(), 0);
    assembler
        .push(last.unwrap(), false, SOURCE, location.offset)
        .unwrap();
    let AssemblyStep::Output(pes) = assembler.poll(&mut work, &cancel).unwrap() else {
        panic!("expected the complete finite PES");
    };
    assert_eq!(
        pes.payload().flatten().copied().collect::<Vec<_>>(),
        [42, 43, 44]
    );
    assert_eq!(
        pes.source_ranges()
            .map(|span| span.offset)
            .collect::<Vec<_>>(),
        [13, 200, 392]
    );
    assert_eq!(pes.spans()[1].backing_bytes().storage_address(), address);
}

#[test]
fn rejected_pes_payload_stays_available_for_a_drained_retry() {
    let mut feed = PayloadFeed::new(&carrier(0, true));
    let pool = feed.input.budget().clone();
    let limits = ParseLimits::default();
    let blocker = Bytes::copy_from_slice(&pool, &[2; 4]).unwrap();
    feed.stream.routes.as_mut_slice()[0]
        .assembler
        .as_mut()
        .unwrap()
        .push(blocker, false, SOURCE, 188)
        .unwrap();
    let mut packet = crate::parse_transport(
        Bytes::copy_from_slice(&pool, &carrier(1, false)).unwrap(),
        PacketFormat::Ts188,
        376,
    )
    .unwrap();
    packet.source_id = Some(SOURCE);
    let address = packet.payload.as_ref().unwrap().storage_address();
    let expected = packet.payload.as_ref().unwrap().as_ref().to_vec();
    feed.stream.packet = Some(packet);
    feed.stream.phase = 5;
    let mut work = WorkBudget::new(limits.pes_bytes as u64);
    assert_eq!(
        feed.stream.feed(&mut work),
        Err(Error::Invalid("PES payload not drained"))
    );
    let payload = feed
        .stream
        .packet
        .as_ref()
        .unwrap()
        .payload
        .as_ref()
        .unwrap();
    assert_eq!(payload.storage_address(), address);
    assert_eq!(payload.as_ref(), expected);
    assert_eq!(feed.stream.phase, 5);
    assert!(matches!(
        feed.stream.routes.as_mut_slice()[0]
            .assembler
            .as_mut()
            .unwrap()
            .poll_source::<false>(&mut work, &AtomicBool::new(false))
            .unwrap(),
        AssemblyStep::NeedPayload
    ));
    feed.stream.feed(&mut work).unwrap();
    assert_eq!(feed.stream.phase, 6);
    assert!(feed.stream.packet.as_ref().unwrap().payload.is_none());
}

#[test]
fn invalid_psi_pointer_restores_payload_before_diagnostic_recovery() {
    let pool = Budget::new(8192).unwrap();
    let mut stream =
        TransportStream::new(pool.clone(), PacketFormat::Ts188, ParseLimits::default(), 1).unwrap();
    let mut raw = carrier(0, true);
    raw[1] = 0x40;
    raw[4] = 0xff;
    let mut packet = crate::parse_transport(
        Bytes::copy_from_slice(&pool, &raw).unwrap(),
        PacketFormat::Ts188,
        0,
    )
    .unwrap();
    packet.source_id = Some(SOURCE);
    let address = packet.payload.as_ref().unwrap().storage_address();
    stream.packet = Some(packet);
    stream.route = Route::Pat;
    stream.phase = 5;
    stream.feed(&mut WorkBudget::new(1)).unwrap();
    assert_eq!(stream.phase, 7);
    assert_eq!(
        stream.fault.as_ref().unwrap().error,
        Error::Invalid("PSI pointer")
    );
    let payload = stream.packet.as_ref().unwrap().payload.as_ref().unwrap();
    assert_eq!(payload.storage_address(), address);
    assert_eq!(payload.as_ref(), &raw[4..]);
}

#[test]
fn payload_run_preserves_work_boundary_and_span_provenance() {
    let wire = [carrier(14, true), carrier(15, false), carrier(0, false)].concat();
    let cancel = AtomicBool::new(false);
    let mut single = PayloadFeed::new(&wire);
    let mut work = WorkBudget::new(2 * PAYLOAD_RUN_WORK);
    assert!(!single.try_run(&mut work, &cancel).unwrap());
    assert_eq!(single.input.position(), TS188_BYTES as u64);
    assert_eq!(work.spent(), 0);
    let observer = &mut single.observer;
    let step = single
        .stream
        .poll_feed_carriers(
            &mut single.input,
            &mut work,
            &cancel,
            &mut |carrier, omitted, epochs, work| {
                assert_eq!(
                    carrier.append_ready_continuation(
                        observer,
                        context(2 + omitted, epochs),
                        work,
                        &cancel
                    )?,
                    ReadyContinuation::Buffered
                );
                assert!(work.try_charge(1));
                Ok(if work.remaining() == 0 {
                    FeedContinuation::Yield
                } else {
                    FeedContinuation::Buffered
                })
            },
        )
        .unwrap();
    assert!(matches!(step, Step::Yield));
    assert_eq!(work.remaining(), 0);
    assert_eq!(single.stream.feed_omissions, Some(3));
    assert_eq!(single.stream.deferred_feed_events, 1);

    let mut batched = PayloadFeed::new(&wire);
    let before = batched.input.budget().used();
    let mut work = WorkBudget::new(2 * PAYLOAD_RUN_WORK + 1);
    assert!(batched.try_run(&mut work, &cancel).unwrap());
    assert_eq!(work.remaining(), 1);
    assert_eq!(batched.stream.take_feed_omissions(), 4);
    assert_eq!(batched.stream.deferred_feed_events, 0);
    assert_eq!(batched.input.position(), single.input.position());
    assert_eq!(batched.input.budget().used(), before);

    let ordinary = single.finish_native();
    let run = batched.finish_native();
    assert_eq!(run.payload_len, ordinary.payload_len);
    assert_eq!(run.spans.len(), 3);
    assert_eq!(run.spans.len(), ordinary.spans.len());
    for (span, expected) in run.spans.as_slice().iter().zip(ordinary.spans.as_slice()) {
        assert_eq!(span.as_ref(), expected.as_ref());
        assert_eq!(span.source, expected.source);
        assert_eq!(span.transport, expected.transport);
        assert_eq!(span.context, expected.context);
        assert!(
            span.backing_bytes()
                .shares_storage_with(run.spans.as_slice()[0].backing_bytes())
        );
    }
}

const TWO_AFTER_GROWTH: u64 = (4 + 8) * size_of::<DemuxSpan>() as u64 + 2 * PAYLOAD_RUN_WORK;

fn full_payload_arena() -> PayloadFeed {
    full_payload_arena_with_format(false)
}

fn full_payload_arena_with_format(compact: bool) -> PayloadFeed {
    let mut start = carrier(0, true);
    start[11..23].copy_from_slice(&[0xc0, 10, 0x31, 0, 1, 0, 7, 0x11, 0, 1, 0, 3]);
    let wire = [
        start,
        carrier(1, false),
        carrier(2, false),
        carrier(3, false),
        carrier(4, false),
        carrier(5, false),
    ]
    .concat();
    let mut feed = PayloadFeed::with_compact_payloads(&wire, compact);
    let mut work = WorkBudget::new(3 * PAYLOAD_RUN_WORK + 1);
    assert!(feed.try_run(&mut work, &AtomicBool::new(false)).unwrap());
    assert_eq!(feed.input.position(), (4 * TS188_BYTES) as u64);
    feed
}

#[test]
fn compact_growth_matches_owned_admission_work_and_every_carrier() {
    let cancel = AtomicBool::new(false);
    let mut owned = full_payload_arena();
    let mut compact = full_payload_arena_with_format(true);
    for feed in [&mut owned, &mut compact] {
        let pool = feed.input.budget().clone();
        let position = feed.input.position();
        let used = pool.used();
        let mut work = WorkBudget::new(TWO_AFTER_GROWTH);
        assert!(!feed.try_run(&mut work, &cancel).unwrap());
        assert_eq!(work.spent(), 0);
        let pressure = pool.reserve(pool.available()).unwrap();
        let mut work = WorkBudget::new(TWO_AFTER_GROWTH + 1);
        assert!(!feed.try_run(&mut work, &cancel).unwrap());
        drop(pressure);
        assert_eq!(
            feed.try_run(&mut work, &AtomicBool::new(true)),
            Err(Error::Cancelled)
        );
        assert_eq!(work.spent(), 0);
        assert_eq!(pool.used(), used);
        assert_eq!(feed.input.position(), position);
        assert!(feed.try_run(&mut work, &cancel).unwrap());
        assert_eq!(work.remaining(), 1);
    }
    assert_eq!(owned.input.budget().used(), compact.input.budget().used());
    assert_eq!(owned.stream.feed_omissions, compact.stream.feed_omissions);
    let owned = owned.finish_native();
    let compact = compact.finish_compact();
    assert_eq!(owned.payload_len, compact.payload_len);
    assert_eq!(owned.spans.len(), compact.payload.len());
    assert_eq!(owned.spans.capacity(), compact.payload.capacity());
    assert_eq!(
        compact.payload.retained_handles(),
        if cfg!(target_pointer_width = "64") {
            3
        } else {
            6
        }
    );
    for (span, expected) in compact.payload.spans().zip(owned.spans.as_slice()) {
        assert_eq!(span.as_ref(), expected.as_ref());
        assert_eq!(span.source, expected.source);
        assert_eq!(span.transport, expected.transport);
        assert_eq!(span.context, expected.context);
    }
}

#[test]
fn payload_run_pays_for_full_arena_growth_and_keeps_span_provenance() {
    let mut serial = full_payload_arena();
    let cancel = AtomicBool::new(false);
    let mut work = WorkBudget::new(TWO_AFTER_GROWTH + 1);
    for ordinal in [8, 10] {
        let Step::Ready(TraceEvent::Transport(packet)) = serial.ordinary(&mut work) else {
            panic!("expected a continuation");
        };
        assert!(work.try_charge(1));
        serial
            .observer
            .push(packet, context(ordinal, serial.stream.source_epochs()))
            .unwrap();
        assert!(matches!(
            serial.observer.poll(&mut work, &cancel).unwrap(),
            AssemblyStep::NeedPayload
        ));
    }
    assert_eq!(work.remaining(), 1);

    let mut batched = full_payload_arena();
    let mut work = WorkBudget::new(TWO_AFTER_GROWTH + 1);
    assert!(batched.try_run(&mut work, &cancel).unwrap());
    assert_eq!(work.remaining(), 1);
    assert_eq!(batched.stream.take_feed_omissions(), 10);
    assert_eq!(batched.input.position(), serial.input.position());
    assert_eq!(batched.input.budget().used(), serial.input.budget().used());

    let serial = serial.finish_native();
    let batched = batched.finish_native();
    assert_eq!(batched.payload_len, serial.payload_len);
    for packet in [&serial, &batched] {
        let header = packet.time_header().unwrap();
        assert_eq!(header.pts_raw33, Some(3));
        assert_eq!(header.dts_raw33, Some(1));
    }
    assert_eq!(batched.spans.len(), 6);
    assert_eq!(batched.spans.capacity(), serial.spans.capacity());
    for (span, expected) in batched.spans.as_slice().iter().zip(serial.spans.as_slice()) {
        assert_eq!(span.as_ref(), expected.as_ref());
        assert_eq!(span.source, expected.source);
        assert_eq!(span.transport, expected.transport);
        assert_eq!(span.context, expected.context);
    }
}

#[test]
fn payload_run_growth_leaves_input_intact_when_work_or_memory_is_tight() {
    let mut feed = full_payload_arena();
    let position = feed.input.position();
    let pool = feed.input.budget().clone();
    let before = pool.used();
    let cancel = AtomicBool::new(false);
    let mut tight = WorkBudget::new(TWO_AFTER_GROWTH);
    assert!(!feed.try_run(&mut tight, &cancel).unwrap());
    assert_eq!(tight.spent(), 0);

    let mut work = WorkBudget::new(TWO_AFTER_GROWTH + 1);
    let hold = pool.reserve(pool.available()).unwrap();
    assert!(!feed.try_run(&mut work, &cancel).unwrap());
    drop(hold);
    assert_eq!(work.spent(), 0);
    assert_eq!(pool.used(), before);
    assert_eq!(feed.input.position(), position);
    assert_eq!(feed.stream.feed_omissions, Some(6));

    assert_eq!(
        feed.try_run(&mut work, &AtomicBool::new(true)),
        Err(Error::Cancelled)
    );
    assert_eq!(pool.used(), before);
    assert_eq!(work.spent(), 0);
    assert_eq!(feed.input.position(), position);
    assert!(feed.try_run(&mut work, &cancel).unwrap());
    assert_eq!(work.remaining(), 1);
}

#[test]
fn payload_run_leaves_a_split_carrier_for_refill() {
    let wire = [
        carrier(0, true),
        carrier(1, false),
        carrier(2, false),
        carrier(3, false),
    ]
    .concat();
    let split = 3 * TS188_BYTES + 17;
    let mut feed = PayloadFeed::new(&wire[..split]);
    let mut work = WorkBudget::new(4096);
    assert!(feed.try_run(&mut work, &AtomicBool::new(false)).unwrap());
    assert_eq!(feed.input.position(), (3 * TS188_BYTES) as u64);
    let Step::NeedInput(request) = feed.ordinary(&mut work) else {
        panic!("expected the missing carrier suffix");
    };
    assert_eq!(request.offset, split as u64);
    provide(&mut feed.input, &wire[split..]);
    let Step::Ready(TraceEvent::Transport(packet)) = feed.ordinary(&mut work) else {
        panic!("expected the reassembled carrier");
    };
    assert_eq!(packet.source_offset, (3 * TS188_BYTES) as u64);
    assert_eq!(packet.source.as_ref(), &wire[3 * TS188_BYTES..]);
}

#[test]
fn payload_run_keeps_the_literal_duplicate_snapshot() {
    for (first, compact) in [(0, false), (14, false), (0, true), (14, true)] {
        let mut last = carrier((first + 2) & 15, false);
        if first == 14 {
            last[1] |= 0x20;
            last[TS188_BYTES - 1] = 0xa5;
        }
        let wire = [
            carrier(first, true),
            carrier(first + 1, false),
            last,
            last,
            last,
        ]
        .concat();
        let mut feed = PayloadFeed::with_compact_payloads(&wire, compact);
        let mut work = WorkBudget::new(4096);
        assert!(feed.try_run(&mut work, &AtomicBool::new(false)).unwrap());
        assert_eq!(feed.stream.take_feed_omissions(), 4);
        let Step::Ready(TraceEvent::Diagnostic(fault)) = feed.ordinary(&mut work) else {
            panic!("expected the excess duplicate diagnostic");
        };
        assert_eq!(fault.error, Error::Invalid("TS excess duplicate"));
        assert_eq!(fault.location.offset, (4 * TS188_BYTES) as u64);
    }
}

#[test]
fn payload_run_storage_remains_charged_until_the_retained_packet_drops() {
    let wire = [carrier(14, true), carrier(15, false), carrier(0, false)].concat();
    let mut feed = PayloadFeed::new(&wire);
    let pool = feed.input.budget().clone();
    let mut work = WorkBudget::new(2 * PAYLOAD_RUN_WORK + 1);
    assert!(feed.try_run(&mut work, &AtomicBool::new(false)).unwrap());
    assert_eq!(work.remaining(), 1);
    let packet = feed.finish_native();
    drop(feed);
    assert_eq!(packet.spans.len(), 3);
    assert_eq!(
        pool.used(),
        Budget::control_storage_bytes()
            + packet.spans.capacity() * size_of::<DemuxSpan>()
            + packet.header_source_ranges.capacity() * size_of::<crate::SourceRange>()
            + packet.spans.as_slice()[0]
                .backing_bytes()
                .retained_storage_bytes()
    );
    for (index, span) in packet.spans.as_slice()[1..].iter().enumerate() {
        let at = (index + 1) * TS188_BYTES;
        assert_eq!(span.as_ref(), &wire[at + 4..at + TS188_BYTES]);
        assert!(
            span.backing_bytes()
                .shares_storage_with(packet.spans.as_slice()[0].backing_bytes())
        );
    }
    drop(packet);
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}

#[test]
fn payload_run_stops_at_a_start_or_clock_event() {
    let mut pcr = carrier(3, false);
    pcr[3] = 0x33;
    pcr[4..12].copy_from_slice(&[7, 0x90, 0, 0, 0, 0, 0x7e, 0]);
    for (boundary, has_pcr) in [(carrier(3, true), false), (pcr, true)] {
        let wire = [
            carrier(0, true),
            carrier(1, false),
            carrier(2, false),
            boundary,
        ]
        .concat();
        let mut feed = PayloadFeed::new(&wire);
        let mut work = WorkBudget::new(4096);
        assert!(feed.try_run(&mut work, &AtomicBool::new(false)).unwrap());
        assert_eq!(feed.input.position(), (3 * TS188_BYTES) as u64);
        let Step::Ready(TraceEvent::Transport(packet)) = feed.ordinary(&mut work) else {
            panic!("expected the original boundary carrier");
        };
        assert_eq!(packet.source.as_ref(), boundary);
        if has_pcr {
            assert!(matches!(
                feed.ordinary(&mut work),
                Step::Ready(TraceEvent::Timeline { clock_epoch: 1, .. })
            ));
        } else {
            assert!(packet.payload_unit_start);
        }
    }
}

#[test]
fn payload_run_declines_when_only_one_span_fits_or_cancelled() {
    let wire = [
        carrier(0, true),
        carrier(1, false),
        carrier(2, false),
        carrier(3, false),
        carrier(4, false),
    ]
    .concat();
    let mut feed = PayloadFeed::new(&wire);
    let mut work = WorkBudget::new(4096);
    let before = feed.input.budget().used();
    assert_eq!(
        feed.try_run(&mut work, &AtomicBool::new(true)),
        Err(Error::Cancelled)
    );
    assert_eq!(work.spent(), 0);
    assert_eq!(feed.input.position(), TS188_BYTES as u64);
    assert_eq!(feed.stream.take_feed_omissions(), 0);
    assert_eq!(feed.input.budget().used(), before);

    let cancel = AtomicBool::new(false);
    assert!(
        feed.try_run(&mut WorkBudget::new(2 * PAYLOAD_RUN_WORK + 1), &cancel)
            .unwrap()
    );
    assert_eq!(feed.stream.take_feed_omissions(), 4);
    let position = feed.input.position();
    assert!(!feed.try_run(&mut work, &cancel).unwrap());
    assert_eq!(work.spent(), 0);
    assert_eq!(feed.input.position(), position);
    assert_eq!(feed.stream.take_feed_omissions(), 0);
    assert_eq!(feed.input.budget().used(), before);
    assert!(matches!(
        feed.ordinary(&mut work),
        Step::Ready(TraceEvent::Transport(packet)) if packet.source_offset == position
    ));
}

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

struct PayloadFeed {
    stream: TransportStream,
    input: InputCursor,
    observer: DemuxObserver,
}

impl PayloadFeed {
    fn new(bytes: &[u8]) -> Self {
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
        observer.push(start, context(0, epochs)).unwrap();
        assert!(matches!(
            observer.poll(&mut work, &cancel).unwrap(),
            AssemblyStep::NeedPayload
        ));
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
    let mut feed = PayloadFeed::new(&wire);
    let mut work = WorkBudget::new(3 * PAYLOAD_RUN_WORK + 1);
    assert!(feed.try_run(&mut work, &AtomicBool::new(false)).unwrap());
    assert_eq!(feed.input.position(), (4 * TS188_BYTES) as u64);
    feed
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
    let last = carrier(2, false);
    let wire = [carrier(0, true), carrier(1, false), last, last, last].concat();
    let mut feed = PayloadFeed::new(&wire);
    let mut work = WorkBudget::new(4096);
    assert!(feed.try_run(&mut work, &AtomicBool::new(false)).unwrap());
    assert_eq!(feed.stream.take_feed_omissions(), 4);
    let Step::Ready(TraceEvent::Diagnostic(fault)) = feed.ordinary(&mut work) else {
        panic!("expected the excess duplicate diagnostic");
    };
    assert_eq!(fault.error, Error::Invalid("TS excess duplicate"));
    assert_eq!(fault.location.offset, (4 * TS188_BYTES) as u64);
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

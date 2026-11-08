//! Reported discards and real original-cursor EOF, independently of NoMedia.
use std::sync::atomic::AtomicBool;
use triergon_core::{
    Budget, Bytes, Error, InputCursor, InputWindow, SourceCapabilities, SourceFailure, SourceId,
    Step, WorkBudget,
};
use triergon_mpegts::compat::pes::{
    DemuxObserver, DemuxPacket, DemuxProfile, DemuxScope, MalformedInputCompletion,
    MalformedInputFact, MalformedInputKind, MalformedSourceDisposition, NoPacketsConfirmation,
    SourceContext, TransportFaultRecovery, TransportObservation,
};
use triergon_mpegts::{
    AssemblyStep, MediaEntityEnd, PacketFormat, ParseLimits, TraceEvent, TransportStream,
};

static RUN: AtomicBool = AtomicBool::new(false);
static CANCEL: AtomicBool = AtomicBool::new(true);
const CASES: [(&[u8], MalformedInputKind); 4] = [
    (
        include_bytes!("data/transport-afc-reserved.ts"),
        MalformedInputKind::ReservedAdaptationControl,
    ),
    (
        include_bytes!("data/transport-adaptation-overrun.ts"),
        MalformedInputKind::AdaptationLengthOverrun,
    ),
    (
        include_bytes!("data/pes-optional-header-overrun.ts"),
        MalformedInputKind::PesHeaderLength,
    ),
    (
        include_bytes!("data/pes-start-prefix-invalid.ts"),
        MalformedInputKind::PesStartCode,
    ),
];
const VALID: &[u8] = include_bytes!("data/ts188-basic.ts");

struct Run {
    raw: Bytes,
    input: InputCursor,
    source: TransportStream,
    completion: MalformedInputCompletion,
    observer: Option<TransportFaultRecovery>,
    ordinal: u64,
    facts: Vec<MalformedInputFact>,
    packets: Vec<DemuxPacket>,
    source_pes: usize,
    diagnostics: usize,
    chunk: usize,
    end: Option<MediaEntityEnd>,
    finished: bool,
    omit_carrier: Option<u64>,
}
impl Run {
    fn new(raw: &[u8], pool: &Budget, chunk: usize) -> Self {
        let input = InputCursor::new(
            pool,
            SourceId(1),
            SourceCapabilities {
                length: None,
                random_access: true,
            },
            4096,
        )
        .unwrap();
        let completion =
            MalformedInputCompletion::new(&input, PacketFormat::Ts188, ParseLimits::default(), 1)
                .unwrap();
        Self {
            raw: Bytes::copy_from_slice(pool, raw).unwrap(),
            input,
            completion,
            source: TransportStream::new(
                pool.clone(),
                PacketFormat::Ts188,
                ParseLimits::default(),
                1,
            )
            .unwrap(),
            observer: None,
            ordinal: 0,
            facts: Vec::new(),
            packets: Vec::new(),
            source_pes: 0,
            diagnostics: 0,
            chunk,
            end: None,
            finished: false,
            omit_carrier: None,
        }
    }
    fn drain(&mut self) {
        let Some(observer) = self.observer.as_mut() else {
            return;
        };
        for _ in 0..100_000 {
            match observer.poll(&mut WorkBudget::new(1), &RUN).unwrap() {
                AssemblyStep::Output(TransportObservation::Accepted(packet)) => {
                    self.packets.push(packet);
                }
                AssemblyStep::Yield => (),
                AssemblyStep::NeedPayload | AssemblyStep::Finished => return,
                other @ AssemblyStep::Output(_) => panic!("unexpected extraction {other:?}"),
            }
        }
        panic!("observer did not drain");
    }
    fn next(&mut self, pool: &Budget, pressure: bool) {
        for _ in 0..100_000 {
            match self
                .source
                .poll(&mut self.input, &mut WorkBudget::new(1), &RUN)
                .unwrap()
            {
                Step::NeedInput(request) => {
                    let at = usize::try_from(request.offset).unwrap();
                    if at == self.raw.len() {
                        self.input.finish(request.source, request.token).unwrap();
                    } else {
                        let end = self.raw.len().min(at + self.chunk.min(request.maximum));
                        self.input
                            .provide_owned(
                                InputWindow::new(
                                    request.source,
                                    request.token,
                                    request.offset,
                                    self.raw.slice(at..end).unwrap(),
                                )
                                .unwrap(),
                            )
                            .unwrap();
                    }
                }
                Step::Yield => (),
                Step::End(_) => {
                    let end = self
                        .source
                        .media_entity_end(&mut self.input, &mut WorkBudget::new(2), &RUN)
                        .unwrap()
                        .unwrap();
                    assert!(
                        !self
                            .completion
                            .seal_physical_end(&end, &mut self.input, &mut WorkBudget::new(0), &RUN)
                            .unwrap()
                    );
                    assert_eq!(
                        self.completion
                            .seal_physical_end(
                                &end,
                                &mut self.input,
                                &mut WorkBudget::new(0),
                                &CANCEL
                            )
                            .unwrap_err(),
                        Error::Cancelled
                    );
                    let blocker = pressure.then(|| pool.reserve(pool.available()).unwrap());
                    assert!(
                        self.completion
                            .seal_physical_end(&end, &mut self.input, &mut WorkBudget::new(1), &RUN)
                            .unwrap()
                    );
                    drop(blocker);
                    self.end = Some(end);
                    self.source.end_of_source().unwrap();
                }
                Step::Ready(event) => {
                    let context = SourceContext {
                        source_event_ordinal: self.ordinal,
                        epochs: self.source.source_epochs(),
                        byte_run_epoch: None,
                    };
                    if let TraceEvent::Transport(packet) = &event
                        && self.omit_carrier == Some(packet.source_offset)
                    {
                        self.ordinal += 1;
                        return;
                    }
                    let used = pool.used();
                    assert!(
                        self.completion
                            .observe_source(
                                &event,
                                context,
                                &mut self.input,
                                self.observer.as_mut(),
                                &mut WorkBudget::new(0),
                                &RUN
                            )
                            .unwrap()
                            .is_none()
                    );
                    assert_eq!(
                        self.completion
                            .observe_source(
                                &event,
                                context,
                                &mut self.input,
                                self.observer.as_mut(),
                                &mut WorkBudget::new(0),
                                &CANCEL
                            )
                            .unwrap_err(),
                        Error::Cancelled
                    );
                    let blocker = pressure.then(|| pool.reserve(pool.available()).unwrap());
                    let work = self.completion.required_work(&event).unwrap();
                    if pressure && let TraceEvent::Diagnostic(fault) = &event {
                        let mut wrong = fault.clone();
                        wrong.location.offset += 1;
                        assert!(
                            self.completion
                                .observe_source(
                                    &TraceEvent::Diagnostic(wrong),
                                    context,
                                    &mut self.input,
                                    self.observer.as_mut(),
                                    &mut WorkBudget::new(work),
                                    &RUN,
                                )
                                .is_err()
                        );
                    }
                    let disposition = self
                        .completion
                        .observe_source(
                            &event,
                            context,
                            &mut self.input,
                            self.observer.as_mut(),
                            &mut WorkBudget::new(work),
                            &RUN,
                        )
                        .unwrap()
                        .unwrap();
                    drop(blocker);
                    assert!(pool.used() <= used);
                    self.ordinal += 1;
                    if matches!(event, TraceEvent::Pes(_)) {
                        self.source_pes += 1;
                    }
                    if matches!(event, TraceEvent::Diagnostic(_)) {
                        self.diagnostics += 1;
                    }
                    match disposition {
                        MalformedSourceDisposition::Reported(fact) => self.facts.push(fact),
                        MalformedSourceDisposition::AwaitDiagnostic => (),
                        MalformedSourceDisposition::Forward => match &event {
                            TraceEvent::ProgramMap {
                                map, active: true, ..
                            } => {
                                let scope = DemuxScope {
                                    program: 1,
                                    pid: map.streams.as_slice()[0].pid,
                                    stream_type: 0x1b,
                                    stream_id: 0xe0,
                                    entity: SourceId(1),
                                    epochs: context.epochs,
                                };
                                let mut observer = TransportFaultRecovery::new(
                                    DemuxObserver::new(
                                        DemuxProfile::PesPackets,
                                        scope,
                                        pool.clone(),
                                        ParseLimits::default(),
                                    )
                                    .unwrap(),
                                )
                                .unwrap();
                                observer.bind_source_input(&self.input).unwrap();
                                self.observer = Some(observer);
                            }
                            TraceEvent::Transport(packet) => {
                                if let Some(observer) = self.observer.as_mut()
                                    && packet.pid == observer.scope().pid
                                {
                                    observer.push(packet.clone(), context).unwrap();
                                }
                            }
                            _ => (),
                        },
                    }
                    self.drain();
                    if matches!(event, TraceEvent::Finished) {
                        self.finished = true;
                    }
                    return;
                }
            }
        }
        panic!("source did not advance");
    }
    fn all(&mut self, pool: &Budget, pressure: bool) {
        for _ in 0..1000 {
            if self.finished {
                return;
            }
            self.next(pool, pressure);
        }
        panic!("no Finished");
    }
    fn confirm(&mut self) -> Result<Option<NoPacketsConfirmation>, Error> {
        self.completion.confirm_no_packets(
            self.observer.as_mut(),
            &mut self.input,
            &mut WorkBudget::new(1),
            &RUN,
        )
    }
}

#[test]
fn four_reported_malformed_inputs_prove_eof_without_any_packet_or_new_allocation() {
    for (raw, kind) in CASES {
        for chunk in [1, 4096] {
            let pool = Budget::new(1 << 20).unwrap();
            let mut run = Run::new(raw, &pool, chunk);
            run.all(&pool, true);
            assert_eq!(run.facts.len(), 1);
            assert_eq!(run.diagnostics, 1);
            assert_eq!(run.source_pes, 0);
            assert!(run.packets.is_empty());
            let fact = &run.facts[0];
            assert_eq!(fact.kind(), kind);
            assert_eq!(fact.source_fault().location.offset, 376);
            assert_eq!(fact.source_bytes().as_ref(), &raw[376..564]);
            assert_eq!(fact.source_fault().location.source, SourceId(1));
            let pressure = pool.reserve(pool.available()).unwrap();
            let confirmation = run.confirm().unwrap().unwrap();
            assert_eq!(confirmation.physical_end().offset, 564);
            assert_eq!(confirmation.physical_carriers(), 3);
            assert_eq!(confirmation.discarded_carriers(), 1);
            drop(pressure);
            let facts = std::mem::take(&mut run.facts);
            drop(run);
            assert_eq!(facts[0].source_bytes().as_ref(), &raw[376..564]);
            drop(facts);
            drop(confirmation);
            assert_eq!(pool.used(), Budget::control_storage_bytes());
        }
    }
}

#[test]
fn later_valid_pes_is_emitted_and_prevents_no_packets_confirmation() {
    for (bad, _) in CASES {
        let mut raw = bad.to_vec();
        let mut packet = VALID[376..564].to_vec();
        packet[3] = packet[3] & 0xf0 | 1;
        raw.extend_from_slice(&packet);
        let pool = Budget::new(1 << 20).unwrap();
        let mut run = Run::new(&raw, &pool, 1);
        run.all(&pool, false);
        assert_eq!(run.facts.len(), 1);
        assert_eq!(run.packets.len(), 1);
        assert_eq!(run.source_pes, 1);
        assert_eq!(
            run.packets[0]
                .payload()
                .flatten()
                .copied()
                .collect::<Vec<_>>(),
            b"lane-ts-original-payload\0\x47\xff"
        );
        assert!(run.confirm().is_err());
    }
}

#[test]
fn zero_carriers_still_need_the_original_cursors_actual_end_and_finished() {
    let pool = Budget::new(1 << 20).unwrap();
    let mut run = Run::new(&[], &pool, 1);
    assert!(run.confirm().is_err());
    run.all(&pool, false);
    let proof = run.confirm().unwrap().unwrap();
    assert_eq!(proof.physical_end().offset, 0);
    assert_eq!(proof.physical_carriers(), 0);
    assert_eq!(proof.discarded_carriers(), 0);
    assert!(run.observer.is_none());
}

#[test]
fn replacement_cursor_receipt_and_sticky_original_failure_cannot_complete() {
    let pool = Budget::new(1 << 20).unwrap();
    let mut a = Run::new(CASES[0].0, &pool, 1);
    while a.facts.is_empty() {
        a.next(&pool, false);
    }
    a.input
        .fail(SourceId(1), a.input.token(), SourceFailure::Transport)
        .unwrap();
    let mut b = Run::new(CASES[0].0, &pool, 1);
    b.all(&pool, false);
    assert!(
        a.completion
            .seal_physical_end(
                b.end.as_ref().unwrap(),
                &mut b.input,
                &mut WorkBudget::new(1),
                &RUN
            )
            .is_err()
    );
    assert!(a.confirm().is_err());
    assert!(
        a.completion
            .seal_physical_end(
                b.end.as_ref().unwrap(),
                &mut a.input,
                &mut WorkBudget::new(1),
                &RUN
            )
            .is_err()
    );
}

#[test]
fn reset_and_failure_after_real_eof_cannot_publish_a_confirmation() {
    for reset in [false, true] {
        let pool = Budget::new(1 << 20).unwrap();
        let mut run = Run::new(CASES[2].0, &pool, 1);
        run.all(&pool, false);
        if reset {
            run.input.reset(564).unwrap();
        } else {
            run.input
                .fail(SourceId(1), run.input.token(), SourceFailure::Transport)
                .unwrap();
        }
        assert!(run.confirm().is_err());
    }
}

#[test]
fn omitted_physical_carrier_prevents_a_receipt_even_after_actual_eof() {
    let pool = Budget::new(1 << 20).unwrap();
    let raw = CASES[3].0;
    let mut wire = raw[..376].to_vec();
    let mut null = [0xff; 188];
    null[..4].copy_from_slice(&[0x47, 0x1f, 0xff, 0x10]);
    wire.extend_from_slice(&null);
    wire.extend_from_slice(&raw[376..]);
    let mut run = Run::new(&wire, &pool, 1);
    run.omit_carrier = Some(376);
    run.all(&pool, false);
    assert_eq!(run.end.as_ref().unwrap().location().offset, 752);
    assert_eq!(run.facts.len(), 1);
    assert!(run.packets.is_empty());
    assert!(run.confirm().is_err());
}

#[test]
fn adaptation_that_fits_without_payload_is_not_an_overrun() {
    let pool = Budget::new(1 << 20).unwrap();
    let mut raw = CASES[1].0.to_vec();
    raw[380] = 183;
    let mut run = Run::new(&raw, &pool, 4096);
    run.all(&pool, false);
    assert_eq!(run.diagnostics, 1);
    assert!(run.facts.is_empty());
    assert!(run.confirm().is_err());
}

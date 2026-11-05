use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use triergon_core::{
    Budget, Bytes, Error as CoreError, InputCursor, InputRequest, InputWindow, SourceCapabilities,
    SourceFailure, SourceId, Time, WorkBudget,
};
use triergon_playback::{
    io::{SelectedInput, SourceOwner},
    mp4_open::{MetadataReadScope, Mp4Open, OpenLimits, OpenProgress},
    mp4_plan::{Mp4IndexJob, Mp4View, PlanLimits},
    progressive::ProgressiveLimits,
    *,
};
#[allow(dead_code)]
mod regular_gold {
    include!("fixtures/mp4_golds.rs");
}
#[allow(dead_code)]
mod fragment_gold {
    include!("fixtures/progressive/source_golds.rs");
}
const ID: SourceId = SourceId(6401);
const REGULAR: &[u8] = include_bytes!("fixtures/open-ended/regular-terminal.mp4");
const PENDING: &[u8] = include_bytes!("fixtures/open-ended/pending-terminal.mp4");
fn off() -> AtomicBool {
    AtomicBool::new(false)
}
#[derive(Default)]
struct Signals {
    allow_end: AtomicBool,
    fail_end: AtomicBool,
    polls: AtomicUsize,
    ends: AtomicUsize,
    aborts: AtomicUsize,
    closes: AtomicUsize,
}
struct FiniteSource {
    bytes: ByteSource,
    length: usize,
    signals: Arc<Signals>,
}
impl Source for FiniteSource {
    fn poll(
        &mut self,
        r: &InputRequest,
        w: &mut WorkBudget,
        c: &AtomicBool,
    ) -> triergon_core::Result<SourceReply> {
        if c.load(Ordering::Relaxed) {
            return Err(CoreError::Cancelled);
        }
        if r.source != ID {
            return Err(CoreError::SourceMismatch);
        }
        if w.remaining() == 0 {
            return Ok(SourceReply::Pending);
        }
        if r.offset == self.length as u64 && !self.signals.allow_end.load(Ordering::Relaxed) {
            assert!(w.try_charge(1));
            self.signals.polls.fetch_add(1, Ordering::Relaxed);
            return Ok(SourceReply::Pending);
        }
        if r.offset == self.length as u64 && self.signals.fail_end.load(Ordering::Relaxed) {
            assert!(w.try_charge(1));
            return Ok(SourceReply::Failed {
                source: r.source,
                token: r.token.clone(),
                failure: SourceFailure::Transport,
            });
        }
        self.signals.polls.fetch_add(1, Ordering::Relaxed);
        let reply = self.bytes.poll(r, w, c)?;
        if matches!(reply, SourceReply::End { .. }) {
            self.signals.ends.fetch_add(1, Ordering::Relaxed);
        }
        Ok(reply)
    }
    fn invalidate(&mut self) {
        self.bytes.invalidate();
    }
    fn abort(&mut self) {
        self.signals.aborts.fetch_add(1, Ordering::Relaxed);
        self.bytes.abort();
    }
    fn close(&mut self) {
        self.signals.closes.fetch_add(1, Ordering::Relaxed);
        self.bytes.close();
    }
}
fn opener(
    raw: &[u8],
    known: Option<u64>,
    allow_end: bool,
) -> (Mp4Open<FiniteSource>, Budget, Arc<Signals>) {
    let b = Budget::new(64 << 20).unwrap();
    let signals = Arc::new(Signals::default());
    signals.allow_end.store(allow_end, Ordering::Relaxed);
    let source = FiniteSource {
        bytes: ByteSource::new(ID, Bytes::copy_from_slice(&b, raw).unwrap(), 7).unwrap(),
        length: raw.len(),
        signals: signals.clone(),
    };
    let input = InputCursor::new(
        &b,
        ID,
        SourceCapabilities {
            length: known,
            random_access: false,
        },
        64,
    )
    .unwrap();
    let open = Mp4Open::new(
        SelectedInput {
            input,
            source: SourceOwner::new(source),
            format: Format::Mp4,
        },
        OpenLimits::default(),
    )
    .unwrap();
    (open, b, signals)
}
fn opened(
    raw: &[u8],
    known: Option<u64>,
    allow_end: bool,
) -> (mp4_open::OpenedMp4<FiniteSource>, Budget, Arc<Signals>) {
    let (mut open, b, signals) = opener(raw, known, allow_end);
    for _ in 0..200_000 {
        if matches!(
            open.advance(&mut WorkBudget::new(1), &off()).unwrap(),
            OpenProgress::Ready
        ) {
            return (open.finish().unwrap(), b, signals);
        }
    }
    panic!("bounded opener did not complete");
}
fn make(
    raw: &[u8],
    known: Option<u64>,
    allow_end: bool,
) -> (Player<FiniteSource>, Budget, Arc<Signals>) {
    let (opened, b, signals) = opened(raw, known, allow_end);
    let mut index = Mp4IndexJob::new(
        &opened.movie,
        Mp4View::SourcePhysical,
        PlanLimits::default(),
    )
    .unwrap();
    for _ in 0..200_000 {
        if index.poll(&mut WorkBudget::new(1), &off()).unwrap() {
            break;
        }
    }
    let plan = index.finish().unwrap();
    let engine =
        Engine::mp4_progressive(opened.input, plan, 1 << 20, ProgressiveLimits::default()).unwrap();
    (
        Player::new(
            engine,
            opened.source,
            Selection::All,
            PlayerPolicy::default(),
        )
        .unwrap(),
        b,
        signals,
    )
}
fn packets(out: &[Output]) -> Vec<&Mp4Packet> {
    out.iter()
        .filter_map(|o| {
            if let RawEvent::Mp4(Mp4Event::Packet(p)) = o.event() {
                Some(p)
            } else {
                None
            }
        })
        .collect()
}
fn read(p: &mut Player<FiniteSource>, out: &mut Vec<Output>) -> Result<()> {
    for _ in 0..300_000 {
        let step = p.advance(&mut WorkBudget::new(1), &off())?;
        while let Some(value) = p.pop() {
            out.push(value);
        }
        if matches!(step, Progress::Ended) {
            return Ok(());
        }
    }
    panic!("bounded terminal source did not complete");
}
fn check_regular(out: &[Output]) {
    let want = regular_gold::CASES
        .iter()
        .find(|x| x.name == "r-front-stco")
        .unwrap();
    let got = packets(out);
    assert_eq!(got.len(), want.samples.len());
    for (p, w) in got.iter().zip(want.samples) {
        assert_eq!(p.source.source, ID);
        assert_eq!(p.source.sample.ordinal, w.ordinal);
        assert_eq!(
            p.source.sample.range,
            w.offset..w.offset + w.payload.len() as u64
        );
        assert_eq!(p.source.payload.as_ref(), w.payload);
        assert_eq!(p.source.sample.dts_ticks, Some(w.dts));
        assert_eq!(p.source.sample.cts_ticks, Some(w.cts));
        assert_eq!(p.source.sample.duration_ticks, w.delta);
        assert_eq!(p.source.sample.description_index, w.description);
        assert_eq!(p.configuration.track.track_id, w.track);
        assert_eq!(p.source.sample.is_container_sync(), w.sync);
        assert!(p.native.is_none());
    }
}
fn check_fragment(out: &[Output], name: &str) {
    let want = fragment_gold::CASES
        .iter()
        .find(|x| x.name == name)
        .unwrap();
    let got = packets(out);
    assert_eq!(got.len(), want.wants.len());
    for (p, w) in got.iter().zip(want.wants) {
        assert_eq!(p.source.sample.ordinal, w.ordinal);
        assert_eq!(p.source.sample.range, w.offset..w.offset + 12);
        assert_eq!(p.source.payload.as_ref(), w.payload);
        assert_eq!(p.source.sample.dts_ticks, w.dts);
        assert_eq!(p.source.sample.cts_ticks, w.dts);
        assert_eq!(p.source.sample.duration_ticks, 1000);
        assert_eq!(p.source.sample.description_index, w.description);
        assert!(p.native.is_none());
    }
}
fn parked() -> (Player<FiniteSource>, Budget, Arc<Signals>, Vec<Output>) {
    let (mut p, b, signals) = make(REGULAR, None, false);
    let mut out = Vec::new();
    for _ in 0..200_000 {
        assert!(!matches!(
            p.advance(&mut WorkBudget::new(1), &off()).unwrap(),
            Progress::Ended
        ));
        while let Some(value) = p.pop() {
            out.push(value);
        }
        if p.pending_request()
            .is_some_and(|r| r.offset == REGULAR.len() as u64)
        {
            assert_eq!(packets(&out).len(), 6);
            return (p, b, signals, out);
        }
    }
    panic!("terminal source did not request actual EOF");
}
#[test]
fn open_ended_required_front_source_waits_for_actual_end() {
    let (mut p, b, signals) = make(REGULAR, None, true);
    let mut out = Vec::new();
    read(&mut p, &mut out).unwrap();
    check_regular(&out);
    assert_eq!(signals.ends.load(Ordering::Relaxed), 1);
    drop(p);
    assert_eq!(packets(&out)[0].source.payload.len(), 12);
    drop(out);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}
#[test]
fn open_ended_required_pending_moofs_and_terminal_source_are_complete() {
    let (mut p, b, signals) = make(PENDING, None, true);
    let mut out = Vec::new();
    read(&mut p, &mut out).unwrap();
    check_fragment(&out, "pending-pair");
    assert_eq!(signals.ends.load(Ordering::Relaxed), 1);
    drop(out);
    drop(p);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}
#[test]
fn open_ended_required_header_pressure_keeps_same_producer_and_header() {
    let raw = include_bytes!("fixtures/mp4/r-front-stco.mov");
    let (mut open, b, signals) = opener(raw, None, true);
    let hold = b.reserve(b.available()).unwrap();
    let mut pressure = false;
    for _ in 0..2000 {
        match open.advance(&mut WorkBudget::new(1), &off()) {
            Err(Error::Core(CoreError::Backpressure { .. })) => {
                pressure = true;
                break;
            }
            Err(error) => panic!("unexpected opener error {error:?}"),
            Ok(_) => {}
        }
    }
    assert!(pressure);
    assert_eq!(signals.closes.load(Ordering::Relaxed), 0);
    drop(hold);
    let mut ready = false;
    for _ in 0..200_000 {
        if matches!(
            open.advance(&mut WorkBudget::new(1), &off()).unwrap(),
            OpenProgress::Ready
        ) {
            ready = true;
            break;
        }
    }
    assert!(ready);
    let opened = open.finish().unwrap();
    assert_eq!(opened.movie.tracks().len(), 1);
    drop(opened);
    assert_eq!(signals.closes.load(Ordering::Relaxed), 1);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}
#[test]
fn open_ended_starvation_empty_data_pause_and_workzero_are_not_eof() {
    let (mut p, b, signals, mut out) = parked();
    let request = p.pending_request().unwrap().clone();
    let empty = SourceReply::Data(
        InputWindow::new(
            ID,
            request.token.clone(),
            request.offset,
            Bytes::copy_from_slice(&b, &[]).unwrap(),
        )
        .unwrap(),
    );
    assert!(!p.provide(&empty).unwrap());
    assert!(p.pending_request().is_some());
    drop(empty);
    let before = (b.used(), signals.polls.load(Ordering::Relaxed));
    for _ in 0..5 {
        assert!(matches!(
            p.advance(&mut WorkBudget::new(0), &off()).unwrap(),
            Progress::Yield
        ));
    }
    p.pause(true).unwrap();
    for _ in 0..5 {
        assert!(matches!(
            p.advance(&mut WorkBudget::new(8), &off()).unwrap(),
            Progress::Paused
        ));
    }
    assert_eq!((b.used(), signals.polls.load(Ordering::Relaxed)), before);
    p.pause(false).unwrap();
    for _ in 0..3 {
        assert!(!matches!(
            p.advance(&mut WorkBudget::new(1), &off()).unwrap(),
            Progress::Ended
        ));
    }
    assert_eq!(signals.ends.load(Ordering::Relaxed), 0);
    signals.allow_end.store(true, Ordering::Relaxed);
    read(&mut p, &mut out).unwrap();
    check_regular(&out);
}
#[test]
fn open_ended_short_sample_keeps_complete_prefix_and_truncation() {
    let (mut p, b, _) = make(&REGULAR[..REGULAR.len() - 4], None, true);
    let mut out = Vec::new();
    assert!(matches!(
        read(&mut p, &mut out),
        Err(Error::Core(CoreError::Truncated))
    ));
    assert_eq!(packets(&out).len(), 5);
    drop(p);
    assert_eq!(packets(&out)[0].source.payload.len(), 12);
    drop(out);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}
#[test]
fn open_ended_promised_longer_entity_end_remains_truncated() {
    let (mut p, _, _) = make(REGULAR, Some(REGULAR.len() as u64 + 4), true);
    let mut out = Vec::new();
    assert!(matches!(
        read(&mut p, &mut out),
        Err(Error::Core(CoreError::Truncated))
    ));
    assert_eq!(packets(&out).len(), 6);
}
#[test]
fn open_ended_borrowed_end_requires_current_source_and_pool() {
    let (mut p, b, signals, mut out) = parked();
    let r = p.pending_request().unwrap().clone();
    assert!(matches!(
        p.provide(&SourceReply::End {
            source: SourceId(999),
            token: r.token.clone()
        }),
        Err(Error::Core(CoreError::SourceMismatch))
    ));
    let foreign_budget = Budget::new(4096).unwrap();
    let foreign = InputCursor::new(
        &foreign_budget,
        ID,
        SourceCapabilities {
            length: None,
            random_access: false,
        },
        64,
    )
    .unwrap();
    assert!(matches!(
        p.provide(&SourceReply::End {
            source: ID,
            token: foreign.token()
        }),
        Err(Error::Core(CoreError::ForeignPool))
    ));
    assert!(p.pending_request().is_some());
    let end = SourceReply::End {
        source: ID,
        token: r.token.clone(),
    };
    assert!(p.provide(&end).unwrap());
    drop(end);
    read(&mut p, &mut out).unwrap();
    check_regular(&out);
    assert_eq!(signals.ends.load(Ordering::Relaxed), 0);
    drop(p);
    drop(out);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}
#[test]
fn open_ended_cancel_at_end_keeps_outputs_and_closes_once() {
    let (mut p, b, signals, out) = parked();
    assert!(matches!(
        p.advance(&mut WorkBudget::new(0), &AtomicBool::new(true)),
        Err(Error::Core(CoreError::Cancelled))
    ));
    drop(p);
    assert_eq!(signals.aborts.load(Ordering::Relaxed), 1);
    assert_eq!(signals.closes.load(Ordering::Relaxed), 1);
    check_regular(&out);
    drop(out);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}
#[test]
fn open_ended_transport_failure_is_not_normal_source_end() {
    let (mut p, b, signals, mut out) = parked();
    signals.fail_end.store(true, Ordering::Relaxed);
    signals.allow_end.store(true, Ordering::Relaxed);
    assert!(matches!(
        read(&mut p, &mut out),
        Err(Error::Core(CoreError::Transport))
    ));
    check_regular(&out);
    assert_eq!(signals.ends.load(Ordering::Relaxed), 0);
    drop(p);
    drop(out);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}
#[test]
fn open_ended_opaque_tail_is_payload_until_actual_end() {
    let raw = include_bytes!("fixtures/open-ended/opaque-tail-terminal.mp4");
    let (mut p, _, signals) = make(raw, None, true);
    let mut out = Vec::new();
    read(&mut p, &mut out).unwrap();
    check_regular(&out);
    assert_eq!(signals.ends.load(Ordering::Relaxed), 1);
    assert!(
        !out.iter()
            .any(|o| matches!(o.event(), RawEvent::Mp4Root(_)))
    );
}
#[test]
fn open_ended_packet_pressure_retries_all_declared_samples() {
    let (mut p, b, _) = make(REGULAR, None, true);
    let mut out = Vec::new();
    let mut ended = false;
    for _ in 0..300_000 {
        let hold = b.reserve(b.available()).unwrap();
        let step = p.advance(&mut WorkBudget::new(1), &off());
        drop(hold);
        let mut step = match step {
            Err(Error::Core(CoreError::Backpressure { .. })) => {
                p.advance(&mut WorkBudget::new(1), &off()).unwrap()
            }
            value => value.unwrap(),
        };
        if matches!(step, Progress::Backpressure { .. }) {
            step = p.advance(&mut WorkBudget::new(1), &off()).unwrap();
        }
        while let Some(value) = p.pop() {
            out.push(value);
        }
        if matches!(step, Progress::Ended) {
            ended = true;
            break;
        }
    }
    assert!(ended);
    check_regular(&out);
    drop(p);
    drop(out);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}
#[test]
fn open_ended_unknown_fragment_clock_stays_unknown() {
    let raw = include_bytes!("fixtures/open-ended/unknown-clock-terminal.mp4");
    let (mut p, _, _) = make(raw, None, true);
    let mut out = Vec::new();
    read(&mut p, &mut out).unwrap();
    check_fragment(&out, "pending-unknown-clock");
    assert!(
        packets(&out)
            .iter()
            .skip(6)
            .all(|p| p.source.sample.dts_ticks.is_none())
    );
}
#[test]
fn open_ended_empty_declared_samples_are_source_facts_before_end() {
    let raw = include_bytes!("fixtures/open-ended/empty-sample-terminal.mp4");
    let (mut p, _, signals) = make(raw, None, false);
    let mut out = Vec::new();
    for _ in 0..300_000 {
        assert!(!matches!(
            p.advance(&mut WorkBudget::new(1), &off()).unwrap(),
            Progress::Ended
        ));
        while let Some(value) = p.pop() {
            out.push(value);
        }
        if p.pending_request()
            .is_some_and(|r| r.offset == raw.len() as u64)
        {
            break;
        }
    }
    let got = packets(&out);
    assert_eq!(got.len(), 9);
    for (i, packet) in got.iter().enumerate().skip(6) {
        assert!(packet.source.payload.is_empty());
        assert_eq!(packet.source.sample.ordinal, i as u64 + 1);
        assert_eq!(
            packet.source.sample.range,
            raw.len() as u64..raw.len() as u64
        );
    }
    signals.allow_end.store(true, Ordering::Relaxed);
    read(&mut p, &mut out).unwrap();
    assert_eq!(packets(&out).len(), 9);
}
#[test]
fn open_ended_unproven_native_and_unattached_completion_are_refused() {
    let (opened, _, _) = opened(REGULAR, None, false);
    assert!(
        matches!(opened.movie.read_scope(), MetadataReadScope::FrontMetadataBeforeMdat(h) if h.size.is_none())
    );
    assert!(matches!(
        Mp4IndexJob::new(
            &opened.movie,
            Mp4View::PinnedMov9NoParseNoFillin,
            PlanLimits::default()
        ),
        Err(Error::Core(CoreError::Unsupported(_)))
    ));
    let mut index = Mp4IndexJob::new(
        &opened.movie,
        Mp4View::SourcePhysical,
        PlanLimits::default(),
    )
    .unwrap();
    while !index.poll(&mut WorkBudget::new(1), &off()).unwrap() {}
    let plan = index.finish().unwrap();
    assert!(matches!(
        Engine::mp4(opened.input, plan, 1 << 20),
        Err(Error::Core(CoreError::Unsupported(_)))
    ));
}
#[test]
fn open_ended_nonseekable_refusal_keeps_current_source_outputs() {
    let (mut p, _, signals, mut out) = parked();
    let held = out
        .iter()
        .find(|o| matches!(o.event(), RawEvent::Mp4(Mp4Event::Packet(_))))
        .unwrap();
    assert!(p.current(held));
    assert!(
        p.seek_and_select(
            TrackKey::Mp4(1),
            Time::new(0, 1).unwrap(),
            Selection::All,
            &mut WorkBudget::new(8),
            &off()
        )
        .is_err()
    );
    assert!(p.current(held));
    signals.allow_end.store(true, Ordering::Relaxed);
    read(&mut p, &mut out).unwrap();
    check_regular(&out);
}

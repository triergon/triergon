mod common;
use common::{annex_b, annex_b_session, collect, off, packets, ready, run_open};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use triergon_core::{
    Budget, Bytes, Error as C, InputCursor, SourceCapabilities, SourceId, Time, WorkBudget,
};
use triergon_playback::{ByteSource, Source, SourceReply};
use triergon_playback::{
    io::{SelectedInput, SourceOwner},
    mp4_open::{Mp4Open, OpenLimits, OpenProgress},
    mp4_plan::{Mp4IndexJob, Mp4View, PlanLimits},
    progressive::ProgressiveLimits,
    pump::*,
    *,
};
const ID: SourceId = SourceId(901);
fn pool() -> Budget {
    Budget::new(64 << 20).unwrap()
}
struct Observed {
    source: ByteSource,
    gate: Arc<AtomicUsize>,
    calls: Arc<Mutex<Vec<&'static str>>>,
}
impl Source for Observed {
    fn poll(
        &mut self,
        r: &triergon_core::InputRequest,
        w: &mut WorkBudget,
        c: &AtomicBool,
    ) -> Result<SourceReply> {
        if w.remaining() == 0 {
            return Ok(SourceReply::Pending);
        }
        self.calls.lock().unwrap().push("read");
        if r.offset >= self.gate.load(Ordering::Relaxed) as u64 {
            return Ok(SourceReply::Pending);
        }
        self.source.poll(r, w, c)
    }
    fn invalidate(&mut self) {
        self.calls.lock().unwrap().push("invalidate");
        self.source.invalidate();
    }
    fn abort(&mut self) {
        self.calls.lock().unwrap().push("abort");
        self.source.abort();
    }
    fn close(&mut self) {
        self.calls.lock().unwrap().push("close");
        self.source.close();
    }
}
type Calls = Arc<Mutex<Vec<&'static str>>>;
fn player(
    raw: &[u8],
    live: bool,
    limits: ProgressiveLimits,
) -> (Player<Observed>, Budget, Arc<AtomicUsize>, Calls) {
    let b = pool();
    let gate = Arc::new(AtomicUsize::new(usize::MAX));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let source = Observed {
        source: ByteSource::new(ID, Bytes::copy_from_slice(&b, raw).unwrap(), 1).unwrap(),
        gate: gate.clone(),
        calls: calls.clone(),
    };
    let input = InputCursor::new(
        &b,
        ID,
        SourceCapabilities {
            length: if live { None } else { Some(raw.len() as u64) },
            random_access: !live,
        },
        64,
    )
    .unwrap();
    let mut open = Mp4Open::new(
        SelectedInput {
            input,
            source: SourceOwner::new(source),
            format: Format::Mp4,
        },
        OpenLimits::default(),
    )
    .unwrap();
    run_open(&mut open, 1).unwrap();
    let opened = open.finish().unwrap();
    let mut index = Mp4IndexJob::new(
        &opened.movie,
        Mp4View::SourcePhysical,
        PlanLimits::default(),
    )
    .unwrap();
    while !index.poll(&mut WorkBudget::new(1), &off()).unwrap() {}
    let plan = index.finish().unwrap();
    let engine = if live {
        Engine::mp4_progressive(opened.input, plan, 1 << 20, limits).unwrap()
    } else {
        Engine::mp4(opened.input, plan, 1 << 20).unwrap()
    };
    (
        Player::new(
            engine,
            opened.source,
            Selection::All,
            PlayerPolicy::default(),
        )
        .unwrap(),
        b,
        gate,
        calls,
    )
}
mod gold {
    include!("fixtures/mp4_golds.rs");
}
#[test]
fn progressive_all_five_fragment_sequences_keep_original_source_golds() {
    assert_eq!(
        gold::CASES.iter().filter(|c| c.fragmented).count(),
        5,
        "all five original fragmented controls remain required"
    );
    for case in gold::CASES.iter().filter(|c| c.fragmented) {
        assert!(
            !case.malformed,
            "selected fragmented controls are valid source inputs"
        );
        assert!(
            case.gap,
            "original native-comparison gap membership must remain visible; source closure does not waive it"
        );
        let (mut p, b, _, _) = player(case.bytes, true, ProgressiveLimits::default());
        let out = collect(&mut p, 1).unwrap();
        let got = packets(&out);
        assert_eq!(got.len(), case.samples.len(), "{}", case.name);
        for want in case.samples {
            let p = got
                .iter()
                .find(|p| {
                    p.configuration.track.track_id == want.track
                        && p.source.sample.ordinal == want.ordinal
                })
                .unwrap();
            assert_eq!(p.source.sample.description_index, want.description);
            assert_eq!(p.source.sample.range.start, want.offset);
            assert_eq!(p.source.payload.as_ref(), want.payload);
            assert_eq!(p.source.sample.duration_ticks, want.delta);
            assert_eq!(p.source.sample.is_container_sync(), want.sync);
            if let Some(d) = p.source.sample.dts_ticks {
                assert_eq!(d, want.dts);
                assert_eq!(p.source.sample.cts_ticks, Some(want.cts));
            } else {
                assert_eq!(case.name, "f-no-second-tfdt");
                assert!(p.source.sample.cts_ticks.is_none());
            }
        }
        assert!(
            out.iter()
                .any(|o| matches!(o.event(),RawEvent::Mp4Root(r)if r.header().kind==*b"moof"))
        );
        drop(out);
        drop(p);
        assert_eq!(b.used(), Budget::control_storage_bytes());
    }
}
#[test]
fn progressive_gap_is_pending_until_actual_later_fragment_data_and_eof() {
    let raw = include_bytes!("fixtures/mp4/f-explicit.mp4");
    let (mut p, _, gate, calls) = player(raw, true, ProgressiveLimits::default());
    gate.store(799, Ordering::Relaxed);
    let mut out = Vec::new();
    for _ in 0..25_000 {
        let r = p.advance(&mut WorkBudget::new(1), &off()).unwrap();
        assert!(!matches!(r, Progress::Ended));
        while let Some(x) = p.pop() {
            out.push(x);
        }
        if packets(&out).len() == 3 && p.pending_request().is_some_and(|r| r.offset == 799) {
            break;
        }
    }
    assert_eq!(packets(&out).len(), 3);
    assert_eq!(p.state(), State::Running);
    let before = calls.lock().unwrap().len();
    for _ in 0..4 {
        assert!(!matches!(
            p.advance(&mut WorkBudget::new(1), &off()).unwrap(),
            Progress::Ended
        ));
    }
    assert!(calls.lock().unwrap().len() > before);
    gate.store(usize::MAX, Ordering::Relaxed);
    out.extend(collect(&mut p, 1).unwrap());
    assert_eq!(packets(&out).len(), 6);
}
#[test]
fn progressive_work_zero_does_not_read_allocate_or_publish_future_roots() {
    let (mut p, b, _, calls) = player(
        include_bytes!("fixtures/mp4/f-explicit.mp4"),
        true,
        ProgressiveLimits::default(),
    );
    let before = (b.used(), calls.lock().unwrap().len());
    for _ in 0..20 {
        assert!(matches!(
            p.advance(&mut WorkBudget::new(0), &off()).unwrap(),
            Progress::Yield
        ));
        assert!(p.pop().is_none());
    }
    assert_eq!((b.used(), calls.lock().unwrap().len()), before);
}
#[test]
fn progressive_second_fragment_description_precedes_its_packets() {
    let (mut p, _, _, _) = player(
        include_bytes!("fixtures/mp4/f-description-change.mp4"),
        true,
        ProgressiveLimits::default(),
    );
    let out = collect(&mut p, 1).unwrap();
    let mut cfg = None;
    let mut descriptions = Vec::new();
    for o in &out {
        match o.event() {
            RawEvent::Mp4(Mp4Event::Configuration(c)) => {
                cfg = Some((c.track.track_id, c.description.index, c.revision));
                descriptions.push(c.description.index);
            }
            RawEvent::Mp4(Mp4Event::Packet(p)) => assert_eq!(
                cfg,
                Some((
                    p.configuration.track.track_id,
                    p.source.sample.description_index,
                    p.configuration.revision
                ))
            ),
            _ => {}
        }
    }
    assert_eq!(packets(&out).len(), 6);
    assert!(descriptions.contains(&1) && descriptions.contains(&2));
}
#[test]
fn progressive_truncated_future_media_retains_completed_source_prefix() {
    let raw = include_bytes!("fixtures/mp4/f-explicit.mp4");
    let (mut p, _, _, _) = player(&raw[..raw.len() - 1], true, ProgressiveLimits::default());
    let mut out = Vec::new();
    let mut failed = false;
    for _ in 0..100_000 {
        match p.advance(&mut WorkBudget::new(1), &off()) {
            Ok(_) => {}
            Err(C::Truncated) => {
                failed = true;
                break;
            }
            Err(e) => panic!("wrong source failure {e:?}"),
        }
        while let Some(x) = p.pop() {
            out.push(x);
        }
    }
    assert!(failed);
    assert_eq!(packets(&out).len(), 5);
    assert_eq!(p.state(), State::Failed);
    assert!(packets(&out).iter().all(|p| !p.source.payload.is_empty()));
}
#[test]
fn progressive_root_limit_is_explicit_and_source_owners_release() {
    let (mut p, b, _, _) = player(
        include_bytes!("fixtures/mp4/f-explicit.mp4"),
        true,
        ProgressiveLimits {
            roots: 1,
            ..ProgressiveLimits::default()
        },
    );
    let mut limit = false;
    for _ in 0..100_000 {
        match p.advance(&mut WorkBudget::new(1), &off()) {
            Err(C::Limit {
                requested: 2,
                maximum: 1,
            }) => {
                limit = true;
                break;
            }
            Ok(_) => {}
            Err(e) => panic!("wrong limit {e:?}"),
        }
        while p.pop().is_some() {}
    }
    assert!(limit);
    drop(p);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}
#[test]
fn progressive_cancel_quiesces_before_source_close_without_false_eof() {
    let (mut p, b, _, calls) = player(
        include_bytes!("fixtures/mp4/f-explicit.mp4"),
        true,
        ProgressiveLimits::default(),
    );
    assert!(matches!(
        p.advance(&mut WorkBudget::new(0), &AtomicBool::new(true)),
        Err(C::Cancelled)
    ));
    drop(p);
    let calls = calls.lock().unwrap();
    assert_eq!(&calls[calls.len() - 2..], &["abort", "close"]);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}

#[derive(Default)]
struct RetainingSink {
    accepted: Vec<Feed>,
    completed: Vec<FeedToken>,
    block: bool,
    fail: bool,
    fail_pause: bool,
    pauses: Vec<bool>,
    resets: Vec<SeekIntent>,
    stops: usize,
}
impl Sink for RetainingSink {
    fn submit(&mut self, feed: Feed) -> Submission {
        if self.fail {
            return Submission::Failed {
                feed,
                error: C::Transport,
            };
        }
        if self.block {
            return Submission::Blocked(feed);
        }
        let token = feed.token().clone();
        self.accepted.push(feed);
        Submission::Accepted(token)
    }
    fn completion(&mut self) -> Option<FeedToken> {
        if self.completed.is_empty() {
            None
        } else {
            Some(self.completed.remove(0))
        }
    }
    fn pause(&mut self, p: bool) -> Result<()> {
        self.pauses.push(p);
        if self.fail_pause {
            return Err(C::Transport);
        }
        Ok(())
    }
    fn reset(&mut self, i: &SeekIntent) -> Result<()> {
        self.resets.push(*i);
        Ok(())
    }
    fn stop(&mut self) {
        self.stops += 1;
    }
}
fn pump() -> (Pump<Observed, RetainingSink>, Budget, Calls) {
    let (p, b, _, calls) = player(
        include_bytes!("fixtures/mp4/r-front-stco.mov"),
        false,
        ProgressiveLimits::default(),
    );
    (Pump::new(p, RetainingSink::default(), 2).unwrap(), b, calls)
}
fn until_feed(p: &mut Pump<Observed, RetainingSink>) {
    for _ in 0..100_000 {
        if matches!(
            p.step(&mut WorkBudget::new(1), &off()).unwrap(),
            PumpStep::Fed
        ) {
            return;
        }
    }
    panic!("no feed")
}
#[test]
fn pump_blocked_owner_stops_producer_and_is_retried_without_new_source_read() {
    let (mut p, _, calls) = pump();
    p.sink_mut().unwrap().block = true;
    for _ in 0..100_000 {
        if matches!(
            p.step(&mut WorkBudget::new(1), &off()).unwrap(),
            PumpStep::SinkFull
        ) {
            break;
        }
    }
    let token = p.pending().unwrap().token().clone();
    let before = calls.lock().unwrap().len();
    for _ in 0..10 {
        assert_eq!(
            p.step(&mut WorkBudget::new(1), &off()).unwrap(),
            PumpStep::SinkFull
        );
        assert_eq!(p.pending().unwrap().token(), &token);
    }
    assert_eq!(calls.lock().unwrap().len(), before);
    p.sink_mut().unwrap().block = false;
    assert_eq!(
        p.step(&mut WorkBudget::new(1), &off()).unwrap(),
        PumpStep::Fed
    );
}
#[test]
fn pump_pause_and_work_zero_preserve_transport_and_held_owner() {
    let (mut p, b, calls) = pump();
    until_feed(&mut p);
    p.pause(true).unwrap();
    let before = (b.used(), calls.lock().unwrap().len(), p.in_flight());
    for _ in 0..10 {
        assert_eq!(
            p.step(&mut WorkBudget::new(100), &off()).unwrap(),
            PumpStep::Paused
        );
    }
    assert_eq!(
        (b.used(), calls.lock().unwrap().len(), p.in_flight()),
        before
    );
    p.pause(false).unwrap();
    for _ in 0..10 {
        assert_eq!(
            p.step(&mut WorkBudget::new(0), &off()).unwrap(),
            PumpStep::Yield
        );
    }
    assert_eq!(
        (b.used(), calls.lock().unwrap().len(), p.in_flight()),
        before
    );
}
#[test]
fn pump_sink_owns_accepted_raw_packets_after_source_and_pump_teardown() {
    let (mut p, b, _) = pump();
    for _ in 0..100_000 {
        if p.sink()
            .unwrap()
            .accepted
            .iter()
            .any(|f| f.output().event().track().is_some())
        {
            break;
        }
        if p.in_flight() == 2 {
            let token = p.sink().unwrap().accepted.last().unwrap().token().clone();
            p.sink_mut().unwrap().completed.push(token);
        }
        p.step(&mut WorkBudget::new(1), &off()).unwrap();
    }
    let sink = p.into_sink().unwrap();
    assert_eq!(sink.stops, 1);
    assert!(sink.accepted.iter().any(|f|matches!(f.output().event(),RawEvent::Mp4(Mp4Event::Packet(p))if !p.source.payload.is_empty())));
    assert!(b.used() > Budget::control_storage_bytes());
    drop(sink);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}
#[test]
fn pump_foreign_or_stale_completion_cannot_release_live_credit() {
    let (mut a, _, _) = pump();
    let (mut b, _, _) = pump();
    until_feed(&mut a);
    until_feed(&mut a);
    until_feed(&mut b);
    let foreign = b.sink().unwrap().accepted[0].token().clone();
    a.sink_mut().unwrap().completed.push(foreign);
    assert_eq!(
        a.step(&mut WorkBudget::new(1), &off()).unwrap(),
        PumpStep::StaleCompletion
    );
    assert_eq!(a.in_flight(), 2);
}
#[test]
fn pump_paused_seek_fences_pending_and_old_callbacks_without_transport_io() {
    let (mut p, _, calls) = pump();
    until_feed(&mut p);
    let old = p.sink().unwrap().accepted[0].token().clone();
    p.pause(true).unwrap();
    let before = calls.lock().unwrap().len();
    assert!(
        p.seek_and_select(
            TrackKey::Mp4(1),
            Time::new(0, 1).unwrap(),
            Selection::All,
            &mut WorkBudget::new(1),
            &off()
        )
        .unwrap()
    );
    assert_eq!(p.in_flight(), 0);
    for _ in 0..10 {
        assert_eq!(
            p.step(&mut WorkBudget::new(100), &off()).unwrap(),
            PumpStep::Paused
        );
    }
    assert_eq!(calls.lock().unwrap().len(), before + 1);
    assert_eq!(p.sink().unwrap().resets.len(), 1);
    p.pause(false).unwrap();
    until_feed(&mut p);
    until_feed(&mut p);
    p.sink_mut().unwrap().completed.push(old);
    assert_eq!(
        p.step(&mut WorkBudget::new(1), &off()).unwrap(),
        PumpStep::StaleCompletion
    );
    assert_eq!(p.in_flight(), 2);
}
#[test]
fn pump_sink_failure_stops_source_before_return_and_does_not_become_eof() {
    let (mut p, b, calls) = pump();
    p.sink_mut().unwrap().fail = true;
    let mut failed = false;
    for _ in 0..100_000 {
        match p.step(&mut WorkBudget::new(1), &off()) {
            Err(C::Transport) => {
                failed = true;
                break;
            }
            Ok(_) => {}
            Err(e) => panic!("wrong {e:?}"),
        }
    }
    assert!(failed);
    let c = calls.lock().unwrap();
    assert_eq!(&c[c.len() - 2..], &["abort", "close"]);
    drop(c);
    assert_eq!(p.sink().unwrap().stops, 1);
    drop(p);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}

struct StartupSource {
    inner: ByteSource,
    allow: Arc<AtomicBool>,
    len: usize,
    reads: Arc<AtomicUsize>,
}
impl Source for StartupSource {
    fn poll(
        &mut self,
        r: &triergon_core::InputRequest,
        w: &mut WorkBudget,
        c: &AtomicBool,
    ) -> Result<SourceReply> {
        if w.remaining() == 0 {
            return Ok(SourceReply::Pending);
        }
        self.reads.fetch_add(1, Ordering::Relaxed);
        if r.offset == self.len as u64 && !self.allow.load(Ordering::Relaxed) {
            return Ok(SourceReply::Pending);
        }
        self.inner.poll(r, w, c)
    }
    fn invalidate(&mut self) {
        self.inner.invalidate();
    }
    fn abort(&mut self) {
        self.inner.abort();
    }
    fn close(&mut self) {
        self.inner.close();
    }
}
fn native_setup(
    quantum: usize,
    known: bool,
) -> (
    native_startup::NativeStartup<StartupSource>,
    Budget,
    Arc<AtomicBool>,
    Arc<AtomicUsize>,
) {
    let raw = include_bytes!("fixtures/ts/m2ts192-opaque-prefix.m2ts");
    let b = pool();
    let allow = Arc::new(AtomicBool::new(true));
    let reads = Arc::new(AtomicUsize::new(0));
    let source = StartupSource {
        inner: ByteSource::new(ID, Bytes::copy_from_slice(&b, raw).unwrap(), quantum).unwrap(),
        allow: allow.clone(),
        len: raw.len(),
        reads: reads.clone(),
    };
    let input = InputCursor::new(
        &b,
        ID,
        SourceCapabilities {
            length: known.then_some(raw.len() as u64),
            random_access: false,
        },
        16 << 10,
    )
    .unwrap();
    (
        native_startup::NativeStartup::new(SelectedInput {
            input,
            source: SourceOwner::new(source),
            format: Format::Ts(triergon_mpegts::PacketFormat::M2ts192),
        })
        .unwrap(),
        b,
        allow,
        reads,
    )
}
fn prepared_native() -> (native_startup::PreparedNative<StartupSource>, Budget) {
    let (mut setup, b, _, _) = native_setup(16 << 10, true);
    for _ in 0..10_000 {
        if setup.advance(&mut WorkBudget::new(1), &off()).unwrap()
            == native_startup::StartupStep::Ready
        {
            return (setup.finish().unwrap(), b);
        }
    }
    panic!("finite native setup stalled")
}
#[test]
fn native_m2ts_context_uses_real_complete_window_and_positive_eof_reply() {
    let (prepared, b) = prepared_native();
    assert_eq!(prepared.context().inspected_bytes(), 576);
    let mut p = prepared
        .into_player(
            1,
            triergon_mpegts::ParseLimits::default(),
            triergon_mpegts::clock::SourceTimePolicy::Unanchored,
            TsNativeSelection {
                pid: 256,
                carriage: triergon_mpegts::compat::pes::RegisteredCarriage::Avc,
            },
            Selection::All,
            PlayerPolicy::default(),
        )
        .unwrap();
    p.set_clock_view(ClockChoice::ExplicitNativeProjection)
        .unwrap();
    let out = collect(&mut p, 1).unwrap();
    let native: Vec<_> = out
        .iter()
        .filter_map(|o| {
            if let RawEvent::NativeTs { packet, clock } = o.event() {
                Some((packet, clock))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(native.len(), 1);
    assert_eq!(native[0].0.position, 388);
    assert_eq!(native[0].0.source_origin.offset, 384);
    assert_eq!(native[0].0.flags, 0);
    assert_eq!(native[0].0.duration90_raw, 0);
    assert_eq!(native[0].1.pts90, Some(90_000));
    assert!(out.iter().any(|o| matches!(
        o.event(),
        RawEvent::Ts(TsEvent {
            source: triergon_mpegts::TraceEvent::Pes(_),
            ..
        })
    )));
    drop(out);
    drop(p);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}
#[test]
fn native_startup_pending_eof_is_never_completed_from_declared_length() {
    let (mut s, _, allow, reads) = native_setup(16 << 10, true);
    allow.store(false, Ordering::Relaxed);
    assert_eq!(
        s.advance(&mut WorkBudget::new(1), &off()).unwrap(),
        native_startup::StartupStep::NeedEof
    );
    for _ in 0..10 {
        assert_eq!(
            s.advance(&mut WorkBudget::new(1), &off()).unwrap(),
            native_startup::StartupStep::NeedEof
        );
    }
    assert_eq!(reads.load(Ordering::Relaxed), 11);
    allow.store(true, Ordering::Relaxed);
    for _ in 0..10_000 {
        if s.advance(&mut WorkBudget::new(1), &off()).unwrap() == native_startup::StartupStep::Ready
        {
            assert_eq!(s.finish().unwrap().context().inspected_bytes(), 576);
            return;
        }
    }
    panic!("startup failed to finish after actual EOF")
}
#[test]
fn native_startup_short_read_is_refusal_and_not_quantum_as_eof() {
    for known in [false, true] {
        let (mut s, b, _, _) = native_setup(7, known);
        let mut refused = false;
        for _ in 0..100 {
            match s.advance(&mut WorkBudget::new(1), &off()) {
                Err(C::Unsupported(_)) => {
                    refused = true;
                    break;
                }
                Ok(native_startup::StartupStep::Ready) => panic!("partial window admitted"),
                Ok(_) => {}
                Err(e) => panic!("wrong {e:?}"),
            }
        }
        assert!(refused);
        assert!(s.finish().is_err());
        assert_eq!(b.used(), Budget::control_storage_bytes());
    }
}
#[test]
fn native_startup_work_zero_and_cancel_do_not_perform_source_reads() {
    let (mut s, b, _, reads) = native_setup(16 << 10, true);
    let before = b.used();
    for _ in 0..10 {
        assert_eq!(
            s.advance(&mut WorkBudget::new(0), &off()).unwrap(),
            native_startup::StartupStep::Yield
        );
    }
    assert_eq!(reads.load(Ordering::Relaxed), 0);
    assert_eq!(b.used(), before);
    assert!(matches!(
        s.advance(&mut WorkBudget::new(0), &AtomicBool::new(true)),
        Err(C::Cancelled)
    ));
    drop(s);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}
#[test]
fn native_m2ts_seek_requires_new_context_and_keeps_old_output_scope() {
    let (prepared, _) = prepared_native();
    let mut p = prepared
        .into_player(
            1,
            triergon_mpegts::ParseLimits::default(),
            triergon_mpegts::clock::SourceTimePolicy::RelativeEpochZero,
            TsNativeSelection {
                pid: 256,
                carriage: triergon_mpegts::compat::pes::RegisteredCarriage::Avc,
            },
            Selection::All,
            PlayerPolicy::default(),
        )
        .unwrap();
    let old = loop {
        p.advance(&mut WorkBudget::new(1), &off()).unwrap();
        if let Some(o) = p.pop()
            && o.event().track().is_some()
        {
            break o;
        }
    };
    assert!(matches!(
        p.seek_and_select(
            TrackKey::Ts {
                program: 1,
                pid: 256
            },
            Time::new(0, 1).unwrap(),
            Selection::All,
            &mut WorkBudget::new(1),
            &off()
        ),
        Err(C::Unsupported(_))
    ));
    assert!(p.current(&old));
}

#[test]
fn real_hevc_mp4_packets_frame_as_annex_b_from_their_source_bytes() {
    let (mut p, b, _, _) = player(
        include_bytes!("fixtures/mp4/encoded-hevc-aac-front.mp4"),
        false,
        ProgressiveLimits::default(),
    );
    let out = collect(&mut p, 1).unwrap();
    let packets: Vec<_> = packets(&out)
        .into_iter()
        .filter(|p| matches!(&p.configuration.description.format, b"hvc1" | b"hev1"))
        .collect();
    assert!(!packets.is_empty());
    let mut session = annex_b_session(&b, packets[0]);
    for packet in packets {
        let result = annex_b(&mut session, packet);
        assert_eq!(
            result.source().bytes.as_ref(),
            packet.source.payload.as_ref()
        );
        assert_eq!(result.source().generation, packet.source.token.generation());
        assert!(
            result.bytes().starts_with(&[0, 0, 0, 1]) || result.bytes().starts_with(&[0, 0, 1])
        );
    }
}
fn aac_fixture_player() -> (Player<ByteSource>, Budget, Bytes, u64) {
    use triergon_isobmff::compat::mov::{encoded::*, packets::CursorStatus};
    use triergon_isobmff::*;
    let raw = include_bytes!("fixtures/mp4/encoded-avc-aac-front.mp4");
    let b = pool();
    let input = InputCursor::new(
        &b,
        ID,
        SourceCapabilities {
            length: Some(raw.len() as u64),
            random_access: true,
        },
        4096,
    )
    .unwrap();
    let source = ByteSource::new(ID, Bytes::copy_from_slice(&b, raw).unwrap(), 64).unwrap();
    let mut open = Mp4Open::new(
        SelectedInput {
            input,
            source: SourceOwner::new(source),
            format: Format::Mp4,
        },
        OpenLimits::default(),
    )
    .unwrap();
    while !matches!(
        open.advance(&mut WorkBudget::new(1), &off()).unwrap(),
        OpenProgress::Ready
    ) {}
    let mut opened = open.finish().unwrap();
    let track = opened
        .movie
        .tracks()
        .iter()
        .find(|t| t.header().handler == *b"soun")
        .unwrap();
    let mut job = EncodedStreamJob::new(
        track.header(),
        track.table(),
        &opened.input,
        Some(raw[8..12].try_into().unwrap()),
        ConfigurationLimits::default(),
        EncodedStreamProfile,
    )
    .unwrap();
    while job
        .poll(&mut opened.input, &mut WorkBudget::new(1), &off())
        .unwrap()
        != CursorStatus::Ready
    {}
    let catalogue = job.take(&mut opened.input, &off()).unwrap();
    let asc = catalogue.demux().extradata.clone();
    let config = catalogue.configuration_box();
    assert!(asc.shares_storage_with(config.bytes()));
    let relative = (asc.as_ref().as_ptr() as usize)
        .checked_sub(config.bytes().as_ref().as_ptr() as usize)
        .unwrap();
    assert!(relative + asc.len() <= config.bytes().len());
    let offset = config.header().start + relative as u64;
    let mut index = Mp4IndexJob::new(
        &opened.movie,
        Mp4View::SourcePhysical,
        PlanLimits::default(),
    )
    .unwrap();
    while !index.poll(&mut WorkBudget::new(1), &off()).unwrap() {}
    let plan = index.finish().unwrap();
    let player = Player::new(
        Engine::mp4(opened.input, plan, 1 << 20).unwrap(),
        opened.source,
        Selection::All,
        PlayerPolicy::default(),
    )
    .unwrap();
    (player, b, asc, offset)
}
fn aac_packets() -> (Vec<Mp4Packet>, Budget, Bytes, u64) {
    let (mut player, b, asc, offset) = aac_fixture_player();
    let out = collect(&mut player, 1).unwrap();
    (
        packets(&out)
            .into_iter()
            .filter(|p| p.configuration.description.format == *b"mp4a")
            .cloned()
            .collect(),
        b,
        asc,
        offset,
    )
}
#[test]
fn real_aac_mp4_adts_bridge_uses_source_asc_and_keeps_whole_raw_packet() {
    use triergon_framing::*;
    let (packets, b, asc, offset) = aac_packets();
    assert!(!packets.is_empty());
    let cfg = packets[0].configuration.clone();
    let mut parser = AacAscParser::new(
        &b,
        AacSource {
            source: ID,
            generation: cfg.revision,
            offset,
        },
        asc,
        AacConfigLimits {
            maximum_record_bytes: 1 << 20,
            maximum_pce_elements: 256,
            maximum_comment_bytes: 255,
        },
    )
    .unwrap();
    let record = loop {
        if let BuildStep::Ready(x) = parser.step(&mut WorkBudget::new(1), &off()).unwrap() {
            break x;
        }
    };
    let core = record.facts().core_frequency.hz;
    let bridge =
        aac_bridge::AacBridge::new(&b, ID, cfg.track.track_id, cfg.revision, record, 8191).unwrap();
    for packet in packets {
        let sample = packet.source.sample.clone();
        let bytes = packet.source.payload.clone();
        let mut job = bridge.packet(packet).unwrap();
        let before = b.used();
        assert!(matches!(
            job.step(&mut WorkBudget::new(0), &off()).unwrap(),
            BuildStep::Yield
        ));
        assert_eq!(b.used(), before);
        let o = loop {
            if let BuildStep::Ready(x) = job.step(&mut WorkBudget::new(1), &off()).unwrap() {
                break x;
            }
        };
        assert_eq!(o.source.source.sample, sample);
        assert_eq!(o.source.source.payload.as_ref(), bytes.as_ref());
        assert_eq!(o.adts.payload_bytes().unwrap().as_ref(), bytes.as_ref());
        assert_eq!(o.adts.constructed_header().coded_frequency_hz(), Some(core));
        assert_eq!(o.adts.payload_source().offset, sample.range.start);
    }
}
#[test]
fn aac_bridge_refuses_configuration_change_before_packet_conversion() {
    use triergon_framing::*;
    let (mut packets, b, asc, offset) = aac_packets();
    let mut p = packets.remove(0);
    let cfg = p.configuration.clone();
    let mut parser = AacAscParser::new(
        &b,
        AacSource {
            source: ID,
            generation: cfg.revision,
            offset,
        },
        asc,
        AacConfigLimits {
            maximum_record_bytes: 1 << 20,
            maximum_pce_elements: 256,
            maximum_comment_bytes: 255,
        },
    )
    .unwrap();
    let record = loop {
        if let BuildStep::Ready(x) = parser.step(&mut WorkBudget::new(1), &off()).unwrap() {
            break x;
        }
    };
    let bridge =
        aac_bridge::AacBridge::new(&b, ID, cfg.track.track_id, cfg.revision, record, 8191).unwrap();
    p.configuration.revision += 1;
    assert!(matches!(bridge.packet(p), Err(C::Invalid(_))));
}

#[test]
fn mp4_playback_feeds_real_simulator_decoders_across_clock_and_seek() {
    use triergon_core::Rounding;
    use triergon_framing::{AacAscParser, AacConfigLimits, AacSource};
    use triergon_session::{
        AudioCodec, AudioSession, PcmBlock, QueueLimits, SessionState, StreamSelection, VideoCodec,
        VideoSession,
    };

    let (mut player, pool, asc, offset) = aac_fixture_player();
    let cancel = off();
    let mut video = VideoSession::new(QueueLimits::VIDEO);
    let mut audio = AudioSession::new(QueueLimits::AUDIO);
    let selection = StreamSelection {
        video: VideoCodec::H264,
        audio: Some(AudioCodec::Aac),
    };
    selection.load(&mut video, &mut audio, &[]).unwrap();
    let mut reference = None;

    for iteration in 0..2 {
        let outputs = collect(&mut player, 1).unwrap();
        let packets = packets(&outputs);
        assert_eq!(packets.len(), 4);
        let avc = packets
            .iter()
            .find(|p| p.configuration.description.format == *b"avc1")
            .unwrap();
        let aac = &packets
            .iter()
            .find(|p| p.configuration.description.format == *b"mp4a")
            .unwrap()
            .configuration;
        let video_track = TrackKey::Mp4(avc.configuration.track.track_id);
        let mut avc = annex_b_session(&pool, avc);
        let mut aac_setup = AacAscParser::new(
            &pool,
            AacSource {
                source: ID,
                generation: aac.revision,
                offset,
            },
            asc.clone(),
            AacConfigLimits {
                maximum_record_bytes: 4096,
                maximum_pce_elements: 64,
                maximum_comment_bytes: 255,
            },
        )
        .unwrap();
        let asc_record = ready(|work| aac_setup.step(work, &cancel).unwrap());
        let aac_bridge = aac_bridge::AacBridge::new(
            &pool,
            ID,
            aac.track.track_id,
            aac.revision,
            asc_record,
            8191,
        )
        .unwrap();

        for packet in packets {
            let pts = packet
                .source
                .sample
                .source_times(packet.configuration.track.base.require().unwrap())
                .unwrap()
                .cts
                .map(|t| t.to_nanoseconds(Rounding::NearestAway).unwrap());
            match &packet.configuration.description.format {
                b"avc1" => {
                    let unit = annex_b(&mut avc, packet);
                    video.feed(unit.bytes().as_ref(), pts).unwrap();
                }
                b"mp4a" => {
                    let mut job = aac_bridge.packet(packet.clone()).unwrap();
                    let unit = ready(|work| job.step(work, &cancel).unwrap());
                    audio.feed(unit.adts.bytes().as_ref(), pts).unwrap();
                }
                codec => panic!("unexpected fixture codec {codec:?}"),
            }
        }
        video.end_of_stream();
        audio.end_of_stream();
        let mut pcm = PcmBlock::default();
        assert!(!video.step(Some(-1)));
        assert_eq!(audio.pull(Some(-1), 0, &mut pcm), 0);
        assert!(video.step(Some(0)));
        let first = video.latest().unwrap().clone();
        assert_eq!(first.pts, Some(0));
        assert_eq!(first.generation, video.generation());
        assert_eq!(audio.pull(Some(0), 0, &mut pcm), 1);
        assert_eq!(
            (pcm.pts, pcm.sample_rate, pcm.samples.len()),
            (Some(0), 48_000, 2048)
        );
        assert!(!video.step(Some(0)));
        assert_eq!(audio.pull(Some(0), 0, &mut pcm), 0);
        assert_eq!(audio.pull(Some(21_333_333), 0, &mut pcm), 1);
        assert_eq!(pcm.samples.len(), 4096);
        assert!(pcm.samples.iter().any(|&sample| sample != 0));
        assert!(!video.step(Some(39_999_999)));
        assert!(video.step(Some(40_000_000)));
        let second = video.latest().unwrap().clone();
        assert_eq!(second.pts, Some(40_000_000));
        assert!(first.rgba != second.rgba, "fixture pictures must differ");
        assert!(!video.step(Some(40_000_000)));
        assert_eq!(video.state(), SessionState::Ended);
        assert_eq!(audio.state(), SessionState::Ended);

        if iteration == 0 {
            assert!(
                player
                    .seek_and_select(
                        video_track,
                        Time::new(0, 1).unwrap(),
                        Selection::All,
                        &mut WorkBudget::new(1),
                        &cancel
                    )
                    .unwrap()
            );
            assert!(outputs.iter().all(|out| !player.current(out)));
            video.flush();
            audio.stop();
            assert_eq!(video.latest().unwrap().sequence, second.sequence);
            reference = Some((first, second, pcm));
        } else {
            let (old_first, old_second, old_pcm) = reference.take().unwrap();
            assert!(first.generation > old_first.generation);
            assert!(first.sequence > old_second.sequence);
            assert!(
                first.rgba == old_first.rgba,
                "first picture changed after seek"
            );
            assert!(
                second.rgba == old_second.rgba,
                "second picture changed after seek"
            );
            assert_eq!(pcm.sample_rate, old_pcm.sample_rate);
            assert_eq!(pcm.pts, old_pcm.pts);
            assert!(
                pcm.samples == old_pcm.samples,
                "PCM changed after seek reset"
            );
        }
    }
    player.stop();
    video.stop();
    audio.load(None);
    assert_eq!(player.state(), State::Stopped);
    assert_eq!(video.state(), SessionState::Idle);
    assert_eq!(audio.state(), SessionState::Idle);
}

#[test]
fn tx3g_real_reader_handoff_retains_source_sample_and_exact_clock_after_teardown() {
    use text_bridge::*;
    use triergon_subtitles::text;
    let (mut p, b, _, _) = player(
        include_bytes!("fixtures/mp4/r-subtitle-tx3g.mov"),
        false,
        ProgressiveLimits::default(),
    );
    let out = collect(&mut p, 1).unwrap();
    let output=out.into_iter().find(|o|matches!(o.event(),RawEvent::Mp4(Mp4Event::Packet(p))if p.configuration.description.format==*b"tx3g")).unwrap();
    let source = TextSource::from_output(output).unwrap_or_else(|e| panic!("{:?}", e.error));
    let packet = source.mp4().unwrap();
    let raw = packet.source.payload.clone();
    let sample = packet.source.sample.clone();
    let generation = packet.source.token.generation();
    let track = packet.configuration.track.track_id;
    let mut bridge = TextBridge::new(
        &b,
        ID,
        u64::from(track),
        generation,
        text::Limits::default(),
    );
    bridge.provide(source).unwrap();
    let before = b.used();
    assert!(matches!(
        bridge.step(&mut WorkBudget::new(0), &off()).unwrap(),
        TextStep::Yield
    ));
    assert_eq!(b.used(), before);
    let result = loop {
        if let TextStep::Event(o) = bridge.step(&mut WorkBudget::new(1), &off()).unwrap() {
            break o;
        }
    };
    p.stop();
    drop(p);
    drop(bridge);
    assert_eq!(
        result.source.mp4().unwrap().source.payload.as_ref(),
        raw.as_ref()
    );
    let text::Event::Cue(cue) = result.event else {
        panic!()
    };
    assert_eq!(cue.input.payload.as_ref(), raw.as_ref());
    assert_eq!(
        cue.input.timing.raw_clock().unwrap().pts_ticks,
        sample.cts_ticks
    );
    assert_eq!(
        cue.input.container_position.unwrap().offset,
        sample.range.start
    );
}
#[test]
fn matroska_utf8_text_bridge_runs_real_reader_without_core_or_native_field_repair() {
    use text_bridge::*;
    use triergon_subtitles::text;
    let raw = include_bytes!("fixtures/mkv/observed-11.mkv");
    let b = pool();
    let engine = Engine::matroska(
        &b,
        ID,
        SourceCapabilities {
            length: Some(raw.len() as u64),
            random_access: true,
        },
        triergon_matroska::Limits::default(),
        4096,
    )
    .unwrap();
    let mut p = Player::new(
        engine,
        SourceOwner::new(ByteSource::new(ID, Bytes::copy_from_slice(&b, raw).unwrap(), 7).unwrap()),
        Selection::All,
        PlayerPolicy::default(),
    )
    .unwrap();
    let out = collect(&mut p, 1).unwrap();
    let output=out.into_iter().find(|o|matches!(o.event(),RawEvent::Matroska(triergon_matroska::typed::Event::Packet(p))if p.configuration.tracks()[p.packet.track].codec.as_ref()=="S_TEXT/UTF8")).unwrap();
    assert_eq!(output.source_id(), ID);
    let source = TextSource::from_output(output).unwrap_or_else(|e| panic!("{:?}", e.error));
    let packet = source.matroska().unwrap();
    let expected = packet.packet.data.as_ref().to_vec();
    let pts = packet.source_pts().unwrap();
    let track = packet.configuration.tracks()[packet.packet.track].number;
    let mut bridge = TextBridge::new(
        &b,
        ID,
        track,
        packet.configuration.generation(),
        text::Limits::default(),
    );
    bridge.provide(source).unwrap();
    let result = loop {
        if let TextStep::Event(o) = bridge.step(&mut WorkBudget::new(1), &off()).unwrap() {
            break o;
        }
    };
    let text::Event::Cue(cue) = result.event else {
        panic!()
    };
    assert_eq!(cue.input.payload.as_ref(), expected);
    assert_eq!(cue.decoded_utf8.as_ref().unwrap().as_ref(), expected);
    assert_eq!(cue.start, pts);
    assert!(cue.input.timing.raw_clock().is_none());
    assert!(result.source.matroska().is_some());
}
#[test]
fn original_srt_webvtt_and_authored_ass_documents_keep_exact_source_bytes() {
    use text_bridge::*;
    use triergon_subtitles::text;
    for (format, encoding, bytes) in [
        (
            text::Format::SubRip,
            text::Encoding::Utf8,
            b"1\n00:00:00,000 --> 00:00:01,000\nsource \xe2\x82\xac\n".as_slice(),
        ),
        (
            text::Format::WebVtt,
            text::Encoding::Utf8,
            b"WEBVTT\n\n00:00.000 --> 00:01.000\nsource\n".as_slice(),
        ),
        (
            text::Format::OpaqueAss,
            text::Encoding::Opaque,
            b"0,0,Default,,0,0,0,,{\\i1}raw\\Nbytes".as_slice(),
        ),
    ] {
        let b = pool();
        let payload = Bytes::copy_from_slice(&b, bytes).unwrap();
        let input = text::Input {
            format,
            encoding,
            payload: payload.clone(),
            codec_id: None,
            codec_private: None,
            charset_label: None,
            addition: None,
            timing: text::Timing::unknown(),
            source_id: ID,
            generation: 7,
            sequence: 0,
            configuration_epoch: 0,
            container_position: None,
        };
        let mut bridge = TextBridge::new(&b, ID, 0, 7, text::Limits::default());
        bridge.provide(TextSource::document(&b, input)).unwrap();
        bridge.finish();
        let mut cues = 0;
        for _ in 0..100_000 {
            match bridge.step(&mut WorkBudget::new(1), &off()).unwrap() {
                TextStep::Event(o) => {
                    if let text::Event::Cue(c) = o.event {
                        assert_eq!(c.input.payload.as_ref(), bytes);
                        if format == text::Format::OpaqueAss {
                            assert!(c.decoded_utf8.is_none());
                        }
                        cues += 1;
                    }
                }
                TextStep::Finished => break,
                _ => {}
            }
        }
        assert_eq!(cues, 1);
        drop(bridge);
        drop(payload);
        assert_eq!(b.used(), Budget::control_storage_bytes());
    }
}

fn bitmap_scope() -> bitmap_bridge::BitmapScope {
    bitmap_bridge::BitmapScope {
        source: ID,
        track: 3,
        generation: 7,
        configuration: 2,
    }
}
fn bitmap_input(b: &Budget, raw: &[u8]) -> bitmap_bridge::BitmapInput<&'static str> {
    bitmap_bridge::BitmapInput {
        scope: bitmap_scope(),
        sequence: 11,
        raw: Bytes::copy_from_slice(b, raw).unwrap(),
        clock: triergon_subtitles::pgs::PacketClock {
            pts: None,
            dts: None,
            timebase: triergon_core::TimeBase::new(1, 90_000).unwrap(),
        },
        duration: None,
        metadata: "original caller packet metadata",
    }
}
#[test]
fn pgs_compatibility_bridge_keeps_the_first_event_and_fences_packet_owners() {
    use bitmap_bridge::*;
    use triergon_subtitles::pgs;
    let b = pool();
    let raw = include_bytes!("fixtures/bitmap/original-pgs-two-pixel.packet");
    let mut bridge = PgsBridge::new(
        &b,
        bitmap_scope(),
        pgs::DisplayLimits::default(),
        pgs::DisplayConfig::default(),
    )
    .unwrap();
    let cancel = off();
    let mut input = bitmap_input(&b, raw);
    input.clock.pts = Some(9_000);
    bridge
        .provide(input)
        .unwrap_or_else(|e| panic!("{:?}", e.error));
    let used = b.used();
    assert!(matches!(
        bridge.step(&mut WorkBudget::new(0), &cancel).unwrap(),
        PgsStep::Yield
    ));
    assert_eq!(b.used(), used);
    let retained = (0..100_000)
        .find_map(
            |_| match bridge.step(&mut WorkBudget::new(1), &cancel).unwrap() {
                PgsStep::Yield => None,
                PgsStep::Output(output) => Some(output),
                _ => panic!("expected native-only packet output"),
            },
        )
        .expect("bounded native-only output");
    let source = retained.packet.input();
    assert_eq!(source.sequence, 11);
    assert_eq!(source.clock.pts, Some(9_000));
    assert_eq!(source.clock.dts, None);
    assert!(
        source
            .raw
            .shares_storage_with(retained.observations.raw_packet())
    );
    let event = retained.observations.receiver_output().unwrap();
    assert_eq!(event.pts_microseconds, Some(100_000));
    assert_eq!(event.rectangles().len(), 1);
    assert_eq!(event.rectangles()[0].pixels().as_ref(), &[1, 2]);
    let Some(PgsQueueAction::Show(picture)) = retained.packet_clock_action().unwrap() else {
        panic!("PGS queue picture");
    };
    assert_eq!(picture.start_ns, 100_000_000);
    assert_eq!(picture.canvas, None);
    let rectangle = picture.rectangles().next().unwrap();
    assert!(std::ptr::eq(rectangle, &raw const event.rectangles()[0]));
    assert!(matches!(
        bridge.step(&mut WorkBudget::new(1), &cancel).unwrap(),
        PgsStep::NeedPacket
    ));

    let scope = BitmapScope {
        generation: 8,
        ..bitmap_scope()
    };
    assert!(
        bridge
            .reset(scope, &mut WorkBudget::new(1), &cancel)
            .unwrap()
    );
    let old = bitmap_input(&b, raw);
    let rejected = bridge.provide(old).err().unwrap();
    assert_eq!(
        rejected.error,
        C::StaleGeneration {
            expected: 8,
            received: 7
        }
    );
    drop(rejected);
    bridge.finish();
    assert!(matches!(
        bridge.step(&mut WorkBudget::new(8), &cancel).unwrap(),
        PgsStep::Finished
    ));
    drop(bridge);
    assert_eq!(retained.packet.input().scope, bitmap_scope());
    assert_eq!(
        retained.packet.input().metadata,
        "original caller packet metadata"
    );
    assert_eq!(
        retained
            .observations
            .receiver_output()
            .unwrap()
            .rectangles()[0]
            .pixels()
            .as_ref(),
        &[1, 2]
    );
    drop(retained);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}

#[test]
fn pgs_packet_clock_queue_distinguishes_a_clear_from_no_output() {
    use bitmap_bridge::*;
    use triergon_subtitles::pgs;

    let b = pool();
    let cancel = off();
    let mut bridge = PgsBridge::new(
        &b,
        bitmap_scope(),
        pgs::DisplayLimits::default(),
        pgs::DisplayConfig::default(),
    )
    .unwrap();
    let pcs = [0x16, 0, 11, 7, 128, 4, 56, 0x10, 0, 1, 0x80, 0, 0, 0];
    for (index, packet) in [&pcs[..], &[0x80, 0, 0]].into_iter().enumerate() {
        let mut input = bitmap_input(&b, packet);
        input.sequence += index as u64;
        input.clock.dts = Some(18_000);
        bridge
            .provide(input)
            .unwrap_or_else(|e| panic!("{:?}", e.error));
        let output = loop {
            match bridge.step(&mut WorkBudget::new(32), &cancel).unwrap() {
                PgsStep::Yield => (),
                PgsStep::Output(output) => break output,
                _ => panic!("expected native-only packet output"),
            }
        };
        let action = output.packet_clock_action().unwrap();
        if index == 0 {
            assert!(action.is_none());
        } else {
            assert!(matches!(
                action,
                Some(PgsQueueAction::Clear { at_ns: 200_000_000 })
            ));
        }
        assert!(matches!(
            bridge.step(&mut WorkBudget::new(32), &cancel).unwrap(),
            PgsStep::NeedPacket
        ));
    }
}

#[test]
fn bitmap_foreign_pool_rejection_returns_original_packet_without_poisoning_session() {
    use bitmap_bridge::*;
    use triergon_subtitles::pgs;
    let b = pool();
    let foreign = pool();
    let raw = include_bytes!("fixtures/bitmap/original-pgs-two-pixel.packet");
    let mut bridge = PgsBridge::new(
        &b,
        bitmap_scope(),
        pgs::DisplayLimits::default(),
        pgs::DisplayConfig::default(),
    )
    .unwrap();
    let input = bitmap_input(&foreign, raw);
    let payload = input.raw.clone();
    let error = bridge.provide(input).err().unwrap();
    assert_eq!(error.error, C::ForeignPool);
    assert!(error.input.raw.shares_storage_with(&payload));
    bridge
        .provide(bitmap_input(&b, raw))
        .unwrap_or_else(|e| panic!("{:?}", e.error));
    let before = b.used();
    assert!(matches!(
        bridge.step(&mut WorkBudget::new(0), &off()).unwrap(),
        PgsStep::Yield
    ));
    assert_eq!(b.used(), before);
    loop {
        if let PgsStep::Output(_) = bridge.step(&mut WorkBudget::new(1), &off()).unwrap() {
            break;
        }
    }
}
#[test]
fn bitmap_reset_uses_new_source_fence_and_keeps_old_output() {
    use bitmap_bridge::*;
    use triergon_subtitles::pgs;
    let b = pool();
    let raw = include_bytes!("fixtures/bitmap/original-pgs-two-pixel.packet");
    let mut bridge = PgsBridge::new(
        &b,
        bitmap_scope(),
        pgs::DisplayLimits::default(),
        pgs::DisplayConfig::default(),
    )
    .unwrap();
    bridge
        .provide(bitmap_input(&b, raw))
        .unwrap_or_else(|e| panic!("{:?}", e.error));
    let old = loop {
        if let PgsStep::Output(o) = bridge.step(&mut WorkBudget::new(1), &off()).unwrap() {
            break o;
        }
    };
    let mut scope = bitmap_scope();
    scope.generation = 8;
    assert!(
        !bridge
            .reset(scope, &mut WorkBudget::new(0), &off())
            .unwrap()
    );
    assert!(
        bridge
            .reset(scope, &mut WorkBudget::new(1), &off())
            .unwrap()
    );
    let rejected = bridge.provide(bitmap_input(&b, raw)).err().unwrap();
    assert!(matches!(
        rejected.error,
        C::StaleGeneration {
            expected: 8,
            received: 7
        }
    ));
    let mut input = bitmap_input(&b, raw);
    input.scope = scope;
    bridge
        .provide(input)
        .unwrap_or_else(|e| panic!("{:?}", e.error));
    let next = loop {
        if let PgsStep::Output(o) = bridge.step(&mut WorkBudget::new(1), &off()).unwrap() {
            break o;
        }
    };
    assert_eq!(old.packet.input().scope.generation, 7);
    assert_eq!(next.packet.input().scope.generation, 8);
    let rectangles = old.observations.receiver_output().unwrap().rectangles();
    assert_eq!(rectangles[0].pixels().as_ref(), &[1, 2]);
}
/// Step a DVD receiver until it outputs the pending packet.
fn dvd_next<M>(
    receiver: &mut bitmap_bridge::DvdReceiver<M>,
    quantum: u64,
) -> Result<bitmap_bridge::DvdOutput<M>> {
    for _ in 0..100_000 {
        match receiver.step(&mut WorkBudget::new(quantum), &off())? {
            bitmap_bridge::DvdReceiverStep::Yield => {}
            bitmap_bridge::DvdReceiverStep::Output(output) => return Ok(output),
            _ => panic!("DVD receiver left the admitted packet"),
        }
    }
    panic!("DVD receiver exhausted work limit");
}

#[test]
fn dvd_receiver_presents_the_first_unit_of_a_packet_and_discards_the_tail() {
    use bitmap_bridge::*;
    let raw = include_bytes!("fixtures/bitmap/original-dvd-two-units-tail.spu");
    let b = pool();
    let mut receiver = DvdReceiver::new(&b, bitmap_scope(), DvdBridgeConfig::default()).unwrap();
    assert!(matches!(
        receiver.step(&mut WorkBudget::new(0), &off()).unwrap(),
        DvdReceiverStep::NeedPacket
    ));
    receiver
        .provide(bitmap_input(&b, raw))
        .unwrap_or_else(|e| panic!("{:?}", e.error));
    let before = b.used();
    assert!(matches!(
        receiver.step(&mut WorkBudget::new(0), &off()).unwrap(),
        DvdReceiverStep::Yield
    ));
    // The decoder keeps a copy of the yielded unit to resume from.
    assert_eq!(b.used() - before, raw.len());
    let out = dvd_next(&mut receiver, 1).unwrap();
    assert_eq!(out.packet.input().raw.as_ref(), raw);
    assert_eq!(out.packet.input().clock.pts, None);
    let picture = out.picture.as_ref().unwrap();
    assert_eq!(
        (picture.x, picture.y, picture.width, picture.height),
        (0, 0, 1, 2)
    );
    assert_eq!((picture.start_ms, picture.end_ms), (193, 0));
    assert_eq!(
        picture.palette(),
        [0xff00_0000, 0xff54_5454, 0xffa9_a9a9, 0xfffe_fefe]
    );
    assert_eq!(
        (picture.row(0), picture.row(1), picture.row(2)),
        (Some(&[0][..]), Some(&[1][..]), None)
    );
    drop((out, receiver));
    assert_eq!(b.used(), Budget::control_storage_bytes());
}

#[test]
fn dvd_receiver_unit_limit_is_an_error_that_drops_the_packet() {
    use bitmap_bridge::*;
    use triergon_subtitles::compat::dvd::Limits;
    let b = pool();
    let raw = include_bytes!("fixtures/bitmap/original-dvd-two-units-tail.spu");
    let config = DvdBridgeConfig {
        limits: Limits {
            max_unit: raw.len() - 1,
            ..Limits::default()
        },
        ..DvdBridgeConfig::default()
    };
    let mut receiver = DvdReceiver::new(&b, bitmap_scope(), config).unwrap();
    receiver
        .provide(bitmap_input(&b, raw))
        .unwrap_or_else(|e| panic!("{:?}", e.error));
    assert!(matches!(
        receiver.step(&mut WorkBudget::new(1), &off()),
        Err(C::Limit {
            requested: 62,
            maximum: 61
        })
    ));
    assert!(matches!(
        receiver.step(&mut WorkBudget::new(1), &off()).unwrap(),
        DvdReceiverStep::NeedPacket
    ));
    drop(receiver);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}

#[test]
fn dvd_presentation_preserves_precise_pts_and_explicit_clear() {
    use bitmap_bridge::*;
    use triergon_core::Time;

    let bytes = include_bytes!("fixtures/bitmap/original-dvd-two-units-tail.spu");
    for (pts, dts, stop, start_ns) in [
        (Some(90_001), Some(-42), Some(90_u16), 1_000_011_111),
        (None, Some(-42), Some(0), -466_667),
        (Some(i64::MIN), Some(-42), None, -466_667),
        (None, Some(i64::MIN), None, 0),
        (Some(0), Some(-42), None, 0),
        (Some(i64::MAX), None, None, i64::MIN),
    ] {
        let b = pool();
        let mut raw = bytes[..30].to_vec();
        if let Some(date) = stop {
            raw[0..2].copy_from_slice(&36_u16.to_be_bytes());
            raw[8..10].copy_from_slice(&30_u16.to_be_bytes());
            raw.extend_from_slice(&date.to_be_bytes());
            raw.extend_from_slice(&[0, 30, 2, 0xff]);
        }
        let mut input = bitmap_input(&b, &raw);
        input.clock.pts = pts;
        input.clock.dts = dts;
        let mut receiver =
            DvdReceiver::new(&b, bitmap_scope(), DvdBridgeConfig::default()).unwrap();
        receiver
            .provide(input)
            .unwrap_or_else(|e| panic!("{:?}", e.error));
        let out = dvd_next(&mut receiver, 32).unwrap();
        let presentation = out.presentation().unwrap().unwrap();
        let at = |ms: u32| {
            pts.map(|pts| Time::new(i128::from(pts) + i128::from(ms) * 90, 90_000).unwrap())
        };
        assert_eq!(
            presentation.show,
            DvdDisplayBoundary {
                milliseconds: 193,
                at: at(193),
            }
        );
        let end = stop.map_or(0, |date| u32::from(date) * 1024 / 90);
        assert_eq!(
            presentation.clear,
            (end != 0).then(|| DvdDisplayBoundary {
                milliseconds: end,
                at: at(end),
            })
        );
        assert!(!presentation.picture.forced);
        assert_eq!(presentation.picture.row(1).unwrap(), &[1]);
        let canvas = [720, 480];
        let queued = out.packet_clock_picture(Some(canvas)).unwrap().unwrap();
        assert_eq!(queued.canvas, Some(canvas));
        assert_eq!(queued.start_ns, start_ns);
        assert_eq!(queued.picture.palette(), presentation.picture.palette());
        let invalid_canvas = [159, 480];
        assert_eq!(
            out.packet_clock_picture(Some(invalid_canvas))
                .unwrap()
                .unwrap()
                .canvas,
            None
        );
        drop((out, receiver));
        assert_eq!(b.used(), Budget::control_storage_bytes());
    }
}
#[test]
fn text_document_foreign_pool_returns_original_source_and_generation_fence() {
    use text_bridge::*;
    use triergon_subtitles::text;
    let b = pool();
    let foreign = pool();
    let payload = Bytes::copy_from_slice(&foreign, b"exact source").unwrap();
    let input = text::Input {
        format: text::Format::Utf8Packet,
        encoding: text::Encoding::Utf8,
        payload: payload.clone(),
        codec_id: None,
        codec_private: None,
        charset_label: None,
        addition: None,
        timing: text::Timing::unknown(),
        source_id: ID,
        generation: 7,
        sequence: 0,
        configuration_epoch: 0,
        container_position: None,
    };
    let mut bridge = TextBridge::new(&b, ID, 0, 7, text::Limits::default());
    let error = bridge
        .provide(TextSource::document(&b, input))
        .err()
        .unwrap();
    assert!(matches!(error.error, C::ForeignPool));
    drop(error);
    drop(payload);
    assert_eq!(foreign.used(), Budget::control_storage_bytes());
    assert_eq!(b.used(), Budget::control_storage_bytes());
}

#[test]
fn pause_lifecycle_ended_refusal_precedes_sink_control() {
    let (mut p, _, _) = pump();
    let mut acknowledged = 0;
    let mut ended = false;
    for _ in 0..100_000 {
        let step = p.step(&mut WorkBudget::new(1), &off()).unwrap();
        let sink = p.sink_mut().unwrap();
        while acknowledged < sink.accepted.len() {
            sink.completed
                .push(sink.accepted[acknowledged].token().clone());
            acknowledged += 1;
        }
        if step == PumpStep::Ended {
            ended = true;
            break;
        }
    }
    assert!(ended);
    let before = p.sink().unwrap().pauses.len();
    assert!(p.pause(true).is_err());
    assert_eq!(
        p.sink().unwrap().pauses.len(),
        before,
        "an ended source must refuse pause before calling the sink"
    );
}
#[test]
fn pause_lifecycle_failed_sink_control_quiesces_source() {
    let (mut p, _, calls) = pump();
    until_feed(&mut p);
    p.sink_mut().unwrap().fail_pause = true;
    assert!(matches!(p.pause(true), Err(C::Transport)));
    assert_eq!(
        p.player().unwrap().state(),
        State::Stopped,
        "uncertain sink control must quiesce the producer before returning"
    );
    let calls = calls.lock().unwrap();
    assert_eq!(&calls[calls.len() - 2..], &["abort", "close"]);
}

#[test]
fn progressive_pressure_at_each_poll_retries_without_losing_fragment_samples() {
    let (mut p, b, _, _) = player(
        include_bytes!("fixtures/mp4/f-explicit.mp4"),
        true,
        ProgressiveLimits::default(),
    );
    let mut out = Vec::new();
    let mut ended = false;
    let mut refusals = 0;
    for _ in 0..1_000_000 {
        let hold = b.reserve(b.available()).unwrap();
        let step = p.advance(&mut WorkBudget::new(1), &off());
        drop(hold);
        let mut step = step.unwrap();
        if matches!(step, Progress::Backpressure { .. }) {
            refusals += 1;
            step = p.advance(&mut WorkBudget::new(1), &off()).unwrap();
        }
        while let Some(o) = p.pop() {
            out.push(o);
        }
        if matches!(step, Progress::Ended) {
            ended = true;
            break;
        }
    }
    assert!(ended);
    assert!(refusals > 0);
    assert_eq!(
        packets(&out).len(),
        6,
        "retry must preserve every indexed source sample"
    );
    for (at, packet) in packets(&out).iter().enumerate() {
        assert_eq!(packet.source.sample.ordinal, at as u64 + 1);
    }
    drop(out);
    drop(p);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}

struct SequentialStartup {
    source: ByteSource,
    next: Arc<AtomicUsize>,
}
impl Source for SequentialStartup {
    fn poll(
        &mut self,
        request: &triergon_core::InputRequest,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<SourceReply> {
        let at = self.next.load(Ordering::Relaxed);
        assert_eq!(
            request.offset, at as u64,
            "startup bytes must never be read twice"
        );
        let reply = self.source.poll(request, work, cancel)?;
        if let SourceReply::Data(window) = &reply {
            self.next
                .store(at + window.bytes().len(), Ordering::Relaxed);
        }
        Ok(reply)
    }
    fn invalidate(&mut self) {
        panic!("native prefix must preserve the same source generation");
    }
    fn abort(&mut self) {
        self.source.abort();
    }
    fn close(&mut self) {
        self.source.close();
    }
}

fn long_m2ts() -> Vec<u8> {
    let mut data = include_bytes!("fixtures/ts/m2ts192-opaque-prefix.m2ts").to_vec();
    assert_eq!(data.len(), 3 * 192);
    let mut null = [0xff; 192];
    null[..8].copy_from_slice(&[0, 0, 0, 0, 0x47, 0x1f, 0xff, 0x10]);
    let mut late: [u8; 192] = data[384..].try_into().unwrap();
    late[7] = (late[7] & 0xf0) | ((late[7] + 1) & 15);
    for _ in 3..95 {
        data.extend_from_slice(&null);
    }
    data.extend_from_slice(&late);
    data
}

fn prefix_setup(
    raw: &[u8],
    known: bool,
    quantum: usize,
) -> (
    native_startup::NativeStartup<SequentialStartup>,
    Budget,
    Arc<AtomicUsize>,
) {
    let budget = pool();
    let next = Arc::new(AtomicUsize::new(0));
    let source = SequentialStartup {
        source: ByteSource::new(ID, Bytes::copy_from_slice(&budget, raw).unwrap(), quantum)
            .unwrap(),
        next: next.clone(),
    };
    let input = InputCursor::new(
        &budget,
        ID,
        SourceCapabilities {
            length: known.then_some(raw.len() as u64),
            random_access: false,
        },
        16 << 10,
    )
    .unwrap();
    let startup = native_startup::NativeStartup::new_m2ts_prefix_v2(SelectedInput {
        input,
        source: SourceOwner::new(source),
        format: Format::Ts(triergon_mpegts::PacketFormat::M2ts192),
    })
    .unwrap();
    (startup, budget, next)
}

fn open_prefix(
    mut startup: native_startup::NativeStartup<SequentialStartup>,
) -> Player<SequentialStartup> {
    for _ in 0..30_000 {
        if startup.advance(&mut WorkBudget::new(31), &off()).unwrap()
            == native_startup::StartupStep::Ready
        {
            let prepared = startup.finish().unwrap();
            assert!(prepared.context().is_prefix());
            assert_eq!(prepared.context().inspected_bytes(), 8192);
            let mut player = prepared
                .into_player(
                    1,
                    triergon_mpegts::ParseLimits::default(),
                    triergon_mpegts::clock::SourceTimePolicy::Unanchored,
                    TsNativeSelection {
                        pid: 256,
                        carriage: triergon_mpegts::compat::pes::RegisteredCarriage::Avc,
                    },
                    Selection::All,
                    PlayerPolicy::default(),
                )
                .unwrap();
            player
                .set_clock_view(ClockChoice::ExplicitNativeProjection)
                .unwrap();
            return player;
        }
    }
    panic!("prefix startup stalled");
}

#[test]
fn native_m2ts_prefix_preserves_the_same_sequential_source_through_late_pes() {
    let raw = long_m2ts();
    for (known, quantum) in [(true, 16384), (false, 13)] {
        let (startup, budget, next) = prefix_setup(&raw, known, quantum);
        let mut player = open_prefix(startup);
        assert_eq!(next.load(Ordering::Relaxed), 8192);
        let output = collect(&mut player, 1).unwrap();
        let positions: Vec<_> = output
            .iter()
            .filter_map(|item| match item.event() {
                RawEvent::NativeTs { packet, .. } => {
                    Some((packet.position, packet.source_origin.offset))
                }
                RawEvent::NativeRefusal { error, .. } => {
                    panic!("unexpected native refusal {error:?}")
                }
                _ => None,
            })
            .collect();
        assert_eq!(positions, [(384, 384), (95 * 192, 95 * 192)]);
        assert_eq!(next.load(Ordering::Relaxed), raw.len());
        drop(output);
        drop(player);
        assert_eq!(budget.used(), Budget::control_storage_bytes());
    }
}

#[test]
fn native_m2ts_prefix_pressure_keeps_the_completed_reply() {
    let raw = long_m2ts();
    let (mut startup, budget, next) = prefix_setup(&raw, false, 13);
    let lease = budget.reserve(budget.available() - 32).unwrap();
    assert!(matches!(
        startup.advance(&mut WorkBudget::new(31), &off()),
        Err(C::Backpressure { .. })
    ));
    assert_eq!(next.load(Ordering::Relaxed), 13);
    drop(lease);
    assert_eq!(
        startup.advance(&mut WorkBudget::new(31), &off()).unwrap(),
        native_startup::StartupStep::Yield
    );
    assert_eq!(next.load(Ordering::Relaxed), 13);
    let mut player = open_prefix(startup);
    assert_eq!(next.load(Ordering::Relaxed), 8192);
    assert!(collect(&mut player, 1).is_ok());
    drop(player);
    assert_eq!(budget.used(), Budget::control_storage_bytes());
}

#[test]
fn native_m2ts_prefix_requires_the_actual_complete_unambiguous_window() {
    let mut ambiguous = long_m2ts();
    for i in 1..=7 {
        ambiguous[188 * i] = 0x47;
    }
    for (raw, message) in [
        (&ambiguous[..], "native M2TS prefix grid is ambiguous"),
        (&ambiguous[..8000], "short source for native M2TS prefix"),
    ] {
        let (mut startup, budget, _) = prefix_setup(raw, false, 1024);
        let failure = loop {
            match startup.advance(&mut WorkBudget::new(1024), &off()) {
                Ok(native_startup::StartupStep::Yield) => {}
                Err(error) => break error,
                _ => panic!("unproven prefix became ready"),
            }
        };
        assert!(matches!(failure, C::Unsupported(error) if error == message));
        drop(startup);
        assert_eq!(budget.used(), Budget::control_storage_bytes());
    }
}

#[test]
fn native_m2ts_prefix_grid_loss_preserves_source_and_refuses_native_tail() {
    let mut raw = long_m2ts();
    raw[50 * 192 + 4] = 0x46;
    let (startup, _, _) = prefix_setup(&raw, false, 512);
    let mut player = open_prefix(startup);
    let output = collect(&mut player, 1).unwrap();
    assert_eq!(
        output
            .iter()
            .filter(|o| matches!(o.event(), RawEvent::NativeTs { .. }))
            .count(),
        1
    );
    assert_eq!(
        output
            .iter()
            .filter(|o| matches!(
                o.event(),
                RawEvent::NativeRefusal {
                    error: C::Unsupported("native M2TS carrier grid lost"),
                    ..
                }
            ))
            .count(),
        1
    );
    assert_eq!(
        output
            .iter()
            .filter(|o| matches!(
                o.event(),
                RawEvent::Ts(TsEvent {
                    source: triergon_mpegts::TraceEvent::Pes(_),
                    ..
                })
            ))
            .count(),
        2
    );
}

#[test]
fn matroska_bitmap_tracks_feed_native_receivers_without_copying_frames() {
    use bitmap_bridge::*;
    use matroska_feed::{
        MatroskaBitmapClockProbe, MatroskaBitmapCodec, MatroskaBitmapTrack, MatroskaDvbStep,
    };
    use triergon_matroska::typed::Event;
    use triergon_subtitles::{dvb, pgs};

    fn element(id: &[u8], data: &[u8]) -> Vec<u8> {
        let mut out = id.to_vec();
        if data.len() < 127 {
            out.push(0x80 | data.len() as u8);
        } else {
            assert!(data.len() < 16_383);
            out.extend_from_slice(&(0x4000 | data.len() as u16).to_be_bytes());
        }
        out.extend_from_slice(data);
        out
    }
    fn track(number: u8, kind: u8, codec: &[u8], extra: &[u8]) -> Vec<u8> {
        element(
            &[0xae],
            &[
                element(&[0xd7], &[number]),
                element(&[0x73, 0xc5], &[number]),
                element(&[0x83], &[kind]),
                element(&[0x86], codec),
                extra.to_vec(),
            ]
            .concat(),
        )
    }
    fn block(number: u8, delta: i16, data: &[u8]) -> Vec<u8> {
        element(
            &[0xa3],
            &[&[0x80 | number], &delta.to_be_bytes()[..], &[0x80], data].concat(),
        )
    }
    fn group(number: u8, delta: i16, duration: u8, data: &[u8]) -> Vec<u8> {
        let payload = [&[0x80 | number], &delta.to_be_bytes()[..], &[0], data].concat();
        element(
            &[0xa0],
            &[element(&[0xa1], &payload), element(&[0x9b], &[duration])].concat(),
        )
    }

    let pgs_frame = include_bytes!("fixtures/bitmap/original-pgs-two-pixel.packet");
    let mut remaining = pgs_frame.as_slice();
    let mut pgs_segments = Vec::new();
    let mut pgs_blocks = Vec::new();
    while !remaining.is_empty() {
        let length = usize::from(u16::from_be_bytes([remaining[1], remaining[2]])) + 3;
        let (segment, tail) = remaining.split_at(length);
        pgs_segments.push(segment);
        pgs_blocks.extend(if tail.is_empty() {
            group(3, -5, 25, segment)
        } else {
            block(3, -5, segment)
        });
        remaining = tail;
    }
    let dvd_frame = &include_bytes!("fixtures/bitmap/original-dvd-two-units-tail.spu")[..30];
    let pes = include_bytes!("../../mpegts/tests/data/dvb/dvb2-literals-fields.pes-data");
    let dvb_frame = &pes[2..pes.len() - 1];
    // A new page version without regions: a mode change that clears the page.
    let dvb_clear = [0x0f, 0x10, 0, 1, 0, 2, 2, 0x1b, 0x0f, 0x80, 0, 1, 0, 0];
    let dvb_private = [0, 1, 0, 2, 0x10];
    let palette = b"size: 720x480\npalette: 112233, 445566, 778899, 000000, 000000, 000000, 000000, 000000, 000000, 000000, 000000, 000000, 000000, 000000, 000000, 000000\n";
    let adler = dvd_frame.iter().fold((1_u32, 0_u32), |(a, b), &byte| {
        let a = (a + u32::from(byte)) % 65_521;
        (a, (b + a) % 65_521)
    });
    let mut compressed = vec![0x78, 1, 1];
    let len = dvd_frame.len() as u16;
    compressed.extend_from_slice(&len.to_le_bytes());
    compressed.extend_from_slice(&(!len).to_le_bytes());
    compressed.extend_from_slice(dvd_frame);
    compressed.extend_from_slice(&((adler.1 << 16) | adler.0).to_be_bytes());
    let zlib = element(
        &[0x6d, 0x80],
        &element(&[0x62, 0x40], &element(&[0x50, 0x34], &[])),
    );
    let data = [
        element(
            &[0x1a, 0x45, 0xdf, 0xa3],
            &element(&[0x42, 0x82], b"matroska"),
        ),
        element(
            &[0x18, 0x53, 0x80, 0x67],
            &[
                element(
                    &[0x16, 0x54, 0xae, 0x6b],
                    &[
                        track(1, 1, b"V_VP9", &[]),
                        track(3, 17, b"S_HDMV/PGS", &[]),
                        track(
                            7,
                            17,
                            b"S_VOBSUB",
                            &[element(&[0x63, 0xa2], palette), zlib].concat(),
                        ),
                        track(8, 17, b"S_DVBSUB", &element(&[0x63, 0xa2], &dvb_private)),
                        track(
                            9,
                            17,
                            b"S_DVBSUB",
                            &element(&[0x63, 0xa2], &dvb_private[..4]),
                        ),
                    ]
                    .concat(),
                ),
                element(
                    &[0x1f, 0x43, 0xb6, 0x75],
                    &[
                        element(&[0xe7], &[4]),
                        block(1, 0, &[0]),
                        pgs_blocks,
                        block(7, 5, &compressed),
                        group(8, 15, 42, dvb_frame),
                        block(8, 90, &dvb_clear),
                    ]
                    .concat(),
                ),
            ]
            .concat(),
        ),
    ]
    .concat();
    let pool = pool();
    let engine = Engine::matroska(
        &pool,
        ID,
        SourceCapabilities {
            length: Some(data.len() as u64),
            random_access: true,
        },
        triergon_matroska::Limits::default(),
        4096,
    )
    .unwrap();
    let source = ByteSource::new(ID, Bytes::copy_from_slice(&pool, &data).unwrap(), 7).unwrap();
    let mut player = Player::new(
        engine,
        SourceOwner::new(source),
        Selection::All,
        PlayerPolicy::default(),
    )
    .unwrap();
    let outputs = collect(&mut player, 1).unwrap();
    let configuration = outputs
        .iter()
        .find(|o| matches!(o.event(), RawEvent::Matroska(Event::Configuration(_))))
        .unwrap();
    let pgs_track = MatroskaBitmapTrack::new(configuration, 3).unwrap();
    let dvd_track = MatroskaBitmapTrack::new(configuration, 7).unwrap();
    let dvb_track = MatroskaBitmapTrack::new(configuration, 8).unwrap();
    assert_eq!(pgs_track.codec(), MatroskaBitmapCodec::Pgs);
    assert_eq!(dvd_track.codec(), MatroskaBitmapCodec::VobSub);
    assert_eq!(dvb_track.codec(), MatroskaBitmapCodec::Dvb);
    assert!(MatroskaBitmapTrack::new(configuration, 1).is_err());
    assert!(matches!(
        MatroskaBitmapTrack::new(configuration, 9),
        Err(C::Truncated)
    ));

    let dvd_input = outputs
        .iter()
        .find_map(|o| dvd_track.packet(o, 2).unwrap())
        .unwrap();
    let check_alias = |input: &BitmapInput<matroska_feed::MatroskaBitmapSource>, frame: &[u8]| {
        assert_eq!(&*input.raw, frame);
        assert!(
            input.raw.shares_storage_with(
                &input
                    .metadata
                    .packet
                    .packet
                    .data
                    .shared_bytes()
                    .unwrap()
                    .unwrap()
            )
        );
    };
    check_alias(&dvd_input, dvd_frame);
    assert_eq!(dvd_input.clock.pts, Some(9_000_000));
    assert_eq!(dvd_input.metadata.delivery, Delivery::Present);

    let mut pgs = PgsBridge::new(
        &pool,
        pgs_track.scope(),
        pgs::DisplayLimits::default(),
        pgs::DisplayConfig::default(),
    )
    .unwrap();
    let mut pgs_count = 0;
    for (index, output) in outputs.iter().enumerate() {
        let Some(input) = pgs_track.packet(output, index as u64).unwrap() else {
            continue;
        };
        check_alias(&input, pgs_segments[pgs_count]);
        pgs_count += 1;
        // The queue sees unknown PTS, while the owner keeps negative source time.
        assert_eq!(input.clock.pts, None);
        assert_eq!(
            input.metadata.packet.source_pts().unwrap(),
            Some(Time::new(-1, 1000).unwrap())
        );
        pgs.provide(input)
            .unwrap_or_else(|e| panic!("{:?}", e.error));
        let mut showed = false;
        loop {
            match pgs.step(&mut WorkBudget::new(1), &off()).unwrap() {
                PgsStep::Yield => {}
                PgsStep::Output(output) => {
                    if let Some(PgsQueueAction::Show(picture)) =
                        output.packet_clock_action().unwrap()
                    {
                        assert_eq!(picture.start_ns, 0);
                        assert_eq!(output.packet.input().duration, Some(25_000_000));
                        assert_eq!(pgs_count, pgs_segments.len());
                        showed = true;
                    }
                }
                PgsStep::NeedPacket => break,
                PgsStep::Finished => panic!("unexpected PGS receiver state"),
            }
        }
        assert_eq!(showed, pgs_count == pgs_segments.len());
    }
    assert_eq!(pgs_count, pgs_segments.len());

    let settings = triergon_subtitles::compat::dvd::Settings::from_configuration(palette);
    assert_eq!(settings.palette.unwrap()[0], 0x112233);
    let mut dvd = DvdReceiver::new(
        &pool,
        dvd_track.scope(),
        DvdBridgeConfig {
            settings,
            ..DvdBridgeConfig::default()
        },
    )
    .unwrap();
    dvd.provide(dvd_input)
        .unwrap_or_else(|e| panic!("{:?}", e.error));
    let dvd_output = dvd_next(&mut dvd, 1).unwrap();
    let queued = dvd_output
        .packet_clock_picture(Some([720, 480]))
        .unwrap()
        .unwrap();
    assert_eq!(queued.start_ns, 9_000_000);
    assert_eq!(queued.canvas, Some([720, 480]));
    assert_eq!(queued.picture.row(1).unwrap(), &[1]);

    let config = dvb_track.dvb_configuration().unwrap();
    assert_eq!(&**config.original(), &dvb_private);
    assert_eq!(config.service().composition_page, 1);
    assert_eq!(config.service().ancillary_page, 2);
    let mut decoder = dvb_track.dvb_decoder().unwrap();
    let mut dvb_clock = MatroskaBitmapClockProbe::new(&dvb_track, &outputs, outputs.len()).unwrap();
    while !dvb_clock.poll(&mut WorkBudget::new(1), &off()).unwrap() {}
    let mut dvb_actions = Vec::new();
    let mut dvb_inputs = Vec::new();
    for (index, output) in outputs.iter().enumerate() {
        let Some(mut input) = dvb_clock.dvb_packet(index, index as u64).unwrap() else {
            continue;
        };
        let raw = dvb_track.dvb_packet(output, index as u64).unwrap().unwrap();
        assert_eq!(raw.packet.pts, input.packet.pts);
        assert!(raw.packet.source.unwrap().timing().dts.is_none());
        let source = input.packet.source.as_ref().unwrap();
        let timing = source.timing();
        assert_eq!(timing.pts, timing.dts);
        let data = input
            .source
            .packet
            .packet
            .data
            .shared_bytes()
            .unwrap()
            .unwrap();
        assert!(input.packet.payload.shares_storage_with(&data));
        assert_eq!(input.packet.envelope, dvb::PacketEnvelope::Segments);
        if input.packet.pts == Some(Time::new(19, 1000).unwrap()) {
            assert_eq!(timing.duration.unwrap().ticks, 42_000_000);
            assert_eq!(source.container_position().unwrap().source, ID);
        }
        let mut work = WorkBudget::new(u64::MAX);
        match input.decode(&mut decoder, &mut work, &off()).unwrap() {
            MatroskaDvbStep::Action(dvb_ts::DvbQueueAction::Show(picture)) => {
                assert_eq!(picture.start_ns, 19_000_000);
                assert_eq!(
                    picture.rectangles().next().unwrap().pixels,
                    include_bytes!(
                        "../../subtitles/tests/boundary-fixtures/dvb2-literals-fields.indices"
                    )
                );
                dvb_actions.push("show");
                // The same packet as preroll updates the decoder without an action.
                input.source.delivery = Delivery::DecodeOnly {
                    until: Time::new(20, 1000).unwrap(),
                };
                let mut preroll = dvb_track.dvb_decoder().unwrap();
                let mut work = WorkBudget::new(u64::MAX);
                assert!(matches!(
                    input.decode(&mut preroll, &mut work, &off()).unwrap(),
                    MatroskaDvbStep::Decoded
                ));
            }
            MatroskaDvbStep::Action(dvb_ts::DvbQueueAction::Clear { at_ns }) => {
                assert_eq!(at_ns, 94_000_000);
                dvb_actions.push("clear");
            }
            step => panic!("DVB packet was not queued: {step:?}"),
        }
        dvb_inputs.push(input);
    }
    assert_eq!(dvb_actions, ["show", "clear"]);
    // A packet the decoder keeps after a cancellation is resumed only by
    // itself: the next packet drops it and is decoded on its own.
    let [shown, cleared] = &dvb_inputs[..] else {
        panic!("two DVB packets")
    };
    let mut decoder = dvb_track.dvb_decoder().unwrap();
    assert!(matches!(
        shown.decode(
            &mut decoder,
            &mut WorkBudget::new(u64::MAX),
            &AtomicBool::new(true)
        ),
        Err(Error::Cancelled)
    ));
    assert!(decoder.has_kept_packet());
    assert!(matches!(
        cleared
            .decode(&mut decoder, &mut WorkBudget::new(u64::MAX), &off())
            .unwrap(),
        MatroskaDvbStep::Action(dvb_ts::DvbQueueAction::Clear { at_ns: 94_000_000 })
    ));
    assert!(!decoder.has_kept_packet());

    let select = Selection::Tracks {
        video: Some(TrackKey::Matroska(1)),
        audio: None,
        subtitle: Some(TrackKey::Matroska(7)),
    };
    let target = Time::new(10, 1000).unwrap();
    assert!(
        player
            .seek_and_select(
                TrackKey::Matroska(1),
                target,
                select,
                &mut WorkBudget::new(4096),
                &off(),
            )
            .unwrap()
    );
    let after_seek = collect(&mut player, 1).unwrap();
    let next_dvd = after_seek
        .iter()
        .find(|output| output.event().track() == Some(TrackKey::Matroska(7)))
        .unwrap();
    assert!(matches!(
        dvd_track.packet(next_dvd, 3),
        Err(C::StaleGeneration { .. })
    ));
    let next_track = MatroskaBitmapTrack::new(next_dvd, 7).unwrap();
    let old_dvd = outputs
        .iter()
        .find(|output| output.event().track() == Some(TrackKey::Matroska(7)))
        .unwrap();
    assert!(matches!(
        next_track.packet(old_dvd, 3),
        Err(C::StaleGeneration { .. })
    ));
    let next_input = next_track.packet(next_dvd, 3).unwrap().unwrap();
    assert_eq!(next_input.clock.pts, Some(9_000_000));
    assert_eq!(
        next_input.metadata.delivery,
        Delivery::DecodeOnly { until: target }
    );
    assert!(after_seek.iter().all(|output| {
        pgs_track
            .packet(output, 4)
            .is_ok_and(|input| input.is_none())
    }));
}

#[test]
fn matroska_bitmap_probe_matches_ffmpeg_startup_clocks() {
    use bitmap_bridge::{PgsBridge, PgsQueueAction, PgsStep};
    use matroska_feed::{MatroskaBitmapClockProbe, MatroskaBitmapTrack};
    use triergon_matroska::typed::Event;
    use triergon_subtitles::pgs;

    type Timing = (Option<i64>, Option<i64>);
    let timed = |pts, duration| (Some(pts), Some(duration));
    let cases: [(&[u8], [Timing; 5]); 7] = [
        (
            include_bytes!("fixtures/matroska-subtitle-probe/observed-relative.mkv"),
            [
                timed(0, 25),
                timed(25, 25),
                timed(50, 25),
                timed(75, 25),
                timed(100, 25),
            ],
        ),
        (
            include_bytes!("fixtures/matroska-subtitle-probe/known-anchor.mkv"),
            [
                timed(100, 25),
                timed(125, 25),
                timed(150, 25),
                timed(175, 25),
                timed(200, 25),
            ],
        ),
        (
            include_bytes!("fixtures/matroska-subtitle-probe/duration-after-known.mkv"),
            [
                timed(150, 25),
                timed(175, 25),
                (Some(200), None),
                (Some(201), None),
                timed(202, 25),
            ],
        ),
        (
            include_bytes!("fixtures/matroska-subtitle-probe/all-unknown.mkv"),
            [(None, None); 5],
        ),
        (
            include_bytes!("fixtures/matroska-subtitle-probe/running-relative.mkv"),
            [
                timed(140, 10),
                timed(150, 20),
                (None, None),
                timed(170, 30),
                (Some(200), None),
            ],
        ),
        (
            include_bytes!("fixtures/matroska-subtitle-probe/known-then-duration.mkv"),
            [
                timed(75, 15),
                timed(90, 15),
                timed(105, 15),
                (None, None),
                timed(120, 20),
            ],
        ),
        (
            include_bytes!("fixtures/matroska-subtitle-probe/known-zero.mkv"),
            [
                timed(0, 15),
                timed(15, 15),
                timed(30, 15),
                (None, None),
                timed(125, 30),
            ],
        ),
    ];

    let read_clock = |input: &bitmap_bridge::BitmapInput<matroska_feed::MatroskaBitmapSource>| {
        assert_eq!(input.clock.pts, input.clock.dts);
        (
            input.clock.pts.map(|pts| pts / 1_000_000),
            input.duration.map(|duration| duration / 1_000_000),
        )
    };

    for (case, (fixture, expected)) in cases.into_iter().enumerate() {
        let pool = pool();
        let engine = Engine::matroska(
            &pool,
            ID,
            SourceCapabilities {
                length: Some(fixture.len() as u64),
                random_access: true,
            },
            triergon_matroska::Limits::default(),
            4096,
        )
        .unwrap();
        let source =
            ByteSource::new(ID, Bytes::copy_from_slice(&pool, fixture).unwrap(), 7).unwrap();
        let mut player = Player::new(
            engine,
            SourceOwner::new(source),
            Selection::All,
            PlayerPolicy::default(),
        )
        .unwrap();
        let outputs = collect(&mut player, 1).unwrap();
        let config = outputs
            .iter()
            .find(|output| matches!(output.event(), RawEvent::Matroska(Event::Configuration(_))))
            .unwrap();
        let track = MatroskaBitmapTrack::new(config, 3).unwrap();
        let target = Some(TrackKey::Matroska(3));
        assert!(matches!(
            MatroskaBitmapClockProbe::new(&track, &outputs, outputs.len() - 1),
            Err(C::Limit { .. })
        ));
        let mut probe = MatroskaBitmapClockProbe::new(&track, &outputs, outputs.len()).unwrap();
        assert!(!probe.poll(&mut WorkBudget::new(0), &off()).unwrap());
        assert!(probe.packet(0, 0).is_err());
        assert!(matches!(
            probe.poll(&mut WorkBudget::new(128), &AtomicBool::new(true)),
            Err(C::Cancelled)
        ));
        let pressure = pool.reserve(pool.limit() - pool.used()).unwrap();
        assert!(matches!(
            probe.poll(&mut WorkBudget::new(4096), &off()),
            Err(C::Backpressure { .. })
        ));
        drop(pressure);
        while !probe.poll(&mut WorkBudget::new(1), &off()).unwrap() {}
        let mut receiver = (case == 0).then(|| {
            PgsBridge::new(
                &pool,
                track.scope(),
                pgs::DisplayLimits::default(),
                pgs::DisplayConfig::default(),
            )
            .unwrap()
        });
        let mut clocks = Vec::new();
        let mut shown_at = Vec::new();
        for (index, output) in outputs.iter().enumerate() {
            if let Some(projected) = probe.packet(index, index as u64).unwrap() {
                let original = track.packet(output, index as u64).unwrap().unwrap();
                assert!(projected.raw.shares_storage_with(&original.raw));
                assert_eq!(
                    projected.metadata.packet.source_pts().unwrap(),
                    original.metadata.packet.source_pts().unwrap(),
                );
                assert_eq!(projected.metadata.delivery, original.metadata.delivery);
                clocks.push(read_clock(&projected));
                if let Some(receiver) = &mut receiver {
                    receiver
                        .provide(projected)
                        .unwrap_or_else(|failure| panic!("{:?}", failure.error));
                    loop {
                        match receiver.step(&mut WorkBudget::new(1), &off()).unwrap() {
                            PgsStep::Yield => {}
                            PgsStep::Output(event) => {
                                if let Some(PgsQueueAction::Show(picture)) =
                                    event.packet_clock_action().unwrap()
                                {
                                    shown_at.push(picture.start_ns);
                                }
                            }
                            PgsStep::NeedPacket => break,
                            PgsStep::Finished => panic!("unexpected PGS receiver state"),
                        }
                    }
                }
            } else {
                assert_ne!(output.event().track(), target);
            }
        }
        assert_eq!(clocks, expected, "startup clock case {case}");
        if case == 0 {
            assert_eq!(shown_at, [100_000_000]);
        }

        if case == 4 {
            // Replay a shorter probe, then deliver the remainder in order.
            // A future anchor cannot move clocks that have reached the queue.
            let split = outputs
                .iter()
                .enumerate()
                .filter(|(_, output)| output.event().track() == target)
                .nth(2)
                .unwrap()
                .0;
            let (prefix, live) = outputs.split_at(split);
            let mut probe = MatroskaBitmapClockProbe::new(&track, prefix, prefix.len()).unwrap();
            assert!(probe.poll(&mut WorkBudget::new(128), &off()).unwrap());
            let first: Vec<_> = prefix
                .iter()
                .enumerate()
                .filter_map(|(index, _)| {
                    probe
                        .packet(index, index as u64)
                        .unwrap()
                        .as_ref()
                        .map(read_clock)
                })
                .collect();
            assert_eq!(first, [timed(0, 10), timed(10, 20)]);
            let mut clock = probe.into_live().unwrap();
            let rest: Vec<_> = live
                .iter()
                .enumerate()
                .filter_map(|(index, output)| {
                    clock
                        .packet(output, (split + index) as u64)
                        .unwrap()
                        .as_ref()
                        .map(read_clock)
                })
                .collect();
            assert_eq!(rest, [(None, None), timed(30, 30), (Some(200), None)]);
        }
    }
}

#[test]
fn matroska_discovery_stops_on_canvas_or_limits_and_replays_the_same_source() {
    use matroska_feed::{
        MatroskaBitmapClockProbe, MatroskaBitmapTrack, MatroskaDiscovered, MatroskaDiscovery,
        MatroskaDiscoveryCutoff, MatroskaDiscoveryLimits, MatroskaDiscoveryStep,
    };
    use triergon_matroska::typed::Event;

    struct Counted {
        inner: ByteSource,
        bytes: Arc<AtomicUsize>,
        ready: Arc<AtomicBool>,
    }
    impl Source for Counted {
        fn poll(
            &mut self,
            request: &triergon_core::InputRequest,
            work: &mut WorkBudget,
            cancel: &AtomicBool,
        ) -> Result<SourceReply> {
            if !self.ready.load(Ordering::Relaxed) {
                return Ok(SourceReply::Pending);
            }
            let reply = self.inner.poll(request, work, cancel)?;
            if let SourceReply::Data(window) = &reply {
                self.bytes
                    .fetch_add(window.bytes().len(), Ordering::Relaxed);
            }
            Ok(reply)
        }
        fn invalidate(&mut self) {
            self.inner.invalidate();
        }
        fn abort(&mut self) {
            self.inner.abort();
        }
        fn close(&mut self) {
            self.inner.close();
        }
    }

    let early = include_bytes!("fixtures/matroska-subtitle-probe/early-canvas.mkv");
    let mixed = include_bytes!("fixtures/matroska-subtitle-probe/observed-relative.mkv");
    let limits = MatroskaDiscoveryLimits::default();
    let cases: [(
        &[u8],
        MatroskaDiscoveryLimits,
        MatroskaDiscoveryCutoff,
        usize,
        usize,
    ); 8] = [
        (early, limits, MatroskaDiscoveryCutoff::StreamsReady, 1, 5),
        (
            include_bytes!("fixtures/matroska-subtitle-probe/bad-first-canvas.mkv"),
            limits,
            MatroskaDiscoveryCutoff::StreamsReady,
            2,
            6,
        ),
        (
            include_bytes!("fixtures/matroska-subtitle-probe/combined-leading.mkv"),
            limits,
            MatroskaDiscoveryCutoff::StreamsReady,
            1,
            5,
        ),
        (
            include_bytes!("fixtures/matroska-subtitle-probe/packet-cap.mkv"),
            MatroskaDiscoveryLimits {
                packet_bytes: 32,
                ..limits
            },
            MatroskaDiscoveryCutoff::PacketBytes,
            2,
            7,
        ),
        (
            early,
            MatroskaDiscoveryLimits {
                source_bytes: 16,
                ..limits
            },
            MatroskaDiscoveryCutoff::SourceBytes,
            0,
            5,
        ),
        (
            early,
            MatroskaDiscoveryLimits {
                outputs: 1,
                ..limits
            },
            MatroskaDiscoveryCutoff::OutputCount,
            0,
            5,
        ),
        (mixed, limits, MatroskaDiscoveryCutoff::SourceEnd, 5, 5),
        (mixed, limits, MatroskaDiscoveryCutoff::StreamsReady, 1, 5),
    ];
    let target = Some(TrackKey::Matroska(3));
    for (fixture, limits, cutoff, prefix_packets, total_packets) in cases {
        let report_video = std::ptr::eq(fixture, mixed.as_slice())
            && cutoff == MatroskaDiscoveryCutoff::StreamsReady;
        let pool = pool();
        let engine = Engine::matroska(
            &pool,
            ID,
            SourceCapabilities {
                length: Some(fixture.len() as u64),
                random_access: true,
            },
            triergon_matroska::Limits::default(),
            4096,
        )
        .unwrap();
        let transferred = Arc::new(AtomicUsize::new(0));
        let ready = Arc::new(AtomicBool::new(false));
        let source = Counted {
            inner: ByteSource::new(ID, Bytes::copy_from_slice(&pool, fixture).unwrap(), 7).unwrap(),
            bytes: transferred.clone(),
            ready: ready.clone(),
        };
        let mut discovery = MatroskaDiscovery::new(
            engine,
            SourceOwner::new(source),
            Selection::All,
            PlayerPolicy::default(),
            limits,
        )
        .unwrap();
        loop {
            match discovery.poll(&mut WorkBudget::new(32), &off()).unwrap() {
                MatroskaDiscoveryStep::Progress(Progress::Yield) => {}
                MatroskaDiscoveryStep::Progress(Progress::NeedInput(_)) => break,
                step => panic!("starvation became {step:?}"),
            }
        }
        assert_eq!(transferred.load(Ordering::Relaxed), 0);
        ready.store(true, Ordering::Relaxed);
        let mut observed = None;
        let mut confirmed = false;
        for _ in 0..10_000 {
            if let MatroskaDiscoveryStep::Ready(stop) =
                discovery.poll(&mut WorkBudget::new(1), &off()).unwrap()
            {
                observed = Some(stop);
                break;
            }
            if report_video
                && !confirmed
                && let Some(RawEvent::Matroska(Event::Configuration(config))) =
                    discovery.buffered().last().map(Output::event)
            {
                let revision = config.revision();
                assert!(discovery.confirm_stream_ready(1, revision + 1).is_err());
                assert!(discovery.confirm_stream_ready(3, revision).is_err());
                discovery.confirm_stream_ready(1, revision).unwrap();
                confirmed = true;
            }
        }
        assert_eq!(confirmed, report_video);
        assert_eq!(observed, Some(cutoff));
        let at_cutoff = transferred.load(Ordering::Relaxed);
        assert!(at_cutoff <= limits.source_bytes);
        if cutoff == MatroskaDiscoveryCutoff::SourceBytes {
            assert_eq!(at_cutoff, limits.source_bytes);
        }
        let MatroskaDiscovered {
            mut player,
            prefix,
            cutoff: actual,
        } = discovery.finish().unwrap();
        assert_eq!(actual, cutoff);
        let prefix = prefix.as_slice();
        assert!(prefix.iter().all(|output| player.may_deliver(output)));
        assert_eq!(
            prefix
                .iter()
                .filter(|output| output.event().track() == target)
                .count(),
            prefix_packets
        );
        let rest = collect(&mut player, 1).unwrap();
        assert_eq!(transferred.load(Ordering::Relaxed), fixture.len());
        let all = prefix.iter().chain(&rest);
        assert_eq!(
            all.clone()
                .filter(|output| output.event().track() == target)
                .count(),
            total_packets
        );

        if prefix_packets != 0 {
            let config = prefix
                .iter()
                .find(|output| {
                    matches!(output.event(), RawEvent::Matroska(Event::Configuration(_)))
                })
                .unwrap();
            let track = MatroskaBitmapTrack::new(config, 3).unwrap();
            let mut probe = MatroskaBitmapClockProbe::new(&track, prefix, limits.outputs).unwrap();
            while !probe.poll(&mut WorkBudget::new(1), &off()).unwrap() {}
            let mut clocks: Vec<_> = prefix
                .iter()
                .enumerate()
                .filter_map(|(index, _)| {
                    probe
                        .packet(index, index as u64)
                        .unwrap()
                        .map(|packet| packet.clock.pts)
                })
                .collect();
            let mut live = probe.into_live().unwrap();
            clocks.extend(rest.iter().enumerate().filter_map(|(index, output)| {
                live.packet(output, (prefix.len() + index) as u64)
                    .unwrap()
                    .map(|packet| packet.clock.pts)
            }));
            if cutoff == MatroskaDiscoveryCutoff::SourceEnd {
                assert_eq!(
                    clocks,
                    [
                        Some(0),
                        Some(25_000_000),
                        Some(50_000_000),
                        Some(75_000_000),
                        Some(100_000_000)
                    ]
                );
            } else {
                assert_eq!(clocks.last(), Some(&Some(0)));
                assert!(clocks[..clocks.len() - 1].iter().all(Option::is_none));
            }
        }
        if cutoff == MatroskaDiscoveryCutoff::SourceEnd {
            assert!(
                player
                    .seek_and_select(
                        TrackKey::Matroska(1),
                        Time::new(0, 1).unwrap(),
                        Selection::All,
                        &mut WorkBudget::new(4096),
                        &off(),
                    )
                    .unwrap()
            );
            assert!(prefix.iter().all(|output| !player.may_deliver(output)));
        }
    }
}

#[test]
fn matroska_discovery_observes_timed_audio_without_caller_confirmation() {
    use matroska_feed::{
        MatroskaDiscovery, MatroskaDiscoveryCutoff, MatroskaDiscoveryLimits, MatroskaDiscoveryStep,
    };
    use triergon_matroska::typed::Event;

    let cases: [(&[u8], &[usize], usize); 2] = [
        (
            include_bytes!("fixtures/mkv-feed/audio-mixed-discovery.mkv"),
            &[1, 0, 2, 3],
            21,
        ),
        (
            include_bytes!("fixtures/mkv-feed/audio-bad-first-discovery.mkv"),
            &[0, 1],
            8,
        ),
    ];
    for (fixture, expected_prefix, expected_count) in cases {
        let pool = pool();
        let engine = Engine::matroska(
            &pool,
            ID,
            SourceCapabilities {
                length: Some(fixture.len() as u64),
                random_access: true,
            },
            triergon_matroska::Limits::default(),
            4096,
        )
        .unwrap();
        let source =
            ByteSource::new(ID, Bytes::copy_from_slice(&pool, fixture).unwrap(), 7).unwrap();
        let mut discovery = MatroskaDiscovery::new(
            engine,
            SourceOwner::new(source),
            Selection::All,
            PlayerPolicy::default(),
            MatroskaDiscoveryLimits::default(),
        )
        .unwrap();
        let cutoff = (0..100_000)
            .find_map(
                |_| match discovery.poll(&mut WorkBudget::new(1), &off()).unwrap() {
                    MatroskaDiscoveryStep::Ready(cutoff) => Some(cutoff),
                    MatroskaDiscoveryStep::Progress(_) => None,
                },
            )
            .expect("audio discovery stalled");
        assert_eq!(cutoff, MatroskaDiscoveryCutoff::StreamsReady);
        let mut done = discovery.finish().unwrap();
        let track = |output: &Output| match output.event() {
            RawEvent::Matroska(Event::Packet(packet)) => {
                assert!(
                    triergon_matroska::compat::clock::packet_timing(packet.packet.timing)
                        .unwrap()
                        .pts
                        .is_some()
                );
                Some(packet.packet.track)
            }
            _ => None,
        };
        let prefix: Vec<_> = done.prefix.as_slice().iter().filter_map(track).collect();
        assert_eq!(prefix, expected_prefix);
        let rest = collect(&mut done.player, 1).unwrap();
        assert_eq!(
            prefix.len() + rest.iter().filter_map(track).count(),
            expected_count
        );
    }
}

#[test]
fn matroska_discovery_observes_video_and_keeps_decode_order() {
    use matroska_feed::{
        MatroskaDiscovery, MatroskaDiscoveryCutoff, MatroskaDiscoveryLimits, MatroskaDiscoveryStep,
    };
    use triergon_matroska::typed::Event;

    use MatroskaDiscoveryCutoff::{SourceEnd, StreamsReady};

    for (fixture, expected_cutoff, packets, pictures, total) in [
        (
            include_bytes!("fixtures/mkv-feed/three-tracks.mkv").as_slice(),
            StreamsReady,
            89,
            8,
            101,
        ),
        (
            include_bytes!("fixtures/mkv-feed/hevc-dolby.mkv").as_slice(),
            StreamsReady,
            5,
            3,
            52,
        ),
        (
            include_bytes!("fixtures/mkv-feed/avc-reordered-discovery.mkv").as_slice(),
            StreamsReady,
            3,
            3,
            12,
        ),
        (
            include_bytes!("fixtures/mkv-feed/hevc-no-default-duration-discovery.mkv").as_slice(),
            SourceEnd,
            12,
            12,
            12,
        ),
    ] {
        let pool = pool();
        let engine = Engine::matroska(
            &pool,
            ID,
            SourceCapabilities {
                length: Some(fixture.len() as u64),
                random_access: true,
            },
            triergon_matroska::Limits::default(),
            4096,
        )
        .unwrap();
        let source =
            ByteSource::new(ID, Bytes::copy_from_slice(&pool, fixture).unwrap(), 7).unwrap();
        let mut discovery = MatroskaDiscovery::new(
            engine,
            SourceOwner::new(source),
            Selection::All,
            PlayerPolicy::default(),
            MatroskaDiscoveryLimits::default(),
        )
        .unwrap();
        let cutoff = (0..500_000)
            .find_map(
                |_| match discovery.poll(&mut WorkBudget::new(1), &off()).unwrap() {
                    MatroskaDiscoveryStep::Ready(cutoff) => Some(cutoff),
                    MatroskaDiscoveryStep::Progress(_) => None,
                },
            )
            .expect("video discovery stalled");
        assert_eq!(cutoff, expected_cutoff, "{packets}");
        let mut done = discovery.finish().unwrap();
        let packet = |output: &Output| match output.event() {
            RawEvent::Matroska(Event::Packet(packet)) => {
                Some((packet.packet.track, packet.packet.pts_ns))
            }
            _ => None,
        };
        let prefix: Vec<_> = done.prefix.as_slice().iter().filter_map(packet).collect();
        assert_eq!(prefix.len(), packets);
        let video: Vec<_> = prefix
            .iter()
            .filter_map(|(track, pts)| (*track == 0).then_some(*pts))
            .collect();
        assert_eq!(video.len(), pictures);
        if packets == 5 {
            assert_eq!(video, [Some(0), Some(250_000_000), Some(167_000_000)]);
        }
        let rest = collect(&mut done.player, 1).unwrap();
        assert_eq!(prefix.len() + rest.iter().filter_map(packet).count(), total);
    }
}

#[test]
fn dvd_stop_only_packet_produces_no_packet_clock_picture() {
    use bitmap_bridge::*;

    let b = pool();
    let raw = [0, 10, 0, 4, 0, 0, 0, 4, 2, 0xff];
    let mut receiver = DvdReceiver::new(&b, bitmap_scope(), DvdBridgeConfig::default()).unwrap();
    receiver
        .provide(bitmap_input(&b, &raw))
        .unwrap_or_else(|e| panic!("{:?}", e.error));
    let out = dvd_next(&mut receiver, 32).unwrap();
    assert!(out.picture.is_none());
    assert!(out.packet_clock_picture(None).unwrap().is_none());
}

#[test]
fn dvd_receiver_carries_colours_across_packets_and_fences_reset() {
    use bitmap_bridge::*;

    let b = pool();
    let scope = bitmap_scope();
    let fixture = include_bytes!("fixtures/bitmap/original-dvd-two-units-tail.spu");
    let first = &fixture[..30];
    // The same unit without its colour and contrast commands.
    let mut inherited = [first[..11].as_ref(), &first[17..]].concat();
    let size = u16::try_from(inherited.len()).unwrap();
    inherited[..2].copy_from_slice(&size.to_be_bytes());

    let mut receiver = DvdReceiver::new(&b, scope, DvdBridgeConfig::default()).unwrap();
    receiver
        .provide(bitmap_input(&b, first))
        .unwrap_or_else(|error| panic!("{:?}", error.error));
    let rejected = receiver.provide(bitmap_input(&b, first)).err().unwrap();
    assert_eq!(rejected.input.raw.as_ref(), first);
    drop(rejected);

    // Memory pressure keeps the packet pending and the decoder unchanged.
    let pressure = b.reserve(b.available()).unwrap();
    assert!(matches!(
        dvd_next(&mut receiver, 1),
        Err(C::Backpressure { .. })
    ));
    drop(pressure);
    let first = dvd_next(&mut receiver, 1).unwrap();
    let palette = first.picture.as_ref().unwrap().palette().to_vec();
    assert!(receiver.provide(bitmap_input(&b, &inherited)).is_err());

    let mut malformed = bitmap_input(&b, &[0, 6, 0, 4, 0xff, 0xff]);
    malformed.sequence = 12;
    receiver
        .provide(malformed)
        .unwrap_or_else(|error| panic!("{:?}", error.error));
    assert!(dvd_next(&mut receiver, 1).unwrap().picture.is_none());

    let mut second = bitmap_input(&b, &inherited);
    second.sequence = 13;
    second.clock.pts = Some(90_000);
    receiver
        .provide(second)
        .unwrap_or_else(|error| panic!("{:?}", error.error));
    receiver.finish();
    let second = dvd_next(&mut receiver, 1).unwrap();
    assert_eq!(second.picture.as_ref().unwrap().palette(), palette);
    assert_eq!(
        second.presentation().unwrap().unwrap().show.at,
        Some(Time::new(1193, 1000).unwrap())
    );
    assert!(matches!(
        receiver.step(&mut WorkBudget::new(1), &off()).unwrap(),
        DvdReceiverStep::Finished
    ));

    let mut new_scope = scope;
    new_scope.generation += 1;
    let config = DvdBridgeConfig::default();
    assert!(
        !receiver
            .reset(new_scope, config.clone(), &mut WorkBudget::new(0), &off())
            .unwrap()
    );
    assert!(
        receiver
            .reset(new_scope, config, &mut WorkBudget::new(1), &off())
            .unwrap()
    );
    let stale = receiver
        .provide(bitmap_input(&b, &inherited))
        .err()
        .unwrap();
    assert!(matches!(stale.error, C::StaleGeneration { .. }));
    assert_eq!(stale.input.raw.as_ref(), inherited);
    drop(stale);
    // A reset forgets the carried colours: the inherited unit is transparent.
    let mut fresh = bitmap_input(&b, &inherited);
    fresh.scope = new_scope;
    receiver
        .provide(fresh)
        .unwrap_or_else(|error| panic!("{:?}", error.error));
    let fresh = dvd_next(&mut receiver, 1).unwrap();
    assert!(fresh.picture.is_none());
    assert_eq!(first.packet.input().scope, scope);
    assert_eq!(second.packet.input().scope, scope);
    assert_eq!(fresh.packet.input().scope, new_scope);
    drop((first, second, fresh, receiver));
    assert_eq!(b.used(), Budget::control_storage_bytes());
}

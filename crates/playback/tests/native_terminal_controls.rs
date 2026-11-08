mod common;
use common::{collect, off, packets, run_open};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
};
use triergon_core::{
    Budget, Bytes, Error, InputCursor, InputRequest, SourceCapabilities, SourceFailure, SourceId,
    WorkBudget,
};
use triergon_isobmff::compat::mov::clock::ClockOrigin;
use triergon_playback::{
    io::{SelectedInput, SourceOwner},
    mp4_open::{Mp4Open, OpenLimits, OpenProgress},
    mp4_plan::{Mp4IndexJob, Mp4View, PlanLimits},
    progressive::ProgressiveLimits,
    *,
};
mod terminal_golds;
use terminal_golds as gold;
const ID: SourceId = SourceId(7611);
#[derive(Default)]
struct Signals {
    eof: AtomicBool,
    fail: AtomicBool,
    ends: AtomicUsize,
    polls: AtomicUsize,
    pause_at: AtomicU64,
}
struct SourceWithEnd {
    inner: ByteSource,
    bytes: usize,
    signals: Arc<Signals>,
}
impl Source for SourceWithEnd {
    fn poll(
        &mut self,
        r: &InputRequest,
        w: &mut WorkBudget,
        c: &AtomicBool,
    ) -> Result<SourceReply> {
        if c.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if w.remaining() == 0 {
            return Ok(SourceReply::Pending);
        }
        let pause_at = self.signals.pause_at.load(Ordering::Relaxed);
        if r.offset == self.bytes as u64 || (pause_at != 0 && r.offset == pause_at) {
            self.signals.polls.fetch_add(1, Ordering::Relaxed);
            if self.signals.fail.load(Ordering::Relaxed) {
                assert!(w.try_charge(1));
                return Ok(SourceReply::Failed {
                    source: r.source,
                    token: r.token.clone(),
                    failure: SourceFailure::Transport,
                });
            }
            if !self.signals.eof.load(Ordering::Relaxed) {
                assert!(w.try_charge(1));
                return Ok(SourceReply::Pending);
            }
        }
        let reply = self.inner.poll(r, w, c)?;
        if matches!(reply, SourceReply::End { .. }) {
            self.signals.ends.fetch_add(1, Ordering::Relaxed);
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
fn make_mode(
    raw: &[u8],
    known: bool,
    chunk: usize,
    quantum: u64,
    eof: bool,
    native: bool,
) -> (Player<SourceWithEnd>, Budget, Arc<Signals>) {
    try_make_mode(raw, known, chunk, quantum, eof, native).unwrap()
}
fn try_make_mode(
    raw: &[u8],
    known: bool,
    chunk: usize,
    quantum: u64,
    eof: bool,
    native: bool,
) -> Result<(Player<SourceWithEnd>, Budget, Arc<Signals>)> {
    let view = if native {
        TestView::Native
    } else {
        TestView::Source
    };
    try_make_source_mode(raw, known, chunk, quantum, eof, view, false)
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum TestView {
    Source,
    Native,
    NativeRecovery,
}
fn try_make_source_mode(
    raw: &[u8],
    known: bool,
    chunk: usize,
    quantum: u64,
    eof: bool,
    view: TestView,
    random_access: bool,
) -> Result<(Player<SourceWithEnd>, Budget, Arc<Signals>)> {
    let native = view != TestView::Source;
    let b = Budget::new(64 << 20).unwrap();
    let signals = Arc::new(Signals::default());
    signals.eof.store(eof, Ordering::Relaxed);
    let source = SourceWithEnd {
        inner: ByteSource::new(ID, Bytes::copy_from_slice(&b, raw).unwrap(), chunk).unwrap(),
        bytes: raw.len(),
        signals: signals.clone(),
    };
    let input = InputCursor::new(
        &b,
        ID,
        SourceCapabilities {
            length: known.then_some(raw.len() as u64),
            random_access,
        },
        64,
    )
    .unwrap();
    let selected = SelectedInput {
        input,
        source: SourceOwner::new(source),
        format: Format::Mp4,
    };
    let mut open = if view == TestView::NativeRecovery {
        Mp4Open::new_recovery(selected, OpenLimits::default())?
    } else {
        Mp4Open::new(selected, OpenLimits::default())?
    };
    let mut ready = false;
    for _ in 0..300_000 {
        if matches!(
            open.advance(&mut WorkBudget::new(quantum), &off())?,
            OpenProgress::Ready
        ) {
            ready = true;
            break;
        }
    }
    assert!(ready);
    let opened = open.finish().unwrap();
    let front = matches!(
        opened.movie.read_scope(),
        mp4_open::MetadataReadScope::FrontMetadataBeforeMdat(_)
    );
    let mut job = if native && front {
        Mp4IndexJob::new_native_terminal_v1(&opened.movie, PlanLimits::default())
    } else {
        Mp4IndexJob::new(
            &opened.movie,
            if native {
                Mp4View::PinnedMov9NoParseNoFillin
            } else {
                Mp4View::SourcePhysical
            },
            PlanLimits::default(),
        )
    }
    .unwrap();
    assert!(!job.poll(&mut WorkBudget::new(0), &off()).unwrap());
    let mut ready = false;
    for _ in 0..300_000 {
        if job.poll(&mut WorkBudget::new(quantum), &off())? {
            ready = true;
            break;
        }
    }
    assert!(ready);
    let plan = job.finish().unwrap();
    if native {
        assert_eq!(plan.rows(), plan.native_rows().len());
    }
    let engine = if native && front {
        Engine::mp4_native_terminal_v1(opened.input, plan, 1 << 20, ProgressiveLimits::default())
    } else if front {
        Engine::mp4_progressive(opened.input, plan, 1 << 20, ProgressiveLimits::default())
    } else {
        Engine::mp4(opened.input, plan, 1 << 20)
    }
    .unwrap();
    Ok((
        Player::new(
            engine,
            opened.source,
            Selection::All,
            PlayerPolicy {
                slots: 2,
                ..PlayerPolicy::default()
            },
        )
        .unwrap(),
        b,
        signals,
    ))
}
fn make(
    raw: &[u8],
    known: bool,
    chunk: usize,
    quantum: u64,
    eof: bool,
) -> (Player<SourceWithEnd>, Budget, Arc<Signals>) {
    make_mode(raw, known, chunk, quantum, eof, true)
}
fn check(out: &[Output], case: &gold::Case) {
    let got: Vec<_> = packets(out)
        .into_iter()
        .filter(|p| p.native.is_some())
        .collect();
    assert_eq!(got.len(), case.packets().len(), "{}", case.name);
    for (p, w) in got.into_iter().zip(case.packets()) {
        assert_eq!(p.source.payload.as_ref(), w.data, "{} payload", case.name);
        assert_eq!(
            p.source.sample.range,
            w.offset()..w.offset() + w.data.len() as u64
        );
        assert_eq!(p.configuration.track.track_id as usize, w.stream_index + 1);
        let n = p.native.as_ref().expect("native occurrence required");
        assert_eq!(n.source, p.source.sample);
        assert_eq!(n.track_id, p.configuration.track.track_id);
        assert_eq!(n.demux.source_ordinal, p.source.sample.ordinal);
        assert_eq!(
            n.demux.source_description,
            p.source.sample.description_index
        );
        assert_eq!(n.demux.source_range, p.source.sample.range);
        assert_eq!(n.demux.pts, w.pts);
        assert_eq!(n.demux.dts, w.dts);
        assert_eq!(n.demux.pts_field.api_ticks(), w.pts);
        assert_eq!(n.demux.dts_field.api_ticks(), w.dts);
        assert_eq!(n.demux.duration, w.duration_ticks());
        assert_eq!(n.demux.flags, w.flag_bits());
    }
}
#[test]
fn native_terminal_all_five_original_whole_packets_all_lengths_chunks_and_work() {
    for case in gold::CASES
        .iter()
        .filter(|c| c.name.starts_with("open-ended--"))
    {
        for known in [false, true] {
            for chunk in [1, 7, 4096] {
                for quantum in [1, 4096] {
                    let (mut p, _b, signals) = make(case.raw, known, chunk, quantum, true);
                    let out = collect(&mut p, quantum).unwrap();
                    check(&out, case);
                    assert!(signals.ends.load(Ordering::Relaxed) >= 1);
                }
            }
        }
    }
}
#[test]
fn native_progressive_pending_forward_corpus_preserves_complete_ordered_objects() {
    for case in gold::CASES
        .iter()
        .filter(|c| c.name.starts_with("progressive--") && !c.name.ends_with("reverse-ranges"))
    {
        for quantum in [1, 4096] {
            let (mut p, _b, _s) = make(case.raw, false, 7, quantum, true);
            check(&collect(&mut p, quantum).unwrap(), case);
        }
    }
}
#[test]
fn native_terminal_unknown_source_clocks_and_literal_origin_chain_are_retained() {
    let c = gold::case("open-ended--unknown-clock-terminal");
    let (mut p, _b, _s) = make(c.raw, false, 7, 1, true);
    let out = collect(&mut p, 1).unwrap();
    check(&out, c);
    for (at, p) in packets(&out).iter().enumerate() {
        if at >= 6 {
            assert_eq!(p.source.sample.dts_ticks, None);
            assert_eq!(p.source.sample.cts_ticks, None);
            let ClockOrigin::PredecessorEnd(origin) = p.native.as_ref().unwrap().clock_origin
            else {
                panic!("derived native origin required")
            };
            assert_eq!(origin.predecessor_sample, at as u64);
            assert_eq!(origin.derived_dts_ticks, at as i128 * 1000);
            assert_eq!(origin.predecessor_source_dts_known, at == 6);
        }
    }
}
#[test]
fn native_terminal_pending_is_not_eof_and_zero_work_never_polls_source() {
    let c = gold::case("open-ended--regular-terminal");
    let (mut p, _b, s) = make(c.raw, false, 7, 1, false);
    let mut out = Vec::new();
    for _ in 0..300_000 {
        let step = p.advance(&mut WorkBudget::new(1), &off()).unwrap();
        assert!(!matches!(step, Progress::Ended));
        while let Some(x) = p.pop() {
            out.push(x);
        }
        if p.pending_request()
            .is_some_and(|r| r.offset == c.raw.len() as u64)
        {
            break;
        }
    }
    check(&out, c);
    let polls = s.polls.load(Ordering::Relaxed);
    for _ in 0..8 {
        assert!(!matches!(
            p.advance(&mut WorkBudget::new(0), &off()).unwrap(),
            Progress::Ended
        ));
    }
    assert_eq!(polls, s.polls.load(Ordering::Relaxed));
    assert_eq!(s.ends.load(Ordering::Relaxed), 0);
    s.eof.store(true, Ordering::Relaxed);
    out.extend(collect(&mut p, 1).unwrap());
    check(&out, c);
}
#[test]
fn native_terminal_failure_never_advertises_eof() {
    let c = gold::case("open-ended--regular-terminal");
    let (mut p, _b, s) = make(c.raw, false, 7, 1, false);
    s.fail.store(true, Ordering::Relaxed);
    let error = collect(&mut p, 1).unwrap_err();
    assert!(matches!(error, Error::Transport));
    assert_eq!(s.ends.load(Ordering::Relaxed), 0);
}
#[test]
fn native_terminal_payload_owners_survive_player_and_plan_retirement() {
    let c = gold::case("open-ended--pending-terminal");
    let (mut p, b, _s) = make(c.raw, false, 1, 1, true);
    let out = collect(&mut p, 1).unwrap();
    drop(p);
    check(&out, c);
    assert!(b.used() > 0);
    drop(out);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}
#[test]
fn native_terminal_output_queue_backpressure_keeps_every_packet() {
    let c = gold::case("open-ended--pending-terminal");
    let (mut p, _b, _s) = make(c.raw, false, 1, 1, true);
    let mut saw = false;
    for _ in 0..300_000 {
        if matches!(
            p.advance(&mut WorkBudget::new(4096), &off()).unwrap(),
            Progress::Backpressure { .. }
        ) {
            saw = true;
            break;
        }
    }
    assert!(saw);
    let mut out = Vec::new();
    while let Some(x) = p.pop() {
        out.push(x);
    }
    out.extend(collect(&mut p, 1).unwrap());
    check(&out, c);
}
#[test]
fn native_terminal_cancel_is_sticky_and_releases_owned_state() {
    let c = gold::case("open-ended--pending-terminal");
    let (mut p, b, _s) = make(c.raw, false, 1, 1, true);
    let error = p
        .advance(&mut WorkBudget::new(1), &AtomicBool::new(true))
        .unwrap_err();
    assert!(matches!(error, Error::Cancelled));
    assert!(matches!(
        p.advance(&mut WorkBudget::new(4096), &off()).unwrap(),
        Progress::Stopped
    ));
    drop(p);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}
#[test]
fn native_terminal_truncated_sample_is_not_a_clean_end() {
    let c = gold::case("open-ended--pending-terminal");
    let (mut p, _b, _s) = make(&c.raw[..c.raw.len() - 1], false, 1, 1, true);
    assert!(matches!(collect(&mut p, 1), Err(Error::Truncated)));
}
#[test]
fn ordinary_progressive_native_context_stays_refused() {
    // Default native planning still requires its original context. A source view
    // cannot be passed to the new native attachment to manufacture that scope.
    let b = Budget::new(64 << 20).unwrap();
    let id = ID;
    let raw = include_bytes!("fixtures/open-ended/regular-terminal.mp4");
    let source = ByteSource::new(id, Bytes::copy_from_slice(&b, raw).unwrap(), 7).unwrap();
    let input = InputCursor::new(
        &b,
        id,
        SourceCapabilities {
            length: None,
            random_access: false,
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
    run_open(&mut open, 4096).unwrap();
    let opened = open.finish().unwrap();
    let mut job = Mp4IndexJob::new(
        &opened.movie,
        Mp4View::SourcePhysical,
        PlanLimits::default(),
    )
    .unwrap();
    while !job.poll(&mut WorkBudget::new(4096), &off()).unwrap() {}
    let plan = job.finish().unwrap();
    assert!(
        Engine::mp4_native_terminal_v1(opened.input, plan, 1 << 20, ProgressiveLimits::default())
            .is_err()
    );
}

#[test]
fn native_zero_boundary_preserves_all_nine_source_records_and_requires_physical_end() {
    let c = gold::case("open-ended--empty-sample-terminal");
    let (mut p, _b, s) = make(c.raw, false, 1, 1, false);
    let mut out = Vec::new();
    let mut parked = false;
    for _ in 0..300_000 {
        assert!(!matches!(
            p.advance(&mut WorkBudget::new(1), &off()).unwrap(),
            Progress::Ended
        ));
        while let Some(x) = p.pop() {
            out.push(x);
        }
        if p.pending_request()
            .is_some_and(|r| r.offset == c.raw.len() as u64)
        {
            parked = true;
            break;
        }
    }
    assert!(parked);
    let source = packets(&out);
    assert_eq!(source.len(), 9);
    check(&out, c);
    for (at, p) in source.iter().enumerate() {
        if at < 6 {
            assert!(p.native_front_read_boundary_v1.is_none());
        } else {
            let boundary = p.native_front_read_boundary_v1.unwrap();
            assert_eq!(boundary.source_ordinal, 7);
            assert_eq!(boundary.source_offset, c.raw.len() as u64);
            assert_eq!(boundary.track_id, 1);
            assert_eq!(p.source.sample.ordinal, at as u64 + 1);
            assert!(p.source.payload.is_empty());
            assert_eq!(p.native.is_some(), at == 6);
        }
    }
    assert_eq!(s.ends.load(Ordering::Relaxed), 0);
    s.eof.store(true, Ordering::Relaxed);
    out.extend(collect(&mut p, 1).unwrap());
    check(&out, c);
    assert_eq!(packets(&out).len(), 9);
}
#[test]
fn native_terminal_allocation_pressure_retries_complete_source_and_native_views() {
    let c = gold::case("open-ended--pending-terminal");
    let (mut p, b, _s) = make(c.raw, false, 1, 1, true);
    let mut out = Vec::new();
    let mut ended = false;
    let mut refusals = 0;
    for _ in 0..400_000 {
        let hold = b.reserve(b.available()).unwrap();
        let step = p.advance(&mut WorkBudget::new(1), &off());
        drop(hold);
        let mut step = match step {
            Err(Error::Backpressure { .. }) => {
                refusals += 1;
                p.advance(&mut WorkBudget::new(1), &off()).unwrap()
            }
            value => value.unwrap(),
        };
        if matches!(step, Progress::Backpressure { .. }) {
            refusals += 1;
            step = p.advance(&mut WorkBudget::new(1), &off()).unwrap();
        }
        while let Some(x) = p.pop() {
            out.push(x);
        }
        if matches!(step, Progress::Ended) {
            ended = true;
            break;
        }
    }
    assert!(ended);
    assert!(refusals > 0);
    check(&out, c);
    drop(p);
    drop(out);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}
#[test]
fn native_terminal_foreign_end_source_pool_and_stale_request_cannot_finish() {
    let c = gold::case("open-ended--regular-terminal");
    let (mut p, _b, s) = make(c.raw, false, 1, 1, false);
    let mut out = Vec::new();
    for _ in 0..300_000 {
        p.advance(&mut WorkBudget::new(1), &off()).unwrap();
        while let Some(x) = p.pop() {
            out.push(x);
        }
        if p.pending_request()
            .is_some_and(|r| r.offset == c.raw.len() as u64)
        {
            break;
        }
    }
    let request = p.pending_request().unwrap().clone();
    assert!(matches!(
        p.provide(&SourceReply::End {
            source: SourceId(98),
            token: request.token.clone()
        }),
        Err(Error::SourceMismatch)
    ));
    let foreign = Budget::new(4096).unwrap();
    let cursor = InputCursor::new(
        &foreign,
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
            token: cursor.token()
        }),
        Err(Error::ForeignPool)
    ));
    assert!(p.pending_request().is_some());
    assert_eq!(s.ends.load(Ordering::Relaxed), 0);
    assert!(
        p.provide(&SourceReply::End {
            source: ID,
            token: request.token
        })
        .unwrap()
    );
    out.extend(collect(&mut p, 1).unwrap());
    check(&out, c);
}

#[test]
fn native_zero_run_start_boundary_independent_of_tail_and_input_geometry() {
    let original = gold::case("open-ended--empty-sample-terminal");
    let cases: [&[u8]; 4] = [
        include_bytes!("fixtures/native-terminal-controls/empty-plus-one-byte.mp4"),
        include_bytes!("fixtures/native-terminal-controls/empty-plus31opaque.mp4"),
        include_bytes!("fixtures/native-terminal-controls/empty-plus32771opaque.mp4"),
        include_bytes!("fixtures/native-terminal-controls/empty-before-two-nonempty.mp4"),
    ];
    for raw in cases {
        for chunk in [1, 7, 4096] {
            for quantum in [1, 4096] {
                let (mut p, _b, s) = make(raw, false, chunk, quantum, true);
                let out = collect(&mut p, quantum).unwrap();
                check(&out, original);
                assert_eq!(packets(&out).len(), 9);
                assert!(s.ends.load(Ordering::Relaxed) > 0);
                for packet in packets(&out) {
                    let sample = &packet.source.sample;
                    assert_eq!(
                        packet.source.payload.as_ref(),
                        &raw[sample.range.start as usize..sample.range.end as usize]
                    );
                }
            }
        }
    }
}
#[test]
fn source_keeps_nonempty_bytes_beyond_native_zero_run_start_boundary() {
    let raw = include_bytes!("fixtures/native-terminal-controls/empty-before-two-nonempty.mp4");
    let (mut native, _b, _s) = make(raw, false, 1, 1, true);
    let got = collect(&mut native, 1).unwrap();
    let (mut source, _b, _s) = make_mode(raw, false, 1, 1, true, false);
    let want = collect(&mut source, 1).unwrap();
    let n = packets(&got);
    let s = packets(&want);
    assert_eq!(n.len(), 9);
    assert_eq!(s.len(), 9);
    for (n, s) in n.iter().zip(s) {
        assert_eq!(n.source.sample, s.source.sample);
        assert_eq!(n.source.payload.as_ref(), s.source.payload.as_ref());
    }
    for packet in n.iter().skip(7) {
        assert_eq!(packet.source.payload.len(), 12);
        assert!(packet.native.is_none());
        assert!(packet.native_front_read_boundary_v1.is_some());
    }
}
#[test]
fn native_seekable_header_rejects_zero_size_fragment_samples() {
    for raw in [
        include_bytes!("fixtures/open-ended/empty-sample-terminal.mp4").as_slice(),
        include_bytes!("fixtures/progressive/pending-empty-endpoint.mp4").as_slice(),
    ] {
        for chunk in [1, 7, 4096] {
            for work in [1, 4096] {
                assert!(matches!(
                    try_make_source_mode(
                        raw,
                        true,
                        chunk,
                        work,
                        true,
                        TestView::NativeRecovery,
                        true
                    ),
                    Err(Error::Invalid(
                        "zero-size fragment sample in native MOV header"
                    ))
                ));
                let (mut source, _, _) =
                    try_make_source_mode(raw, true, chunk, work, true, TestView::Source, true)
                        .unwrap();
                let source_output = collect(&mut source, work).unwrap();
                let (mut pipe, _, signals) = try_make_source_mode(
                    raw,
                    true,
                    chunk,
                    work,
                    true,
                    TestView::NativeRecovery,
                    false,
                )
                .unwrap();
                let pipe_output = collect(&mut pipe, work).unwrap();
                let literal = packets(&source_output);
                let observed = packets(&pipe_output);
                assert_eq!(literal.len(), 9);
                assert_eq!(observed.len(), 9);
                assert_eq!(observed.iter().filter(|p| p.native.is_some()).count(), 7);
                for (a, b) in literal.iter().zip(observed) {
                    assert_eq!(a.source.sample, b.source.sample);
                    assert_eq!(a.source.payload.as_ref(), b.source.payload.as_ref());
                }
                assert_eq!(signals.ends.load(Ordering::Relaxed), 1);
            }
        }
    }
}

#[test]
fn native_header_rejects_explicit_and_default_zero_size_before_first_media() {
    let raw = include_bytes!("fixtures/open-ended/empty-sample-terminal.mp4");
    let trun = raw.windows(4).position(|bytes| bytes == b"trun").unwrap() - 4;
    let trex = raw.windows(4).position(|bytes| bytes == b"trex").unwrap() - 4;
    assert_eq!(
        &raw[trun..trun + 20],
        b"\0\0\0 trun\0\0\x07\x01\0\0\0\x01\0\0\0\x9c"
    );
    for use_default in [false, true] {
        let mut changed = raw.to_vec();
        if use_default {
            // The run uses trex defaults. Keep the following source positions
            // stable by replacing the removed entry fields with a sibling box.
            changed[trun..trun + 4].copy_from_slice(&20_u32.to_be_bytes());
            changed[trun + 8..trun + 12].copy_from_slice(&1_u32.to_be_bytes());
            changed[trun + 20..trun + 32].copy_from_slice(b"\0\0\0\x0cfree\0\0\0\0");
            changed[trex + 24..trex + 28].fill(0);
        } else {
            changed[trun + 24..trun + 28].fill(0);
        }
        assert!(matches!(
            try_make_source_mode(&changed, false, 7, 1, true, TestView::NativeRecovery, false),
            Err(Error::Invalid(
                "zero-size fragment sample in native MOV header"
            ))
        ));
        let (mut source, _, _) = make_mode(&changed, false, 7, 1, true, false);
        let output = collect(&mut source, 1).unwrap();
        assert!(packets(&output)[0].source.payload.is_empty());
    }
}

#[test]
fn native_out_of_source_first_sample_requires_a_physical_end() {
    let raw = include_bytes!("fixtures/mp4/bad-chunk-offset-outside-source.mov");
    for (random, known) in [(false, false), (false, true), (true, true)] {
        for chunk in [1, 7, 4096] {
            for work in [1, 4096] {
                let (mut native, pool, signals) = try_make_source_mode(
                    raw,
                    known,
                    chunk,
                    work,
                    true,
                    TestView::NativeRecovery,
                    random,
                )
                .unwrap();
                let out = collect(&mut native, work).unwrap();
                assert!(packets(&out).is_empty());
                let [refusal] = out.as_slice() else {
                    panic!("expected one native refusal")
                };
                let RawEvent::Mp4(Mp4Event::NativeMissingSample(refusal)) = refusal.event() else {
                    panic!("expected a missing first sample");
                };
                assert_eq!(refusal.track_id, 1);
                assert_eq!(refusal.declared.ordinal, 1);
                assert_eq!(refusal.declared.range, 0x12eb..0x12f7);
                assert_eq!(refusal.end.source, ID);
                assert_eq!(refusal.end.offset, raw.len() as u64);
                assert_eq!(signals.ends.load(Ordering::Relaxed), 1);
                // The source view refuses the sample as outside the source:
                // while indexing when the length is declared, at the physical
                // end otherwise.
                let source =
                    try_make_source_mode(raw, known, chunk, work, true, TestView::Source, random);
                let refusal = match source {
                    Ok((mut source, _, _)) if !known => collect(&mut source, work).unwrap_err(),
                    Ok(_) => panic!("a declared length shows the sample outside the source"),
                    Err(error) => error,
                };
                assert_eq!(refusal, Error::Invalid("sample outside media extent"));
                drop(native);
                drop(out);
                assert_eq!(pool.used(), Budget::control_storage_bytes());
            }
        }
    }
}

#[test]
fn native_missing_first_sample_does_not_infer_end_from_starvation_or_layout() {
    let raw = include_bytes!("fixtures/mp4/bad-chunk-offset-outside-source.mov");
    let (mut native, _, signals) =
        try_make_source_mode(raw, false, 7, 1, false, TestView::NativeRecovery, false).unwrap();
    for _ in 0..30_000 {
        assert!(!matches!(
            native.advance(&mut WorkBudget::new(1), &off()).unwrap(),
            Progress::Ended
        ));
        assert!(native.pop().is_none());
        if native
            .pending_request()
            .is_some_and(|r| r.offset == raw.len() as u64)
        {
            break;
        }
    }
    assert_eq!(native.pending_request().unwrap().offset, raw.len() as u64);
    assert_eq!(signals.ends.load(Ordering::Relaxed), 0);
    signals.eof.store(true, Ordering::Relaxed);
    assert_eq!(collect(&mut native, 1).unwrap().len(), 1);

    let mut extended = raw.to_vec();
    extended.extend_from_slice(&4097_u32.to_be_bytes());
    extended.extend_from_slice(b"free");
    extended.resize(raw.len() + 4097, 0);
    let (mut native, _, _) = try_make_source_mode(
        &extended,
        false,
        7,
        1,
        true,
        TestView::NativeRecovery,
        false,
    )
    .unwrap();
    assert!(matches!(
        collect(&mut native, 1),
        Err(Error::Invalid(
            "native first sample starts before physical end"
        ))
    ));
}

#[test]
fn strict_out_of_source_first_sample_waits_for_the_physical_end() {
    let raw = include_bytes!("fixtures/mp4/bad-chunk-offset-outside-source.mov");
    for view in [TestView::Source, TestView::Native] {
        let (mut strict, _, signals) =
            try_make_source_mode(raw, false, 7, 1, false, view, false).unwrap();
        for _ in 0..30_000 {
            assert!(matches!(
                strict.advance(&mut WorkBudget::new(1), &off()),
                Ok(progress) if !matches!(progress, Progress::Ended)
            ));
            assert!(strict.pop().is_none());
            if strict
                .pending_request()
                .is_some_and(|r| r.offset == raw.len() as u64)
            {
                break;
            }
        }
        assert_eq!(strict.pending_request().unwrap().offset, raw.len() as u64);
        signals.eof.store(true, Ordering::Relaxed);
        assert_eq!(
            collect(&mut strict, 1).unwrap_err(),
            Error::Invalid("sample outside media extent")
        );

        // A source that continues past the declared first sample may hold it
        // in a later mdat, so the backward chunk order is refused as a layout.
        let mut extended = raw.to_vec();
        extended.extend_from_slice(&4097_u32.to_be_bytes());
        extended.extend_from_slice(b"free");
        extended.resize(raw.len() + 4097, 0);
        let (mut strict, _, _) =
            try_make_source_mode(&extended, false, 7, 1, true, view, false).unwrap();
        assert_eq!(
            collect(&mut strict, 1).unwrap_err(),
            Error::Unsupported("physical merge needs nondecreasing per-track source offsets")
        );
    }
}

#[test]
fn native_zero_timescale_keeps_the_declared_source_clock_unavailable() {
    let raw = include_bytes!("fixtures/mp4/bad-zero-timescale.mov");
    for (random, known) in [(false, false), (false, true), (true, true)] {
        for chunk in [1, 7, 4096] {
            for work in [1, 4096] {
                let (mut native, pool, signals) = try_make_source_mode(
                    raw,
                    known,
                    chunk,
                    work,
                    true,
                    TestView::NativeRecovery,
                    random,
                )
                .unwrap();
                let out = collect(&mut native, work).unwrap();
                let mut count = 0;
                for output in &out {
                    let RawEvent::Mp4(Mp4Event::Packet(packet)) = output.event() else {
                        continue;
                    };
                    let track = &packet.configuration.track;
                    let clock = &packet.native.as_ref().unwrap().media_clock;
                    assert_eq!(&track.raw_mdhd.payload()[12..16], &[0; 4]);
                    assert_eq!(track.base.get(), None);
                    assert_eq!(clock.source_base.get(), None);
                    assert_eq!(
                        clock.demux_base,
                        triergon_core::TimeBase::new(1, 1).unwrap()
                    );
                    assert_eq!(
                        clock.cause,
                        triergon_isobmff::compat::mov::clock::MediaClockCause::ObservedZeroScaleWord
                    );
                    assert_eq!(track.native_catalogue.as_ref().unwrap().clock, *clock);
                    assert_eq!(output.event().source_pts().unwrap(), None);
                    assert_eq!(
                        output.event().native_pts().unwrap(),
                        Some(triergon_core::Time::new(count * 1000, 1).unwrap())
                    );
                    let start = 675 + 12 * count as usize;
                    assert_eq!(packet.source.payload.as_ref(), &raw[start..start + 12]);
                    count += 1;
                }
                assert_eq!(count, 6);
                assert!(signals.ends.load(Ordering::Relaxed) >= 1);
                assert!(matches!(
                    try_make_source_mode(raw, known, chunk, work, true, TestView::Source, random),
                    Err(Error::Invalid("zero media/movie timescale"))
                ));
                drop(native);
                drop(out);
                assert_eq!(pool.used(), Budget::control_storage_bytes());
            }
        }
    }
}

#[test]
fn native_short_media_packet_requires_a_positive_prefix_at_eof() {
    let regular = include_bytes!("fixtures/mp4/bad-truncate-final-byte.mov").as_slice();
    let fragment =
        include_bytes!("fixtures/native-terminal-controls/short-before-empty.mp4").as_slice();
    for (raw, start, random) in [
        (regular, 735, false),
        (regular, 735, true),
        (fragment, 1127, false),
    ] {
        for known in [false, true] {
            if random && !known {
                continue;
            }
            for chunk in [1, 7, 4096] {
                for work in [1, 4096] {
                    let (mut native, pool, signals) = try_make_source_mode(
                        raw,
                        known,
                        chunk,
                        work,
                        true,
                        TestView::NativeRecovery,
                        random,
                    )
                    .unwrap();
                    let out = collect(&mut native, work).unwrap();
                    assert_eq!(packets(&out).len(), 5);
                    let last = out.last().unwrap().event();
                    let RawEvent::Mp4(Mp4Event::NativeShortPacket(p)) = last else {
                        panic!("expected a native short packet");
                    };
                    assert_eq!(p.prefix.declared, p.native.source);
                    assert_eq!(p.prefix.declared.range, start..start + 12);
                    assert_eq!(p.prefix.end.offset, start + 11);
                    assert_eq!(p.prefix.payload.as_ref(), &raw[start as usize..]);
                    assert_eq!(p.native.demux.dts, Some(5000));
                    assert_eq!(p.native.demux.duration, 1000);
                    assert_eq!(p.native.demux.flags, 2);
                    assert_eq!(last.payload_bytes(), 11);
                    assert_eq!(last.source_pts().unwrap(), None);
                    assert!(signals.ends.load(Ordering::Relaxed) >= 1);
                    drop(native);
                    drop(out);
                    assert_eq!(pool.used(), Budget::control_storage_bytes());
                    let source = try_make_source_mode(
                        raw,
                        known,
                        chunk,
                        work,
                        true,
                        TestView::Source,
                        random,
                    )
                    .and_then(|(mut player, _, _)| collect(&mut player, work));
                    assert!(source.is_err());
                }
            }
        }
    }
}

#[test]
fn native_short_packet_distinguishes_pending_failure_and_empty_eof() {
    let raw = include_bytes!("fixtures/mp4/bad-truncate-final-byte.mov");
    let (mut native, _, signals) =
        try_make_source_mode(raw, false, 7, 1, false, TestView::NativeRecovery, false).unwrap();
    for _ in 0..30_000 {
        assert!(!matches!(
            native.advance(&mut WorkBudget::new(1), &off()).unwrap(),
            Progress::Ended
        ));
        while let Some(output) = native.pop() {
            assert!(!matches!(
                output.event(),
                RawEvent::Mp4(Mp4Event::NativeShortPacket(_))
            ));
        }
        if native
            .pending_request()
            .is_some_and(|r| r.offset == raw.len() as u64)
        {
            break;
        }
    }
    assert_eq!(native.pending_request().unwrap().offset, raw.len() as u64);
    assert_eq!(signals.ends.load(Ordering::Relaxed), 0);
    signals.fail.store(true, Ordering::Relaxed);
    assert!(matches!(collect(&mut native, 1), Err(Error::Transport)));

    let (mut empty, _, _) = try_make_source_mode(
        &raw[..735],
        false,
        7,
        1,
        true,
        TestView::NativeRecovery,
        false,
    )
    .unwrap();
    assert!(matches!(collect(&mut empty, 1), Err(Error::Truncated)));
}

#[test]
fn interior_zero_read_preserves_the_native_packet_prefix_and_source() {
    let raw = include_bytes!("fixtures/native-terminal-controls/one-empty-between-nonempty.mp4");
    for known in [false, true] {
        for chunk in [1, 7, 4096] {
            for work in [1, 4096] {
                let (mut native, pool, signals) = make(raw, known, chunk, work, true);
                let output = collect(&mut native, work).unwrap();
                let got = packets(&output);
                let (mut source, _, _) = make_mode(raw, known, chunk, work, true, false);
                let literal = collect(&mut source, work).unwrap();
                let physical = packets(&literal);
                assert_eq!(got.len(), 9);
                assert_eq!(got.len(), physical.len());
                for (n, s) in got.iter().zip(physical) {
                    assert_eq!(n.source.sample, s.source.sample);
                    assert_eq!(n.source.payload.as_ref(), s.source.payload.as_ref());
                }
                assert!(got[..8].iter().all(|p| p.native.is_some()));
                assert!(got[8].native.is_none() && !got[8].source.payload.is_empty());
                let zero = got[7].native.as_ref().unwrap();
                assert_eq!(zero.demux.dts, Some(7000));
                assert_eq!(zero.demux.source_range, 1151..1151);
                let empty: Vec<_> = output
                    .iter()
                    .enumerate()
                    .filter_map(|(i, e)| match e.event() {
                        RawEvent::Mp4(Mp4Event::NativeEmptyPacket(empty)) => Some((i, empty)),
                        _ => None,
                    })
                    .collect();
                let [(at, empty)] = empty.as_slice() else {
                    panic!("expected a single unassociated native packet");
                };
                assert!(
                    matches!(output[*at - 1].event(), RawEvent::Mp4(Mp4Event::Packet(p)) if p.source.sample.ordinal == 8)
                );
                assert!(
                    matches!(output[*at + 1].event(), RawEvent::Mp4(Mp4Event::Packet(p)) if p.source.sample.ordinal == 9)
                );
                assert_eq!(Some(empty.boundary), got[7].native_front_read_boundary_v1);
                assert_eq!(empty.boundary.source_ordinal, 8);
                assert_eq!(empty.time_base, zero.media_clock.demux_base);
                assert_eq!(empty.handler, *b"vide");
                assert_eq!(
                    (
                        empty.pts,
                        empty.dts,
                        empty.duration,
                        empty.position,
                        empty.flags
                    ),
                    (0, 0, 7000, 0, 0)
                );
                assert_eq!(signals.ends.load(Ordering::Relaxed), 1);
                drop(native);
                drop(output);
                assert_eq!(pool.used(), Budget::control_storage_bytes());
            }
        }
    }
}

#[test]
fn native_empty_run_tail_uses_the_greatest_completed_run_end_once() {
    let raw = include_bytes!("fixtures/native-terminal-controls/one-empty-between-nonempty.mp4");
    // Extend the second run of the last fragment by one sample. Update the
    // offsets of both fragments that share the final MDAT.
    let mut extended = raw.to_vec();
    for (at, old, new) in [
        (887, 304_u32, 316_u32),
        (919, 316, 328),
        (947, 148, 160),
        (971, 124, 136),
        (1035, 192, 204),
        (1051, 44, 56),
        (1063, 2, 3),
        (1067, 204, 216),
    ] {
        assert_eq!(&extended[at..at + 4], &old.to_be_bytes());
        extended[at..at + 4].copy_from_slice(&new.to_be_bytes());
    }
    extended.splice(1095..1095, raw[1083..1095].iter().copied());
    extended.extend_from_slice(&[0xa5; 12]);
    for (clock, last_duration) in [(6000_u64, 7000), (9000, 10000), (0, 6000)] {
        extended[1011..1019].copy_from_slice(&clock.to_be_bytes());
        let (mut native, _, _) = make(&extended, false, 7, 1, true);
        let output = collect(&mut native, 1).unwrap();
        assert_eq!(packets(&output).len(), 10);
        let durations: Vec<_> = output
            .iter()
            .filter_map(|e| match e.event() {
                RawEvent::Mp4(Mp4Event::NativeEmptyPacket(empty)) => Some(empty.duration),
                _ => None,
            })
            .collect();
        assert_eq!(durations, [0, last_duration]);
    }
}

#[test]
fn final_indexed_zero_read_preserves_the_literal_packet() {
    let case = gold::case("two-nonempty-then-empty");
    for known in [false, true] {
        for chunk in [1, 7, 4096] {
            for quantum in [1, 4096] {
                let (mut native, b, signals) = make(case.raw, known, chunk, quantum, true);
                let got = collect(&mut native, quantum).unwrap();
                check(&got, case);
                let (mut source, _, _) = make_mode(case.raw, known, chunk, quantum, true, false);
                let literal = collect(&mut source, quantum).unwrap();
                let source_packets = packets(&literal);
                let native_packets = packets(&got);
                assert_eq!(source_packets.len(), 9);
                assert_eq!(native_packets.len(), source_packets.len());
                for (observed, expected) in native_packets.iter().zip(source_packets) {
                    assert_eq!(observed.source.sample, expected.source.sample);
                    assert_eq!(
                        observed.source.payload.as_ref(),
                        expected.source.payload.as_ref()
                    );
                }
                let boundary = native_packets[8].native_front_read_boundary_v1.unwrap();
                assert_eq!(
                    (
                        boundary.track_id,
                        boundary.source_ordinal,
                        boundary.source_offset
                    ),
                    (1, 9, 1163)
                );
                assert!(signals.ends.load(Ordering::Relaxed) >= 1);
                drop(native);
                check(&got, case);
                drop(got);
                assert_eq!(b.used(), Budget::control_storage_bytes());
            }
        }
    }
}

#[test]
fn final_indexed_zero_read_leaves_the_source_open_for_a_later_fragment() {
    let case = gold::case("two-nonempty-then-empty");
    let mut raw = case.raw.to_vec();
    // Make the terminal MDAT finite and append a new fragment at the next
    // decode time. The zero read has already ended native packet delivery.
    let terminal = 1095;
    assert_eq!(&raw[terminal..terminal + 8], b"\0\0\0\0mdat");
    let size = u32::try_from(raw.len() - terminal).unwrap();
    raw[terminal..terminal + 4].copy_from_slice(&size.to_be_bytes());
    let mut next = raw[607..799].to_vec();
    assert_eq!(&next[12..16], b"mfhd");
    next[20..24].copy_from_slice(&4u32.to_be_bytes());
    assert_eq!(&next[56..61], b"tfdt\x01");
    next[64..72].copy_from_slice(&9000u64.to_be_bytes());
    raw.extend_from_slice(&next);

    for known in [false, true] {
        let (mut native, _, signals) = make(&raw, known, 1, 1, false);
        let mut got = Vec::new();
        for _ in 0..30_000 {
            assert!(!matches!(
                native.advance(&mut WorkBudget::new(1), &off()).unwrap(),
                Progress::Ended
            ));
            while let Some(output) = native.pop() {
                got.push(output);
            }
            if native
                .pending_request()
                .is_some_and(|r| r.offset == raw.len() as u64)
            {
                break;
            }
        }
        assert_eq!(native.pending_request().unwrap().offset, raw.len() as u64);
        assert_eq!(signals.ends.load(Ordering::Relaxed), 0);
        check(&got, case);
        let observed = packets(&got);
        assert_eq!(observed.len(), 12);
        let boundary = observed[8].native_front_read_boundary_v1.unwrap();
        for (packet, expected_dts) in observed[9..].iter().zip([9000, 10000, 11000]) {
            assert_eq!(packet.source.sample.dts_ticks, Some(expected_dts));
            assert_eq!(packet.source.payload.len(), 12);
            assert!(packet.native.is_none());
            assert_eq!(packet.native_front_read_boundary_v1, Some(boundary));
        }
        signals.eof.store(true, Ordering::Relaxed);
        assert!(collect(&mut native, 1).unwrap().is_empty());
        assert_eq!(signals.ends.load(Ordering::Relaxed), 1);
    }
}

#[test]
fn final_zero_read_at_media_end_keeps_later_indexed_payloads() {
    let raw =
        include_bytes!("fixtures/native-terminal-controls/later-run-zero-next-indexed-mdat.mp4");
    for known in [false, true] {
        let (mut source, _, _) = make_mode(raw, known, 7, 1, true, false);
        let literal = collect(&mut source, 1).unwrap();
        let (mut native, _, signals) = make(raw, known, 7, 1, true);
        let got = collect(&mut native, 1).unwrap();
        let got = packets(&got);
        let literal = packets(&literal);
        assert_eq!(got.len(), 12);
        assert_eq!(literal.len(), got.len());
        for (packet, source) in got.iter().zip(literal) {
            assert_eq!(packet.source.sample, source.source.sample);
            assert_eq!(
                packet.source.payload.as_ref(),
                source.source.payload.as_ref()
            );
        }
        assert!(got[..9].iter().all(|p| p.native.is_some()));
        let boundary = got[8].native_front_read_boundary_v1.unwrap();
        assert_eq!((boundary.source_ordinal, boundary.source_offset), (9, 1311));
        assert_eq!(got[8].native.as_ref().unwrap().demux.dts, Some(8000));
        assert!(got[8].source.payload.is_empty());
        for p in &got[9..] {
            assert_eq!(p.source.payload.len(), 12);
            assert!(p.native.is_none());
            assert_eq!(p.native_front_read_boundary_v1, Some(boundary));
        }
        assert!(signals.ends.load(Ordering::Relaxed) >= 1);
    }
}
#[test]
fn reversed_fragment_ranges_keep_native_decode_order_on_a_forward_source() {
    let c = gold::case("progressive--pending-reverse-ranges");
    for known in [false, true] {
        for chunk in [1, 7, 4096] {
            for work in [1, 4096] {
                let (mut native, pool, signals) = make(c.raw, known, chunk, work, true);
                let output = collect(&mut native, work).unwrap();
                check(&output, c);
                let (mut source, _, _) = make_mode(c.raw, known, chunk, work, true, false);
                let literal = collect(&mut source, work).unwrap();
                check_reversed_fragment_source(&output, &literal);
                let got = packets(&output);
                assert!(
                    got[3]
                        .source
                        .payload
                        .shares_storage_with(&got[8].source.payload)
                );
                assert_eq!(signals.ends.load(Ordering::Relaxed), 1);
                drop(native);
                check(&output, c);
                drop(output);
                assert_eq!(pool.used(), Budget::control_storage_bytes());
            }
        }
    }
}

#[test]
fn reversed_fragment_ranges_keep_physical_source_order_on_a_seekable_source() {
    let c = gold::case("progressive--pending-reverse-ranges");
    let (mut native, _, _) =
        try_make_source_mode(c.raw, true, 7, 1, true, TestView::Native, true).unwrap();
    let output = collect(&mut native, 1).unwrap();
    check(&output, c);
    let (mut source, _, _) =
        try_make_source_mode(c.raw, true, 7, 1, true, TestView::Source, true).unwrap();
    let literal = collect(&mut source, 1).unwrap();
    check_reversed_fragment_source(&output, &literal);
}

#[test]
fn reversed_forward_fragments_require_a_complete_bounded_extent() {
    let c = gold::case("progressive--pending-reverse-ranges");
    let mut oversized = c.raw.to_vec();
    assert_eq!(&oversized[1095..1103], b"\0\0\0Pmdat");
    let length = 32 * 1024 + 1_u32;
    oversized[1095..1099].copy_from_slice(&(length + 8).to_be_bytes());
    oversized.resize(1103 + length as usize, 0);
    let (mut native, _, _) = make(&oversized, true, 7, 13, true);
    assert!(matches!(
        collect(&mut native, 13),
        Err(Error::Unsupported(
            "native replay exceeds media cache bound"
        ))
    ));

    let (mut native, pool, signals) = make(&c.raw[..c.raw.len() - 1], false, 7, 13, true);
    assert!(matches!(collect(&mut native, 13), Err(Error::Truncated)));
    assert_eq!(signals.ends.load(Ordering::Relaxed), 1);
    drop(native);
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}

fn check_reversed_fragment_source(native: &[Output], literal: &[Output]) {
    let source = packets(literal);
    assert_eq!(
        source
            .iter()
            .map(|p| p.source.sample.ordinal)
            .collect::<Vec<_>>(),
        [1, 2, 3, 7, 8, 9, 4, 5, 6]
    );
    for (at, packet) in packets(native).into_iter().enumerate() {
        assert_eq!(packet.source.sample.ordinal, at as u64 + 1);
        let expected = source
            .iter()
            .find(|p| p.source.sample.ordinal == at as u64 + 1)
            .unwrap();
        assert_eq!(packet.source.sample, expected.source.sample);
        assert_eq!(
            packet.source.payload.as_ref(),
            expected.source.payload.as_ref()
        );
    }
}

#[test]
fn native_from_start_origin_never_claims_complete_entity_or_source_clock() {
    let raw = include_bytes!("fixtures/native-terminal-controls/first-source-clock-unknown.mp4");
    let (mut p, _b, s) = make(raw, false, 1, 1, false);
    let mut out = Vec::new();
    for _ in 0..300_000 {
        assert!(!matches!(
            p.advance(&mut WorkBudget::new(1), &off()).unwrap(),
            Progress::Ended
        ));
        while let Some(x) = p.pop() {
            out.push(x);
        }
        if p.pending_request()
            .is_some_and(|r| r.offset == raw.len() as u64)
        {
            break;
        }
    }
    let samples = packets(&out);
    assert_eq!(samples.len(), 9);
    assert_eq!(s.ends.load(Ordering::Relaxed), 0);
    for p in samples.iter().take(3) {
        assert_eq!(p.source.sample.dts_ticks, None);
        assert_eq!(p.source.sample.cts_ticks, None);
    }
    assert_eq!(
        format!("{:?}", samples[0].native.as_ref().unwrap().clock_origin),
        "FrontSourceMediaStart"
    );
    assert_eq!(samples[0].native.as_ref().unwrap().demux.dts, Some(0));
    s.eof.store(true, Ordering::Relaxed);
    out.extend(collect(&mut p, 1).unwrap());
    assert_eq!(packets(&out).len(), 9);
}

fn regular_multi_mdat() -> &'static gold::Case {
    gold::case("mp4--r-multiple-mdats")
}

#[test]
fn regular_multiple_finite_mdats_preserve_whole_native_and_source_packets() {
    let case = regular_multi_mdat();
    for known in [false, true] {
        for chunk in [1, 7, 4096] {
            for work in [1, 4096] {
                let (mut native, b, _) = make(case.raw, known, chunk, work, true);
                let output = collect(&mut native, work).unwrap();
                check(&output, case);
                let (mut source, _, _) = make_mode(case.raw, known, chunk, work, true, false);
                let literal = collect(&mut source, work).unwrap();
                let source_packets = packets(&literal);
                let native_packets = packets(&output);
                assert_eq!(source_packets.len(), 6);
                assert_eq!(source_packets.len(), native_packets.len());
                for (source, native) in source_packets.into_iter().zip(native_packets) {
                    assert_eq!(source.source.sample, native.source.sample);
                    assert_eq!(
                        source.source.payload.as_ref(),
                        native.source.payload.as_ref()
                    );
                    assert!(source.native.is_none());
                    assert!(native.native_front_read_boundary_v1.is_none());
                }
                drop(native);
                check(&output, case);
                drop(output);
                assert_eq!(b.used(), Budget::control_storage_bytes());
            }
        }
    }
}

#[test]
fn forward_native_edits_replay_payloads_without_duplicating_source_samples() {
    let case = gold::case("mp4--r-edits-repeat");
    for known in [false, true] {
        for chunk in [1, 7, 4096] {
            for work in [1, 4096] {
                let (mut source, _, _) = make_mode(case.raw, known, chunk, work, true, false);
                let literal = collect(&mut source, work).unwrap();
                let physical = packets(&literal);
                assert_eq!(physical.len(), 6);
                let (mut native, pool, signals) = make(case.raw, known, chunk, work, true);
                let output = collect(&mut native, work).unwrap();
                check(&output, case);
                let emitted = packets(&output);
                for packet in &emitted {
                    let sample = &physical[packet.source.sample.ordinal as usize - 1].source;
                    assert_eq!(packet.source.sample, sample.sample);
                    assert_eq!(packet.source.payload.as_ref(), sample.payload.as_ref());
                }
                assert!(
                    emitted[0]
                        .source
                        .payload
                        .shares_storage_with(&emitted[7].source.payload)
                );
                assert_eq!(signals.ends.load(Ordering::Relaxed), 1);
                drop(native);
                check(&output, case);
                drop(output);
                assert_eq!(pool.used(), Budget::control_storage_bytes());
            }
        }
    }
}

#[test]
fn forward_native_edit_replay_requires_a_complete_extent_within_the_cache_limit() {
    let case = gold::case("mp4--r-edits-repeat");
    let mut oversized = case.raw.to_vec();
    assert_eq!(&oversized[727..735], b"\0\0\0Pmdat");
    let length = 32 * 1024 + 1_u32;
    oversized[727..731].copy_from_slice(&(length + 8).to_be_bytes());
    oversized.resize(735 + length as usize, 0);
    assert!(matches!(
        try_make_mode(&oversized, true, 64, 4096, true, true),
        Err(Error::Unsupported(
            "native front source occurrence order is not physical"
        ))
    ));

    let truncated = &case.raw[..case.raw.len() - 1];
    let (mut player, pool, signals) = make(truncated, false, 7, 13, true);
    assert!(matches!(collect(&mut player, 13), Err(Error::Truncated)));
    assert_eq!(signals.ends.load(Ordering::Relaxed), 1);
    drop(player);
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}

#[test]
fn forward_tail_moov_reuses_media_consumed_during_index_discovery() {
    let case = gold::case("mp4--r-end-co64");
    for known in [false, true] {
        for chunk in [1, 7, 4096] {
            for work in [1, 4096] {
                let (mut native, pool, signals) = make(case.raw, known, chunk, work, true);
                let output = collect(&mut native, work).unwrap();
                check(&output, case);
                assert_eq!(signals.ends.load(Ordering::Relaxed), 1);
                let (mut source, _, _) = make_mode(case.raw, known, chunk, work, true, false);
                let literal = collect(&mut source, work).unwrap();
                let got = packets(&output);
                let want = packets(&literal);
                assert_eq!(got.len(), want.len());
                for (n, s) in got.iter().zip(want) {
                    assert_eq!(n.source.sample, s.source.sample);
                    assert_eq!(n.source.payload.as_ref(), s.source.payload.as_ref());
                    assert!(n.source.payload.shares_storage_with(&got[0].source.payload));
                    assert!(s.native.is_none());
                }
                drop(native);
                check(&output, case);
                drop(output);
                assert_eq!(pool.used(), Budget::control_storage_bytes());
            }
        }
    }
}

#[test]
fn forward_tail_moov_retains_only_whole_media_extents_within_the_cache_limit() {
    let case = gold::case("mp4--r-end-co64");
    for (payload_bytes, retained) in [(32 * 1024, true), (32 * 1024 + 1, false)] {
        let mut raw = case.raw[..100].to_vec();
        assert_eq!(&raw[20..28], b"\0\0\0Pmdat");
        raw[20..24].copy_from_slice(&(payload_bytes + 8_u32).to_be_bytes());
        raw.resize(28 + payload_bytes as usize, 0);
        raw.extend_from_slice(&case.raw[100..]);
        let (mut player, pool, signals) = make(&raw, false, 37, 251, true);
        let result = collect(&mut player, 251);
        if retained {
            check(&result.unwrap(), case);
        } else {
            assert!(matches!(
                result,
                Err(triergon_core::Error::Unsupported(
                    "sequential source sample bytes already passed"
                ))
            ));
        }
        assert_eq!(signals.ends.load(Ordering::Relaxed), 1);
        drop(player);
        assert_eq!(pool.used(), Budget::control_storage_bytes());
    }
}

#[test]
fn forward_tail_moov_resolves_offsets_across_multiple_retained_mdats() {
    let case = gold::case("mp4--r-end-co64");
    let mut raw = case.raw.to_vec();
    let co64 = raw.windows(4).position(|bytes| bytes == b"co64").unwrap();
    let third_chunk = co64 + 12 + 2 * 8;
    assert_eq!(&raw[third_chunk..third_chunk + 8], &64_u64.to_be_bytes());
    raw[third_chunk..third_chunk + 8].copy_from_slice(&72_u64.to_be_bytes());
    raw[20..24].copy_from_slice(&44_u32.to_be_bytes());
    raw.splice(64..64, *b"\0\0\0,mdat");
    let (mut player, _, signals) = make(&raw, true, 7, 11, true);
    let output = collect(&mut player, 11).unwrap();
    let got = packets(&output);
    assert_eq!(got.len(), case.packets().len());
    for (i, (packet, gold)) in got.iter().zip(case.packets()).enumerate() {
        let offset = gold.offset() + if i < 3 { 0 } else { 8 };
        assert_eq!(packet.source.sample.range, offset..offset + 12);
        assert_eq!(packet.source.payload.as_ref(), gold.data);
        assert_eq!(packet.native.as_ref().unwrap().demux.dts, gold.dts);
        assert!(
            packet
                .source
                .payload
                .shares_storage_with(&got[i / 3 * 3].source.payload)
        );
    }
    assert!(
        !got[0]
            .source
            .payload
            .shares_storage_with(&got[3].source.payload)
    );
    assert_eq!(signals.ends.load(Ordering::Relaxed), 1);
}

#[test]
fn regular_forward_samples_in_a_terminal_mdat_require_actual_eof() {
    let case = regular_multi_mdat();
    let mut raw = case.raw.to_vec();
    assert_eq!(&raw[719..727], b"\0\0\0,mdat");
    raw[719..723].fill(0);
    raw.extend_from_slice(&[0x83; 31]);

    for known in [false, true] {
        for native in [false, true] {
            let (mut p, _, signals) = make_mode(&raw, known, 1, 1, false, native);
            let mut out = Vec::new();
            for _ in 0..30_000 {
                assert!(!matches!(
                    p.advance(&mut WorkBudget::new(1), &off()).unwrap(),
                    Progress::Ended
                ));
                while let Some(output) = p.pop() {
                    out.push(output);
                }
                if p.pending_request()
                    .is_some_and(|r| r.offset == raw.len() as u64)
                {
                    break;
                }
            }
            assert_eq!(p.pending_request().unwrap().offset, raw.len() as u64);
            assert_eq!(signals.ends.load(Ordering::Relaxed), 0);
            let got = packets(&out);
            assert_eq!(got.len(), case.packets().len());
            for (packet, expected) in got.iter().zip(case.packets()) {
                assert_eq!(packet.source.payload.as_ref(), expected.data);
                assert_eq!(packet.source.sample.range.start, expected.offset());
                assert_eq!(packet.source.sample.dts_ticks, expected.dts.map(i128::from));
            }
            if native {
                check(&out, case);
            } else {
                assert!(got.iter().all(|p| p.native.is_none()));
            }
            signals.eof.store(true, Ordering::Relaxed);
            assert!(collect(&mut p, 1).unwrap().is_empty());
            assert_eq!(signals.ends.load(Ordering::Relaxed), 1);
        }
    }
}

#[test]
fn regular_forward_terminal_mdat_rejects_header_offsets_and_truncated_payloads() {
    let case = regular_multi_mdat();
    let mut terminal = case.raw.to_vec();
    terminal[719..723].fill(0);
    let mut header_offset = terminal.clone();
    let stco = header_offset
        .windows(4)
        .position(|bytes| bytes == b"stco")
        .unwrap();
    assert_eq!(&header_offset[stco + 20..stco + 24], &727u32.to_be_bytes());
    header_offset[stco + 20..stco + 24].copy_from_slice(&719u32.to_be_bytes());
    for native in [false, true] {
        let (mut p, _, signals) = make_mode(&header_offset, false, 1, 1, true, native);
        assert!(matches!(
            collect(&mut p, 1),
            Err(Error::Invalid("sample outside terminal MDAT declaration"))
        ));
        assert_eq!(signals.ends.load(Ordering::Relaxed), 0);
        let truncated = &terminal[..terminal.len() - 1];
        let (mut p, _, signals) = make_mode(truncated, false, 1, 1, true, native);
        assert!(matches!(collect(&mut p, 1), Err(Error::Truncated)));
        assert_eq!(signals.ends.load(Ordering::Relaxed), 1);
    }
}

#[test]
fn regular_multiple_finite_mdats_pending_and_transport_keep_the_prefix() {
    let case = regular_multi_mdat();
    for fail in [false, true] {
        let (mut player, _, signals) = make(case.raw, false, 1, 1, false);
        signals.pause_at.store(699, Ordering::Relaxed);
        let mut prefix = Vec::new();
        for _ in 0..20_000 {
            let step = player.advance(&mut WorkBudget::new(1), &off()).unwrap();
            assert!(!matches!(step, Progress::Ended));
            while let Some(output) = player.pop() {
                prefix.push(output);
            }
            if player.pending_request().is_some_and(|r| r.offset == 699) {
                break;
            }
        }
        let request = player
            .pending_request()
            .expect("second root request")
            .clone();
        assert_eq!(request.offset, 699);
        assert_eq!(packets(&prefix).len(), 2);
        let polls = signals.polls.load(Ordering::Relaxed);
        for _ in 0..4 {
            assert!(!matches!(
                player.advance(&mut WorkBudget::new(0), &off()).unwrap(),
                Progress::Ended
            ));
        }
        assert_eq!(polls, signals.polls.load(Ordering::Relaxed));
        assert!(!matches!(
            player.advance(&mut WorkBudget::new(1), &off()).unwrap(),
            Progress::Ended
        ));
        let pending = player.pending_request().expect("still pending");
        assert_eq!(pending.source, request.source);
        assert_eq!(pending.offset, request.offset);
        assert_eq!(pending.maximum, request.maximum);
        assert_eq!(pending.token, request.token);
        assert_eq!(signals.ends.load(Ordering::Relaxed), 0);
        if fail {
            signals.fail.store(true, Ordering::Relaxed);
            assert!(matches!(collect(&mut player, 1), Err(Error::Transport)));
            assert_eq!(signals.ends.load(Ordering::Relaxed), 0);
        } else {
            signals.pause_at.store(0, Ordering::Relaxed);
            signals.eof.store(true, Ordering::Relaxed);
            prefix.extend(collect(&mut player, 1).unwrap());
            check(&prefix, case);
        }
    }
}

#[test]
fn regular_multiple_finite_mdats_reject_nonmedia_and_crossing_ranges() {
    let case = regular_multi_mdat();
    let stco = case
        .raw
        .windows(4)
        .position(|bytes| bytes == b"stco")
        .unwrap();
    let second_chunk_offset = stco + 16;
    assert_eq!(
        &case.raw[second_chunk_offset..second_chunk_offset + 4],
        &707u32.to_be_bytes()
    );
    for mutation in [Some(699u32), Some(712u32), None] {
        let mut raw = case.raw.to_vec();
        if let Some(offset) = mutation {
            raw[second_chunk_offset..second_chunk_offset + 4]
                .copy_from_slice(&offset.to_be_bytes());
        } else {
            assert_eq!(&raw[703..707], b"mdat");
            raw[703..707].copy_from_slice(b"free");
        }
        for native in [false, true] {
            let (mut player, _, signals) = make_mode(&raw, false, 1, 1, true, native);
            let mut prefix = Vec::new();
            let mut refused = false;
            for _ in 0..20_000 {
                match player.advance(&mut WorkBudget::new(1), &off()) {
                    Err(Error::Invalid("sample outside media extent")) => {
                        refused = true;
                        break;
                    }
                    result => assert!(!matches!(result.unwrap(), Progress::Ended)),
                }
                while let Some(output) = player.pop() {
                    prefix.push(output);
                }
            }
            assert!(refused);
            assert_eq!(packets(&prefix).len(), 2);
            assert_eq!(signals.ends.load(Ordering::Relaxed), 0);
        }
    }
}

#[test]
fn regular_multiple_finite_mdats_missing_extents_never_end_cleanly() {
    let case = regular_multi_mdat();
    for end in [699, 703, 718] {
        for native in [false, true] {
            let (mut player, _, signals) = make_mode(&case.raw[..end], false, 1, 1, true, native);
            let error = collect(&mut player, 1).unwrap_err();
            assert!(matches!(error, Error::Truncated));
            assert_eq!(signals.ends.load(Ordering::Relaxed), 1);
        }
    }
}

#[test]
fn regular_multiple_finite_mdats_refuse_backward_layout_without_claiming_malformed_ranges() {
    let mut raw = regular_multi_mdat().raw.to_vec();
    let stco = raw.windows(4).position(|bytes| bytes == b"stco").unwrap();
    let stsc = raw.windows(4).position(|bytes| bytes == b"stsc").unwrap();
    // Keep each chunk wholly inside its MDAT. Only decode-order traversal is
    // impossible on a forward-only source.
    assert_eq!(&raw[stco + 16..stco + 20], &707u32.to_be_bytes());
    assert_eq!(&raw[stco + 20..stco + 24], &727u32.to_be_bytes());
    raw[stco + 16..stco + 20].copy_from_slice(&727u32.to_be_bytes());
    raw[stco + 20..stco + 24].copy_from_slice(&707u32.to_be_bytes());
    assert_eq!(&raw[stsc + 28..stsc + 32], &1u32.to_be_bytes());
    assert_eq!(&raw[stsc + 40..stsc + 44], &3u32.to_be_bytes());
    raw[stsc + 28..stsc + 32].copy_from_slice(&3u32.to_be_bytes());
    raw[stsc + 40..stsc + 44].copy_from_slice(&1u32.to_be_bytes());
    for native in [false, true] {
        match try_make_mode(&raw, true, 7, 1, true, native) {
            Err(Error::Unsupported(
                "physical merge needs nondecreasing per-track source offsets",
            )) => {}
            Err(error) => panic!("unexpected refusal: {error:?}"),
            Ok(_) => panic!("backward chunk order must be refused"),
        }
    }
}

#[test]
fn regular_multiple_finite_mdats_retry_pressure_and_preserve_held_packets() {
    let case = regular_multi_mdat();
    let (mut player, b, _) = make(case.raw, false, 1, 1, true);
    let mut outputs = Vec::new();
    let mut ended = false;
    let mut refusals = 0;
    for _ in 0..30_000 {
        let hold = b.reserve(b.available()).unwrap();
        let step = player.advance(&mut WorkBudget::new(1), &off());
        drop(hold);
        let step = match step {
            Err(Error::Backpressure { .. }) | Ok(Progress::Backpressure { .. }) => {
                refusals += 1;
                player.advance(&mut WorkBudget::new(1), &off()).unwrap()
            }
            other => other.unwrap(),
        };
        while let Some(output) = player.pop() {
            outputs.push(output);
        }
        if matches!(step, Progress::Ended) {
            ended = true;
            break;
        }
    }
    assert!(ended && refusals > 0);
    drop(player);
    check(&outputs, case);
    drop(outputs);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}

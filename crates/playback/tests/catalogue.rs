use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use triergon_core::{
    Budget, Bytes, Error as C, InputCursor, InputRequest, SourceCapabilities, SourceId, Time,
    WorkBudget,
};
use triergon_playback::{
    catalogue::{Mp4Catalogue, Mp4CatalogueMode},
    io::{SelectedInput, SourceOwner},
    mp4_open::{Mp4Open, OpenLimits, OpenProgress, OpenedMp4},
    mp4_plan::{Mp4IndexJob, Mp4View, PlanLimits, PreparedMp4},
    *,
};
mod gold {
    include!("fixtures/mp4_golds.rs");
}
fn off() -> AtomicBool {
    AtomicBool::new(false)
}
#[derive(Default)]
struct Signals {
    polls: AtomicUsize,
    aborts: AtomicUsize,
    closes: AtomicUsize,
    stall_end: AtomicBool,
}
struct Transport {
    bytes: ByteSource,
    signals: Arc<Signals>,
}
impl Source for Transport {
    fn poll(
        &mut self,
        request: &InputRequest,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<SourceReply> {
        if work.remaining() != 0 && !cancel.load(Ordering::Relaxed) {
            self.signals.polls.fetch_add(1, Ordering::Relaxed);
        }
        match self.bytes.poll(request, work, cancel)? {
            SourceReply::End { .. } if self.signals.stall_end.load(Ordering::Relaxed) => {
                Ok(SourceReply::Pending)
            }
            reply => Ok(reply),
        }
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
    b: &Budget,
    id: SourceId,
    raw: &[u8],
    quantum: usize,
) -> (Mp4Open<Transport>, Arc<Signals>) {
    range_opener(b, id, raw, quantum, false)
}

fn range_opener(
    b: &Budget,
    id: SourceId,
    raw: &[u8],
    quantum: usize,
    progressive: bool,
) -> (Mp4Open<Transport>, Arc<Signals>) {
    let signals = Arc::new(Signals::default());
    let source = Transport {
        bytes: ByteSource::new(id, Bytes::copy_from_slice(b, raw).unwrap(), quantum).unwrap(),
        signals: signals.clone(),
    };
    let input = InputCursor::new(
        b,
        id,
        SourceCapabilities {
            length: Some(raw.len() as u64),
            random_access: true,
        },
        4096,
    )
    .unwrap();
    let selected = SelectedInput {
        input,
        source: SourceOwner::new(source),
        format: Format::Mp4,
    };
    let limits = OpenLimits::default();
    let open = if progressive {
        Mp4Open::new_progressive(selected, limits)
    } else {
        Mp4Open::new(selected, limits)
    };
    (open.unwrap(), signals)
}
fn index(opened: &OpenedMp4<Transport>, view: Mp4View) -> PreparedMp4 {
    let mut job = Mp4IndexJob::new(&opened.movie, view, PlanLimits::default()).unwrap();
    for _ in 0..200_000 {
        if job.poll(&mut WorkBudget::new(1), &off()).unwrap() {
            return job.finish().unwrap();
        }
    }
    panic!("bounded fixture index did not complete");
}
fn check_plan(plan: &PreparedMp4, case: &gold::Case) {
    assert_eq!(
        plan.source_rows().len(),
        case.samples.len(),
        "{}",
        case.name
    );
    for expected in case.samples {
        let row = plan
            .source_rows()
            .iter()
            .find(|row| row.track_id == expected.track && row.sample.ordinal == expected.ordinal)
            .unwrap();
        assert_eq!(row.track_id, expected.track);
        assert_eq!(row.sample.ordinal, expected.ordinal);
        assert_eq!(row.sample.description_index, expected.description);
        assert_eq!(
            row.sample.range,
            expected.offset..expected.offset + expected.payload.len() as u64
        );
        if case.name == "f-no-second-tfdt" && expected.ordinal > 3 {
            assert_eq!(row.sample.dts_ticks, None);
            assert_eq!(row.sample.cts_ticks, None);
        } else {
            assert_eq!(row.sample.dts_ticks, Some(expected.dts));
            assert_eq!(row.sample.cts_ticks, Some(expected.cts));
        }
        assert_eq!(row.sample.duration_ticks, expected.delta);
        assert_eq!(row.sample.is_container_sync(), expected.sync);
    }
}
#[test]
fn catalogue_required_opener_pressure_keeps_source_alive_and_complete_facts() {
    for case in gold::CASES.iter().filter(|x| !x.malformed) {
        // gap is the historical native-view disposition, never a Source exclusion.
        println!(
            "PRESSURE_SOURCE {} native_gap={} fragmented={}",
            case.name, case.gap, case.fragmented
        );
        for quantum in [1, 4096] {
            let b = Budget::new(64 << 20).unwrap();
            let (mut open, signals) = opener(&b, SourceId(8101), case.bytes, quantum);
            let mut refused = 0;
            let mut ready = false;
            for _ in 0..200_000 {
                let hold = b.reserve(b.available()).unwrap();
                let result = open.advance(&mut WorkBudget::new(1), &off());
                drop(hold);
                let step = match result {
                    Err(C::Backpressure { .. }) => {
                        refused += 1;
                        assert_eq!(
                            signals.aborts.load(Ordering::Relaxed),
                            0,
                            "allocation refusal must not retire source {}",
                            case.name
                        );
                        assert_eq!(signals.closes.load(Ordering::Relaxed), 0);
                        let before = (b.used(), signals.polls.load(Ordering::Relaxed));
                        assert!(matches!(
                            open.advance(&mut WorkBudget::new(0), &off()).unwrap(),
                            OpenProgress::Yield
                        ));
                        assert_eq!((b.used(), signals.polls.load(Ordering::Relaxed)), before);
                        open.advance(&mut WorkBudget::new(1), &off()).unwrap()
                    }
                    value => value.unwrap(),
                };
                if matches!(step, OpenProgress::Ready) {
                    ready = true;
                    break;
                }
            }
            assert!(ready, "{}", case.name);
            assert!(refused > 0);
            let opened = open.finish().unwrap();
            let plan = index(&opened, Mp4View::SourcePhysical);
            check_plan(&plan, case);
            for raw in opened.movie.raw_roots() {
                let start = usize::try_from(raw.header().start).unwrap();
                assert_eq!(
                    raw.bytes().as_ref(),
                    &case.bytes[start..start + raw.bytes().len()]
                );
            }
            drop(opened);
            check_plan(&plan, case);
            drop(plan);
            assert_eq!(signals.aborts.load(Ordering::Relaxed), 1);
            assert_eq!(signals.closes.load(Ordering::Relaxed), 1);
            assert_eq!(b.used(), Budget::control_storage_bytes());
        }
    }
}

fn prepared(
    b: &Budget,
    id: SourceId,
    case: &gold::Case,
    view: Mp4View,
) -> (OpenedMp4<Transport>, PreparedMp4, Arc<Signals>) {
    let (mut job, signals) = opener(b, id, case.bytes, 7);
    for _ in 0..200_000 {
        if matches!(
            job.advance(&mut WorkBudget::new(1), &off()).unwrap(),
            OpenProgress::Ready
        ) {
            let opened = job.finish().unwrap();
            let plan = index(&opened, view);
            return (opened, plan, signals);
        }
    }
    panic!("bounded fixture open did not complete");
}
fn player(
    b: &Budget,
    id: SourceId,
    case: &gold::Case,
    policy: PlayerPolicy,
) -> (Player<Transport>, Arc<Signals>) {
    let (opened, plan, signals) = prepared(b, id, case, Mp4View::SourcePhysical);
    (
        Player::new(
            Engine::mp4(opened.input, plan, 1 << 20).unwrap(),
            opened.source,
            Selection::All,
            policy,
        )
        .unwrap(),
        signals,
    )
}
fn candidate(
    b: &Budget,
    id: SourceId,
    case: &gold::Case,
    view: Mp4View,
) -> (Option<Mp4Catalogue<Transport>>, Arc<Signals>) {
    let (opened, plan, signals) = prepared(b, id, case, view);
    (
        Some(Mp4Catalogue::new(opened, plan, 1 << 20, Mp4CatalogueMode::Indexed).unwrap()),
        signals,
    )
}
fn read(p: &mut Player<Transport>) -> Vec<Output> {
    let mut out = Vec::new();
    for _ in 0..200_000 {
        let step = p.advance(&mut WorkBudget::new(1), &off()).unwrap();
        while let Some(value) = p.pop() {
            out.push(value);
        }
        if matches!(step, Progress::Ended) {
            return out;
        }
    }
    panic!("bounded fixture delivery did not complete");
}
fn first_packet(p: &mut Player<Transport>) -> Output {
    for _ in 0..200_000 {
        p.advance(&mut WorkBudget::new(1), &off()).unwrap();
        while let Some(value) = p.pop() {
            if matches!(value.event(), RawEvent::Mp4(Mp4Event::Packet(_))) {
                return value;
            }
        }
    }
    panic!("bounded fixture produced no packet");
}
fn fill(p: &mut Player<Transport>, count: usize) {
    for _ in 0..200_000 {
        if p.queued() == count {
            return;
        }
        p.advance(&mut WorkBudget::new(1), &off()).unwrap();
    }
    panic!("fixture queue did not fill");
}
mod root_facts {
    include!("fixtures/catalogue_root_facts.rs");
}
fn physical_fold(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x100000001b3)
    })
}
fn check_outputs(out: &[Output], case: &gold::Case, id: SourceId) {
    check_outputs_with_roots(out, case, id, case.bytes, None);
}
fn check_outputs_with_roots(
    out: &[Output],
    case: &gold::Case,
    id: SourceId,
    physical: &[u8],
    proof: Option<&root_facts::Fixture>,
) {
    let roots = if let Some(proof) = proof {
        assert_eq!(physical.len(), proof.length);
        assert_eq!(physical_fold(physical), proof.fold);
        proof.roots
    } else {
        &[]
    };
    let mut root_at = 0;
    let mut configs = std::collections::BTreeMap::new();
    let mut packets = 0;
    for output in out {
        assert_eq!(output.source_id(), id);
        match output.event() {
            RawEvent::Mp4(Mp4Event::Configuration(c)) => {
                configs.insert(c.track.track_id, (c.description.index, c.revision));
                let raw = c.description.raw().bytes();
                let at = c.description.raw().header().start as usize;
                assert_eq!(raw.as_ref(), &case.bytes[at..at + raw.len()]);
            }
            RawEvent::Mp4(Mp4Event::Packet(p)) => {
                packets += 1;
                let wanted = case
                    .samples
                    .iter()
                    .find(|wanted| {
                        wanted.track == p.configuration.track.track_id
                            && wanted.ordinal == p.source.sample.ordinal
                    })
                    .unwrap();
                assert_eq!(p.source.source, id);
                assert_eq!(p.source.payload.as_ref(), wanted.payload);
                assert_eq!(
                    p.source.sample.range,
                    wanted.offset..wanted.offset + wanted.payload.len() as u64
                );
                assert_eq!(p.source.sample.description_index, wanted.description);
                assert_eq!(p.source.sample.duration_ticks, wanted.delta);
                assert_eq!(p.source.sample.is_container_sync(), wanted.sync);
                if case.name == "f-no-second-tfdt" && wanted.ordinal > 3 {
                    assert_eq!(p.source.sample.dts_ticks, None);
                    assert_eq!(p.source.sample.cts_ticks, None);
                } else {
                    assert_eq!(p.source.sample.dts_ticks, Some(wanted.dts));
                    assert_eq!(p.source.sample.cts_ticks, Some(wanted.cts));
                }
                assert_eq!(
                    configs.get(&p.configuration.track.track_id),
                    Some(&(wanted.description, p.configuration.revision)),
                    "configuration must precede its packet"
                );
            }
            RawEvent::Mp4Root(raw) => {
                let wanted = roots.get(root_at).expect("unexpected extra source root");
                let header = raw.header();
                assert_eq!(header.kind, wanted.kind);
                assert_eq!(header.start, wanted.start);
                assert_eq!(header.end().unwrap(), Some(wanted.end));
                assert_eq!(header.header_size, wanted.header);
                assert_eq!(raw.bytes().len() as u64, wanted.end - wanted.start);
                let start = usize::try_from(wanted.start).unwrap();
                let end = usize::try_from(wanted.end).unwrap();
                assert_eq!(raw.bytes().as_ref(), &physical[start..end]);
                assert_eq!(physical_fold(raw.bytes().as_ref()), wanted.fold);
                root_at += 1;
            }
            event => panic!("unexpected event {event:?}"),
        }
    }
    assert_eq!(
        root_at,
        roots.len(),
        "missing retained physical source root"
    );
    assert_eq!(packets, case.samples.len());
}
fn untouched(signals: &Signals) {
    assert_eq!(signals.aborts.load(Ordering::Relaxed), 0);
    assert_eq!(signals.closes.load(Ordering::Relaxed), 0);
}
fn once(signals: &Signals) {
    assert_eq!(signals.aborts.load(Ordering::Relaxed), 1);
    assert_eq!(signals.closes.load(Ordering::Relaxed), 1);
}

#[test]
fn catalogue_all_valid_original_movies_replace_without_rewriting_source_facts() {
    for case in gold::CASES.iter().filter(|x| !x.malformed) {
        let b = Budget::new(64 << 20).unwrap();
        let (mut p, old) = player(
            &b,
            SourceId(8201),
            &gold::CASES[0],
            PlayerPolicy {
                slots: 3,
                ..PlayerPolicy::default()
            },
        );
        let retained = first_packet(&mut p);
        fill(&mut p, 2);
        let (mut next, new) = candidate(&b, SourceId(8202), case, Mp4View::SourcePhysical);
        let before = (
            old.polls.load(Ordering::Relaxed),
            new.polls.load(Ordering::Relaxed),
        );
        let mut retired = p
            .replace_mp4_catalogue(&mut next, Selection::All, &mut WorkBudget::new(1), &off())
            .unwrap()
            .unwrap();
        assert!(next.is_none());
        assert_eq!(
            before,
            (
                old.polls.load(Ordering::Relaxed),
                new.polls.load(Ordering::Relaxed)
            )
        );
        once(&old);
        untouched(&new);
        assert!(!p.current(&retained));
        assert_eq!(retained.source_id(), SourceId(8201));
        assert_eq!(retired.queued(), 2);
        assert_eq!(retired.indexed_row_cursor(), Some(3));
        let mut ordinal = 2;
        while let Some(output) = retired.pop() {
            assert!(!p.current(&output));
            assert_eq!(output.source_id(), SourceId(8201));
            let RawEvent::Mp4(Mp4Event::Packet(packet)) = output.event() else {
                panic!()
            };
            assert_eq!(packet.source.sample.ordinal, ordinal);
            assert_eq!(
                packet.source.payload.as_ref(),
                gold::CASES[0].samples[ordinal as usize - 1].payload
            );
            ordinal += 1;
        }
        assert_eq!(ordinal, 4);
        let Engine::Mp4(old_engine) = retired.engine().unwrap() else {
            panic!()
        };
        check_plan(&old_engine.plan, &gold::CASES[0]);
        let out = read(&mut p);
        check_outputs(&out, case, SourceId(8202));
        assert!(out.iter().all(|o| p.current(o) && o.epoch() == 1));
        drop(p);
        once(&old);
        once(&new);
        check_outputs(&out, case, SourceId(8202));
        drop(out);
        drop(retained);
        drop(retired);
        assert_eq!(b.used(), Budget::control_storage_bytes());
    }
}

#[test]
fn catalogue_retirement_pressure_returns_candidate_and_keeps_old_output_current() {
    let b = Budget::new(64 << 20).unwrap();
    let (mut p, old) = player(&b, SourceId(8301), &gold::CASES[0], PlayerPolicy::default());
    let output = first_packet(&mut p);
    fill(&mut p, 2);
    let (mut next, new) = candidate(&b, SourceId(8302), &gold::CASES[4], Mp4View::SourcePhysical);
    let polls = (
        old.polls.load(Ordering::Relaxed),
        new.polls.load(Ordering::Relaxed),
    );
    let hold = b.reserve(b.available()).unwrap();
    for _ in 0..3 {
        assert!(matches!(
            p.replace_mp4_catalogue(&mut next, Selection::All, &mut WorkBudget::new(1), &off()),
            Err(C::Backpressure { .. })
        ));
        assert!(next.is_some());
        assert!(p.current(&output));
        assert_eq!(p.queued(), 2);
        untouched(&old);
        untouched(&new);
        assert_eq!(
            polls,
            (
                old.polls.load(Ordering::Relaxed),
                new.polls.load(Ordering::Relaxed)
            )
        );
    }
    drop(hold);
    let retired = p
        .replace_mp4_catalogue(&mut next, Selection::All, &mut WorkBudget::new(1), &off())
        .unwrap()
        .unwrap();
    assert_eq!(retired.queued(), 2);
    check_outputs(&read(&mut p), &gold::CASES[4], SourceId(8302));
}

#[test]
fn catalogue_workzero_and_attempt_cancel_preserve_both_entities() {
    let b = Budget::new(64 << 20).unwrap();
    let (mut p, old) = player(&b, SourceId(8401), &gold::CASES[0], PlayerPolicy::default());
    let output = first_packet(&mut p);
    let (mut next, new) = candidate(&b, SourceId(8402), &gold::CASES[4], Mp4View::SourcePhysical);
    let before = (
        b.used(),
        old.polls.load(Ordering::Relaxed),
        new.polls.load(Ordering::Relaxed),
    );
    assert!(
        p.replace_mp4_catalogue(&mut next, Selection::All, &mut WorkBudget::new(0), &off())
            .unwrap()
            .is_none()
    );
    assert!(matches!(
        p.replace_mp4_catalogue(
            &mut next,
            Selection::All,
            &mut WorkBudget::new(0),
            &AtomicBool::new(true)
        ),
        Err(C::Cancelled)
    ));
    assert_eq!(
        before,
        (
            b.used(),
            old.polls.load(Ordering::Relaxed),
            new.polls.load(Ordering::Relaxed)
        )
    );
    assert_eq!(p.state(), State::Running);
    assert!(p.current(&output));
    assert!(next.is_some());
    untouched(&old);
    untouched(&new);
    p.replace_mp4_catalogue(&mut next, Selection::All, &mut WorkBudget::new(1), &off())
        .unwrap()
        .unwrap();
    assert!(!p.current(&output));
}

#[test]
fn catalogue_dropped_preparation_and_late_cancellation_are_rollback_only() {
    let b = Budget::new(64 << 20).unwrap();
    let (mut p, old) = player(&b, SourceId(8501), &gold::CASES[0], PlayerPolicy::default());
    let output = first_packet(&mut p);
    fill(&mut p, 2);
    let (mut next, new) = candidate(&b, SourceId(8502), &gold::CASES[4], Mp4View::SourcePhysical);
    let before = b.used();
    let cancel = off();
    {
        let tx = p
            .prepare_mp4_catalogue(&mut next, Selection::All, &mut WorkBudget::new(1), &cancel)
            .unwrap()
            .unwrap();
        assert_eq!(tx.boundary().unwrap().source, SourceId(8502));
    }
    assert_eq!(b.used(), before);
    assert!(p.current(&output));
    let tx = p
        .prepare_mp4_catalogue(&mut next, Selection::All, &mut WorkBudget::new(1), &cancel)
        .unwrap()
        .unwrap();
    cancel.store(true, Ordering::Relaxed);
    assert!(matches!(tx.commit(), Err(C::Cancelled)));
    assert_eq!(b.used(), before);
    assert_eq!(p.queued(), 2);
    assert!(next.is_some());
    assert!(p.current(&output));
    untouched(&old);
    untouched(&new);
}

#[test]
fn catalogue_identity_selection_and_foreign_pool_refusals_are_non_destructive() {
    let b = Budget::new(64 << 20).unwrap();
    let foreign = Budget::new(64 << 20).unwrap();
    let (mut p, old) = player(&b, SourceId(8601), &gold::CASES[0], PlayerPolicy::default());
    let output = first_packet(&mut p);
    for (pool, id, selection, expected) in [
        (&b, SourceId(8601), Selection::All, C::SourceMismatch),
        (&foreign, SourceId(8602), Selection::All, C::ForeignPool),
        (
            &b,
            SourceId(8603),
            Selection::Tracks {
                video: Some(TrackKey::Mp4(999)),
                audio: None,
                subtitle: None,
            },
            C::Invalid("selected source track missing/disabled/kind"),
        ),
    ] {
        let (mut next, signals) = candidate(pool, id, &gold::CASES[0], Mp4View::SourcePhysical);
        assert!(
            matches!(p.replace_mp4_catalogue(&mut next, selection, &mut WorkBudget::new(1), &off()),
            Err(e) if e == expected)
        );
        assert!(p.current(&output));
        assert!(next.is_some());
        untouched(&old);
        untouched(&signals);
        drop(next);
        once(&signals);
    }
}

#[test]
fn catalogue_selection_storage_pressure_retries_before_retiring_any_owner() {
    let b = Budget::new(64 << 20).unwrap();
    let (mut p, old) = player(&b, SourceId(8701), &gold::CASES[0], PlayerPolicy::default());
    let (mut next, new) = candidate(&b, SourceId(8702), &gold::CASES[0], Mp4View::SourcePhysical);
    let selected = Selection::Tracks {
        video: Some(TrackKey::Mp4(1)),
        audio: None,
        subtitle: None,
    };
    let hold = b.reserve(b.available()).unwrap();
    assert!(matches!(
        p.replace_mp4_catalogue(&mut next, selected, &mut WorkBudget::new(1), &off()),
        Err(C::Backpressure { .. })
    ));
    untouched(&old);
    untouched(&new);
    assert!(next.is_some());
    drop(hold);
    p.replace_mp4_catalogue(&mut next, selected, &mut WorkBudget::new(1), &off())
        .unwrap()
        .unwrap();
    check_outputs(&read(&mut p), &gold::CASES[0], SourceId(8702));
}

#[test]
fn catalogue_empty_retirement_under_pressure_retains_unqueued_configuration() {
    let b = Budget::new(64 << 20).unwrap();
    let (mut p, old) = player(&b, SourceId(8801), &gold::CASES[0], PlayerPolicy::default());
    // The first engine event is pending, not yet admitted to the FIFO.
    assert!(matches!(
        p.advance(&mut WorkBudget::new(1), &off()).unwrap(),
        Progress::Yield
    ));
    assert_eq!(p.queued(), 0);
    let (mut next, _) = candidate(&b, SourceId(8802), &gold::CASES[4], Mp4View::SourcePhysical);
    let hold = b.reserve(b.available()).unwrap();
    let retired = p
        .replace_mp4_catalogue(&mut next, Selection::All, &mut WorkBudget::new(1), &off())
        .unwrap()
        .unwrap();
    assert_eq!(retired.queued(), 0);
    assert_eq!(retired.pending_source(), Some(SourceId(8801)));
    let Some(RawEvent::Mp4(Mp4Event::Configuration(config))) = retired.pending_event() else {
        panic!()
    };
    assert_eq!(config.description.index, 1);
    assert_eq!(config.revision, 1);
    assert_eq!(config.track.track_id, 1);
    once(&old);
    drop(hold);
    check_outputs(&read(&mut p), &gold::CASES[4], SourceId(8802));
}

#[test]
fn catalogue_pause_survives_replacement_and_resume_emits_fresh_configuration() {
    let b = Budget::new(64 << 20).unwrap();
    let (mut p, old) = player(&b, SourceId(8901), &gold::CASES[0], PlayerPolicy::default());
    fill(&mut p, 2);
    p.pause(true).unwrap();
    let (mut next, new) = candidate(&b, SourceId(8902), &gold::CASES[4], Mp4View::SourcePhysical);
    let mut retired = p
        .replace_mp4_catalogue(&mut next, Selection::All, &mut WorkBudget::new(1), &off())
        .unwrap()
        .unwrap();
    assert_eq!(p.state(), State::Paused);
    assert!(p.pop().is_none());
    assert!(retired.pop().is_some());
    let before = new.polls.load(Ordering::Relaxed);
    assert!(matches!(
        p.advance(&mut WorkBudget::new(32), &off()).unwrap(),
        Progress::Paused
    ));
    assert_eq!(new.polls.load(Ordering::Relaxed), before);
    p.pause(false).unwrap();
    check_outputs(&read(&mut p), &gold::CASES[4], SourceId(8902));
    once(&old);
}

#[test]
fn catalogue_eof_handoff_keeps_old_outputs_and_restarts_only_new_entity() {
    let b = Budget::new(64 << 20).unwrap();
    let (mut p, old) = player(&b, SourceId(9001), &gold::CASES[0], PlayerPolicy::default());
    let out = read(&mut p);
    assert_eq!(p.state(), State::Ended);
    let (mut next, _) = candidate(&b, SourceId(9002), &gold::CASES[4], Mp4View::SourcePhysical);
    let retired = p
        .replace_mp4_catalogue(&mut next, Selection::All, &mut WorkBudget::new(1), &off())
        .unwrap()
        .unwrap();
    assert_eq!(p.state(), State::Running);
    assert!(out.iter().all(|o| !p.current(o)));
    check_outputs(&out, &gold::CASES[0], SourceId(9001));
    check_outputs(&read(&mut p), &gold::CASES[4], SourceId(9002));
    once(&old);
    drop(retired);
    check_outputs(&out, &gold::CASES[0], SourceId(9001));
}

#[test]
fn catalogue_old_range_receipt_remains_readable_and_late_completion_is_rejected() {
    let b = Budget::new(64 << 20).unwrap();
    let (mut p, old) = player(&b, SourceId(9101), &gold::CASES[0], PlayerPolicy::default());
    for _ in 0..200_000 {
        p.advance(&mut WorkBudget::new(1), &off()).unwrap();
        if p.pending_request().is_some() {
            break;
        }
    }
    let old_request = p.pending_request().unwrap().clone();
    let (mut next, _) = candidate(&b, SourceId(9102), &gold::CASES[4], Mp4View::SourcePhysical);
    let retired = p
        .replace_mp4_catalogue(&mut next, Selection::All, &mut WorkBudget::new(1), &off())
        .unwrap()
        .unwrap();
    let kept = retired.pending_request().unwrap();
    assert_eq!(kept.source, old_request.source);
    assert_eq!(kept.token, old_request.token);
    assert_eq!(kept.offset, old_request.offset);
    for _ in 0..200_000 {
        p.advance(&mut WorkBudget::new(1), &off()).unwrap();
        if p.pending_request().is_some() {
            break;
        }
    }
    let current = p.pending_request().unwrap().clone();
    assert!(
        p.provide(&SourceReply::End {
            source: old_request.source,
            token: old_request.token
        })
        .is_err()
    );
    assert_eq!(p.pending_request().unwrap().token, current.token);
    let mut out = Vec::new();
    while let Some(value) = p.pop() {
        out.push(value);
    }
    out.extend(read(&mut p));
    check_outputs(&out, &gold::CASES[4], SourceId(9102));
    once(&old);
}

#[test]
fn catalogue_replacement_does_not_reset_cumulative_event_limit() {
    let b = Budget::new(64 << 20).unwrap();
    let (mut p, old) = player(
        &b,
        SourceId(9201),
        &gold::CASES[0],
        PlayerPolicy {
            maximum_events: 2,
            ..PlayerPolicy::default()
        },
    );
    fill(&mut p, 2);
    let (mut next, _) = candidate(&b, SourceId(9202), &gold::CASES[4], Mp4View::SourcePhysical);
    let retired = p
        .replace_mp4_catalogue(&mut next, Selection::All, &mut WorkBudget::new(1), &off())
        .unwrap()
        .unwrap();
    let mut error = None;
    for _ in 0..20 {
        if let Err(value) = p.advance(&mut WorkBudget::new(1), &off()) {
            error = Some(value);
            break;
        }
    }
    assert!(matches!(
        error,
        Some(C::Limit {
            requested: 3,
            maximum: 2
        })
    ));
    assert_eq!(retired.queued(), 2);
    once(&old);
}

#[test]
fn catalogue_explicit_native_projection_keeps_literal_source_and_clock_view() {
    let b = Budget::new(64 << 20).unwrap();
    let (mut p, _) = player(&b, SourceId(9301), &gold::CASES[0], PlayerPolicy::default());
    p.set_clock_view(ClockChoice::ExplicitNativeProjection)
        .unwrap();
    let (mut next, _) = candidate(
        &b,
        SourceId(9302),
        &gold::CASES[7],
        Mp4View::PinnedMov9NoParseNoFillin,
    );
    p.replace_mp4_catalogue(&mut next, Selection::All, &mut WorkBudget::new(1), &off())
        .unwrap()
        .unwrap();
    let out = read(&mut p);
    check_outputs(&out, &gold::CASES[7], SourceId(9302));
    let order: Vec<_> = out
        .iter()
        .filter_map(|o| match o.event() {
            RawEvent::Mp4(Mp4Event::Packet(p)) => {
                Some((p.configuration.track.track_id, p.source.sample.ordinal))
            }
            _ => None,
        })
        .collect();
    // Literal existing native-order guard; no reordered source facts or new gold.
    assert_eq!(
        order,
        [
            (1, 1),
            (1, 2),
            (2, 1),
            (2, 2),
            (1, 3),
            (2, 3),
            (1, 4),
            (1, 5),
            (2, 4),
            (1, 6),
            (2, 5),
            (2, 6)
        ]
    );
    assert!(
        out.iter()
            .filter_map(|o| match o.event() {
                RawEvent::Mp4(Mp4Event::Packet(p)) => Some(p),
                _ => None,
            })
            .all(|p| p.native.is_some())
    );
}

#[test]
fn catalogue_bad_preparation_and_stopped_player_do_not_consume_new_owner() {
    let b = Budget::new(64 << 20).unwrap();
    let (mut p, old) = player(&b, SourceId(9401), &gold::CASES[0], PlayerPolicy::default());
    let output = first_packet(&mut p);
    for case in gold::CASES.iter().filter(|x| x.malformed) {
        let (mut bad, signals) = opener(&b, SourceId(9402), case.bytes, 7);
        let mut failed = false;
        for _ in 0..200_000 {
            match bad.advance(&mut WorkBudget::new(1), &off()) {
                Err(_) => {
                    failed = true;
                    break;
                }
                Ok(OpenProgress::Ready) => {
                    // Range validation belongs to the real index, not open metadata.
                    let opened = bad.finish().unwrap();
                    match Mp4IndexJob::new(
                        &opened.movie,
                        Mp4View::SourcePhysical,
                        PlanLimits::default(),
                    ) {
                        Err(_) => failed = true,
                        Ok(mut index) => {
                            for _ in 0..200_000 {
                                if index.poll(&mut WorkBudget::new(1), &off()).is_err() {
                                    failed = true;
                                    break;
                                }
                            }
                        }
                    }
                    break;
                }
                _ => {}
            }
        }
        assert!(failed, "{}", case.name);
        assert!(p.current(&output));
        untouched(&old);
        once(&signals);
    }
    p.stop();
    once(&old);
    let (mut next, new) = candidate(&b, SourceId(9403), &gold::CASES[0], Mp4View::SourcePhysical);
    assert!(matches!(
        p.replace_mp4_catalogue(&mut next, Selection::All, &mut WorkBudget::new(1), &off()),
        Err(Error::Closed)
    ));
    assert!(next.is_some());
    untouched(&new);
}

fn open_progressive(b: &Budget, id: SourceId, raw: &[u8]) -> (OpenedMp4<Transport>, Arc<Signals>) {
    let signals = Arc::new(Signals::default());
    let source = Transport {
        bytes: ByteSource::new(id, Bytes::copy_from_slice(b, raw).unwrap(), 1).unwrap(),
        signals: signals.clone(),
    };
    let input = InputCursor::new(
        b,
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
    for _ in 0..200_000 {
        if matches!(
            open.advance(&mut WorkBudget::new(1), &off()).unwrap(),
            OpenProgress::Ready
        ) {
            return (open.finish().unwrap(), signals);
        }
    }
    panic!("progressive fixture startup did not complete");
}

fn progressive_candidate(
    b: &Budget,
    id: SourceId,
    raw: &[u8],
) -> (Option<Mp4Catalogue<Transport>>, Arc<Signals>) {
    let (opened, signals) = open_progressive(b, id, raw);
    let plan = index(&opened, Mp4View::SourcePhysical);
    let catalogue = Mp4Catalogue::new(
        opened,
        plan,
        1 << 20,
        Mp4CatalogueMode::Progressive(progressive::ProgressiveLimits::default()),
    )
    .unwrap();
    (Some(catalogue), signals)
}

#[test]
fn catalogue_replacement_selects_native_terminal_and_waits_for_source_end() {
    let b = Budget::new(64 << 20).unwrap();
    let case = gold::CASES
        .iter()
        .find(|c| c.name == "r-front-stco")
        .unwrap();
    let (mut p, old) = player(&b, SourceId(9511), case, PlayerPolicy::default());
    let retained = first_packet(&mut p);
    p.pause(true).unwrap();

    let raw = include_bytes!("fixtures/open-ended/regular-terminal.mp4");
    let (opened, next_signals) = open_progressive(&b, SourceId(9512), raw);
    let mut index =
        Mp4IndexJob::new_native_terminal_v1(&opened.movie, PlanLimits::default()).unwrap();
    let cancel = off();
    assert!((0..200_000).any(|_| index.poll(&mut WorkBudget::new(1), &cancel).unwrap()));
    let plan = index.finish().unwrap();
    assert!(plan.terminal_mdat().is_some());
    let mut next = Some(
        Mp4Catalogue::new(
            opened,
            plan,
            1 << 20,
            Mp4CatalogueMode::NativeTerminalV1(progressive::ProgressiveLimits::default()),
        )
        .unwrap(),
    );
    let polls = next_signals.polls.load(Ordering::Relaxed);
    p.replace_mp4_catalogue(&mut next, Selection::All, &mut WorkBudget::new(1), &cancel)
        .unwrap()
        .unwrap();
    once(&old);
    assert!(!p.current(&retained));
    assert!(matches!(
        p.advance(&mut WorkBudget::new(1), &cancel).unwrap(),
        Progress::Paused
    ));
    assert_eq!(next_signals.polls.load(Ordering::Relaxed), polls);

    next_signals.stall_end.store(true, Ordering::Relaxed);
    p.pause(false).unwrap();
    let mut packets = Vec::new();
    for _ in 0..200_000 {
        assert!(!matches!(
            p.advance(&mut WorkBudget::new(1), &cancel).unwrap(),
            Progress::Ended
        ));
        while let Some(output) = p.pop() {
            assert_eq!(output.source_id(), SourceId(9512));
            assert!(p.current(&output));
            if let RawEvent::Mp4(Mp4Event::Packet(packet)) = output.into_event() {
                packets.push(packet);
            }
        }
        if p.pending_request()
            .is_some_and(|request| request.offset == raw.len() as u64)
        {
            break;
        }
    }
    assert_eq!(p.pending_request().unwrap().offset, raw.len() as u64);
    assert_eq!(packets.len(), case.samples.len());
    for (packet, expected) in packets.iter().zip(case.samples) {
        assert_eq!(packet.source.sample.ordinal, expected.ordinal);
        assert_eq!(packet.source.payload.as_ref(), expected.payload);
        let native = &packet.native.as_ref().unwrap().demux;
        assert_eq!(native.dts.map(i128::from), Some(expected.dts));
        assert_eq!(native.pts.map(i128::from), Some(expected.cts));
    }
    untouched(&next_signals);
    next_signals.stall_end.store(false, Ordering::Relaxed);
    assert!(read(&mut p).is_empty());
    assert_eq!(p.state(), State::Ended);
    drop(p);
    once(&next_signals);
}

#[test]
fn catalogue_progressive_replacement_keeps_future_batches_and_unknown_clocks() {
    let b = Budget::new(64 << 20).unwrap();
    let (mut p, old) = player(&b, SourceId(9501), &gold::CASES[0], PlayerPolicy::default());
    let case = &gold::CASES[9]; // independently authored absent-second-tfdt facts
    let (mut next, new) = progressive_candidate(&b, SourceId(9502), case.bytes);
    let Engine::Mp4(engine) = next.as_ref().unwrap().engine() else {
        panic!()
    };
    assert_eq!(engine.plan.rows(), 3);
    p.replace_mp4_catalogue(&mut next, Selection::All, &mut WorkBudget::new(1), &off())
        .unwrap()
        .unwrap();
    let out = read(&mut p);
    check_outputs_with_roots(
        &out,
        case,
        SourceId(9502),
        case.bytes,
        Some(&root_facts::NO_SECOND_TFDT),
    );
    assert!(
        out.iter()
            .filter_map(|o| match o.event() {
                RawEvent::Mp4(Mp4Event::Packet(packet)) if packet.source.sample.ordinal > 3 =>
                    Some(packet),
                _ => None,
            })
            .all(|packet| packet.source.sample.dts_ticks.is_none()
                && packet.source.sample.cts_ticks.is_none()
                && packet.native.is_none())
    );
    once(&old);
    drop(p);
    once(&new);
    check_outputs_with_roots(
        &out,
        case,
        SourceId(9502),
        case.bytes,
        Some(&root_facts::NO_SECOND_TFDT),
    );
}

#[test]
fn catalogue_replacement_terminal_mdat_needs_actual_source_end_and_keeps_unknown_bound() {
    let b = Budget::new(64 << 20).unwrap();
    let (mut p, old) = player(&b, SourceId(9601), &gold::CASES[0], PlayerPolicy::default());
    let raw = include_bytes!("fixtures/open-ended/regular-terminal.mp4");
    let (mut next, _) = progressive_candidate(&b, SourceId(9602), raw);
    let Engine::Mp4(engine) = next.as_ref().unwrap().engine() else {
        panic!()
    };
    assert!(engine.plan.terminal_mdat().is_some());
    assert_eq!(engine.plan.terminal_mdat().unwrap().header().size, None);
    p.replace_mp4_catalogue(&mut next, Selection::All, &mut WorkBudget::new(1), &off())
        .unwrap()
        .unwrap();
    let out = read(&mut p);
    let mut packets = 0;
    for output in &out {
        assert_eq!(output.source_id(), SourceId(9602));
        if let RawEvent::Mp4(Mp4Event::Packet(packet)) = output.event() {
            packets += 1;
            let sample = &packet.source.sample;
            assert_eq!(
                packet.source.payload.as_ref(),
                &raw[sample.range.start as usize..sample.range.end as usize]
            );
            assert!(packet.native.is_none());
        }
    }
    assert_eq!(packets, 6);
    let Engine::Mp4(engine) = p.engine().unwrap() else {
        panic!()
    };
    assert_eq!(engine.plan.terminal_mdat().unwrap().header().size, None);
    once(&old);
}

#[test]
fn catalogue_second_inband_movie_stays_refused_without_implicit_replacement() {
    let b = Budget::new(64 << 20).unwrap();
    let case = &gold::CASES[8];
    let (opened, plan, _) = prepared(&b, SourceId(9700), case, Mp4View::SourcePhysical);
    let moov = opened
        .movie
        .raw_roots()
        .iter()
        .find(|raw| raw.header().kind == *b"moov")
        .unwrap()
        .bytes()
        .as_ref()
        .to_vec();
    drop(plan);
    drop(opened);
    let mut raw = case.bytes.to_vec();
    raw.extend(moov);
    let (mut next, _) = progressive_candidate(&b, SourceId(9702), &raw);
    let (mut p, old) = player(&b, SourceId(9701), &gold::CASES[0], PlayerPolicy::default());
    p.replace_mp4_catalogue(&mut next, Selection::All, &mut WorkBudget::new(1), &off())
        .unwrap()
        .unwrap();
    let mut packets = Vec::new();
    let mut error = None;
    for _ in 0..200_000 {
        let result = p.advance(&mut WorkBudget::new(1), &off());
        while let Some(value) = p.pop() {
            packets.push(value);
        }
        if let Err(value) = result {
            error = Some(value);
            break;
        }
    }
    assert!(matches!(
        error,
        Some(C::Unsupported(
            "new movie catalogue requires explicit source discontinuity"
        ))
    ));
    check_outputs_with_roots(
        &packets,
        case,
        SourceId(9702),
        &raw,
        Some(&root_facts::SECOND_MOOV_REFUSAL),
    );
    once(&old);
}

#[test]
fn catalogue_existing_seek_intent_is_retained_but_not_applied_to_new_source_clock() {
    let b = Budget::new(64 << 20).unwrap();
    let (mut p, old) = player(&b, SourceId(9801), &gold::CASES[0], PlayerPolicy::default());
    let target = Time::new(4, 1).unwrap();
    assert!(
        p.seek_and_select(
            TrackKey::Mp4(1),
            target,
            Selection::All,
            &mut WorkBudget::new(1),
            &off()
        )
        .unwrap()
    );
    let intent = p.seek_intent().unwrap();
    let (mut next, _) = candidate(&b, SourceId(9802), &gold::CASES[0], Mp4View::SourcePhysical);
    let retired = p
        .replace_mp4_catalogue(&mut next, Selection::All, &mut WorkBudget::new(1), &off())
        .unwrap()
        .unwrap();
    assert_eq!(retired.seek_intent(), Some(intent));
    assert!(matches!(retired.pending_event(), Some(RawEvent::Seek(x)) if *x == intent));
    assert_eq!(p.seek_intent(), None);
    let out = read(&mut p);
    check_outputs(&out, &gold::CASES[0], SourceId(9802));
    assert!(
        out.iter()
            .filter(|o| matches!(o.event(), RawEvent::Mp4(Mp4Event::Packet(_))))
            .all(|o| o.delivery() == Delivery::Present)
    );
    once(&old);
}

#[derive(Default)]
struct Consumer {
    held: Vec<pump::Feed>,
    completions: std::collections::VecDeque<pump::FeedToken>,
    blocked: bool,
    refuse: bool,
    prepared: bool,
    abandoned: usize,
    boundaries: Vec<catalogue::CatalogueBoundary>,
    seeks: Vec<SeekIntent>,
    paused: bool,
    stopped: usize,
    cancel_during_prepare: Option<Arc<AtomicBool>>,
}
impl pump::Sink for Consumer {
    fn submit(&mut self, feed: pump::Feed) -> pump::Submission {
        if self.blocked {
            return pump::Submission::Blocked(feed);
        }
        let token = feed.token().clone();
        self.held.push(feed);
        pump::Submission::Accepted(token)
    }
    fn completion(&mut self) -> Option<pump::FeedToken> {
        self.completions.pop_front()
    }
    fn pause(&mut self, paused: bool) -> Result<()> {
        self.paused = paused;
        Ok(())
    }
    fn reset(&mut self, seek: &SeekIntent) -> Result<()> {
        self.seeks.push(*seek);
        Ok(())
    }
    fn stop(&mut self) {
        self.stopped += 1;
    }
}
impl pump::CatalogueSink for Consumer {
    fn prepare_catalogue(&mut self, next: &Engine) -> Result<()> {
        assert!(matches!(next, Engine::Mp4(_)));
        if self.refuse {
            return Err(C::Backpressure {
                requested: 1,
                available: 0,
            });
        }
        self.prepared = true;
        if let Some(cancel) = self.cancel_during_prepare.as_ref() {
            cancel.store(true, Ordering::Relaxed);
        }
        Ok(())
    }
    fn abandon_catalogue(&mut self) {
        self.prepared = false;
        self.abandoned += 1;
    }
    fn commit_catalogue(&mut self, boundary: catalogue::CatalogueBoundary) {
        assert!(self.prepared);
        self.prepared = false;
        self.boundaries.push(boundary);
    }
}

#[test]
fn catalogue_pump_blocked_and_accepted_owners_must_retire_before_commit() {
    let b = Budget::new(64 << 20).unwrap();
    let (p, old) = player(&b, SourceId(9901), &gold::CASES[0], PlayerPolicy::default());
    let mut pump = pump::Pump::new(
        p,
        Consumer {
            blocked: true,
            ..Consumer::default()
        },
        1,
    )
    .unwrap();
    let (mut next, new) = candidate(&b, SourceId(9902), &gold::CASES[4], Mp4View::SourcePhysical);
    for _ in 0..200_000 {
        pump.step(&mut WorkBudget::new(1), &off()).unwrap();
        if pump.pending().is_some() {
            break;
        }
    }
    let old_token = pump.pending().unwrap().token().clone();
    assert!(matches!(
        pump.replace_mp4_catalogue(&mut next, Selection::All, &mut WorkBudget::new(1), &off()),
        Err(C::Backpressure { .. })
    ));
    assert_eq!(pump.pending().unwrap().token(), &old_token);
    untouched(&old);
    untouched(&new);
    pump.sink_mut().unwrap().blocked = false;
    for _ in 0..20 {
        pump.step(&mut WorkBudget::new(1), &off()).unwrap();
        if pump.in_flight() == 1 {
            break;
        }
    }
    assert_eq!(pump.in_flight(), 1);
    assert!(matches!(
        pump.replace_mp4_catalogue(&mut next, Selection::All, &mut WorkBudget::new(1), &off()),
        Err(C::Backpressure { .. })
    ));
    assert!(next.is_some());
    pump.sink_mut()
        .unwrap()
        .completions
        .push_back(old_token.clone());
    pump.step(&mut WorkBudget::new(1), &off()).unwrap();
    assert_eq!(pump.in_flight(), 0);
    pump.replace_mp4_catalogue(&mut next, Selection::All, &mut WorkBudget::new(1), &off())
        .unwrap()
        .unwrap();
    once(&old);
    assert_eq!(
        pump.sink().unwrap().held[0].output().source_id(),
        SourceId(9901)
    );
    assert_eq!(pump.sink().unwrap().boundaries[0].source, SourceId(9902));
    for _ in 0..200_000 {
        pump.step(&mut WorkBudget::new(1), &off()).unwrap();
        if pump.in_flight() == 1 {
            break;
        }
    }
    let sink = pump.sink_mut().unwrap();
    assert_eq!(sink.held[1].output().source_id(), SourceId(9902));
    assert!(matches!(
        sink.held[1].output().event(),
        RawEvent::Mp4(Mp4Event::Configuration(_))
    ));
    assert_ne!(sink.held[1].token(), &old_token);
    sink.completions.push_back(old_token);
    assert_eq!(
        pump.step(&mut WorkBudget::new(1), &off()).unwrap(),
        pump::PumpStep::StaleCompletion
    );
    assert_eq!(pump.in_flight(), 1);
}

#[test]
fn catalogue_pump_sink_preparation_refusal_and_pause_are_transactional() {
    let b = Budget::new(64 << 20).unwrap();
    let (p, old) = player(
        &b,
        SourceId(10001),
        &gold::CASES[0],
        PlayerPolicy::default(),
    );
    let mut pump = pump::Pump::new(
        p,
        Consumer {
            refuse: true,
            ..Consumer::default()
        },
        1,
    )
    .unwrap();
    let (mut next, new) = candidate(
        &b,
        SourceId(10002),
        &gold::CASES[4],
        Mp4View::SourcePhysical,
    );
    let before = b.used();
    assert!(
        pump.replace_mp4_catalogue(&mut next, Selection::All, &mut WorkBudget::new(0), &off())
            .unwrap()
            .is_none()
    );
    assert_eq!(b.used(), before);
    assert!(matches!(
        pump.replace_mp4_catalogue(&mut next, Selection::All, &mut WorkBudget::new(1), &off()),
        Err(C::Backpressure { .. })
    ));
    assert_eq!(pump.sink().unwrap().abandoned, 1);
    assert!(!pump.sink().unwrap().prepared);
    assert!(pump.sink().unwrap().boundaries.is_empty());
    assert_eq!(b.used(), before);
    assert!(next.is_some());
    untouched(&old);
    untouched(&new);
    pump.pause(true).unwrap();
    pump.sink_mut().unwrap().refuse = false;
    pump.replace_mp4_catalogue(&mut next, Selection::All, &mut WorkBudget::new(1), &off())
        .unwrap()
        .unwrap();
    assert_eq!(pump.player().unwrap().state(), State::Paused);
    assert!(pump.sink().unwrap().paused);
    assert_eq!(
        pump.step(&mut WorkBudget::new(1), &off()).unwrap(),
        pump::PumpStep::Paused
    );
    pump.pause(false).unwrap();
    for _ in 0..200_000 {
        pump.step(&mut WorkBudget::new(1), &off()).unwrap();
        if pump.in_flight() == 1 {
            break;
        }
    }
    assert_eq!(
        pump.sink().unwrap().held[0].output().source_id(),
        SourceId(10002)
    );
    once(&old);
}

#[test]
fn catalogue_pump_cancel_during_sink_preparation_abandons_without_source_swap() {
    let b = Budget::new(64 << 20).unwrap();
    let (p, old) = player(
        &b,
        SourceId(10101),
        &gold::CASES[0],
        PlayerPolicy::default(),
    );
    let cancel = Arc::new(off());
    let mut pump = pump::Pump::new(
        p,
        Consumer {
            cancel_during_prepare: Some(cancel.clone()),
            ..Consumer::default()
        },
        1,
    )
    .unwrap();
    let (mut next, new) = candidate(
        &b,
        SourceId(10102),
        &gold::CASES[4],
        Mp4View::SourcePhysical,
    );
    assert!(matches!(
        pump.replace_mp4_catalogue(&mut next, Selection::All, &mut WorkBudget::new(1), &cancel),
        Err(C::Cancelled)
    ));
    assert!(next.is_some());
    assert_eq!(
        pump.player().unwrap().engine().unwrap().source(),
        SourceId(10101)
    );
    assert_eq!(pump.sink().unwrap().abandoned, 1);
    assert!(!pump.sink().unwrap().prepared);
    assert!(pump.sink().unwrap().boundaries.is_empty());
    assert_eq!(pump.sink().unwrap().stopped, 0);
    untouched(&old);
    untouched(&new);
    cancel.store(false, Ordering::Relaxed);
    pump.sink_mut().unwrap().cancel_during_prepare = None;
    pump.replace_mp4_catalogue(&mut next, Selection::All, &mut WorkBudget::new(1), &cancel)
        .unwrap()
        .unwrap();
    once(&old);
}

#[test]
fn catalogue_stale_candidate_constructor_and_cancelled_opener_keep_active_source() {
    let b = Budget::new(64 << 20).unwrap();
    let (mut p, old) = player(
        &b,
        SourceId(10201),
        &gold::CASES[0],
        PlayerPolicy::default(),
    );
    let retained = first_packet(&mut p);
    let (mut opened, plan, signals) = prepared(
        &b,
        SourceId(10202),
        &gold::CASES[0],
        Mp4View::SourcePhysical,
    );
    opened.input.reset(0).unwrap();
    assert!(matches!(
        Mp4Catalogue::new(opened, plan, 1 << 20, Mp4CatalogueMode::Indexed),
        Err(C::StaleGeneration { .. })
    ));
    once(&signals);
    assert!(p.current(&retained));
    untouched(&old);
    let (mut open, signals) = opener(&b, SourceId(10203), gold::CASES[4].bytes, 7);
    let mut refused = false;
    for _ in 0..200_000 {
        let hold = b.reserve(b.available()).unwrap();
        let result = open.advance(&mut WorkBudget::new(1), &off());
        drop(hold);
        if matches!(result, Err(C::Backpressure { .. })) {
            refused = true;
            break;
        }
        result.unwrap();
    }
    assert!(refused);
    assert!(matches!(
        open.advance(&mut WorkBudget::new(0), &AtomicBool::new(true)),
        Err(C::Cancelled)
    ));
    assert!(matches!(
        open.advance(&mut WorkBudget::new(1), &off()),
        Err(Error::Closed)
    ));
    drop(open);
    once(&signals);
    assert!(p.current(&retained));
    untouched(&old);
}

fn finish_range_open(mut open: Mp4Open<Transport>) -> OpenedMp4<Transport> {
    for _ in 0..200_000 {
        if matches!(
            open.advance(&mut WorkBudget::new(17), &off()).unwrap(),
            OpenProgress::Ready
        ) {
            return open.finish().unwrap();
        }
    }
    panic!("MP4 range open stalled")
}

#[test]
fn progressive_range_seek_promotes_the_same_fragmented_entity_through_the_pump() {
    let b = Budget::new(64 << 20).unwrap();
    let id = SourceId(10401);
    let case = gold::CASES
        .iter()
        .find(|c| c.name == "f-multitrack")
        .unwrap();
    let (front, old) = range_opener(&b, id, case.bytes, 23, true);
    let front = finish_range_open(front);
    assert!(matches!(
        front.movie.read_scope(),
        mp4_open::MetadataReadScope::FrontMetadataBeforeMdat(_)
    ));
    let plan = index(&front, Mp4View::SourcePhysical);
    let engine = Engine::mp4_progressive(
        front.input,
        plan,
        1 << 20,
        progressive::ProgressiveLimits::default(),
    )
    .unwrap();
    let mut p = Player::new(
        engine,
        front.source,
        Selection::All,
        PlayerPolicy::default(),
    )
    .unwrap();
    let old_output = first_packet(&mut p);
    fill(&mut p, 1);
    let (complete, new) = opener(&b, id, case.bytes, 23);
    let complete = finish_range_open(complete);
    assert_eq!(
        complete.movie.read_scope(),
        mp4_open::MetadataReadScope::CompleteEntity
    );
    assert!(
        front.movie.tracks()[0].fragments().len() < complete.movie.tracks()[0].fragments().len()
    );
    let original_candidate_generation = complete.input.generation();
    let plan = index(&complete, Mp4View::SourcePhysical);
    let mut next =
        Some(Mp4Catalogue::new(complete, plan, 1 << 20, Mp4CatalogueMode::Indexed).unwrap());
    let old_polls = old.polls.load(Ordering::Relaxed);
    let new_polls = new.polls.load(Ordering::Relaxed);
    let request = Mp4SyncSeekRequest {
        video: 1,
        source_time: Time::new(7, 2).unwrap(),
        audio_preroll: Time::new(0, 1).unwrap(),
        maximum_scanned_rows: 100,
    };
    let selection = Selection::Tracks {
        video: Some(TrackKey::Mp4(1)),
        audio: None,
        subtitle: None,
    };
    let mut job = p
        .prepare_mp4_progressive_sync_seek(next.as_ref().unwrap(), request, selection)
        .unwrap();
    assert!(!job.poll(&mut WorkBudget::new(0), &off()).unwrap());
    for _ in 0..200 {
        if job.poll(&mut WorkBudget::new(1), &off()).unwrap() {
            break;
        }
    }
    let command = job.finish().unwrap();
    assert_eq!(command.sync_ordinal(), 4);
    assert_eq!(command.start_offset(), 1647);
    assert_eq!(old.polls.load(Ordering::Relaxed), old_polls);
    assert_eq!(new.polls.load(Ordering::Relaxed), new_polls);
    assert!(matches!(
        p.apply_mp4_progressive_sync_seek(
            &mut next,
            &command,
            &mut WorkBudget::new(1),
            &AtomicBool::new(true),
        ),
        Err(C::Cancelled)
    ));
    assert!(p.current(&old_output));
    untouched(&old);
    untouched(&new);

    let mut pump = pump::Pump::new(
        p,
        Consumer {
            refuse: true,
            ..Consumer::default()
        },
        1,
    )
    .unwrap();
    assert!(
        pump.apply_mp4_progressive_sync_seek(&mut next, &command, &mut WorkBudget::new(0), &off())
            .unwrap()
            .is_none()
    );
    assert!(matches!(
        pump.apply_mp4_progressive_sync_seek(&mut next, &command, &mut WorkBudget::new(1), &off()),
        Err(C::Backpressure { .. })
    ));
    assert!(pump.player().unwrap().current(&old_output));
    assert_eq!(
        next.as_ref().unwrap().engine().generation(),
        original_candidate_generation
    );
    untouched(&old);
    untouched(&new);

    pump.pause(true).unwrap();
    pump.sink_mut().unwrap().refuse = false;
    let mut retired = pump
        .apply_mp4_progressive_sync_seek(&mut next, &command, &mut WorkBudget::new(1), &off())
        .unwrap()
        .unwrap();
    assert!(next.is_none());
    once(&old);
    untouched(&new);
    assert_eq!(retired.queued(), 1);
    assert!(retired.pop().is_some());
    assert!(!pump.player().unwrap().current(&old_output));
    assert_eq!(pump.player().unwrap().state(), State::Paused);
    let boundary = pump.sink().unwrap().boundaries[0];
    assert_eq!(boundary.source, id);
    assert_ne!(boundary.input_generation, original_candidate_generation);
    assert!(matches!(
        pump.apply_mp4_progressive_sync_seek(&mut next, &command, &mut WorkBudget::new(1), &off()),
        Err(C::StaleGeneration { .. })
    ));
    assert_eq!(pump.sink().unwrap().seeks.len(), 1);
    assert!(!pump.sink().unwrap().seeks[0].replay_from_start);
    assert_eq!(
        pump.step(&mut WorkBudget::new(1), &off()).unwrap(),
        pump::PumpStep::Paused
    );
    assert_eq!(new.polls.load(Ordering::Relaxed), new_polls);

    pump.pause(false).unwrap();
    let mut out = Vec::new();
    let mut first_read = None;
    for _ in 0..200_000 {
        let step = pump.step(&mut WorkBudget::new(1), &off()).unwrap();
        if let Some(request) = pump.player().unwrap().pending_request() {
            first_read.get_or_insert(request.offset);
        }
        if let Some(feed) = pump.sink_mut().unwrap().held.pop() {
            let token = feed.token().clone();
            out.push(feed.into_output());
            pump.sink_mut().unwrap().completions.push_back(token);
        }
        if matches!(step, pump::PumpStep::Ended) {
            break;
        }
    }
    assert_eq!(first_read, Some(1647));
    assert!(
        matches!(out[0].event(), RawEvent::Seek(intent) if intent.target == request.source_time)
    );
    assert!(out.iter().all(|event| event.generation() == boundary.input_generation && event.source_id() == id));
    let video: Vec<_> = out
        .iter()
        .filter_map(|out| match out.event() {
            RawEvent::Mp4(Mp4Event::Packet(packet)) if packet.configuration.track.track_id == 1 => {
                Some(packet)
            }
            _ => None,
        })
        .collect();
    assert_eq!(video.len(), 3);
    for (packet, expected) in video.iter().zip(
        case.samples
            .iter()
            .filter(|sample| sample.track == 1 && sample.ordinal >= 4),
    ) {
        assert_eq!(packet.source.sample.ordinal, expected.ordinal);
        assert_eq!(packet.source.sample.cts_ticks, Some(expected.cts));
        assert_eq!(packet.source.payload.as_ref(), expected.payload);
    }
    drop(out);
    drop(pump);
    once(&new);
    drop(retired);
    drop(old_output);
    drop(command);
    drop(front.movie);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}

#[test]
fn progressive_native_seek_matches_pinned_fragment_replay_after_reopening_ranges() {
    let b = Budget::new(64 << 20).unwrap();
    let id = SourceId(10402);
    let raw = include_bytes!("fixtures/mp4/f-multitrack.mp4");
    let (front, old) = open_progressive(&b, id, raw);
    let mut job = Mp4IndexJob::new_native_terminal_v1(&front.movie, PlanLimits::default()).unwrap();
    let cancel = off();
    assert!((0..200_000).any(|_| job.poll(&mut WorkBudget::new(1), &cancel).unwrap()));
    let engine = Engine::mp4_native_terminal_v1(
        front.input,
        job.finish().unwrap(),
        1 << 20,
        progressive::ProgressiveLimits::default(),
    )
    .unwrap();
    let mut p = Player::new(
        engine,
        front.source,
        Selection::All,
        PlayerPolicy::default(),
    )
    .unwrap();
    p.set_clock_view(ClockChoice::ExplicitNativeProjection)
        .unwrap();
    let before = first_packet(&mut p);
    let RawEvent::Mp4(Mp4Event::Packet(packet)) = before.event() else {
        panic!("expected progressive native packet");
    };
    assert!(packet.native.is_some());

    let (complete, new) = opener(&b, id, raw, 23);
    let complete = finish_range_open(complete);
    let plan = index(&complete, Mp4View::PinnedMov9NoParseNoFillin);
    let (candidate, movie) =
        Mp4Catalogue::new_with_movie(complete, plan, 1 << 20, Mp4CatalogueMode::Indexed).unwrap();
    let mut next = Some(candidate);
    let before_new = new.polls.load(Ordering::Relaxed);
    let request = Mp4NativeSeekRequest {
        video: 1,
        timestamp: 3500,
        limits: triergon_isobmff::compat::mov::seek::SeekLimits::default(),
    };
    let mut job = p
        .prepare_mp4_progressive_native_seek(
            next.as_ref().unwrap(),
            &movie,
            request,
            Selection::All,
        )
        .unwrap();
    assert!((0..200_000).any(|_| job.poll(&mut WorkBudget::new(1), &cancel).unwrap()));
    let command = job.finish().unwrap();
    assert_eq!(command.receipt().selected.timestamp, 3000);
    assert_eq!(command.start_offset(), 1647);
    assert_eq!(new.polls.load(Ordering::Relaxed), before_new);
    assert!(p.current(&before));
    p.pause(true).unwrap();
    let mut pump = pump::Pump::new(p, Consumer::default(), 1).unwrap();
    let retired = pump
        .apply_mp4_progressive_native_seek(&mut next, &command, &mut WorkBudget::new(1), &cancel)
        .unwrap()
        .unwrap();
    assert!(next.is_none());
    once(&old);
    untouched(&new);
    let boundary = pump.sink().unwrap().boundaries[0];
    let seek = pump.sink().unwrap().seeks[0];
    assert_eq!(seek.clock, ClockChoice::ExplicitNativeProjection);
    assert_eq!(seek.target, Time::new(7, 2).unwrap());
    assert!(!seek.replay_from_start);
    assert_eq!(pump.player().unwrap().state(), State::Paused);
    assert!(!pump.player().unwrap().current(&before));
    pump.pause(false).unwrap();

    let mut packets = Vec::new();
    let mut first_request = None;
    let mut finished = false;
    for _ in 0..200_000 {
        let step = pump.step(&mut WorkBudget::new(1), &cancel).unwrap();
        if let Some(request) = pump.player().unwrap().pending_request() {
            first_request.get_or_insert(request.offset);
        }
        if let Some(feed) = pump.sink_mut().unwrap().held.pop() {
            let token = feed.token().clone();
            let output = feed.into_output();
            assert_eq!(output.generation(), boundary.input_generation);
            assert_eq!(output.source_id(), id);
            if let RawEvent::Mp4(Mp4Event::Packet(packet)) = output.into_event() {
                packets.push(packet);
            }
            pump.sink_mut().unwrap().completions.push_back(token);
        }
        if matches!(step, pump::PumpStep::Ended) {
            finished = true;
            break;
        }
    }
    assert!(finished);
    assert_eq!(first_request, Some(command.start_offset()));
    let expected = include_str!("fixtures/mp4/f-multitrack.mp4.native-seek");
    assert_eq!(packets.len(), expected.lines().count());
    for (packet, row) in packets.iter().zip(expected.lines()) {
        let native = packet.native.as_ref().unwrap();
        let fields: Vec<_> = row.split_ascii_whitespace().collect();
        assert_eq!(native.track_id, fields[0].parse().unwrap());
        assert_eq!(native.demux.pts, fields[1].parse().ok());
        assert_eq!(native.demux.dts, fields[2].parse().ok());
        assert_eq!(native.demux.duration, fields[3].parse::<i64>().unwrap());
        let offset = fields[4].parse::<usize>().unwrap();
        let size = fields[5].parse::<usize>().unwrap();
        assert_eq!(native.demux.flags, fields[6].parse::<u32>().unwrap());
        assert_eq!(packet.source.sample.range.start, offset as u64);
        assert_eq!(packet.source.payload.as_ref(), &raw[offset..offset + size]);
    }
    drop(packets);
    drop(before);
    drop(retired);
    drop(command);
    drop(movie);
    drop(front.movie);
    drop(pump);
    once(&new);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}

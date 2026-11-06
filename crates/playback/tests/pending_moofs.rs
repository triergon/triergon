use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use triergon_core::{
    Budget, Bytes, Error, InputCursor, InputRequest, SourceCapabilities, SourceId, Time, WorkBudget,
};
use triergon_playback::{
    io::{SelectedInput, SourceOwner},
    mp4_open::{Mp4Open, OpenLimits, OpenProgress},
    mp4_plan::{Mp4IndexJob, Mp4View, PlanLimits},
    progressive::ProgressiveLimits,
    *,
};
mod art {
    include!("fixtures/progressive/source_golds.rs");
}
const ID: SourceId = SourceId(6301);
fn off() -> AtomicBool {
    AtomicBool::new(false)
}
struct Transport {
    bytes: ByteSource,
    gate: Arc<AtomicUsize>,
    calls: Arc<Mutex<Vec<&'static str>>>,
}
impl Source for Transport {
    fn poll(
        &mut self,
        r: &InputRequest,
        w: &mut WorkBudget,
        c: &AtomicBool,
    ) -> triergon_core::Result<SourceReply> {
        if w.remaining() == 0 {
            return Ok(SourceReply::Pending);
        }
        self.calls.lock().unwrap().push("read");
        if r.offset >= self.gate.load(Ordering::Relaxed) as u64 {
            return Ok(SourceReply::Pending);
        }
        self.bytes.poll(r, w, c)
    }
    fn invalidate(&mut self) {
        self.calls.lock().unwrap().push("invalidate");
        self.bytes.invalidate();
    }
    fn abort(&mut self) {
        self.calls.lock().unwrap().push("abort");
        self.bytes.abort();
    }
    fn close(&mut self) {
        self.calls.lock().unwrap().push("close");
        self.bytes.close();
    }
}
type Calls = Arc<Mutex<Vec<&'static str>>>;
fn make(
    raw: &[u8],
    limits: ProgressiveLimits,
) -> (Player<Transport>, Budget, Arc<AtomicUsize>, Calls) {
    let b = Budget::new(64 << 20).unwrap();
    let gate = Arc::new(AtomicUsize::new(usize::MAX));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let source = Transport {
        bytes: ByteSource::new(ID, Bytes::copy_from_slice(&b, raw).unwrap(), 7).unwrap(),
        gate: gate.clone(),
        calls: calls.clone(),
    };
    let input = InputCursor::new(
        &b,
        ID,
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
    for _ in 0..100000 {
        if matches!(
            open.advance(&mut WorkBudget::new(1), &off()).unwrap(),
            OpenProgress::Ready
        ) {
            break;
        }
    }
    let opened = open.finish().unwrap();
    let mut j = Mp4IndexJob::new(
        &opened.movie,
        Mp4View::SourcePhysical,
        PlanLimits::default(),
    )
    .unwrap();
    while !j.poll(&mut WorkBudget::new(1), &off()).unwrap() {}
    let plan = j.finish().unwrap();
    let engine = Engine::mp4_progressive(opened.input, plan, 1 << 20, limits).unwrap();
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
fn read(p: &mut Player<Transport>) -> Result<Vec<Output>> {
    let mut out = Vec::new();
    for _ in 0..200000 {
        let r = p.advance(&mut WorkBudget::new(1), &off())?;
        while let Some(o) = p.pop() {
            out.push(o)
        }
        if matches!(r, Progress::Ended) {
            return Ok(out);
        }
    }
    panic!("bounded pending-moof source did not complete")
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
fn check(out: &[Output], case: &art::Case) {
    let got = packets(out);
    assert_eq!(
        got.len(),
        case.wants.len(),
        "{} all declared source packets are required",
        case.name
    );
    for (p, w) in got.iter().zip(case.wants) {
        assert_eq!(p.source.source, ID);
        assert_eq!(p.source.sample.ordinal, w.ordinal);
        assert_eq!(p.source.sample.range, w.offset..w.offset + 12);
        assert_eq!(p.source.payload.as_ref(), w.payload);
        assert_eq!(p.source.sample.dts_ticks, w.dts);
        assert_eq!(p.source.sample.cts_ticks, w.dts);
        assert_eq!(p.source.sample.duration_ticks, 1000);
        assert_eq!(p.source.sample.description_index, w.description);
    }
}
#[test]
fn pending_moofs_required_whole_source_matches_independent_box_recipe() {
    assert_eq!(art::CASES.len(), 5);
    for case in art::CASES {
        println!("DECLARED_SOURCE {}", case.name);
        let (mut p, b, _, _) = make(case.bytes, ProgressiveLimits::default());
        let out = read(&mut p).unwrap();
        check(&out, case);
        assert_eq!(
            out.iter()
                .filter(|o| matches!(o.event(),RawEvent::Mp4Root(v)if v.header().kind==*b"moof"))
                .count(),
            2
        );
        drop(out);
        drop(p);
        assert_eq!(b.used(), Budget::control_storage_bytes());
    }
}
#[test]
fn pending_moofs_limit_is_explicit_and_retained_prefix_remains_readable() {
    let (mut p, b, _, _) = make(
        art::CASES[0].bytes,
        ProgressiveLimits {
            fragments_per_batch: 1,
            ..ProgressiveLimits::default()
        },
    );
    let mut out = Vec::new();
    let mut failure = None;
    for _ in 0..200000 {
        match p.advance(&mut WorkBudget::new(1), &off()) {
            Ok(_) => {
                while let Some(o) = p.pop() {
                    out.push(o)
                }
            }
            Err(e) => {
                failure = Some(e);
                break;
            }
        }
    }
    assert!(matches!(
        failure,
        Some(Error::Limit {
            requested: 2,
            maximum: 1
        })
    ));
    assert_eq!(packets(&out).len(), 3);
    assert_eq!(packets(&out)[0].source.payload.len(), 12);
    drop(p);
    assert_eq!(packets(&out)[0].source.payload.len(), 12);
    drop(out);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}
fn parked() -> (
    Player<Transport>,
    Budget,
    Arc<AtomicUsize>,
    Calls,
    Vec<Output>,
) {
    let (mut p, b, gate, calls) = make(art::CASES[0].bytes, ProgressiveLimits::default());
    gate.store(1095, Ordering::Relaxed);
    let mut out = Vec::new();
    for _ in 0..200000 {
        let r = p.advance(&mut WorkBudget::new(1), &off()).unwrap();
        assert!(!matches!(r, Progress::Ended));
        while let Some(o) = p.pop() {
            out.push(o)
        }
        if p.pending_request().is_some_and(|r| r.offset >= 1095) {
            break;
        }
    }
    assert_eq!(packets(&out).len(), 3);
    (p, b, gate, calls, out)
}
#[test]
fn pending_moofs_required_starvation_pause_workzero_are_distinct_from_eof() {
    let (mut p, b, gate, calls, mut out) = parked();
    let before = (b.used(), calls.lock().unwrap().len());
    for _ in 0..5 {
        assert!(matches!(
            p.advance(&mut WorkBudget::new(0), &off()).unwrap(),
            Progress::Yield
        ));
    }
    assert_eq!((b.used(), calls.lock().unwrap().len()), before);
    p.pause(true).unwrap();
    for _ in 0..5 {
        assert!(matches!(
            p.advance(&mut WorkBudget::new(8), &off()).unwrap(),
            Progress::Paused
        ));
    }
    assert_eq!((b.used(), calls.lock().unwrap().len()), before);
    p.pause(false).unwrap();
    gate.store(usize::MAX, Ordering::Relaxed);
    out.extend(read(&mut p).unwrap());
    check(&out, &art::CASES[0]);
}
#[test]
fn pending_moofs_cancel_retires_parser_owners_but_keeps_caller_packets() {
    let (mut p, b, _, calls, out) = parked();
    let c = AtomicBool::new(true);
    assert!(matches!(
        p.advance(&mut WorkBudget::new(1), &c),
        Err(Error::Cancelled)
    ));
    drop(p);
    assert_eq!(
        calls
            .lock()
            .unwrap()
            .iter()
            .filter(|x| **x == "abort")
            .count(),
        1
    );
    assert_eq!(
        calls
            .lock()
            .unwrap()
            .iter()
            .filter(|x| **x == "close")
            .count(),
        1
    );
    assert_eq!(packets(&out).len(), 3);
    assert_eq!(packets(&out)[0].source.payload.len(), 12);
    drop(out);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}
#[test]
fn pending_moofs_budget_pressure_retries_without_losing_any_sample() {
    let case = &art::CASES[0];
    let (mut p, b, _, _) = make(case.bytes, ProgressiveLimits::default());
    let mut out = Vec::new();
    let mut done = false;
    for _ in 0..200000 {
        let held = b.reserve(b.available()).unwrap();
        let r = p.advance(&mut WorkBudget::new(1), &off());
        drop(held);
        let mut r = match r {
            Err(Error::Backpressure { .. }) => p.advance(&mut WorkBudget::new(1), &off()).unwrap(),
            v => v.unwrap(),
        };
        if matches!(r, Progress::Backpressure { .. }) {
            r = p.advance(&mut WorkBudget::new(1), &off()).unwrap();
        }
        while let Some(o) = p.pop() {
            out.push(o)
        }
        if matches!(r, Progress::Ended) {
            done = true;
            break;
        }
    }
    assert!(done);
    check(&out, case);
    drop(out);
    drop(p);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}
#[test]
fn pending_moofs_truncated_media_preserves_complete_prefix_and_reports_error() {
    let raw = art::CASES[0].bytes;
    let (mut p, b, _, _) = make(&raw[..raw.len() - 4], ProgressiveLimits::default());
    let mut out = Vec::new();
    let mut failure = None;
    for _ in 0..200000 {
        match p.advance(&mut WorkBudget::new(1), &off()) {
            Ok(_) => {
                while let Some(o) = p.pop() {
                    out.push(o)
                }
            }
            Err(e) => {
                failure = Some(e);
                break;
            }
        }
    }
    assert!(matches!(failure, Some(Error::Truncated)));
    assert_eq!(packets(&out).len(), 8);
    drop(p);
    assert_eq!(packets(&out)[0].source.payload.len(), 12);
    drop(out);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}
#[test]
fn pending_moofs_nonseekable_refusal_preserves_live_source_fence() {
    let (mut p, _, gate, _, mut out) = parked();
    let held = out
        .iter()
        .find(|o| matches!(o.event(), RawEvent::Mp4(Mp4Event::Packet(_))))
        .unwrap();
    assert!(p.current(held));
    let result = p.seek_and_select(
        TrackKey::Mp4(1),
        Time::new(0, 1).unwrap(),
        Selection::All,
        &mut WorkBudget::new(8),
        &off(),
    );
    assert!(result.is_err());
    assert!(p.current(held));
    gate.store(usize::MAX, Ordering::Relaxed);
    out.extend(read(&mut p).unwrap());
    check(&out, &art::CASES[0]);
}

#[test]
fn pending_moofs_empty_samples_at_media_endpoint_keep_source_declarations() {
    let raw = include_bytes!("fixtures/progressive/pending-empty-endpoint.mp4");
    let (mut p, b, _, _) = make(raw, ProgressiveLimits::default());
    let out = read(&mut p).unwrap();
    let got = packets(&out);
    assert_eq!(got.len(), 9);
    for (i, packet) in got.iter().enumerate().skip(6) {
        assert_eq!(packet.source.sample.ordinal, i as u64 + 1);
        assert_eq!(
            packet.source.sample.range,
            raw.len() as u64..raw.len() as u64
        );
        assert!(packet.source.payload.is_empty());
        assert_eq!(packet.source.sample.dts_ticks, Some(i as i128 * 1000));
        assert_eq!(packet.source.sample.duration_ticks, 1000);
    }
    drop(out);
    drop(p);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}

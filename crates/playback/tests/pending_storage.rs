mod common;
use common::{off, packets, read};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use triergon_core::{
    Budget, Bytes, Error, InputCursor, InputRequest, SourceCapabilities, SourceId, WorkBudget,
};
use triergon_playback::{
    io::{SelectedInput, SourceOwner},
    mp4_open::{Mp4Open, OpenLimits, OpenProgress, OpenedMp4},
    mp4_plan::{Mp4IndexJob, Mp4View, PlanLimits, PreparedMp4},
    progressive::ProgressiveLimits,
    *,
};
mod gold {
    include!("fixtures/progressive/source_golds.rs");
}
const ID: SourceId = SourceId(6501);
struct Signals {
    gate: AtomicUsize,
    polls: AtomicUsize,
    aborts: AtomicUsize,
    closes: AtomicUsize,
}
impl Signals {
    fn new() -> Self {
        Self {
            gate: AtomicUsize::new(usize::MAX),
            polls: AtomicUsize::new(0),
            aborts: AtomicUsize::new(0),
            closes: AtomicUsize::new(0),
        }
    }
}
struct Transport {
    bytes: ByteSource,
    signals: Arc<Signals>,
}
impl Source for Transport {
    fn poll(
        &mut self,
        r: &InputRequest,
        w: &mut WorkBudget,
        c: &AtomicBool,
    ) -> Result<SourceReply> {
        if c.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if r.source != ID {
            return Err(Error::SourceMismatch);
        }
        if w.remaining() == 0 {
            return Ok(SourceReply::Pending);
        }
        self.signals.polls.fetch_add(1, Ordering::Relaxed);
        if r.offset >= self.signals.gate.load(Ordering::Relaxed) as u64 {
            assert!(w.try_charge(1));
            return Ok(SourceReply::Pending);
        }
        self.bytes.poll(r, w, c)
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
type Prepared = (OpenedMp4<Transport>, PreparedMp4, Budget, Arc<Signals>);
fn prepare(raw: &[u8]) -> Prepared {
    let b = Budget::new(64 << 20).unwrap();
    let signals = Arc::new(Signals::new());
    let source = Transport {
        bytes: ByteSource::new(ID, Bytes::copy_from_slice(&b, raw).unwrap(), 7).unwrap(),
        signals: signals.clone(),
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
    let mut index = Mp4IndexJob::new(
        &opened.movie,
        Mp4View::SourcePhysical,
        PlanLimits::default(),
    )
    .unwrap();
    let mut indexed = false;
    for _ in 0..200_000 {
        if index.poll(&mut WorkBudget::new(1), &off()).unwrap() {
            indexed = true;
            break;
        }
    }
    assert!(indexed);
    let plan = index.finish().unwrap();
    (opened, plan, b, signals)
}
fn player(raw: &[u8], limits: ProgressiveLimits) -> (Player<Transport>, Budget, Arc<Signals>) {
    let (opened, plan, b, signals) = prepare(raw);
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
        signals,
    )
}
fn check(out: &[Output], case: &gold::Case) {
    let got = packets(out);
    assert_eq!(got.len(), case.wants.len(), "{}", case.name);
    for (packet, wanted) in got.iter().zip(case.wants) {
        assert_eq!(packet.source.source, ID);
        assert_eq!(packet.source.sample.ordinal, wanted.ordinal);
        assert_eq!(
            packet.source.sample.range,
            wanted.offset..wanted.offset + 12
        );
        assert_eq!(packet.source.payload.as_ref(), wanted.payload);
        assert_eq!(packet.source.sample.description_index, wanted.description);
        assert_eq!(packet.source.sample.dts_ticks, wanted.dts);
        assert_eq!(packet.source.sample.cts_ticks, wanted.dts);
        assert_eq!(packet.source.sample.duration_ticks, 1000);
        assert!(packet.native.is_none());
    }
}
#[test]
fn pending_storage_required_constructor_survives_unrelated_budget_pressure() {
    let (opened, plan, b, _) = prepare(gold::CASES[0].bytes);
    let hold = b.reserve(b.available()).unwrap();
    let result = Engine::mp4_progressive(opened.input, plan, 1 << 20, ProgressiveLimits::default());
    drop(hold);
    let engine = match result {
        Ok(engine) => engine,
        Err(error) => panic!("prepared startup has no pending fragment to allocate: {error:?}"),
    };
    drop(engine);
    drop(opened.movie);
    drop(opened.source);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}
#[test]
fn pending_storage_declared_capacity_does_not_charge_unused_future_slots() {
    for maximum in [1, 2, 64] {
        let (opened, plan, b, _) = prepare(gold::CASES[0].bytes);
        let before = b.used();
        let engine = Engine::mp4_progressive(
            opened.input,
            plan,
            1 << 20,
            ProgressiveLimits {
                fragments_per_batch: maximum,
                ..ProgressiveLimits::default()
            },
        )
        .unwrap();
        assert_eq!(
            b.used(),
            before,
            "future capacity {maximum} is a limit, not present media"
        );
        drop(engine);
        drop(opened.movie);
        drop(opened.source);
        assert_eq!(b.used(), Budget::control_storage_bytes());
    }
}
#[test]
fn pending_storage_all_future_ranges_clocks_and_descriptions_remain_literal() {
    for case in gold::CASES {
        let (mut p, b, _) = player(case.bytes, ProgressiveLimits::default());
        let mut out = Vec::new();
        read(&mut p, &mut out).unwrap();
        check(&out, case);
        drop(p);
        check(&out, case);
        drop(out);
        assert_eq!(b.used(), Budget::control_storage_bytes());
    }
}
#[test]
fn pending_storage_growth_refusal_retries_without_ordinal_commit() {
    for case in gold::CASES {
        let (mut p, b, _) = player(case.bytes, ProgressiveLimits::default());
        let mut out = Vec::new();
        let mut ended = false;
        let mut pressure = 0;
        for _ in 0..300_000 {
            let hold = b.reserve(b.available()).unwrap();
            let step = p.advance(&mut WorkBudget::new(1), &off());
            drop(hold);
            let mut step = match step {
                Err(Error::Backpressure { .. }) => {
                    pressure += 1;
                    p.advance(&mut WorkBudget::new(1), &off()).unwrap()
                }
                value => value.unwrap(),
            };
            if matches!(step, Progress::Backpressure { .. }) {
                pressure += 1;
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
        assert!(pressure > 0);
        check(&out, case);
        drop(p);
        drop(out);
        assert_eq!(b.used(), Budget::control_storage_bytes());
    }
}
#[test]
fn pending_storage_one_slot_recycles_across_ten_real_fragment_batches() {
    let raw = include_bytes!("fixtures/progressive/ten-sequential-batches.mp4");
    let (mut p, b, _) = player(
        raw,
        ProgressiveLimits {
            fragments_per_batch: 1,
            ..ProgressiveLimits::default()
        },
    );
    let mut out = Vec::new();
    read(&mut p, &mut out).unwrap();
    let got = packets(&out);
    assert_eq!(got.len(), 30);
    for (i, packet) in got.iter().enumerate() {
        assert_eq!(packet.source.sample.ordinal, i as u64 + 1);
        assert_eq!(packet.source.sample.dts_ticks, Some(i as i128 * 1000));
        assert_eq!(packet.source.sample.cts_ticks, Some(i as i128 * 1000));
        assert_eq!(packet.source.sample.duration_ticks, 1000);
        assert_eq!(packet.source.payload.len(), 12);
        assert_eq!(packet.source.sample.description_index, 1);
    }
    drop(p);
    assert_eq!(packets(&out)[29].source.payload.len(), 12);
    drop(out);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}
#[test]
fn pending_storage_limit_keeps_prefix_and_caller_packet_owner() {
    let (mut p, b, _) = player(
        gold::CASES[0].bytes,
        ProgressiveLimits {
            fragments_per_batch: 1,
            ..ProgressiveLimits::default()
        },
    );
    let mut out = Vec::new();
    assert!(matches!(
        read(&mut p, &mut out),
        Err(Error::Limit {
            requested: 2,
            maximum: 1
        })
    ));
    assert_eq!(packets(&out).len(), 3);
    drop(p);
    assert_eq!(packets(&out)[0].source.payload.len(), 12);
    drop(out);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}
type Parked = (Player<Transport>, Budget, Arc<Signals>, Vec<Output>);
fn parked() -> Parked {
    let (mut p, b, signals) = player(gold::CASES[0].bytes, ProgressiveLimits::default());
    signals.gate.store(1095, Ordering::Relaxed);
    let mut out = Vec::new();
    for _ in 0..200_000 {
        assert!(!matches!(
            p.advance(&mut WorkBudget::new(1), &off()).unwrap(),
            Progress::Ended
        ));
        while let Some(value) = p.pop() {
            out.push(value);
        }
        if p.pending_request().is_some_and(|r| r.offset >= 1095) {
            assert_eq!(packets(&out).len(), 3);
            return (p, b, signals, out);
        }
    }
    panic!("pending input was not requested");
}
#[test]
fn pending_storage_pause_and_workzero_preserve_pending_source() {
    let (mut p, b, signals, mut out) = parked();
    let before = (b.used(), signals.polls.load(Ordering::Relaxed));
    for _ in 0..4 {
        assert!(matches!(
            p.advance(&mut WorkBudget::new(0), &off()).unwrap(),
            Progress::Yield
        ));
    }
    p.pause(true).unwrap();
    for _ in 0..4 {
        assert!(matches!(
            p.advance(&mut WorkBudget::new(8), &off()).unwrap(),
            Progress::Paused
        ));
    }
    assert_eq!((b.used(), signals.polls.load(Ordering::Relaxed)), before);
    p.pause(false).unwrap();
    signals.gate.store(usize::MAX, Ordering::Relaxed);
    read(&mut p, &mut out).unwrap();
    check(&out, &gold::CASES[0]);
}
#[test]
fn pending_storage_cancel_retires_source_once_and_keeps_outputs() {
    let (mut p, b, signals, out) = parked();
    assert!(matches!(
        p.advance(&mut WorkBudget::new(0), &AtomicBool::new(true)),
        Err(Error::Cancelled)
    ));
    drop(p);
    assert_eq!(signals.aborts.load(Ordering::Relaxed), 1);
    assert_eq!(signals.closes.load(Ordering::Relaxed), 1);
    assert_eq!(packets(&out).len(), 3);
    assert_eq!(packets(&out)[0].source.payload.len(), 12);
    drop(out);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}

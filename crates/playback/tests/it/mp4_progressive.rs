use crate::support::golds::mp4 as gold;
use crate::support::sequential::*;
use crate::support::*;
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Error as C, WorkBudget};
use triergon_playback::mp4::Mp4Event;
use triergon_playback::{mp4::ProgressiveLimits, *};

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
            "case must remain in the demux comparison gap set after source closure"
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
                .any(|o| matches!(o.event(),Event::Mp4Root(r)if r.header().kind==*b"moof"))
        );
        drop(out);
        drop(p);
        assert_eq!(b.used(), Budget::control_storage_bytes());
    }
}

#[test]
fn progressive_gap_is_pending_until_actual_later_fragment_data_and_eof() {
    let raw = include_bytes!("../fixtures/mp4/f-explicit.mp4");
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
        include_bytes!("../fixtures/mp4/f-explicit.mp4"),
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
        include_bytes!("../fixtures/mp4/f-description-change.mp4"),
        true,
        ProgressiveLimits::default(),
    );
    let out = collect(&mut p, 1).unwrap();
    let mut cfg = None;
    let mut descriptions = Vec::new();
    for o in &out {
        match o.event() {
            Event::Mp4(Mp4Event::Configuration(c)) => {
                cfg = Some((c.track.track_id, c.description.index, c.revision));
                descriptions.push(c.description.index);
            }
            Event::Mp4(Mp4Event::Packet(p)) => assert_eq!(
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
    let raw = include_bytes!("../fixtures/mp4/f-explicit.mp4");
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
        include_bytes!("../fixtures/mp4/f-explicit.mp4"),
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
        include_bytes!("../fixtures/mp4/f-explicit.mp4"),
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

#[test]
fn progressive_pressure_at_each_poll_retries_without_losing_fragment_samples() {
    let (mut p, b, _, _) = player(
        include_bytes!("../fixtures/mp4/f-explicit.mp4"),
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

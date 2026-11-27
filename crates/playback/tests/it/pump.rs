use crate::support::sequential::*;
use crate::support::*;
use triergon_core::{Budget, Error as C, Time, WorkBudget};
use triergon_playback::mp4::Mp4Event;
use triergon_playback::{mp4::ProgressiveLimits, pump::*, *};

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
        include_bytes!("../fixtures/mp4/r-front-stco.mov"),
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
    assert!(sink.accepted.iter().any(|f|matches!(f.output().event(),Event::Mp4(Mp4Event::Packet(p))if !p.source.payload.is_empty())));
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

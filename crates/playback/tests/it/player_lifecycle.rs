use crate::support::players::*;
use crate::support::*;
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, Bytes, Error as C, InputCursor, InputWindow, SourceCapabilities, SourceId, Time,
    WorkBudget,
};
use triergon_mpegts::stream::TraceEvent;
use triergon_mpegts::{PacketFormat, clock::SourceTimePolicy};
use triergon_playback::{
    SourceOwner,
    mp4::{Mp4IndexJob, Mp4Open, OpenLimits, OpenProgress, PlanLimits},
    probe::SelectedInput,
    *,
};
use triergon_playback::{SourceReply, mp4::Mp4Event};

#[test]
fn mp4_work_zero_after_open_and_index_construction_neither_polls_nor_allocates() {
    let raw = include_bytes!("../fixtures/mp4/r-front-stco.mov");
    let b = pool();
    let (source, calls, _) = observed(&b, raw, 7, false);
    let input = InputCursor::new(
        &b,
        ID,
        SourceCapabilities {
            length: Some(raw.len() as u64),
            random_access: true,
        },
        64,
    )
    .unwrap();
    let mut o = Mp4Open::new(
        SelectedInput {
            input,
            source: SourceOwner::new(source),
            format: Format::Mp4,
        },
        OpenLimits::default(),
    )
    .unwrap();
    let before = b.used();
    for _ in 0..10 {
        assert!(matches!(
            o.advance(&mut WorkBudget::new(0), &off()).unwrap(),
            OpenProgress::Yield
        ));
    }
    assert_eq!(b.used(), before);
    assert!(calls.lock().unwrap().is_empty());
    drop(o);
    assert_eq!(b.used(), Budget::control_storage_bytes());
    let (opened, b) = open(raw, 7, 1).unwrap();
    let mut job = Mp4IndexJob::new(&opened.movie, PlanLimits::default()).unwrap();
    let before = b.used();
    assert!(!job.poll(&mut WorkBudget::new(0), &off()).unwrap());
    assert_eq!(b.used(), before);
}

#[test]
fn live_player_work_zero_has_no_queue_reader_or_transport_progress() {
    let (mut p, b, calls, _) = observed_ts();
    let before = (b.used(), calls.lock().unwrap().len());
    for _ in 0..10 {
        assert!(matches!(
            p.advance(&mut WorkBudget::new(0), &off()).unwrap(),
            Progress::Yield
        ));
    }
    assert_eq!((b.used(), calls.lock().unwrap().len()), before);
    assert_eq!(p.queued(), 0);
}

#[test]
fn pause_resume_retains_scope_and_prevents_read_pop_and_delivery() {
    let (mut p, b, calls, _) = observed_ts();
    let first = first_packet(&mut p);
    p.pause(true).unwrap();
    let before = (b.used(), calls.lock().unwrap().len());
    assert!(p.current(&first));
    assert!(!p.may_deliver(&first));
    for _ in 0..4 {
        assert!(matches!(
            p.advance(&mut WorkBudget::new(100), &off()).unwrap(),
            Progress::Paused
        ));
        assert!(p.pop().is_none());
    }
    assert_eq!((b.used(), calls.lock().unwrap().len()), before);
    p.pause(false).unwrap();
    assert!(p.may_deliver(&first));
    collect(&mut p, 1).unwrap();
}

#[test]
fn transport_failure_is_terminal_distinct_from_native_and_source_eof() {
    let (mut p, _, calls, fail) = observed_ts();
    fail.store(true, Ordering::Relaxed);
    let mut failed = false;
    for _ in 0..20 {
        if let Err(C::Transport) = p.advance(&mut WorkBudget::new(7), &off()) {
            failed = true;
            break;
        }
    }
    assert!(failed);
    assert_eq!(p.state(), State::Failed);
    let l = calls.lock().unwrap();
    assert_eq!(&l[l.len() - 2..], &[Call::Abort, Call::Close]);
    assert!(p.pop().is_none());
}

#[test]
fn empty_and_pending_input_are_retryable_starvation_not_eof() {
    let (mut p, b, _, _) = observed_ts();
    let r = loop {
        p.advance(&mut WorkBudget::new(1), &off()).unwrap();
        if let Some(r) = p.pending_request() {
            break r.clone();
        }
    };
    let empty = SourceReply::Data(
        InputWindow::new(
            ID,
            r.token.clone(),
            r.offset,
            Bytes::copy_from_slice(&b, &[]).unwrap(),
        )
        .unwrap(),
    );
    assert!(!p.provide(&SourceReply::Pending).unwrap());
    assert!(!p.provide(&empty).unwrap());
    assert_eq!(p.state(), State::Running);
    assert!(p.pending_request().is_some());
    let out = collect(&mut p, 1).unwrap();
    assert_eq!(pes(&out).len(), 1);
}

#[test]
fn stale_foreign_source_and_foreign_pool_windows_preserve_live_request() {
    let (mut p, b, _, _) = observed_ts();
    let r = loop {
        p.advance(&mut WorkBudget::new(1), &off()).unwrap();
        if let Some(r) = p.pending_request() {
            break r.clone();
        }
    };
    let foreign = pool();
    let wrong = SourceReply::Data(
        InputWindow::new(
            ID,
            r.token.clone(),
            r.offset,
            Bytes::copy_from_slice(&foreign, &[0x47]).unwrap(),
        )
        .unwrap(),
    );
    assert!(matches!(p.provide(&wrong), Err(C::ForeignPool)));
    let wrong = SourceReply::Data(
        InputWindow::new(
            SourceId(900),
            r.token.clone(),
            r.offset,
            Bytes::copy_from_slice(&b, &[0x47]).unwrap(),
        )
        .unwrap(),
    );
    assert!(matches!(p.provide(&wrong), Err(C::SourceMismatch)));
    assert!(p.pending_request().is_some());
    assert_eq!(pes(&collect(&mut p, 1).unwrap()).len(), 1);
}

#[test]
fn paused_seek_changes_scope_now_and_waits_for_resume_before_any_io() {
    let (mut p, _, calls, _) = observed_ts();
    let old = first_packet(&mut p);
    p.pause(true).unwrap();
    p.seek_and_select(
        TrackKey::Ts {
            program: 1,
            pid: 256,
        },
        Time::new(1, 1).unwrap(),
        Selection::All,
        &mut WorkBudget::new(1),
        &off(),
    )
    .unwrap();
    let before = calls.lock().unwrap().len();
    assert!(!p.current(&old));
    assert!(matches!(
        p.advance(&mut WorkBudget::new(100), &off()).unwrap(),
        Progress::Paused
    ));
    assert_eq!(calls.lock().unwrap().len(), before);
    p.pause(false).unwrap();
    assert_eq!(pes(&collect(&mut p, 1).unwrap()).len(), 1);
}

#[test]
fn invalid_track_switch_and_work_zero_leave_current_owners_and_transport_unchanged() {
    let (mut p, _, calls, _) = observed_ts();
    let old = first_packet(&mut p);
    let before = calls.lock().unwrap().len();
    let bad = Selection::Tracks {
        video: Some(TrackKey::Ts {
            program: 1,
            pid: 999,
        }),
        audio: None,
        subtitle: None,
    };
    assert!(
        p.seek_and_select(
            TrackKey::Ts {
                program: 1,
                pid: 256
            },
            Time::new(1, 1).unwrap(),
            bad,
            &mut WorkBudget::new(1),
            &off()
        )
        .is_err()
    );
    assert!(p.current(&old));
    assert!(
        !p.seek_and_select(
            TrackKey::Ts {
                program: 1,
                pid: 256
            },
            Time::new(1, 1).unwrap(),
            Selection::All,
            &mut WorkBudget::new(0),
            &off()
        )
        .unwrap()
    );
    assert_eq!(calls.lock().unwrap().len(), before);
    assert!(p.current(&old));
}

#[test]
fn retained_real_mp4_packet_config_and_native_view_outlive_player() {
    let (mut p, b) = mp4(
        include_bytes!("../fixtures/mp4/r-front-stco.mov"),
        View::Demux,
        7,
        1,
    )
    .unwrap();
    let output = first_packet(&mut p);
    p.stop();
    assert!(!p.current(&output));
    drop(p);
    let Event::Mp4(Mp4Event::Packet(packet)) = output.event() else {
        panic!()
    };
    assert!(!packet.source.payload.is_empty());
    assert!(!packet.configuration.description.raw().bytes().is_empty());
    assert!(packet.occurrence.is_some());
    assert!(b.used() > Budget::control_storage_bytes());
    drop(output);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}

#[test]
fn retained_ts_packet_spans_and_header_outlive_stop_without_leaks() {
    let (mut p, b, _, _) = observed_ts();
    let output = first_packet(&mut p);
    p.stop();
    drop(p);
    let Event::Ts(TsEvent {
        source: TraceEvent::Pes(pes),
        ..
    }) = output.event()
    else {
        panic!()
    };
    assert_eq!(pes.header.pts_raw33, Some(90_000));
    assert_eq!(
        pes.payload().flatten().copied().collect::<Vec<_>>(),
        b"lane-ts-original-payload\x00\x47\xff"
    );
    drop(output);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}

#[test]
fn cancellation_while_paused_pending_is_quiescent_before_close_once() {
    let (mut p, b, calls, _) = observed_ts();
    p.pause(true).unwrap();
    assert!(matches!(
        p.advance(&mut WorkBudget::new(0), &AtomicBool::new(true)),
        Err(C::Cancelled)
    ));
    p.cancel();
    p.stop();
    drop(p);
    assert_eq!(
        calls.lock().unwrap().as_slice(),
        &[Call::Abort, Call::Close]
    );
    assert_eq!(b.used(), Budget::control_storage_bytes());
}

#[test]
fn actual_queue_pressure_stops_read_and_release_resumes_whole_operation() {
    let raw = include_bytes!("../fixtures/ts/ts188-basic.ts");
    let b = pool();
    let (source, calls, _) = observed(&b, raw, 7, false);
    let input = InputCursor::new(
        &b,
        ID,
        SourceCapabilities {
            length: Some(raw.len() as u64),
            random_access: true,
        },
        64,
    )
    .unwrap();
    let mut p = Player::new(
        Engine::ts(
            input,
            PacketFormat::Ts188,
            1,
            triergon_mpegts::ParseLimits::default(),
            SourceTimePolicy::RelativeEpochZero,
            triergon_mpegts::psi::DescriptorNamespace::Unspecified,
        )
        .unwrap(),
        SourceOwner::new(source),
        Selection::All,
        PlayerPolicy {
            slots: 1,
            ..PlayerPolicy::default()
        },
    )
    .unwrap();
    let mut pressure = false;
    for _ in 0..10000 {
        if matches!(
            p.advance(&mut WorkBudget::new(32), &off()).unwrap(),
            Progress::Backpressure { .. }
        ) {
            pressure = true;
            break;
        }
    }
    assert!(pressure, "a one-slot queue must stop real reader progress");
    let before = calls.lock().unwrap().len();
    assert!(matches!(
        p.advance(&mut WorkBudget::new(32), &off()).unwrap(),
        Progress::Backpressure { .. }
    ));
    assert_eq!(calls.lock().unwrap().len(), before);
    let mut out = Vec::new();
    while let Some(o) = p.pop() {
        out.push(o);
    }
    out.extend(collect(&mut p, 1).unwrap());
    assert_eq!(pes(&out).len(), 1);
}

#[test]
fn mp4_plan_cannot_attach_to_another_entity_or_generation() {
    let raw = include_bytes!("../fixtures/mp4/r-front-stco.mov");
    let (opened, b) = open(raw, 7, 1).unwrap();
    let mut job = Mp4IndexJob::new(&opened.movie, PlanLimits::default()).unwrap();
    while !job.poll(&mut WorkBudget::new(11), &off()).unwrap() {}
    let plan = job.finish().unwrap();
    let other = InputCursor::new(
        &b,
        SourceId(999),
        SourceCapabilities {
            length: Some(raw.len() as u64),
            random_access: true,
        },
        64,
    )
    .unwrap();
    assert!(matches!(
        Engine::mp4(other, plan, 1 << 20),
        Err(C::SourceMismatch)
    ));
    let mut job = Mp4IndexJob::new(&opened.movie, PlanLimits::default()).unwrap();
    while !job.poll(&mut WorkBudget::new(11), &off()).unwrap() {}
    let plan = job.finish().unwrap();
    let other = InputCursor::new(
        &b,
        ID,
        SourceCapabilities {
            length: Some(raw.len() as u64),
            random_access: true,
        },
        64,
    )
    .unwrap();
    assert!(matches!(
        Engine::mp4(other, plan, 1 << 20),
        Err(C::StaleGeneration { .. })
    ));
}

#[test]
fn actual_bounded_source_index_refuses_resource_exhaustion_and_releases_owners() {
    let raw = include_bytes!("../fixtures/mp4/r-front-stco.mov");
    let (opened, b) = open(raw, 7, 1).unwrap();
    let before = b.used();
    let mut job = Mp4IndexJob::new(
        &opened.movie,
        PlanLimits {
            samples: 2,
            ..PlanLimits::default()
        },
    )
    .unwrap();
    let mut refused = false;
    for _ in 0..10_000 {
        match job.poll(&mut WorkBudget::new(1), &off()) {
            Err(C::Limit { .. }) => {
                refused = true;
                break;
            }
            Ok(_) => {}
            Err(e) => panic!("wrong resource refusal {e:?}"),
        }
    }
    assert!(refused);
    drop(job);
    assert_eq!(b.used(), before);
    drop(opened);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}

use crate::support::players::*;
use crate::support::*;
use triergon_core::{Time, WorkBudget};
use triergon_mpegts::stream::TraceEvent;
use triergon_playback::*;

#[test]
fn eof_mp4_seek_replays_all_source_preroll_without_editing_packet_time() {
    let raw = include_bytes!("../fixtures/mp4/r-front-stco.mov");
    let (mut p, _) = mp4(raw, View::Source, 1, 1).unwrap();
    let old = collect(&mut p, 1).unwrap();
    let first = &old[0];
    assert!(p.current(first));
    assert!(
        p.seek_and_select(
            TrackKey::Mp4(1),
            Time::new(3, 1).unwrap(),
            Selection::All,
            &mut WorkBudget::new(1),
            &off()
        )
        .unwrap()
    );
    assert!(!p.current(first));
    let replay = collect(&mut p, 1).unwrap();
    assert_eq!(mp4_packets(&old).len(), mp4_packets(&replay).len());
    for o in replay.iter().filter(|o| o.event().track().is_some()) {
        let pts = o.event().source_pts().unwrap().unwrap();
        if pts < Time::new(3, 1).unwrap() {
            assert_eq!(
                o.delivery(),
                Delivery::DecodeOnly {
                    until: Time::new(3, 1).unwrap()
                }
            );
        } else {
            assert_eq!(o.delivery(), Delivery::Present);
        }
    }
}

#[test]
fn source_ts_seek_replay_has_new_generation_and_keeps_complete_psi_before_pes() {
    let (mut p, _, _, _) = observed_ts();
    let old = first_packet(&mut p);
    assert!(
        p.seek_and_select(
            TrackKey::Ts {
                program: 1,
                pid: 256
            },
            Time::new(1, 1).unwrap(),
            Selection::All,
            &mut WorkBudget::new(1),
            &off()
        )
        .unwrap()
    );
    assert!(!p.current(&old));
    let out = collect(&mut p, 1).unwrap();
    let map = out
        .iter()
        .position(|o| {
            matches!(
                o.event(),
                Event::Ts(TsEvent {
                    source: TraceEvent::ProgramMap { active: true, .. },
                    ..
                })
            )
        })
        .unwrap();
    let packet = out
        .iter()
        .position(|o| o.event().track().is_some())
        .unwrap();
    assert!(map < packet);
    assert_eq!(pes(&out).len(), 1);
    assert!(out.iter().all(|o| p.current(o)));
}

#[test]
fn track_switch_keeps_all_mp4_source_packets_and_marks_unselected() {
    let raw = include_bytes!("../fixtures/mp4/r-multitrack.mov");
    let (mut p, _) = mp4(raw, View::Source, 1, 1).unwrap();
    let old = first_packet(&mut p);
    let selection = Selection::Tracks {
        video: Some(TrackKey::Mp4(1)),
        audio: None,
        subtitle: None,
    };
    p.seek_and_select(
        TrackKey::Mp4(1),
        Time::new(0, 1).unwrap(),
        selection,
        &mut WorkBudget::new(1),
        &off(),
    )
    .unwrap();
    assert!(!p.current(&old));
    let out = collect(&mut p, 1).unwrap();
    assert_eq!(mp4_packets(&out).len(), 12);
    for o in out.iter().filter(|o| o.event().track().is_some()) {
        assert_eq!(
            o.delivery() == Delivery::Unselected,
            o.event().track() == Some(TrackKey::Mp4(2))
        );
    }
}

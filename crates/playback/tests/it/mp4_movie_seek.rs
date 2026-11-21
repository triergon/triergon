use crate::support::players::*;
use crate::support::*;
use triergon_core::{Error as C, Time, WorkBudget};
use triergon_playback::*;

#[test]
fn real_movie_edit_seek_maps_exact_source_point_before_playback_fence() {
    let raw = include_bytes!("../fixtures/mp4/r-edits-repeat.mov");
    let (mut p, _) = mp4(raw, View::Source, 7, 1).unwrap();
    let old = first_packet(&mut p);
    let plan = p.engine().and_then(Engine::mp4_plan).unwrap();
    let mut job = plan.movie_seek(1, Time::new(1, 2).unwrap(), 1024).unwrap();
    while !job.poll(&mut WorkBudget::new(1), &off()).unwrap() {}
    let command = job.finish().unwrap();
    let triergon_isobmff::seek::MoviePoint::Media(point) = command.point() else {
        panic!("frozen edit fixture must contain this movie point")
    };
    assert_eq!(point.requested_movie_time, Time::new(1, 2).unwrap());
    #[cfg(feature = "reference-trace")]
    assert!(command.raw_edits().is_some());
    #[cfg(feature = "reference-trace")]
    assert!(command.raw_media_clock().payload().len() >= 20);
    let target = point.source_time;
    assert!(matches!(
        p.apply_movie_seek(&command, Selection::All, &mut WorkBudget::new(0), &off())
            .unwrap(),
        mp4::MovieSeekOutcome::Yield
    ));
    assert!(p.current(&old));
    assert!(matches!(
        p.apply_movie_seek(&command, Selection::All, &mut WorkBudget::new(1), &off())
            .unwrap(),
        mp4::MovieSeekOutcome::Applied(_)
    ));
    assert!(!p.current(&old));
    let out = collect(&mut p, 1).unwrap();
    assert!(!mp4_packets(&out).is_empty());
    for o in out.iter().filter(|o| o.event().track().is_some()) {
        if o.event().source_pts().unwrap().is_some_and(|t| t < target) {
            assert_eq!(o.delivery(), Delivery::DecodeOnly { until: target });
        }
    }
}

#[test]
fn movie_seek_command_cannot_cross_source_owner_pools() {
    let raw = include_bytes!("../fixtures/mp4/r-front-stco.mov");
    let (a, _) = mp4(raw, View::Source, 7, 1).unwrap();
    let (mut b, _) = mp4(raw, View::Source, 7, 1).unwrap();
    let plan = a.engine().and_then(Engine::mp4_plan).unwrap();
    let mut job = plan.movie_seek(1, Time::new(0, 1).unwrap(), 16).unwrap();
    while !job.poll(&mut WorkBudget::new(1), &off()).unwrap() {}
    let command = job.finish().unwrap();
    assert!(matches!(
        b.apply_movie_seek(&command, Selection::All, &mut WorkBudget::new(1), &off()),
        Err(C::ForeignPool)
    ));
    assert_eq!(b.state(), State::Running);
}

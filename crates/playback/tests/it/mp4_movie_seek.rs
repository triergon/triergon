use crate::support::players::*;
use crate::support::*;
use triergon_core::{Time, WorkBudget};
use triergon_playback::*;

#[test]
fn movie_time_maps_to_a_source_seek_that_lands_on_the_sync_sample() {
    let raw = include_bytes!("../fixtures/mp4/r-edits-repeat.mov");
    let (mut p, _) = mp4(raw, View::Source, 7, 1).unwrap();
    let old = first_packet(&mut p);
    let plan = p.engine().and_then(Engine::mp4_plan).unwrap();
    // 4.5 s lies in the third edit, which presents media time 0.5 s again.
    let mut job = plan.movie_seek(1, Time::new(9, 2).unwrap(), 1024).unwrap();
    while !job.poll(&mut WorkBudget::new(1), &off()).unwrap() {}
    let command = job.finish().unwrap();
    let triergon_isobmff::seek::MoviePoint::Media(point) = command.point() else {
        panic!("frozen edit fixture must contain this movie point")
    };
    assert_eq!(point.requested_movie_time, Time::new(9, 2).unwrap());
    assert_eq!(point.edit_ordinal, Some(3));
    #[cfg(feature = "reference-trace")]
    assert!(command.raw_edits().is_some());
    #[cfg(feature = "reference-trace")]
    assert!(command.raw_media_clock().payload().len() >= 20);
    let target = point.source_time;
    assert_eq!(target, Time::new(1, 2).unwrap());
    let request = command.sync_seek(Time::new(0, 1).unwrap(), 1024).unwrap();
    assert_eq!((request.video, request.source_time), (1, target));
    let seek = {
        let mut job = p.prepare_mp4_sync_seek(request, Selection::All).unwrap();
        while !job.poll(&mut WorkBudget::new(1), &off()).unwrap() {}
        job.finish().unwrap()
    };
    assert!(
        !p.apply_mp4_sync_seek(&seek, &mut WorkBudget::new(0), &off())
            .unwrap()
    );
    assert!(p.current(&old));
    assert!(
        p.apply_mp4_sync_seek(&seek, &mut WorkBudget::new(1), &off())
            .unwrap()
    );
    assert!(!p.current(&old));
    let out = collect(&mut p, 1).unwrap();
    let packets = mp4_packets(&out);
    // The replay starts at the sync sample that holds the mapped time.
    assert_eq!(packets[0].source.sample.ordinal, 1);
    for o in out.iter().filter(|o| o.event().track().is_some()) {
        if o.event().source_pts().unwrap().is_some_and(|t| t < target) {
            assert_eq!(o.delivery(), Delivery::DecodeOnly { until: target });
        }
    }
}

#[test]
fn movie_time_in_an_empty_edit_or_outside_the_edits_has_no_seek() {
    let raw = include_bytes!("../fixtures/mp4/forward-avc-empty-edit.mp4");
    let (p, _) = mp4(raw, View::Source, 7, 1).unwrap();
    let plan = p.engine().and_then(Engine::mp4_plan).unwrap();
    let limits = triergon_isobmff::compat::mov::seek::SeekLimits::default();
    for (ticks, empty) in [(0, true), (1179, true), (1_000_000, false)] {
        let time = Time::new(ticks, 12288).unwrap();
        let mut job = plan.movie_seek(1, time, 16).unwrap();
        while !job.poll(&mut WorkBudget::new(1), &off()).unwrap() {}
        let command = job.finish().unwrap();
        assert_eq!(
            matches!(
                command.point(),
                triergon_isobmff::seek::MoviePoint::Empty(_)
            ),
            empty
        );
        assert!(command.demux_seek(limits).unwrap().is_none());
        assert!(command.sync_seek(Time::new(0, 1).unwrap(), 16).is_none());
    }
}

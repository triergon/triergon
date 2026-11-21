use crate::support::golds::mp4 as gold;
use crate::support::players::*;
use crate::support::*;
use triergon_core::{Budget, WorkBudget};
use triergon_mpegts::{
    PacketFormat,
    clock::{SourceTimePolicy, SourceTimeStatus},
};
use triergon_playback::mp4::Mp4Event;
use triergon_playback::{
    mp4::{Mp4IndexJob, Mp4Open, OpenLimits, PlanLimits},
    *,
};

#[test]
fn all17_frozen_mp4_controls_keep_independent_original_source_gold() {
    assert_eq!(gold::CASES.len(), 17);
    for case in gold::CASES {
        println!(
            "SOURCE_CASE {} fragmented={} gap={} malformed={}",
            case.name, case.fragmented, case.gap, case.malformed
        );
        let result = mp4(case.bytes, View::Source, 7, 1);
        if case.malformed {
            assert!(
                result.is_err(),
                "{} must refuse malformed source",
                case.name
            );
            continue;
        }
        let (mut p, b) = result.unwrap_or_else(|e| panic!("{} {e:?}", case.name));
        let out = collect(&mut p, 1).unwrap();
        let packets = mp4_packets(&out);
        assert_eq!(packets.len(), case.samples.len(), "{}", case.name);
        for expected in case.samples {
            let p = packets
                .iter()
                .find(|p| {
                    p.configuration.track.track_id == expected.track
                        && p.source.sample.ordinal == expected.ordinal
                })
                .unwrap();
            let sample = &p.source.sample;
            assert_eq!(
                sample.description_index, expected.description,
                "{}",
                case.name
            );
            assert_eq!(sample.range.start, expected.offset);
            assert_eq!(p.source.payload.as_ref(), expected.payload);
            assert_eq!(sample.duration_ticks, expected.delta);
            assert_eq!(sample.is_container_sync(), expected.sync);
            if let Some(dts) = sample.dts_ticks {
                assert_eq!(dts, expected.dts);
                assert_eq!(sample.cts_ticks, Some(expected.cts));
            } else {
                assert_eq!(case.name, "f-no-second-tfdt");
                assert!(sample.cts_ticks.is_none());
            }
            assert_eq!(
                p.source.payload.as_ref(),
                &case.bytes[sample.range.start as usize..sample.range.end as usize]
            );
        }
        drop(out);
        drop(p);
        assert_eq!(b.used(), Budget::control_storage_bytes());
    }
}

#[test]
fn actual_mp4_native_order_stays_distinct_from_all_source_facts() {
    let raw = include_bytes!("../fixtures/mp4/r-multitrack.mov");
    let (mut p, _) = mp4(raw, View::Demux, 7, 1).unwrap();
    if let Some(Engine::Mp4(engine)) = p.engine() {
        assert_eq!(engine.plan.source_rows().len(), 12);
        assert_eq!(engine.plan.demux_rows().len(), 12);
    } else {
        panic!()
    }
    let out = collect(&mut p, 1).unwrap();
    let packets = mp4_packets(&out);
    let order: Vec<_> = packets
        .iter()
        .map(|p| (p.configuration.track.track_id, p.source.sample.ordinal))
        .collect();
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
    for p in packets {
        let n = p.occurrence.as_ref().unwrap();
        assert_eq!(n.source, p.source.sample);
        assert_eq!(n.demux.source_range, p.source.sample.range);
    }
}

#[test]
fn description_changes_publish_raw_config_before_each_matching_packet() {
    for raw in [
        include_bytes!("../fixtures/mp4/r-description-change.mov").as_slice(),
        include_bytes!("../fixtures/mp4/f-description-change.mp4").as_slice(),
    ] {
        let (mut p, _) = mp4(raw, View::Source, 1, 1).unwrap();
        let out = collect(&mut p, 1).unwrap();
        let mut last = None;
        let mut seen = Vec::new();
        for o in &out {
            match o.event() {
                Event::Mp4(Mp4Event::Configuration(c)) => {
                    last = Some((c.track.track_id, c.description.index, c.revision));
                    seen.push(c.description.index);
                }
                Event::Mp4(Mp4Event::Packet(p)) => {
                    assert_eq!(
                        last,
                        Some((
                            p.configuration.track.track_id,
                            p.source.sample.description_index,
                            p.configuration.revision
                        ))
                    );
                    assert_eq!(
                        p.configuration.description.index,
                        p.source.sample.description_index
                    );
                }
                _ => {}
            }
        }
        assert!(seen.contains(&1) && seen.contains(&2));
    }
}

#[test]
fn multitrack_packet_configuration_uses_its_track_revision_not_latest_other_track() {
    let (mut p, _) = mp4(
        include_bytes!("../fixtures/mp4/r-multitrack.mov"),
        View::Source,
        7,
        1,
    )
    .unwrap();
    let out = collect(&mut p, 17).unwrap();
    let mut revisions = std::collections::BTreeMap::new();
    for o in out {
        match o.event() {
            Event::Mp4(Mp4Event::Configuration(c)) => {
                revisions.insert(c.track.track_id, c.revision);
            }
            Event::Mp4(Mp4Event::Packet(p)) => assert_eq!(
                Some(&p.configuration.revision),
                revisions.get(&p.configuration.track.track_id)
            ),
            _ => {}
        }
    }
}

#[test]
fn source_unknown_fragment_clock_is_not_filled_from_native_projection() {
    let raw = include_bytes!("../fixtures/mp4/f-no-second-tfdt.mp4");
    let (mut p, _) = mp4(raw, View::Demux, 1, 1).unwrap();
    let out = collect(&mut p, 1).unwrap();
    let unknown: Vec<_> = mp4_packets(&out)
        .into_iter()
        .filter(|p| p.source.sample.dts_ticks.is_none())
        .collect();
    assert!(!unknown.is_empty());
    assert!(unknown.iter().all(|p| p.source.sample.cts_ticks.is_none()));
    assert!(
        unknown
            .iter()
            .any(|p| p.occurrence.as_ref().is_some_and(|n| n.demux.dts.is_some()))
    );
}

#[test]
fn source_and_native_clock_delivery_are_explicit_independent_views() {
    let raw = include_bytes!("../fixtures/ts/pts-negative-against-dts.ts");
    let (mut strict, _) = ts(raw, PacketFormat::Ts188, 7, SourceTimePolicy::Unanchored).unwrap();
    let source = collect(&mut strict, 1).unwrap();
    let e = pes(&source)[0];
    assert_eq!(e.source_pts.unwrap().raw33, Some((1_u64 << 33) - 2));
    assert_eq!(
        e.source_pts.unwrap().status,
        SourceTimeStatus::AnchorBoundMissing
    );
    assert!(e.source_pts.unwrap().exact.is_none());
    assert_eq!(e.demux_clock.unwrap().pts90, Some((1_i128 << 33) - 2));
    assert_eq!(e.demux_clock.unwrap().dts90, Some(3));
    assert!(
        source
            .iter()
            .filter(|o| o.event().track().is_some())
            .all(|o| o.delivery() == Delivery::UnknownTime)
    );
    let (mut native, _) = ts(raw, PacketFormat::Ts188, 7, SourceTimePolicy::Unanchored).unwrap();
    native.set_clock_view(ClockChoice::Demux).unwrap();
    let out = collect(&mut native, 1).unwrap();
    assert!(
        out.iter()
            .filter(|o| o.event().track().is_some())
            .all(|o| o.delivery() == Delivery::Present)
    );
    assert!(native.set_clock_view(ClockChoice::Source).is_err());
}

#[test]
fn front_moov_mp4_streams_real_samples_without_source_seek_or_preloading_mdat() {
    let raw = include_bytes!("../fixtures/mp4/r-front-stco.mov");
    for q in [1, 7, 4096] {
        let (selected, b, calls) = probe(raw, q, true);
        let mut open = Mp4Open::new(selected, OpenLimits::default()).unwrap();
        run_open(&mut open, 1).unwrap();
        let opened = open.finish().unwrap();
        assert!(matches!(
            opened.movie.read_scope(),
            mp4::MetadataReadScope::FrontMetadataBeforeMdat(_)
        ));
        let at = opened.input.position();
        assert!(at < raw.len() as u64);
        let mut index = Mp4IndexJob::new(&opened.movie, PlanLimits::default()).unwrap();
        while !index.poll(&mut WorkBudget::new(1), &off()).unwrap() {}
        let plan = index.finish().unwrap();
        let mut player = Player::new(
            Engine::mp4(opened.input, plan, 1 << 20).unwrap(),
            opened.source,
            Selection::All,
            PlayerPolicy::default(),
        )
        .unwrap();
        drop(opened.movie);
        let out = collect(&mut player, 1).unwrap();
        assert_eq!(mp4_packets(&out).len(), 6);
        for p in mp4_packets(&out) {
            assert_eq!(
                p.source.payload.as_ref(),
                &raw[p.source.sample.range.start as usize..p.source.sample.range.end as usize]
            );
        }
        let offsets: Vec<_> = calls
            .lock()
            .unwrap()
            .iter()
            .filter_map(|c| {
                if let Call::Poll(at) = c {
                    Some(*at)
                } else {
                    None
                }
            })
            .collect();
        assert!(offsets.windows(2).all(|p| p[0] < p[1]));
        drop(out);
        drop(player);
        assert_eq!(b.used(), Budget::control_storage_bytes());
    }
}

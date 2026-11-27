use crate::support::players::*;
use crate::support::*;
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Error as C, Time, WorkBudget};
use triergon_playback::mp4::{Mp4Event, Mp4SyncSeekRequest};
use triergon_playback::{
    mp4::{Mp4IndexJob, Mp4Open, OpenLimits, OpenProgress, PlanLimits},
    *,
};

#[test]
fn indexed_mp4_seek_starts_at_late_sync_and_republishes_its_configuration() {
    let raw = include_bytes!("../fixtures/mp4/r-description-change.mov");
    let (mut p, _) = mp4(raw, View::Source, 7, 1).unwrap();
    let old = first_packet(&mut p);
    let target = Time::new(7, 2).unwrap();
    let mut unsafe_transition = p
        .prepare_mp4_sync_seek(
            indexed_seek_request(Time::new(9, 2).unwrap()),
            Selection::All,
        )
        .unwrap();
    assert!(matches!(
        unsafe_transition.poll(&mut WorkBudget::new(64), &off()),
        Err(C::Unsupported("no MP4 sync in target configuration"))
    ));
    assert!(p.current(&old));
    let request = indexed_seek_request(target);
    let mut job = p.prepare_mp4_sync_seek(request, Selection::All).unwrap();
    assert!(!job.poll(&mut WorkBudget::new(0), &off()).unwrap());
    while !job.poll(&mut WorkBudget::new(1), &off()).unwrap() {}
    let command = job.finish().unwrap();
    #[cfg(feature = "reference-trace")]
    assert_eq!(command.sync_ordinal(), 4);
    assert_eq!(command.start_offset(), 815);
    assert!(
        !p.apply_mp4_sync_seek(&command, &mut WorkBudget::new(0), &off())
            .unwrap()
    );
    assert!(p.current(&old));
    assert!(
        p.apply_mp4_sync_seek(&command, &mut WorkBudget::new(1), &off())
            .unwrap()
    );
    assert!(!p.current(&old));
    assert!(matches!(
        p.apply_mp4_sync_seek(&command, &mut WorkBudget::new(1), &off()),
        Err(C::StaleGeneration { .. })
    ));
    let events = collect(&mut p, 1).unwrap();
    let packets = mp4_packets(&events);
    assert_eq!(
        packets
            .iter()
            .map(|p| p.source.sample.ordinal)
            .collect::<Vec<_>>(),
        [4, 5, 6]
    );
    assert_eq!(packets[0].source.sample.range.start, 815);
    assert_eq!(packets[0].source.payload.as_ref(), &raw[815..833]);
    assert_eq!(packets[0].configuration.description.index, 2);
    assert!(matches!(
        events[0].event(),
        Event::Seek(SeekIntent {
            replay_from_start: false,
            ..
        })
    ));
    assert!(matches!(
        events[1].event(),
        Event::Mp4(Mp4Event::Configuration(_))
    ));
    let delivered = events
        .iter()
        .filter(|e| matches!(e.event(), Event::Mp4(Mp4Event::Packet(_))))
        .map(Output::delivery)
        .collect::<Vec<_>>();
    assert_eq!(
        delivered,
        [
            Delivery::DecodeOnly { until: target },
            Delivery::Present,
            Delivery::Present
        ]
    );
}

fn indexed_seek_request(source_time: Time) -> Mp4SyncSeekRequest {
    Mp4SyncSeekRequest {
        video: 1,
        source_time,
        audio_preroll: Time::new(0, 1).unwrap(),
        maximum_scanned_rows: 1000,
    }
}

#[test]
fn indexed_mp4_seek_budget_covers_preroll_and_stops_at_the_target() {
    let raw = include_bytes!("../fixtures/mp4/r-description-change.mov");
    let target = Time::new(7, 2).unwrap();
    // Five seven-byte windows reach the 4s packet. The final
    // 5s packet needs a sixth window.
    for seek_input_bytes in [34, 35] {
        let (opened, _) = open(raw, 7, 64).unwrap();
        let mut index = Mp4IndexJob::new(&opened.movie, PlanLimits::default()).unwrap();
        while !index.poll(&mut WorkBudget::new(64), &off()).unwrap() {}
        let engine = Engine::mp4(opened.input, index.finish().unwrap(), 1 << 20).unwrap();
        let mut player = Player::new(
            engine,
            opened.source,
            Selection::All,
            PlayerPolicy {
                seek_input_bytes,
                ..PlayerPolicy::default()
            },
        )
        .unwrap();
        let mut job = player
            .prepare_mp4_sync_seek(indexed_seek_request(target), Selection::All)
            .unwrap();
        while !job.poll(&mut WorkBudget::new(64), &off()).unwrap() {}
        assert!(
            player
                .apply_mp4_sync_seek(&job.finish().unwrap(), &mut WorkBudget::new(64), &off())
                .unwrap()
        );
        let output = collect(&mut player, 64);
        if seek_input_bytes == 34 {
            assert!(matches!(output, Err(C::Limit { maximum: 34, .. })));
        } else {
            let output = output.unwrap();
            let deliveries: Vec<_> = output
                .iter()
                .filter(|output| matches!(output.event(), Event::Mp4(Mp4Event::Packet(_))))
                .map(Output::delivery)
                .collect();
            assert_eq!(
                deliveries,
                [
                    Delivery::DecodeOnly { until: target },
                    Delivery::Present,
                    Delivery::Present,
                ]
            );
            assert_eq!(player.seek_intent().unwrap().target, target);
        }
    }
}

#[test]
fn indexed_mp4_seek_keeps_selected_subtitle_prefix_and_only_late_video() {
    let raw = include_bytes!("../fixtures/mp4/r-multitrack.mov");
    let (mut p, _) = mp4(raw, View::Source, 7, 1).unwrap();
    let before = collect(&mut p, 7).unwrap();
    let all = mp4_packets(&before);
    let expected: Vec<_> = all
        .iter()
        .filter(|p| p.configuration.track.track_id == 2 || p.source.sample.ordinal >= 4)
        .map(|p| (&p.source.sample, p.source.payload.as_ref()))
        .collect();
    let target = Time::new(9, 2).unwrap();
    let selection = Selection::Tracks {
        video: Some(TrackKey::Mp4(1)),
        audio: None,
        subtitle: Some(TrackKey::Mp4(2)),
    };
    let request = indexed_seek_request(target);

    let mut cancelled = p.prepare_mp4_sync_seek(request, selection).unwrap();
    assert!(matches!(
        cancelled.poll(&mut WorkBudget::new(1), &AtomicBool::new(true)),
        Err(C::Cancelled)
    ));
    assert!(matches!(cancelled.finish(), Err(C::Cancelled)));

    let mut limited = p
        .prepare_mp4_sync_seek(
            Mp4SyncSeekRequest {
                maximum_scanned_rows: 2,
                ..request
            },
            selection,
        )
        .unwrap();
    assert!(matches!(
        limited.poll(&mut WorkBudget::new(3), &off()),
        Err(C::Limit {
            requested: 3,
            maximum: 2
        })
    ));
    assert!(matches!(limited.finish(), Err(C::Limit { .. })));
    assert!(p.current(before.last().unwrap()));
    let mut job = p.prepare_mp4_sync_seek(request, selection).unwrap();
    while !job.poll(&mut WorkBudget::new(1), &off()).unwrap() {}
    let command = job.finish().unwrap();
    #[cfg(feature = "reference-trace")]
    assert_eq!(command.sync_ordinal(), 4);
    assert_eq!(command.start_offset(), 1175);
    assert!(
        p.apply_mp4_sync_seek(&command, &mut WorkBudget::new(1), &off())
            .unwrap()
    );
    let after = collect(&mut p, 1).unwrap();
    let actual: Vec<_> = mp4_packets(&after)
        .into_iter()
        .map(|p| (&p.source.sample, p.source.payload.as_ref()))
        .collect();
    assert_eq!(actual, expected);
}

#[test]
fn indexed_mp4_seek_reads_aac_preroll_and_matches_the_encoded_suffix() {
    let raw = include_bytes!("../fixtures/mp4/encoded-avc-aac-indexed-seek.mp4");
    let (selected, budget, calls) = probe(raw, 37, false);
    let mut open = Mp4Open::new(selected, OpenLimits::default()).unwrap();
    while !matches!(
        open.advance(&mut WorkBudget::new(64), &off()).unwrap(),
        OpenProgress::Ready
    ) {}
    let opened = open.finish().unwrap();
    let mut index = Mp4IndexJob::new(&opened.movie, PlanLimits::default()).unwrap();
    while !index.poll(&mut WorkBudget::new(64), &off()).unwrap() {}
    let engine = Engine::mp4(opened.input, index.finish().unwrap(), 1 << 20).unwrap();
    let mut p = Player::new(
        engine,
        opened.source,
        Selection::All,
        PlayerPolicy {
            seek_input_bytes: 8192,
            ..PlayerPolicy::default()
        },
    )
    .unwrap();
    let before = collect(&mut p, 64).unwrap();
    let packets = mp4_packets(&before);
    let sync = packets
        .iter()
        .find(|p| p.configuration.track.track_id == 1 && p.source.sample.ordinal == 17)
        .unwrap();
    assert_eq!(sync.source.sample.range.start, 13038);
    assert!(sync.source.sample.is_container_sync());
    let sync_time = sync
        .source
        .sample
        .source_times(sync.configuration.track.base.require().unwrap())
        .unwrap()
        .cts
        .unwrap();
    let preroll = Time::new(1, 2).unwrap();
    let audio_from = sync_time.checked_sub(preroll).unwrap();
    let target = sync_time.checked_add(Time::new(1, 8).unwrap()).unwrap();
    let first_audio = packets
        .iter()
        .find(|p| {
            if p.configuration.track.track_id != 2 {
                return false;
            }
            let times = p
                .source
                .sample
                .source_times(p.configuration.track.base.require().unwrap())
                .unwrap();
            times.cts.unwrap().checked_add(times.decode_delta).unwrap() > audio_from
        })
        .unwrap();
    let first_offset = first_audio.source.sample.range.start;
    assert_eq!(first_offset, 12886);
    let expected: Vec<_> = packets
        .iter()
        .filter(|p| {
            p.source.sample.range.start >= first_offset
                && (p.configuration.track.track_id == 2 || p.source.sample.ordinal >= 17)
        })
        .map(|p| (&p.source.sample, p.source.payload.as_ref()))
        .collect();

    let request = Mp4SyncSeekRequest {
        audio_preroll: preroll,
        ..indexed_seek_request(target)
    };
    let mut job = p.prepare_mp4_sync_seek(request, Selection::All).unwrap();
    while !job.poll(&mut WorkBudget::new(1), &off()).unwrap() {}
    let command = job.finish().unwrap();
    #[cfg(feature = "reference-trace")]
    assert_eq!(command.sync_ordinal(), 17);
    assert_eq!(command.start_offset(), first_offset);
    calls.lock().unwrap().clear();
    assert!(
        p.apply_mp4_sync_seek(&command, &mut WorkBudget::new(1), &off())
            .unwrap()
    );
    let after = collect(&mut p, 11).unwrap();
    let actual: Vec<_> = mp4_packets(&after)
        .into_iter()
        .map(|p| (&p.source.sample, p.source.payload.as_ref()))
        .collect();
    assert_eq!(actual, expected);
    assert_eq!(actual.len(), 41);
    assert_eq!(
        actual
            .iter()
            .map(|(_, payload)| payload.len())
            .sum::<usize>(),
        4373
    );
    let calls = calls.lock().unwrap();
    assert_eq!(calls[0], Call::Invalidate);
    let offsets: Vec<_> = calls
        .iter()
        .filter_map(|call| match call {
            Call::Poll(offset) => Some(*offset),
            _ => None,
        })
        .collect();
    assert_eq!(offsets.first(), Some(&first_offset));
    assert!(offsets.iter().all(|offset| *offset >= first_offset));
    assert!(
        after
            .iter()
            .any(|o| matches!(o.delivery(), Delivery::DecodeOnly { .. }))
    );
    assert!(after.iter().any(|o| o.delivery() == Delivery::Present));
    drop(calls);
    drop(command);
    drop(p);
    drop(after);
    drop(before);
    drop(opened.movie);
    assert_eq!(budget.used(), Budget::control_storage_bytes());
}

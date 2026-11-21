use crate::support::players::*;
use crate::support::*;
use triergon_core::{Budget, Bytes, Error as C, InputWindow, SourceCapabilities, Time, WorkBudget};
use triergon_playback::{
    ByteSource, Source, SourceReply,
    mp4::{Mp4DemuxSeekRequest, Mp4Event, Mp4RangeOpen, Mp4RangePlayer},
};
use triergon_playback::{
    SourceOwner,
    mp4::{Mp4IndexJob, PlanLimits},
    *,
};

#[test]
fn late_preseek_range_reply_is_rejected_before_current_source_advance() {
    let (mut p, b, _, _) = observed_ts();
    first_packet(&mut p);
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
    let old = loop {
        p.advance(&mut WorkBudget::new(1), &off()).unwrap();
        while p.pop().is_some() {}
        if let Some(r) = p.pending_request() {
            break r.clone();
        }
    };
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
    for _ in 0..20 {
        p.advance(&mut WorkBudget::new(1), &off()).unwrap();
        while p.pop().is_some() {}
        if p.pending_request().is_some() {
            break;
        }
    }
    let bad = SourceReply::Data(
        InputWindow::new(
            ID,
            old.token,
            old.offset,
            Bytes::copy_from_slice(&b, &[0x47]).unwrap(),
        )
        .unwrap(),
    );
    assert!(matches!(p.provide(&bad), Err(C::StaleGeneration { .. })));
    assert_eq!(pes(&collect(&mut p, 1).unwrap()).len(), 1);
}

fn observed_mp4_open(bytes: &[u8]) -> (mp4::OpenedMp4<Observed>, Calls, Budget) {
    open_with_timing_policy(
        bytes,
        triergon_isobmff::table::TimingRunPolicy::Strict,
        true,
    )
    .unwrap()
}

fn native_range_player(bytes: &[u8]) -> (Player<Observed>, mp4::Movie, Calls, Budget) {
    let (opened, calls, pool) = observed_mp4_open(bytes);
    let mut job = compat::mov::index(&opened.movie, PlanLimits::default()).unwrap();
    assert!((0..100_000).any(|_| job.poll(&mut WorkBudget::new(17), &off()).unwrap()));
    let engine = Engine::mp4(opened.input, job.finish().unwrap(), 1 << 20).unwrap();
    let mut player = Player::new(
        engine,
        opened.source,
        Selection::All,
        PlayerPolicy::default(),
    )
    .unwrap();
    player.set_clock_view(ClockChoice::Demux).unwrap();
    (player, opened.movie, calls, pool)
}

fn native_seek_cases() -> [(&'static [u8], i64, i64, &'static str); 5] {
    [
        (
            include_bytes!("../fixtures/mp4/r-multitrack.mov").as_slice(),
            3500,
            3000,
            include_str!("../fixtures/mp4/r-multitrack.mov.native-seek"),
        ),
        (
            include_bytes!("../fixtures/mp4/f-multitrack.mp4").as_slice(),
            3500,
            3000,
            include_str!("../fixtures/mp4/f-multitrack.mp4.native-seek"),
        ),
        (
            include_bytes!("../fixtures/mp4/r-edits-repeat.mov").as_slice(),
            1500,
            0,
            include_str!("../fixtures/mp4/r-edits-repeat.mov.native-seek"),
        ),
        (
            include_bytes!("../fixtures/mp4/r-description-change.mov").as_slice(),
            3500,
            3000,
            include_str!("../fixtures/mp4/r-description-change.mov.native-seek"),
        ),
        (
            include_bytes!("../fixtures/mp4/encoded-avc-aac-native-seek.mp4").as_slice(),
            28672,
            16384,
            include_str!("../fixtures/mp4/encoded-avc-aac-native-seek.mp4.native-seek"),
        ),
    ]
}

#[test]
fn native_range_seek_matches_ffprobe_packet_order_bytes_and_clocks() {
    for (bytes, target, selected, expected) in native_seek_cases() {
        let (mut player, movie, calls, pool) = native_range_player(bytes);
        let old = first_packet(&mut player);
        calls.lock().unwrap().clear();
        let request = Mp4DemuxSeekRequest {
            video: 1,
            timestamp: target,
            limits: triergon_isobmff::compat::mov::seek::SeekLimits::default(),
        };
        let mut job = player
            .prepare_mp4_demux_seek(&movie, request, Selection::All)
            .unwrap();
        assert!(!job.poll(&mut WorkBudget::new(0), &off()).unwrap());
        assert!((0..100_000).any(|_| job.poll(&mut WorkBudget::new(1), &off()).unwrap()));
        let command = job.finish().unwrap();
        assert_eq!(command.receipt().requested_timestamp, target);
        assert_eq!(command.receipt().selected.timestamp, selected);
        assert!(
            calls.lock().unwrap().is_empty(),
            "demux seek planning read media"
        );
        assert!(
            !player
                .apply_mp4_demux_seek(&command, &mut WorkBudget::new(0), &off())
                .unwrap()
        );
        assert!(player.current(&old));
        assert!(
            player
                .apply_mp4_demux_seek(&command, &mut WorkBudget::new(1), &off())
                .unwrap()
        );
        assert!(!player.current(&old));
        assert!(matches!(
            player.apply_mp4_demux_seek(&command, &mut WorkBudget::new(1), &off()),
            Err(C::StaleGeneration { .. })
        ));

        let events = collect(&mut player, 1).unwrap();
        assert_native_seek_packets(bytes, expected, &events, |event| player.current(event));
        let first_read = calls.lock().unwrap().iter().find_map(|call| match call {
            Call::Poll(offset) => Some(*offset),
            _ => None,
        });
        assert_eq!(first_read, Some(command.start_offset()));
        assert!(command.start_offset() > 0);
        drop(command);
        drop(events);
        drop(old);
        drop(player);
        drop(movie);
        assert_eq!(pool.used(), Budget::control_storage_bytes());
    }
}

fn assert_native_seek_packets(
    bytes: &[u8],
    expected: &str,
    events: &[Output],
    current: impl Fn(&Output) -> bool,
) {
    assert!(matches!(
        events[0].event(),
        Event::Seek(SeekIntent {
            clock: ClockChoice::Demux,
            replay_from_start: false,
            ..
        })
    ));
    let mut configurations = [None; 2];
    let mut wanted = expected.lines();
    for event in &events[1..] {
        match event.event() {
            Event::Mp4(Mp4Event::Configuration(config)) => {
                configurations[(config.track.track_id - 1) as usize] =
                    Some((config.revision, config.description.index));
            }
            Event::Mp4(Mp4Event::Packet(packet)) => {
                let fields: Vec<_> = wanted
                    .next()
                    .expect("extra packet after demux seek")
                    .split_ascii_whitespace()
                    .collect();
                let track: u32 = fields[0].parse().unwrap();
                assert_eq!(packet.configuration.track.track_id, track);
                assert_eq!(
                    configurations[(track - 1) as usize],
                    Some((
                        packet.configuration.revision,
                        packet.configuration.description.index
                    ))
                );
                let native = packet.occurrence.as_ref().unwrap();
                assert_eq!(native.track_id, track);
                assert_eq!(native.demux.pts, fields[1].parse().ok());
                assert_eq!(native.demux.dts, fields[2].parse().ok());
                assert_eq!(native.demux.duration, fields[3].parse::<i64>().unwrap());
                assert_eq!(native.demux.flags, fields[6].parse::<u32>().unwrap());
                let pos: usize = fields[4].parse().unwrap();
                let size: usize = fields[5].parse().unwrap();
                assert_eq!(packet.source.sample.range, pos as u64..(pos + size) as u64);
                assert_eq!(packet.source.payload.as_ref(), &bytes[pos..pos + size]);
                assert!(current(event));
            }
            _ => panic!("unexpected demux seek output"),
        }
    }
    assert!(wanted.next().is_none(), "short demux seek packet sequence");
}

fn next_range_output<S: Source>(player: &mut Mp4RangePlayer<'_, S>) -> Option<Output> {
    let mut slot = None;
    for _ in 0..100_000 {
        match player
            .read_output(&mut slot, &mut WorkBudget::new(1), &off())
            .unwrap()
        {
            Progress::Queued => return slot,
            Progress::Ended => return None,
            Progress::NeedInput(_) | Progress::Yield => {}
            progress => panic!("unexpected range progress {progress:?}"),
        }
    }
    panic!("MP4 range output stalled");
}

#[test]
fn bounded_timing_range_playback_preserves_packets_and_seeks() {
    use triergon_isobmff::table::{TimingRunPolicy, TimingRunSurplus};

    for (bytes, offset, surplus) in [
        (
            include_bytes!("../../../isobmff/tests/fixtures/bad-stts-count-mismatch.mov")
                .as_slice(),
            0,
            TimingRunSurplus {
                decode_samples: 1,
                composition_samples: 0,
            },
        ),
        (
            include_bytes!("../../../isobmff/tests/fixtures/bad-ctts-count-mismatch.mov")
                .as_slice(),
            1,
            TimingRunSurplus {
                decode_samples: 0,
                composition_samples: 6,
            },
        ),
    ] {
        assert!(matches!(
            open_with_timing_policy(bytes, TimingRunPolicy::Strict, true),
            Err(C::Invalid("time run exceeds sample count"))
        ));
        let (opened, calls, pool) =
            open_with_timing_policy(bytes, TimingRunPolicy::BoundedPrefix, true).unwrap();
        let [track] = opened.movie.tracks() else {
            panic!("expected one track");
        };
        assert_eq!(track.table().sample_count(), 6);
        assert_eq!(track.table().timing_run_surplus(), surplus);
        assert!(matches!(
            Mp4IndexJob::new(&opened.movie, PlanLimits::default()),
            Err(C::Invalid("source view requires exact timing coverage"))
        ));
        assert!(compat::mov::index(&opened.movie, PlanLimits::default()).is_ok());

        let mut job = Mp4RangeOpen::new(&opened.movie, None, PlanLimits::default()).unwrap();
        assert!((0..100_000).any(|_| job.poll(&mut WorkBudget::new(3), &off()).unwrap()));
        let mut player = job
            .finish(
                opened.input,
                opened.source,
                1 << 20,
                Selection::All,
                PlayerPolicy::default(),
            )
            .unwrap();
        let events: Vec<_> = std::iter::from_fn(|| next_range_output(&mut player)).collect();
        let packets = mp4_packets(&events);
        assert_eq!(packets.len(), 6);
        for (index, packet) in packets.into_iter().enumerate() {
            let dts = index as i64 * 1000;
            let native = &packet.occurrence.as_ref().unwrap().demux;
            assert_eq!((native.dts, native.pts), (Some(dts), Some(dts + offset)));
            let range = &packet.source.sample.range;
            assert_eq!(range.end - range.start, 12);
            assert_eq!(
                packet.source.payload.as_ref(),
                &bytes[range.start as usize..range.end as usize]
            );
        }

        calls.lock().unwrap().clear();
        let mut seek = player
            .prepare_seek(Mp4DemuxSeekRequest {
                video: track.header().track_id,
                timestamp: 3000,
                limits: triergon_isobmff::compat::mov::seek::SeekLimits::default(),
            })
            .unwrap();
        assert!((0..100_000).any(|_| seek.poll(&mut WorkBudget::new(3), &off()).unwrap()));
        let mut seek = seek.finish().unwrap();
        assert!(calls.lock().unwrap().is_empty());
        assert!(
            player
                .apply_seek(&mut seek, &mut WorkBudget::new(1), &off())
                .unwrap()
        );
        let resumed: Vec<_> = std::iter::from_fn(|| next_range_output(&mut player)).collect();
        let packets = mp4_packets(&resumed);
        assert_eq!(packets.len(), 3);
        assert_eq!(
            packets[0].occurrence.as_ref().unwrap().demux.dts,
            Some(3000)
        );
        assert!(
            calls
                .lock()
                .unwrap()
                .contains(&Call::Poll(seek.start_offset()))
        );
        drop(packets);
        drop((resumed, seek, events, player));
        drop(opened.movie);
        assert_eq!(pool.used(), Budget::control_storage_bytes());
    }
}

#[test]
fn range_playback_derives_duration_when_the_movie_header_is_unknown() {
    let seed = include_bytes!("../fixtures/mp4/r-front-stco.mov");
    let mut short = seed.to_vec();
    short[52..56].copy_from_slice(&u32::MAX.to_be_bytes());
    // The version-one fixture widens the same header and keeps all media bytes.
    let wide = include_bytes!("../fixtures/mp4/r-unknown-movie-duration.mov");
    let mut oversized = wide.to_vec();
    oversized[60..68].copy_from_slice(&(u64::MAX - 1).to_be_bytes());
    for (bytes, declared) in [
        (short.as_slice(), None),
        (wide.as_slice(), None),
        (oversized.as_slice(), Some(u64::MAX - 1)),
    ] {
        let (opened, _) = open(bytes, 97, 64).unwrap();
        assert_eq!(opened.movie.header().declared_duration_ticks(), declared);
        let mut job = Mp4RangeOpen::new(&opened.movie, None, PlanLimits::default()).unwrap();
        assert!((0..1000).any(|_| job.poll(&mut WorkBudget::new(16), &off()).unwrap()));
        let mut player = job
            .finish(
                opened.input,
                opened.source,
                1 << 20,
                Selection::All,
                PlayerPolicy::default(),
            )
            .unwrap();
        assert_eq!(player.duration_us(), 6_000_000);
        let outputs: Vec<_> = std::iter::from_fn(|| next_range_output(&mut player)).collect();
        let packets = mp4_packets(&outputs);
        assert_eq!(packets.len(), 6);
        for (ordinal, packet) in packets.iter().enumerate() {
            assert_eq!(
                packet.occurrence.as_ref().unwrap().demux.dts,
                Some(ordinal as i64 * 1000)
            );
        }
    }
}

#[test]
fn native_range_packet_reclaims_optional_seek_state_under_pool_pressure() {
    let bytes = include_bytes!("../fixtures/mp4/r-front-stco.mov");
    let (opened, _, pool) = observed_mp4_open(bytes);
    let mut job = Mp4RangeOpen::new(&opened.movie, None, PlanLimits::default()).unwrap();
    assert!((0..100_000).any(|_| job.poll(&mut WorkBudget::new(64), &off()).unwrap()));
    let mut player = job
        .finish(
            opened.input,
            opened.source,
            1 << 20,
            Selection::All,
            PlayerPolicy::default(),
        )
        .unwrap();
    let _held = pool.reserve(pool.available()).unwrap();
    let packet = std::iter::from_fn(|| next_range_output(&mut player))
        .find(|output| matches!(output.event(), Event::Mp4(Mp4Event::Packet(_))))
        .unwrap();
    assert!(player.current(&packet));
    assert!(!player.discard_seek_checkpoints());
}

#[test]
fn native_range_cursor_matches_ffprobe_and_preserves_seek_lifecycle() {
    for (bytes, target, selected, expected) in native_seek_cases() {
        let (opened, calls, pool) = observed_mp4_open(bytes);
        let mut job = Mp4RangeOpen::new(&opened.movie, None, PlanLimits::default()).unwrap();
        assert!((0..100_000).any(|_| job.poll(&mut WorkBudget::new(1), &off()).unwrap()));
        let mut player = job
            .finish(
                opened.input,
                opened.source,
                1 << 20,
                Selection::All,
                PlayerPolicy::default(),
            )
            .unwrap();
        let old = std::iter::from_fn(|| next_range_output(&mut player))
            .find(|event| event.event().track().is_some())
            .unwrap();
        player.pause(true).unwrap();
        calls.lock().unwrap().clear();
        let mut job = player
            .prepare_seek(Mp4DemuxSeekRequest {
                video: 1,
                timestamp: target,
                limits: triergon_isobmff::compat::mov::seek::SeekLimits::default(),
            })
            .unwrap();
        assert!(!job.poll(&mut WorkBudget::new(0), &off()).unwrap());
        assert!((0..100_000).any(|_| job.poll(&mut WorkBudget::new(1), &off()).unwrap()));
        let mut command = job.finish().unwrap();
        assert_eq!(command.receipt().selected.timestamp, selected);
        assert!(calls.lock().unwrap().is_empty());
        assert!(
            !player
                .apply_seek(&mut command, &mut WorkBudget::new(0), &off())
                .unwrap()
        );
        assert!(player.current(&old));
        assert!(
            player
                .apply_seek(&mut command, &mut WorkBudget::new(1), &off())
                .unwrap()
        );
        assert!(!player.current(&old));
        assert!(matches!(
            player.apply_seek(&mut command, &mut WorkBudget::new(1), &off()),
            Err(C::StaleGeneration { .. })
        ));
        assert!(matches!(
            player
                .read_output(&mut None, &mut WorkBudget::new(10), &off())
                .unwrap(),
            Progress::Paused
        ));
        player.pause(false).unwrap();
        let events: Vec<_> = std::iter::from_fn(|| next_range_output(&mut player)).collect();
        assert_native_seek_packets(bytes, expected, &events, |event| player.current(event));
        let first_read = calls.lock().unwrap().iter().find_map(|call| match call {
            Call::Poll(offset) => Some(*offset),
            _ => None,
        });
        assert_eq!(first_read, Some(command.start_offset()));
        assert!(command.start_offset() > 0);
        drop((command, events, old, player));
        drop(opened.movie);
        assert_eq!(pool.used(), Budget::control_storage_bytes());
    }
}

#[test]
fn repeated_range_seek_preserves_catalogue_order_with_video_second() {
    let mut raw = include_bytes!("../fixtures/mp4/r-multitrack.mov").to_vec();
    let tracks: Vec<_> = raw
        .windows(4)
        .enumerate()
        .filter(|(_, kind)| *kind == b"trak")
        .map(|(at, _)| at - 4)
        .collect();
    let [first, second] = tracks[..] else {
        panic!("expected two contiguous track boxes");
    };
    let size = |at| u32::from_be_bytes(raw[at..at + 4].try_into().unwrap()) as usize;
    let first_len = size(first);
    let end = second + size(second);
    assert_eq!(first + first_len, second);
    raw[first..end].rotate_left(first_len);

    let (opened, _, _) = observed_mp4_open(&raw);
    assert_eq!(opened.movie.tracks()[0].header().handler, *b"sbtl");
    assert_eq!(opened.movie.tracks()[1].header().track_id, 1);
    let mut open = Mp4RangeOpen::new(&opened.movie, None, PlanLimits::default()).unwrap();
    assert!((0..1000).any(|_| open.poll(&mut WorkBudget::new(4096), &off()).unwrap()));
    let mut player = open
        .finish(
            opened.input,
            opened.source,
            1 << 20,
            Selection::All,
            PlayerPolicy::default(),
        )
        .unwrap();
    for _ in 0..2 {
        let mut seek = player
            .prepare_seek(Mp4DemuxSeekRequest {
                video: 1,
                timestamp: 3500,
                limits: triergon_isobmff::compat::mov::seek::SeekLimits::default(),
            })
            .unwrap();
        assert!((0..1000).any(|_| seek.poll(&mut WorkBudget::new(4096), &off()).unwrap()));
        let mut command = seek.finish().unwrap();
        assert!(
            player
                .apply_seek(&mut command, &mut WorkBudget::new(1), &off())
                .unwrap()
        );
        let output = std::iter::from_fn(|| next_range_output(&mut player))
            .find(|item| item.event().track() == Some(TrackKey::Mp4(1)))
            .unwrap();
        let Event::Mp4(Mp4Event::Packet(packet)) = output.event() else {
            panic!("expected a video packet");
        };
        assert_eq!(packet.occurrence.as_ref().unwrap().demux.dts, Some(3000));
    }
}

#[test]
fn native_range_cursor_checks_all_extents_and_replays_after_duration_scan() {
    use triergon_core::{Rounding, TimeBase};

    let mut raw = include_bytes!("../fixtures/mp4/f-multitrack.mp4").to_vec();
    let at = raw.windows(4).position(|kind| kind == b"mvhd").unwrap() + 4;
    assert_eq!(raw[at], 0);
    raw[at + 16..at + 20].fill(0);
    let (mut indexed, _, _, _) = native_range_player(&raw);
    let expected = collect(&mut indexed, 4096).unwrap();
    let expected = mp4_packets(&expected);
    let (opened, _, _) = observed_mp4_open(&raw);
    assert_eq!(opened.movie.header().duration, 0);
    let mut limited = Mp4RangeOpen::new(
        &opened.movie,
        None,
        PlanLimits {
            samples: 1,
            ..PlanLimits::default()
        },
    )
    .unwrap();
    let refusal = (0..1000).find_map(|_| match limited.poll(&mut WorkBudget::new(4096), &off()) {
        Ok(false) => None,
        result => Some(result),
    });
    assert!(matches!(refusal, Some(Err(C::Limit { maximum: 1, .. }))));
    drop(limited);
    let mut job = Mp4RangeOpen::new(&opened.movie, None, PlanLimits::default()).unwrap();
    while !job.poll(&mut WorkBudget::new(4096), &off()).unwrap() {}
    let mut ranged = job
        .finish(
            opened.input,
            opened.source,
            1 << 20,
            Selection::All,
            PlayerPolicy::default(),
        )
        .unwrap();
    let end = expected
        .iter()
        .map(|packet| {
            let row = packet.occurrence.as_ref().unwrap();
            row.media_clock
                .demux_base
                .rescale(
                    row.demux.pts.unwrap() + row.demux.duration,
                    TimeBase::new(1, 1_000_000).unwrap(),
                    Rounding::NearestAway,
                )
                .unwrap()
        })
        .max()
        .unwrap();
    assert_eq!(ranged.duration_us(), end);
    let actual: Vec<_> = std::iter::from_fn(|| next_range_output(&mut ranged)).collect();
    let actual = mp4_packets(&actual);
    assert_eq!(actual.len(), expected.len());
    for (actual, expected) in actual.into_iter().zip(expected) {
        assert_eq!(actual.source.sample, expected.source.sample);
        assert_eq!(
            actual.occurrence.as_ref().unwrap().demux,
            expected.occurrence.as_ref().unwrap().demux
        );
        assert_eq!(
            actual.source.payload.as_ref(),
            expected.source.payload.as_ref()
        );
    }
}

#[test]
fn range_preflight_limits_occurrences_across_tracks() {
    let bytes = include_bytes!("../fixtures/mp4/encoded-avc-aac-native-seek.mp4");
    let (mut indexed, _, _, _) = native_range_player(bytes);
    let outputs = collect(&mut indexed, 4096).unwrap();
    let mut counts = std::collections::BTreeMap::new();
    for packet in mp4_packets(&outputs) {
        *counts
            .entry(packet.configuration.track.track_id)
            .or_insert(0) += 1;
    }
    assert!(counts.len() > 1);
    let maximum = *counts.values().max().unwrap();
    let (opened, calls, _) = observed_mp4_open(bytes);
    assert!(opened.movie.header().duration > 0);
    calls.lock().unwrap().clear();
    let mut job = Mp4RangeOpen::new(
        &opened.movie,
        None,
        PlanLimits {
            occurrences: maximum as u64,
            ..PlanLimits::default()
        },
    )
    .unwrap();
    let result = (0..100_000).find_map(|_| match job.poll(&mut WorkBudget::new(1), &off()) {
        Ok(false) => None,
        result => Some(result),
    });
    assert_eq!(
        result,
        Some(Err(C::Limit {
            requested: maximum + 1,
            maximum
        }))
    );
    assert!(calls.lock().unwrap().is_empty());
}

#[test]
fn native_range_open_refuses_late_unprojectable_metadata_before_playback() {
    let mut raw = include_bytes!("../fixtures/mp4/encoded-avc-aac-native-seek.mp4").to_vec();
    let at = raw.windows(4).position(|kind| kind == b"stss").unwrap();
    assert_eq!(&raw[at + 12..at + 16], &1u32.to_be_bytes());
    raw[at..at + 4].copy_from_slice(b"stps");
    raw[at + 12..at + 16].copy_from_slice(&2u32.to_be_bytes());
    let (opened, calls, _) = observed_mp4_open(&raw);
    assert!(opened.movie.header().duration > 0);
    calls.lock().unwrap().clear();
    let mut job = Mp4RangeOpen::new(&opened.movie, None, PlanLimits::default()).unwrap();
    let result = (0..1000).find_map(|_| match job.poll(&mut WorkBudget::new(4096), &off()) {
        Ok(false) => None,
        result => Some(result),
    });
    assert!(matches!(
        result,
        Some(Err(C::Unsupported("dependency or recovery flags")))
    ));
    assert!(calls.lock().unwrap().is_empty());
}

fn encoded_range_open(
    raw: &[u8],
) -> (
    mp4::OpenedMp4<ByteSource>,
    Vec<Option<triergon_isobmff::compat::mov::edits::EncodedEditPlan>>,
) {
    let (mut opened, _) = open(raw, 97, 4096).unwrap();
    let plans = encoded_plans(&mut opened);
    (opened, plans)
}

const RTP_HINTS: &[u8] = include_bytes!("../fixtures/mp4/avc-aac-rtp-hints.mp4");

fn rtp_hints_with_wide_clock() -> Vec<u8> {
    let mut bytes = RTP_HINTS.to_vec();
    let (at, _) = bytes
        .windows(4)
        .enumerate()
        .filter(|(_, word)| *word == b"mdhd")
        .nth(1)
        .unwrap();
    assert_eq!(bytes[at + 4], 0); // version 0
    bytes[at + 16..at + 20].copy_from_slice(&(1_u32 << 31).to_be_bytes());
    bytes
}

#[test]
fn range_media_projection_keeps_packets_and_both_seek_paths() {
    let (opened, plans) = encoded_range_open(RTP_HINTS);
    let kinds: Vec<_> = opened
        .movie
        .tracks()
        .iter()
        .map(|track| track.header().handler)
        .collect();
    assert_eq!(kinds, [*b"vide", *b"hint", *b"soun", *b"hint"]);
    assert!(plans[0].is_some() && plans[1].is_none() && plans[2].is_some() && plans[3].is_none());

    let mut complete =
        Mp4RangeOpen::new(&opened.movie, Some(&plans), PlanLimits::default()).unwrap();
    let refusal = (0..100_000).find_map(|_| match complete.poll(&mut WorkBudget::new(3), &off()) {
        Ok(false) => None,
        Ok(true) => panic!("expected the hint track edit to be refused"),
        Err(error) => Some(error),
    });
    assert_eq!(
        refusal,
        Some(C::Unsupported(
            "edit over reordered, zero-delta or recovery samples"
        ))
    );
    drop(complete);

    let mut projected =
        Mp4RangeOpen::for_media(&opened.movie, Some(&plans), PlanLimits::default()).unwrap();
    assert!((0..100_000).any(|_| projected.poll(&mut WorkBudget::new(3), &off()).unwrap()));
    let mut player = projected
        .finish(
            opened.input,
            opened.source,
            1 << 20,
            Selection::All,
            PlayerPolicy::default(),
        )
        .unwrap();
    let startup: Vec<_> = std::iter::from_fn(|| next_range_output(&mut player)).collect();
    let startup = mp4_packets(&startup);
    assert_eq!(
        startup
            .iter()
            .filter(|p| p.configuration.track.track_id == 1)
            .count(),
        100
    );
    assert_eq!(
        startup
            .iter()
            .filter(|p| p.configuration.track.track_id == 2)
            .count(),
        189
    );
    assert_eq!(startup.len(), 289);

    let seek = |player: &mut Mp4RangePlayer<'_, ByteSource>| {
        let mut job = player
            .prepare_seek(Mp4DemuxSeekRequest {
                video: 1,
                timestamp: 26_880,
                limits: triergon_isobmff::compat::mov::seek::SeekLimits::default(),
            })
            .unwrap();
        assert!((0..100_000).any(|_| job.poll(&mut WorkBudget::new(3), &off()).unwrap()));
        let mut command = job.finish().unwrap();
        assert_eq!(command.landing().selected.occurrence.track_id, 1);
        assert_eq!(command.landing().selected.timestamp, 25_600);
        assert!(
            player
                .apply_seek(&mut command, &mut WorkBudget::new(1), &off())
                .unwrap()
        );
        std::iter::from_fn(|| next_range_output(player)).collect::<Vec<_>>()
    };
    let require_source_track_limit = |player: &Mp4RangePlayer<'_, ByteSource>| {
        let mut limits = triergon_isobmff::compat::mov::seek::SeekLimits::default();
        limits.index.packets.max_tracks = 2;
        assert!(matches!(
            player.prepare_seek(Mp4DemuxSeekRequest {
                video: 1,
                timestamp: 26_880,
                limits,
            }),
            Err(C::Limit {
                requested: 4,
                maximum: 2,
            })
        ));
    };
    require_source_track_limit(&player);
    let with_checkpoints = seek(&mut player);
    require_source_track_limit(&player);
    assert!(player.discard_seek_checkpoints());
    require_source_track_limit(&player);
    let without_checkpoints = seek(&mut player);
    let with_checkpoints = mp4_packets(&with_checkpoints);
    let without_checkpoints = mp4_packets(&without_checkpoints);
    assert!(!with_checkpoints.is_empty());
    assert_eq!(with_checkpoints.len(), without_checkpoints.len());
    for (left, right) in with_checkpoints.into_iter().zip(without_checkpoints) {
        let left_row = left.occurrence.as_ref().unwrap();
        let right_row = right.occurrence.as_ref().unwrap();
        assert!(matches!(left_row.track_id, 1 | 2));
        assert_eq!(left_row.track_id, right_row.track_id);
        assert_eq!(left_row.demux, right_row.demux);
        assert_eq!(left_row.source, right_row.source);
        assert_eq!(left.encoded_skip_samples, right.encoded_skip_samples);
        assert_eq!(left.source.payload.as_ref(), right.source.payload.as_ref());
    }
}

#[test]
fn range_projection_does_not_admit_a_demux_clock_for_data_tracks() {
    let raw = rtp_hints_with_wide_clock();
    let (opened, plans) = encoded_range_open(&raw);
    assert_eq!(opened.movie.tracks()[1].header().handler, *b"hint");
    assert_eq!(opened.movie.tracks()[1].header().timescale, 1_u32 << 31);
    let mut range =
        Mp4RangeOpen::for_media(&opened.movie, Some(&plans), PlanLimits::default()).unwrap();
    assert!((0..100_000).any(|_| range.poll(&mut WorkBudget::new(3), &off()).unwrap()));
    let mut player = range
        .finish(
            opened.input,
            opened.source,
            1 << 20,
            Selection::All,
            PlayerPolicy::default(),
        )
        .unwrap();
    assert!(player.discard_seek_checkpoints());
    let mut seek = player
        .prepare_seek(Mp4DemuxSeekRequest {
            video: 1,
            timestamp: 26_880,
            limits: triergon_isobmff::compat::mov::seek::SeekLimits::default(),
        })
        .unwrap();
    assert!((0..100_000).any(|_| seek.poll(&mut WorkBudget::new(3), &off()).unwrap()));
    let mut command = seek.finish().unwrap();
    assert_eq!(command.landing().selected.occurrence.track_id, 1);
    assert_eq!(command.landing().selected.timestamp, 25_600);
    assert!(
        player
            .apply_seek(&mut command, &mut WorkBudget::new(1), &off())
            .unwrap()
    );
}

#[test]
fn range_media_projection_checks_excluded_sample_extents() {
    use triergon_isobmff::{Limits, Next};

    let (opened, _) = open(RTP_HINTS, 97, 4096).unwrap();
    let hint = &opened.movie.tracks()[1];
    assert_eq!(hint.header().handler, *b"hint");
    let mut children = hint.table().raw().children(0, Limits::default()).unwrap();
    let offset = loop {
        match children.poll(&mut WorkBudget::new(16)).unwrap() {
            Next::Item(view) if view.header().kind == *b"stco" => {
                break view.header().start as usize + usize::from(view.header().header_size) + 8;
            }
            Next::Item(_) | Next::Yield => {}
            Next::End => panic!("hint track has no chunk offsets"),
        }
    };
    let mut invalid = rtp_hints_with_wide_clock();
    invalid[offset..offset + 4].copy_from_slice(&u32::MAX.to_be_bytes());
    let (opened, plans) = encoded_range_open(&invalid);
    let mut projected =
        Mp4RangeOpen::for_media(&opened.movie, Some(&plans), PlanLimits::default()).unwrap();
    let refusal =
        (0..100_000).find_map(|_| match projected.poll(&mut WorkBudget::new(3), &off()) {
            Ok(false) => None,
            Ok(true) => panic!("an excluded sample exceeds the source extent"),
            Err(error) => Some(error),
        });
    assert_eq!(refusal, Some(C::Invalid("sample outside media extent")));
}

fn encoded_range_player(raw: &[u8]) -> (Player<ByteSource>, mp4::Movie) {
    let (opened, plans) = encoded_range_open(raw);
    let cancel = off();
    let mut index =
        compat::mov::index_encoded_range(&opened.movie, &plans, PlanLimits::default()).unwrap();
    while !index.poll(&mut WorkBudget::new(4096), &cancel).unwrap() {}
    let plan = index.finish().unwrap();
    drop(plans);
    let engine = Engine::mp4(opened.input, plan, 1 << 20).unwrap();
    let mut player = Player::new_direct(
        engine,
        opened.source,
        Selection::All,
        PlayerPolicy::default(),
    )
    .unwrap();
    player.set_clock_view(ClockChoice::Demux).unwrap();
    (player, opened.movie)
}

fn edited_visual_description_fixture(
    format: &[u8; 4],
    config_kind: &[u8; 4],
    records: [&[u8]; 2],
) -> Vec<u8> {
    // Derive in memory from the encoded-config 1, 2, 1 transition fixture.
    // Keep its AVC packets as opaque bytes and replace each avcC with an
    // authored record and free padding. Both media and movie clocks are 1 kHz.
    let mut raw =
        include_bytes!("../../../isobmff/tests/fixtures/encoded-config/different121.mp4").to_vec();
    for ((entry, config, end), record) in
        [(417, 503, 556), (592, 678, 707)].into_iter().zip(records)
    {
        assert_eq!(&raw[entry + 4..entry + 8], b"avc1");
        assert_eq!(&raw[config + 4..config + 8], b"avcC");
        raw[entry + 4..entry + 8].copy_from_slice(format);
        let record_end = config + 8 + record.len();
        raw[config..config + 4].copy_from_slice(&((8 + record.len()) as u32).to_be_bytes());
        raw[config + 4..config + 8].copy_from_slice(config_kind);
        raw[config + 8..record_end].copy_from_slice(record);
        raw[record_end..record_end + 4].copy_from_slice(&((end - record_end) as u32).to_be_bytes());
        raw[record_end + 4..record_end + 8].copy_from_slice(b"free");
        raw[record_end + 8..end].fill(0);
    }

    // Append a 36-byte edts to trak before mdat. The edit covers media [40, 120).
    let edit = [
        36u32.to_be_bytes(),
        *b"edts",
        28u32.to_be_bytes(),
        *b"elst",
        [0; 4],
        1u32.to_be_bytes(),
        80u32.to_be_bytes(),
        40u32.to_be_bytes(),
        [0, 1, 0, 0],
    ]
    .concat();
    let grow = edit.len() as u32;
    // Update moov/trak sizes and the three stco offsets.
    for at in [28, 144, 831, 835, 839] {
        let old = u32::from_be_bytes(raw[at..at + 4].try_into().unwrap());
        raw[at..at + 4].copy_from_slice(&(old + grow).to_be_bytes());
    }
    // mvhd and tkhd durations in movie ticks.
    for at in [60, 180] {
        raw[at..at + 4].copy_from_slice(&80u32.to_be_bytes());
    }
    raw.splice(871..871, edit);
    raw
}

#[test]
fn native_range_cursor_keeps_edited_av1_and_vp9_description_transitions() {
    use triergon_isobmff::{
        Limits, Next,
        compat::mov::{
            edits::{EncodedEditContext, EncodedEditJob},
            encoded::{
                EncodedConfigProfile, EncodedConfigSequence, EncodedContextProfile, EncodedInitial,
                EncodedSelection, EncodedStreamJob,
            },
        },
        movie::EntryLayout,
    };

    for (format, config_kind, records) in [
        (
            b"av01",
            b"av1C",
            [
                b"\x81\x00\x0c\x00".as_slice(),
                b"\x81\x01\x0c\x00".as_slice(),
            ],
        ),
        (
            b"vp09",
            b"vpcC",
            [
                b"\x01\x00\x00\x00\x00\x0a\x82\x01\x01\x01\x00\x00".as_slice(),
                b"\x01\x00\x00\x00\x00\x0b\x82\x01\x01\x01\x00\x00".as_slice(),
            ],
        ),
    ] {
        let raw = edited_visual_description_fixture(format, config_kind, records);
        let (mut opened, plans) = encoded_range_open(&raw);
        let [track] = opened.movie.tracks() else {
            panic!("expected one video track");
        };
        for description in [1, 2] {
            assert!(matches!(
                EncodedStreamJob::new_description_with_contexts(
                    track.header(),
                    track.table(),
                    &opened.input,
                    EncodedSelection {
                        major_brand: opened.movie.major_brand(),
                        description,
                        limits: triergon_isobmff::config::ConfigurationLimits::default(),
                    },
                    EncodedContextProfile,
                ),
                Err(C::Unsupported("encoded catalogue namespace/handler"))
            ));
        }
        let configs = range_configs(track, opened.movie.major_brand(), &mut opened.input);
        let table = track.table();
        let Next::Item(first) = table.cursor().poll(&mut WorkBudget::new(64)).unwrap() else {
            panic!("expected the first source sample");
        };
        let refusal = Some(C::Unsupported("encoded codec requires range playback"));
        assert_eq!(
            EncodedInitial::new(table, &configs, &first, &opened.input, EncodedConfigProfile,)
                .err(),
            refusal,
            "initial catalogue"
        );
        assert_eq!(
            EncodedConfigSequence::new(table, &configs, &opened.input, EncodedConfigProfile,).err(),
            refusal,
            "configuration sequence"
        );
        assert_eq!(
            EncodedEditJob::new_with_recovery_contexts(
                EncodedEditContext {
                    table,
                    edits: track.edits().unwrap(),
                    stream: &configs[0],
                    media_base: track.header().time_base().unwrap(),
                    movie_base: opened.movie.header().time_base().unwrap(),
                },
                &configs,
                &opened.input,
                triergon_isobmff::compat::mov::edits::EncodedEditLimits::default(),
                EncodedContextProfile,
            )
            .err(),
            refusal,
            "encoded edit job"
        );

        let mut job =
            Mp4RangeOpen::new(&opened.movie, Some(&plans), PlanLimits::default()).unwrap();
        assert!((0..1000).any(|_| job.poll(&mut WorkBudget::new(1), &off()).unwrap()));
        let mut player = job
            .finish(
                opened.input,
                opened.source,
                1 << 20,
                Selection::All,
                PlayerPolicy::default(),
            )
            .unwrap();
        assert_eq!(player.duration_us(), 80_000);
        let events: Vec<_> = std::iter::from_fn(|| next_range_output(&mut player)).collect();
        let events: Vec<_> = events
            .iter()
            .filter_map(|event| match event.event() {
                Event::Mp4(event) => Some(event),
                _ => None,
            })
            .collect();
        assert_eq!(events.len(), 6);
        for (pair, (description, revision, range, pts, flags)) in
            events.as_chunks::<2>().0.iter().zip([
                (1, 1, 915..3995, -40, 5),
                (2, 2, 3995..4395, 0, 1),
                (1, 3, 4395..7475, 40, 1),
            ])
        {
            let [Mp4Event::Configuration(config), Mp4Event::Packet(packet)] = pair else {
                panic!("configuration must precede its packet");
            };
            assert_eq!(config.description.format, *format);
            assert_eq!(config.description.index, description);
            assert_eq!(config.revision, revision);
            assert_eq!(packet.configuration.revision, revision);
            assert_eq!(packet.source.sample.description_index, description);
            assert!(matches!(
                config.track.demux_stream,
                Err(C::Unsupported("codec/carriage catalogue"))
            ));
            let mut children = config
                .description
                .extensions(EntryLayout::Visual, Limits::default())
                .unwrap();
            let Next::Item(config_box) = children.poll(&mut WorkBudget::new(64)).unwrap() else {
                panic!("missing visual configuration box");
            };
            assert_eq!(config_box.header().kind, *config_kind);
            assert_eq!(config_box.payload(), records[(description - 1) as usize]);
            assert_eq!(packet.source.sample.ordinal, revision);
            assert_eq!(
                packet.source.sample.range,
                range.start as u64..range.end as u64
            );
            assert_eq!(packet.source.payload.as_ref(), &raw[range]);
            assert_eq!(packet.source.sample.cts_ticks, Some(i128::from(pts + 40)));
            let clock = &packet.occurrence.as_ref().unwrap().demux;
            assert_eq!(
                (clock.pts, clock.dts, clock.duration, clock.flags),
                (Some(pts), Some(pts), 40, flags)
            );
            assert_eq!(packet.encoded_skip_samples, None);
        }
    }
}

#[test]
fn range_open_releases_seek_restarts_to_read_the_aac_trim() {
    let mut raw = include_bytes!("../fixtures/mp4/forward-fragment-aac-edit.mp4").to_vec();
    let program = include_bytes!("../fixtures/mp4/avc-aac-program.mov");
    let atom = |data: &[u8], kind: &[u8; 4]| {
        let at = data.windows(4).rposition(|bytes| bytes == kind).unwrap() - 4;
        let size = u32::from_be_bytes(data[at..at + 4].try_into().unwrap()) as usize;
        at..at + size
    };
    let esds = atom(&raw, b"esds");
    let replacement = atom(program, b"esds");
    let added = u32::try_from(replacement.len() - esds.len()).unwrap();
    // The fragment data offsets are relative to their moof. Growing the
    // front movie leaves the fragment payload declarations intact.
    for kind in [
        b"moov", b"trak", b"mdia", b"minf", b"stbl", b"stsd", b"mp4a",
    ] {
        let range = atom(&raw, kind);
        assert!(range.start < esds.start && range.end >= esds.end);
        let size = u32::try_from(range.len()).unwrap() + added;
        raw[range.start..range.start + 4].copy_from_slice(&size.to_be_bytes());
    }
    raw.splice(esds, program[replacement].iter().copied());
    let (opened, pool) = open(&raw, 97, 4096).unwrap();
    let cancel = off();
    let mut index = Mp4RangeOpen::new(&opened.movie, None, PlanLimits::default()).unwrap();
    // Record the allocation retained by the open plan and seek restarts.
    assert!((0..4096).any(|_| index.poll(&mut WorkBudget::new(1), &cancel).unwrap()));
    let retained = pool.used();
    drop(index);

    // Hold the rest of the pool once that storage is live, before the
    // temporary program configuration reserves its element vector.
    let mut index = Mp4RangeOpen::new(&opened.movie, None, PlanLimits::default()).unwrap();
    assert!((0..4096).any(|_| {
        assert!(!index.poll(&mut WorkBudget::new(1), &cancel).unwrap());
        pool.used() == retained
    }));
    let held = pool.reserve(pool.available()).unwrap();
    assert!((0..4096).any(|_| index.poll(&mut WorkBudget::new(1), &cancel).unwrap()));
    drop(held);
    assert!(
        pool.used() < retained,
        "the optional restarts should be released"
    );
}

#[test]
fn range_player_keeps_seek_checkpoints_for_unedited_fragments() {
    let raw = include_bytes!("../fixtures/mp4/avc-aac-quicktime-fragmented.mov");
    let (opened, _) = open(raw, 97, 4096).unwrap();
    assert!(opened.movie.tracks().iter().all(|track| {
        track.edits().is_none()
            && track.table().sample_count() == 0
            && !track.fragments().is_empty()
    }));
    let cancel = off();
    let mut job = Mp4RangeOpen::new(&opened.movie, None, PlanLimits::default()).unwrap();
    assert!((0..1000).any(|_| job.poll(&mut WorkBudget::new(64), &cancel).unwrap()));
    let mut player = job
        .finish(
            opened.input,
            opened.source,
            1 << 20,
            Selection::All,
            PlayerPolicy::default(),
        )
        .unwrap();
    assert!(player.discard_seek_checkpoints());
}

#[test]
fn range_player_keeps_the_active_seek_after_refusing_opus_recovery() {
    let seed =
        include_bytes!("../../../isobmff/tests/fixtures/range-encoded-edits/avc-opus-edit.mp4");
    let mut ended = seed.to_vec();
    for at in [1420, 1508] {
        ended[at..at + 4].copy_from_slice(&1500_u32.to_be_bytes());
    }
    let mut incomplete = seed.to_vec();
    assert_eq!(&incomplete[2902..2906], &157_u32.to_be_bytes());
    incomplete[2902..2906].copy_from_slice(&156_u32.to_be_bytes());

    let mut fragmented = include_bytes!("../fixtures/mp4/fragmented-avc-aac.mp4").to_vec();
    // The fragment packet bytes stay opaque. Use the regular Opus sample
    // description and pad it to the existing entry size.
    assert_eq!(&fragmented[978..982], b"mp4a");
    assert_eq!(&seed[1693..1697], b"Opus");
    fragmented[974..1049].copy_from_slice(&seed[1689..1764]);
    fragmented[974..978].copy_from_slice(&110_u32.to_be_bytes());
    fragmented[1049..1053].copy_from_slice(&35_u32.to_be_bytes());
    fragmented[1053..1057].copy_from_slice(b"free");
    fragmented[1057..1084].fill(0);

    for (raw, is_fragmented, timestamp) in [
        (ended.as_slice(), false, 32_000),
        (incomplete.as_slice(), false, 32_000),
        (fragmented.as_slice(), true, 12_800),
    ] {
        for checkpoints in [true, false] {
            let (opened, plans) = if is_fragmented {
                (open(raw, 97, 4096).unwrap().0, Vec::new())
            } else {
                encoded_range_open(raw)
            };
            let edits = (!is_fragmented).then_some(plans.as_slice());
            let tracks = opened.movie.tracks();
            let video = tracks[0].header().track_id;
            let audio = tracks[1].header().track_id;
            let mut job = Mp4RangeOpen::new(&opened.movie, edits, PlanLimits::default()).unwrap();
            let cancel = off();
            assert!((0..1000).any(|_| job.poll(&mut WorkBudget::new(64), &cancel).unwrap()));
            let mut player = job
                .finish(
                    opened.input,
                    opened.source,
                    1 << 20,
                    Selection::All,
                    PlayerPolicy::default(),
                )
                .unwrap();
            let first = next_range_output(&mut player).unwrap();
            if !checkpoints {
                assert!(player.discard_seek_checkpoints());
            }
            let request = Mp4DemuxSeekRequest {
                video,
                timestamp,
                limits: triergon_isobmff::compat::mov::seek::SeekLimits::default(),
            };
            let error = match player.prepare_seek_with_opus_recovery(request, audio) {
                Err(error) => error,
                Ok(mut job) => (0..1000)
                    .find_map(|_| match job.poll(&mut WorkBudget::new(64), &cancel) {
                        Err(error) => Some(error),
                        Ok(false) => None,
                        Ok(true) => panic!("invalid Opus recovery completed"),
                    })
                    .expect("Opus recovery refusal"),
            };
            assert!(matches!(error, C::Unsupported(_)), "{error:?}");
            assert!(player.current(&first));
            let mut ordinary = player.prepare_seek(request).unwrap();
            assert!((0..1000).any(|_| ordinary.poll(&mut WorkBudget::new(64), &cancel).unwrap()));
            let mut command = ordinary.finish().unwrap();
            assert!(
                player
                    .apply_seek(&mut command, &mut WorkBudget::new(4096), &cancel)
                    .unwrap()
            );
            assert!(!player.current(&first));
            assert!(next_range_output(&mut player).is_some());
        }
    }
}

#[test]
fn native_range_cursor_keeps_encoded_source_clocks_and_trim() {
    let original = include_bytes!("../../../isobmff/tests/fixtures/encoded-edit/original.mp4");
    let mut variable = original.to_vec();
    let audio_timing = variable
        .windows(4)
        .rposition(|kind| kind == b"stts")
        .unwrap()
        + 4;
    for (offset, before, after) in [(20, 1016u32, 1272u32), (36, 528, 272)] {
        let duration = &mut variable[audio_timing + offset..audio_timing + offset + 4];
        assert_eq!(duration, &before.to_be_bytes());
        duration.copy_from_slice(&after.to_be_bytes());
    }
    let fixtures: [&[u8]; 4] = [
        original,
        &variable,
        include_bytes!("../../../isobmff/tests/fixtures/range-encoded-edits/leading-ac3.mp4"),
        include_bytes!("../../../isobmff/tests/fixtures/range-encoded-edits/leading-eac3.mp4"),
    ];
    for raw in fixtures {
        let (mut indexed, _) = encoded_range_player(raw);
        let expected = collect(&mut indexed, 4096).unwrap();
        let (opened, plans) = encoded_range_open(raw);
        let mut job =
            Mp4RangeOpen::new(&opened.movie, Some(&plans), PlanLimits::default()).unwrap();
        while !job.poll(&mut WorkBudget::new(4096), &off()).unwrap() {}
        let mut streamed = job
            .finish(
                opened.input,
                opened.source,
                1 << 20,
                Selection::All,
                PlayerPolicy::default(),
            )
            .unwrap();
        let actual: Vec<_> = std::iter::from_fn(|| next_range_output(&mut streamed)).collect();
        let actual = mp4_packets(&actual);
        let expected = mp4_packets(&expected);
        assert_eq!(actual.len(), expected.len());
        for (a, e) in actual.into_iter().zip(expected) {
            assert_eq!(a.source.sample, e.source.sample);
            assert_eq!(a.source.payload.as_ref(), e.source.payload.as_ref());
            assert_eq!(a.encoded_skip_samples, e.encoded_skip_samples);
            assert_eq!(
                a.configuration.track.track_id,
                e.configuration.track.track_id
            );
            assert_eq!(
                a.configuration.description.index,
                e.configuration.description.index
            );
            let (a, e) = (
                a.occurrence.as_ref().unwrap(),
                e.occurrence.as_ref().unwrap(),
            );
            assert_eq!(a.demux, e.demux);
            assert_eq!(a.edit, e.edit);
            assert_eq!(a.clock_origin, e.clock_origin);
            assert_eq!(a.media_clock, e.media_clock);
            assert_eq!(a.scheduling_clock, e.scheduling_clock);
        }
    }
}

#[test]
fn native_range_player_carries_dolby_leading_trim() {
    let fixtures: [&[u8]; 2] = [
        include_bytes!("../../../isobmff/tests/fixtures/range-encoded-edits/leading-ac3.mp4"),
        include_bytes!("../../../isobmff/tests/fixtures/range-encoded-edits/leading-eac3.mp4"),
    ];
    for raw in fixtures {
        let (mut player, _) = encoded_range_player(raw);
        let events = collect(&mut player, 4096).unwrap();
        let packets: Vec<_> = events
            .iter()
            .filter_map(|event| match event.event() {
                Event::Mp4(Mp4Event::Packet(packet)) => Some(packet),
                _ => None,
            })
            .collect();
        assert_eq!(packets.len(), 6);
        assert_eq!(
            packets[0].encoded_skip_samples,
            Some([0, 1, 0, 0, 0, 0, 0, 0, 0, 0]),
        );
        assert_eq!(packets[0].source.payload.as_ref(), &raw[48..432]);
        assert!(
            packets[1..]
                .iter()
                .all(|p| p.encoded_skip_samples.is_none())
        );
    }
}

#[test]
fn native_range_seek_replays_the_encoded_edit_occurrences() {
    let raw = include_bytes!("../../../isobmff/tests/fixtures/encoded-edit/original.mp4");
    let (mut player, movie) = encoded_range_player(raw);
    let cancel = off();
    let leading = [0xf0, 3, 0, 0, 0, 0, 0, 0, 0, 0];
    let trailing = [0, 0, 0, 0, 0xf0, 1, 0, 0, 0, 0];
    let linear = collect(&mut player, 4096).unwrap();
    let audio_trim: Vec<_> = linear
        .iter()
        .filter_map(|output| match output.event() {
            Event::Mp4(Mp4Event::Packet(packet)) if packet.configuration.track.track_id == 2 => {
                packet.encoded_skip_samples
            }
            _ => None,
        })
        .collect();
    assert_eq!(audio_trim, [leading, trailing]);
    drop(linear);
    let command = {
        let mut job = player
            .prepare_mp4_demux_seek(
                &movie,
                Mp4DemuxSeekRequest {
                    video: 1,
                    timestamp: 43_200,
                    limits: triergon_isobmff::compat::mov::seek::SeekLimits::default(),
                },
                Selection::All,
            )
            .unwrap();
        while !job.poll(&mut WorkBudget::new(4096), &cancel).unwrap() {}
        job.finish().unwrap()
    };
    assert_eq!(command.receipt().selected.timestamp, 29_328);
    assert_eq!(command.start_offset(), 38_074);
    assert!(
        player
            .apply_mp4_demux_seek(&command, &mut WorkBudget::new(4096), &cancel)
            .unwrap()
    );
    let output = collect(&mut player, 4096).unwrap();
    let packets: Vec<_> = output
        .iter()
        .filter_map(|event| match event.event() {
            Event::Mp4(Mp4Event::Packet(packet)) => Some(packet),
            _ => None,
        })
        .collect();
    // First two public packets from the independent range seek oracle.
    for (packet, (id, pts, dts, start, length)) in packets.iter().zip([
        (2, 87_048, 87_048, 38_074, 133),
        (1, 32_000, 29_328, 38_207, 3220),
    ]) {
        let native = &packet.occurrence.as_ref().unwrap().demux;
        assert_eq!(packet.configuration.track.track_id, id);
        assert_eq!((native.pts, native.dts), (Some(pts), Some(dts)));
        assert_eq!(packet.source.payload.as_ref(), &raw[start..start + length]);
    }
    assert!(packets.len() > 2);
    assert_eq!(
        packets
            .iter()
            .filter_map(|packet| packet.encoded_skip_samples)
            .collect::<Vec<_>>(),
        [trailing],
    );
}

#[test]
fn native_range_seek_refusals_leave_the_current_player_untouched() {
    let (mut player, movie, calls, _) =
        native_range_player(include_bytes!("../fixtures/mp4/r-edits-repeat.mov"));
    let current = first_packet(&mut player);
    calls.lock().unwrap().clear();
    let request = Mp4DemuxSeekRequest {
        video: 1,
        timestamp: 3500,
        limits: triergon_isobmff::compat::mov::seek::SeekLimits::default(),
    };
    let mut job = player
        .prepare_mp4_demux_seek(&movie, request, Selection::All)
        .unwrap();
    assert!(matches!(
        job.poll(&mut WorkBudget::new(10_000), &off()),
        Err(C::Unsupported(
            "seek query between overlapping index clocks"
        ))
    ));
    drop(job);
    assert!(player.current(&current));
    assert!(calls.lock().unwrap().is_empty());
}

#[test]
fn direct_read_exposes_pending_input_after_one_transport_poll() {
    let raw = include_bytes!("../fixtures/mkv-feed/three-tracks.mkv");
    let b = pool();
    let (mut source, calls, _) = observed(&b, raw, 4096, false);
    source.pending = 3;
    let engine = Engine::matroska(
        triergon_matroska::Reader::new(
            &b,
            ID,
            SourceCapabilities {
                length: Some(raw.len() as u64),
                random_access: true,
            },
            triergon_matroska::Limits::default(),
            4096,
        )
        .unwrap(),
    );
    let mut player = Player::new(
        engine,
        SourceOwner::new(source),
        Selection::All,
        PlayerPolicy::default(),
    )
    .unwrap();
    let mut slot = None;
    let mut token = None;
    for count in 1..=3 {
        let Progress::NeedInput(request) = player
            .read_output(&mut slot, &mut WorkBudget::new(1 << 17), &off())
            .unwrap()
        else {
            panic!("transport has not completed this request")
        };
        if let Some(first) = &token {
            assert_eq!(&request.token, first);
        }
        token = Some(request.token);
        assert!(slot.is_none());
        assert_eq!(calls.lock().unwrap().len(), count);
    }
    assert!(matches!(
        player
            .read_output(&mut slot, &mut WorkBudget::new(1 << 17), &off())
            .unwrap(),
        Progress::Queued
    ));
    assert!(slot.as_ref().is_some_and(|output| player.current(output)));
}

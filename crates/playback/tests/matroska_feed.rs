use std::sync::atomic::AtomicBool;
use triergon_core::{
    Budget, Bytes, Error as CoreError, SourceCapabilities, SourceId, Time, WorkBudget,
};
use triergon_framing::{
    AacConfigLimits, Ac3Family, AvcAnnexBLimits, AvcConfigLimits, BuildStep, HvcConfigLimits,
};
use triergon_matroska::{
    incremental::ElementEvent,
    typed::{Configuration, Event},
};
use triergon_playback::{
    ByteSource, Delivery, Engine, Error, Output, Player, PlayerPolicy, Progress, RawEvent,
    Selection, TrackKey,
    io::SourceOwner,
    matroska_feed::{
        MatroskaDolbyLimits, MatroskaFeedLimits, MatroskaFeedPacket, MatroskaFeedPayload,
        MatroskaFeedSetup, MatroskaHevcDolbyFeed, MatroskaHevcDolbyLimits, MatroskaHevcDolbyPacket,
        MatroskaHevcDolbyPayload, MatroskaHevcDolbySetup, MatroskaTrackFeed,
    },
};

const ID: SourceId = SourceId(172);
const INPUT: &[u8] = include_bytes!("fixtures/mkv-feed/three-tracks.mkv");
const GOLD: &[u8] = include_bytes!("fixtures/mkv-feed/three-tracks.feed");
const LIMITS: MatroskaFeedLimits = MatroskaFeedLimits {
    avc_config: AvcConfigLimits {
        maximum_record_bytes: 8192,
        maximum_nals: 64,
        maximum_nal_bytes: 4096,
    },
    avc_packet: AvcAnnexBLimits {
        maximum_packet_bytes: 16384,
        maximum_output_bytes: 32768,
        maximum_nals: 64,
    },
    aac_config: AacConfigLimits {
        maximum_record_bytes: 128,
        maximum_pce_elements: 32,
        maximum_comment_bytes: 64,
    },
    aac_output_bytes: 8191,
};

fn player(quantum: usize) -> (Player<ByteSource>, Budget) {
    player_with_input(INPUT, quantum)
}

fn player_with_input(input: &[u8], quantum: usize) -> (Player<ByteSource>, Budget) {
    player_with_policy(input, quantum, PlayerPolicy::default())
}

fn player_with_policy(
    input: &[u8],
    quantum: usize,
    policy: PlayerPolicy,
) -> (Player<ByteSource>, Budget) {
    player_with_ownership(
        input,
        quantum,
        policy,
        triergon_matroska::typed::PayloadOwnership::OwnedExact,
    )
}

fn player_with_ownership(
    input: &[u8],
    quantum: usize,
    policy: PlayerPolicy,
    ownership: triergon_matroska::typed::PayloadOwnership,
) -> (Player<ByteSource>, Budget) {
    let (engine, source, pool) = matroska_source(input, quantum, ownership);
    (
        Player::new(engine, source, Selection::All, policy).unwrap(),
        pool,
    )
}

fn matroska_source(
    input: &[u8],
    quantum: usize,
    ownership: triergon_matroska::typed::PayloadOwnership,
) -> (Engine, SourceOwner<ByteSource>, Budget) {
    let pool = Budget::new(2 << 20).unwrap();
    let engine = Engine::matroska_with_payload_ownership(
        &pool,
        ID,
        SourceCapabilities {
            length: Some(input.len() as u64),
            random_access: true,
        },
        triergon_matroska::Limits::default(),
        4096,
        ownership,
    )
    .unwrap();
    let source =
        ByteSource::new(ID, Bytes::copy_from_slice(&pool, input).unwrap(), quantum).unwrap();
    (engine, SourceOwner::new(source), pool)
}

fn setup(pool: &Budget, configuration: &Configuration, track: usize) -> MatroskaTrackFeed {
    let mut setup = MatroskaFeedSetup::new(pool, ID, configuration, track, LIMITS).unwrap();
    let cancel = AtomicBool::new(false);
    for _ in 0..2048 {
        match setup.step(&mut WorkBudget::new(1), &cancel).unwrap() {
            BuildStep::Ready(feed) => {
                assert!(matches!(
                    setup.step(&mut WorkBudget::new(1), &cancel).unwrap(),
                    BuildStep::Complete
                ));
                return feed;
            }
            BuildStep::Yield => {}
            BuildStep::Complete => panic!("setup ended before readiness"),
        }
    }
    panic!("setup stalled")
}

fn frame<'a>(feed: &'a MatroskaTrackFeed, packet: &'a Output) -> MatroskaFeedPacket<'a> {
    let mut job = feed.packet(packet).unwrap().unwrap();
    let cancel = AtomicBool::new(false);
    assert!(matches!(
        job.step(&mut WorkBudget::new(0), &cancel).unwrap(),
        BuildStep::Yield
    ));
    for _ in 0..65 {
        match job.step(&mut WorkBudget::new(1), &cancel).unwrap() {
            BuildStep::Ready(output) => {
                assert!(matches!(
                    job.step(&mut WorkBudget::new(1), &cancel).unwrap(),
                    BuildStep::Complete
                ));
                return output;
            }
            BuildStep::Yield => {}
            BuildStep::Complete => panic!("packet ended before publication"),
        }
    }
    panic!("frame stalled")
}

fn collect(player: &mut Player<ByteSource>) -> Vec<Output> {
    collect_with_work(player, 17)
}

fn collect_with_work(player: &mut Player<ByteSource>, work_units: u64) -> Vec<Output> {
    let cancel = AtomicBool::new(false);
    let mut events = Vec::new();
    for _ in 0..100_000 {
        let status = player
            .advance(&mut WorkBudget::new(work_units), &cancel)
            .unwrap();
        while let Some(output) = player.pop() {
            assert!(player.current(&output));
            events.push(output);
        }
        if matches!(status, Progress::Ended) {
            return events;
        }
    }
    panic!("read stalled")
}

#[test]
fn matroska_avc_and_two_aac_tracks_match_complete_feed_packets_and_source_pts() {
    for quantum in [7, 4096] {
        let (mut player, pool) = player(quantum);
        let cancel = AtomicBool::new(false);
        player.pause(true).unwrap();
        assert!(matches!(
            player.advance(&mut WorkBudget::new(17), &cancel).unwrap(),
            Progress::Paused
        ));
        player.pause(false).unwrap();
        let events = collect(&mut player);
        let mut feeds = Vec::new();
        let mut gold = GOLD;
        let mut counts = [0; 3];
        let mut retained_subtick_clock = false;
        for event in &events {
            match event.event() {
                RawEvent::Matroska(Event::Configuration(configuration)) => {
                    feeds = (0..3).map(|i| setup(&pool, configuration, i)).collect();
                }
                RawEvent::Matroska(Event::Packet(packet)) => {
                    let track = packet.packet.track;
                    let output = frame(&feeds[track], event);
                    assert!(std::ptr::eq(output.source, packet));
                    counts[track] += 1;
                    let (header, rest) = gold.split_at(15);
                    let pts = i64::from_be_bytes(header[1..9].try_into().unwrap());
                    assert_eq!(usize::from(header[0]), track);
                    let timing = packet.packet.timing;
                    let reference_pts =
                        timing.ffmpeg9().unwrap().pts.unwrap().rounded_ns().unwrap();
                    assert_eq!(pts, reference_pts);
                    let exact = timing.source().unwrap().pts.unwrap().rounded_ns().unwrap();
                    assert_eq!(Some(exact), output.source.packet.pts_ns);
                    retained_subtick_clock |= exact != reference_pts;
                    assert_eq!(header[9] != 0, packet.packet.keyframe);
                    let idr =
                        matches!(&output.payload, MatroskaFeedPayload::Avc(unit) if unit.is_idr());
                    assert_eq!(header[10] != 0, idr);
                    let size = u32::from_be_bytes(header[11..15].try_into().unwrap()) as usize;
                    assert_eq!(output.payload.byte_len(), size);
                    let (mut expected, after) = rest.split_at(size);
                    for segment in output.payload.segments() {
                        let (want, tail) = expected.split_at(segment.len());
                        assert_eq!(segment, want);
                        expected = tail;
                    }
                    assert!(expected.is_empty());
                    if let MatroskaFeedPayload::Aac { raw, .. } = output.payload {
                        assert!(std::ptr::eq(raw, packet.packet.data.as_ref()));
                    }
                    gold = after;
                }
                _ => {}
            }
        }
        assert_eq!(counts.iter().sum::<usize>(), 101);
        assert!(counts.into_iter().all(|count| count > 0));
        assert!(gold.is_empty());
        assert!(retained_subtick_clock);
    }
}

#[test]
fn matroska_seek_and_audio_switch_fence_old_feed_bindings() {
    let (mut player, pool) = player(11);
    let initial = collect(&mut player);
    let config = initial
        .iter()
        .find_map(|event| match event.event() {
            RawEvent::Matroska(Event::Configuration(config)) => Some(config),
            _ => None,
        })
        .unwrap();
    let video = TrackKey::Matroska(config.tracks()[0].number);
    let audio = TrackKey::Matroska(config.tracks()[2].number);
    let old = setup(&pool, config, 0);
    let sample = initial
        .iter()
        .find(|event| event.event().track() == Some(video))
        .unwrap();
    let cancel = AtomicBool::new(false);
    assert!(
        player
            .seek_and_select(
                video,
                Time::new(0, 1).unwrap(),
                Selection::Tracks {
                    video: Some(video),
                    audio: Some(audio),
                    subtitle: None,
                },
                &mut WorkBudget::new(4096),
                &cancel
            )
            .unwrap()
    );
    assert!(!player.current(sample));
    let replay = collect(&mut player);
    let mut selected = [0; 3];
    for event in &replay {
        if let RawEvent::Matroska(Event::Packet(packet)) = event.event() {
            if event.delivery() == Delivery::Unselected {
                assert_eq!(packet.packet.track, 1);
                assert!(old.packet(event).unwrap().is_none());
                continue;
            }
            selected[packet.packet.track] += 1;
            if packet.packet.track == 0 {
                assert!(matches!(
                    old.packet(event),
                    Err(Error::Core(CoreError::Invalid(_)))
                ));
            }
            let fresh = setup(&pool, &packet.configuration, packet.packet.track);
            assert!(frame(&fresh, event).payload.byte_len() > 0);
        }
    }
    assert!(selected[0] > 0 && selected[2] > 0);
    assert_eq!(selected[1], 0);
}

const HEVC_DOLBY_INPUT: &[u8] = include_bytes!("fixtures/mkv-feed/hevc-dolby.mkv");
const HEVC_DOLBY_GOLD: &[u8] = include_bytes!("fixtures/mkv-feed/hevc-dolby.feed");
const HEVC_DOLBY_LIMITS: MatroskaHevcDolbyLimits = MatroskaHevcDolbyLimits {
    hevc_config: HvcConfigLimits {
        maximum_record_bytes: 8192,
        maximum_arrays: 16,
        maximum_nals: 64,
        maximum_nal_bytes: 4096,
    },
    hevc_packet: AvcAnnexBLimits {
        maximum_packet_bytes: 32768,
        maximum_output_bytes: 49152,
        maximum_nals: 64,
    },
    dolby_packet: MatroskaDolbyLimits {
        maximum_packet_bytes: 16384,
        maximum_syncframes: 8,
    },
};

fn hevc_dolby_setup(
    pool: &Budget,
    configuration: &Configuration,
    track: usize,
) -> MatroskaHevcDolbyFeed {
    let mut setup =
        MatroskaHevcDolbySetup::new(pool, ID, configuration, track, HEVC_DOLBY_LIMITS).unwrap();
    let cancel = AtomicBool::new(false);
    assert!(matches!(
        setup.step(&mut WorkBudget::new(0), &cancel).unwrap(),
        BuildStep::Yield
    ));
    for _ in 0..4096 {
        match setup.step(&mut WorkBudget::new(1), &cancel).unwrap() {
            BuildStep::Ready(feed) => {
                assert!(matches!(
                    setup.step(&mut WorkBudget::new(1), &cancel).unwrap(),
                    BuildStep::Complete
                ));
                return feed;
            }
            BuildStep::Yield => {}
            BuildStep::Complete => panic!("setup ended before readiness"),
        }
    }
    panic!("setup stalled")
}

fn hevc_dolby_frame<'a>(
    feed: &'a MatroskaHevcDolbyFeed,
    event: &'a Output,
) -> MatroskaHevcDolbyPacket<'a> {
    let mut job = feed.packet(event).unwrap().unwrap();
    let cancel = AtomicBool::new(false);
    assert!(matches!(
        job.step(&mut WorkBudget::new(0), &cancel).unwrap(),
        BuildStep::Yield
    ));
    for _ in 0..65 {
        match job.step(&mut WorkBudget::new(1), &cancel).unwrap() {
            BuildStep::Ready(packet) => {
                assert!(matches!(
                    job.step(&mut WorkBudget::new(1), &cancel).unwrap(),
                    BuildStep::Complete
                ));
                return packet;
            }
            BuildStep::Yield => {}
            BuildStep::Complete => panic!("packet ended before publication"),
        }
    }
    panic!("frame stalled")
}

#[test]
fn matroska_hevc_ac3_and_eac3_match_complete_feed_packets_and_source_pts() {
    for quantum in [7, 4096] {
        let (mut player, pool) = player_with_input(HEVC_DOLBY_INPUT, quantum);
        let events = collect(&mut player);
        let mut feeds = Vec::new();
        let mut gold = HEVC_DOLBY_GOLD;
        let mut counts = [0; 3];
        for event in &events {
            match event.event() {
                RawEvent::Matroska(Event::Configuration(configuration)) => {
                    feeds = (0..3)
                        .map(|track| hevc_dolby_setup(&pool, configuration, track))
                        .collect();
                }
                RawEvent::Matroska(Event::Packet(packet)) => {
                    let track = packet.packet.track;
                    let output = hevc_dolby_frame(&feeds[track], event);
                    assert!(std::ptr::eq(output.source, packet));
                    counts[track] += 1;

                    let (header, rest) = gold.split_at(15);
                    assert_eq!(usize::from(header[0]), track);
                    let pts = i64::from_be_bytes(header[1..9].try_into().unwrap());
                    let timing = packet.packet.timing;
                    let actual = timing.ffmpeg9().unwrap().pts.unwrap().rounded_ns().unwrap();
                    assert_eq!(pts, actual);
                    let exact = timing.source().unwrap().pts.unwrap().rounded_ns().unwrap();
                    assert_eq!(Some(exact), output.source.packet.pts_ns);
                    assert_eq!(header[9] != 0, packet.packet.keyframe);
                    let size = u32::from_be_bytes(header[11..15].try_into().unwrap()) as usize;
                    assert_eq!(output.payload.byte_len(), size);
                    match &output.payload {
                        MatroskaHevcDolbyPayload::Hevc(unit) => {
                            assert_eq!(track, 0);
                            assert_eq!(header[10] != 0, unit.is_random_access());
                        }
                        MatroskaHevcDolbyPayload::Dolby { family, raw } => {
                            assert_eq!(*family, [Ac3Family::Ac3, Ac3Family::Eac3][track - 1]);
                            assert!(std::ptr::eq(*raw, packet.packet.data.as_ref()));
                            assert_eq!(output.native_duration_ns().unwrap(), Some(32_000_000));
                        }
                    }
                    let (mut expected, after) = rest.split_at(size);
                    for segment in output.payload.segments() {
                        let (want, tail) = expected.split_at(segment.len());
                        assert_eq!(segment, want);
                        expected = tail;
                    }
                    assert!(expected.is_empty());
                    gold = after;
                }
                _ => {}
            }
        }
        assert_eq!(counts, [10, 21, 21]);
        assert!(gold.is_empty());
    }
}

#[test]
fn matroska_native_video_clocks_preserve_unknown_and_source_timing() {
    let avc = include_bytes!("fixtures/mkv-feed/avc-reordered-discovery.mkv");
    let hevc = include_bytes!("fixtures/mkv-feed/hevc-no-default-duration-discovery.mkv");
    let expected_dts = [None, Some(0), Some(42_000_000), Some(83_000_000)];

    let (mut player, pool) = player_with_input(avc, 7);
    let events = collect(&mut player);
    let configuration = events
        .iter()
        .find_map(|event| match event.event() {
            RawEvent::Matroska(Event::Configuration(configuration)) => Some(configuration),
            _ => None,
        })
        .unwrap();
    let feed = setup(&pool, configuration, 0);
    let foreign_feed = setup(&pool, configuration, 0);
    let mut clock = feed.native_avc_clock().unwrap().unwrap();
    let packets = events
        .iter()
        .filter(|event| event.event().track().is_some());
    for (expected, event) in expected_dts.into_iter().zip(packets) {
        let original = frame(&feed, event);
        assert_eq!(original.source.packet.dts_ns, None);
        assert!(clock.observe(&frame(&foreign_feed, event)).is_err());
        let native = clock.observe(&original).unwrap();
        assert_eq!(native.dts_ns, expected);
        assert_eq!(native.pts_ns, original.source.packet.pts_ns);
    }

    let (mut player, pool) = player_with_input(hevc, 7);
    let events = collect(&mut player);
    let configuration = events
        .iter()
        .find_map(|event| match event.event() {
            RawEvent::Matroska(Event::Configuration(configuration)) => Some(configuration),
            _ => None,
        })
        .unwrap();
    let feed = hevc_dolby_setup(&pool, configuration, 0);
    let mut clock = feed.native_hevc_clock().unwrap().unwrap();
    let packets = events
        .iter()
        .filter(|event| event.event().track().is_some());
    for (expected, event) in expected_dts.into_iter().zip(packets) {
        let original = hevc_dolby_frame(&feed, event);
        assert_eq!(original.source.packet.dts_ns, None);
        assert_eq!(original.source.packet.duration_ns, None);
        let native = clock.observe(&original).unwrap();
        assert_eq!(native.dts_ns, expected);
        assert_eq!(native.pts_ns, original.source.packet.pts_ns);
        assert_eq!(native.duration_ns, Some(41_000_000));
    }
}

#[test]
fn matroska_video_seek_gate_admits_the_cue_keyframe_and_preserves_preroll() {
    use triergon_playback::matroska_feed::MatroskaVideoSeekGate;

    let input = include_bytes!("fixtures/mkv-feed/hevc-indexed-seek.mkv");
    let (mut player, pool) = player_with_input(input, 7);
    let initial = collect_with_work(&mut player, 1);
    let video = TrackKey::Matroska(1);
    let target = Time::new(7, 10).unwrap();
    let cancel = AtomicBool::new(false);
    assert!(
        player
            .seek_and_select(
                video,
                target,
                Selection::All,
                &mut WorkBudget::new(1),
                &cancel
            )
            .unwrap()
    );
    let resumed = collect_with_work(&mut player, 1);
    let intent = resumed
        .iter()
        .find(|output| matches!(output.event(), RawEvent::Seek(_)))
        .unwrap();
    let receipt = resumed
        .iter()
        .find(|output| matches!(output.event(), RawEvent::Matroska(Event::Seek(_))))
        .unwrap();
    let mut gate = MatroskaVideoSeekGate::new(intent, receipt).unwrap();
    assert!(gate.admits(&initial[0]).is_err());

    let (mut foreign, foreign_pool) = player_with_input(input, 7);
    assert!(!pool.same_pool(&foreign_pool));
    collect_with_work(&mut foreign, 1);
    assert!(
        foreign
            .seek_and_select(
                video,
                target,
                Selection::All,
                &mut WorkBudget::new(1),
                &cancel
            )
            .unwrap()
    );
    let foreign_outputs = collect_with_work(&mut foreign, 1);
    let foreign_receipt = foreign_outputs
        .iter()
        .find(|output| matches!(output.event(), RawEvent::Matroska(Event::Seek(_))))
        .unwrap();
    assert_eq!(foreign_receipt.source_id(), receipt.source_id());
    assert_eq!(foreign_receipt.epoch(), receipt.epoch());
    assert_eq!(foreign_receipt.generation(), receipt.generation());
    assert!(MatroskaVideoSeekGate::new(intent, foreign_receipt).is_err());
    assert!(gate.admits(foreign_receipt).is_err());

    let mut skipped = 0;
    let admitted: Vec<_> = resumed
        .iter()
        .filter(|output| {
            let admit = gate.admits(output).unwrap();
            if !admit && output.event().track() == Some(video) {
                assert_eq!(output.delivery(), Delivery::DecodeOnly { until: target });
                skipped += 1;
            }
            admit
        })
        .collect();
    assert!(gate.landed());
    assert_eq!((skipped, admitted.len()), (4, 48));

    let RawEvent::Matroska(Event::Packet(landing)) = admitted[0].event() else {
        panic!("missing landing packet");
    };
    assert!(landing.packet.keyframe);
    assert_eq!(landing.packet.pts_ns, Some(667_000_000));
    assert_eq!(
        admitted[0].delivery(),
        Delivery::DecodeOnly { until: target }
    );

    let feed = hevc_dolby_setup(&pool, &landing.configuration, 0);
    let mut clock = feed.native_hevc_clock().unwrap().unwrap();
    let dts: Vec<_> = admitted
        .iter()
        .take(3)
        .map(|output| {
            let packet = hevc_dolby_frame(&feed, output);
            clock.observe(&packet).unwrap().dts_ns
        })
        .collect();
    assert_eq!(dts, [None, None, Some(667_000_000)]);
}

#[test]
fn matroska_hevc_seek_and_eac3_selection_reject_stale_configuration() {
    let (mut player, pool) = player_with_input(HEVC_DOLBY_INPUT, 23);
    let initial = collect(&mut player);
    let configuration = initial
        .iter()
        .find_map(|event| match event.event() {
            RawEvent::Matroska(Event::Configuration(configuration)) => Some(configuration),
            _ => None,
        })
        .unwrap();
    let video = TrackKey::Matroska(configuration.tracks()[0].number);
    let audio = TrackKey::Matroska(configuration.tracks()[2].number);
    let old_video = hevc_dolby_setup(&pool, configuration, 0);
    let old_audio = hevc_dolby_setup(&pool, configuration, 2);
    let sample = initial
        .iter()
        .find(|event| event.event().track() == Some(video))
        .unwrap();
    let cancel = AtomicBool::new(false);
    assert!(
        player
            .seek_and_select(
                video,
                Time::new(0, 1).unwrap(),
                Selection::Tracks {
                    video: Some(video),
                    audio: Some(audio),
                    subtitle: None,
                },
                &mut WorkBudget::new(4096),
                &cancel,
            )
            .unwrap()
    );
    assert!(!player.current(sample));

    let mut selected = [0; 3];
    let mut unselected = 0;
    let mut feeds = None;
    for event in &collect(&mut player) {
        let RawEvent::Matroska(Event::Packet(packet)) = event.event() else {
            continue;
        };
        let track = packet.packet.track;
        if event.delivery() == Delivery::Unselected {
            assert_eq!(track, 1);
            assert!(old_audio.packet(event).unwrap().is_none());
            unselected += 1;
            continue;
        }
        let (old, index) = match track {
            0 => (&old_video, 0),
            2 => (&old_audio, 1),
            _ => panic!("unexpected selected track"),
        };
        assert!(matches!(
            old.packet(event),
            Err(Error::Core(CoreError::Invalid(
                "Matroska feed configuration changed"
            )))
        ));
        let fresh = feeds.get_or_insert_with(|| {
            [0, 2].map(|i| hevc_dolby_setup(&pool, &packet.configuration, i))
        });
        assert!(hevc_dolby_frame(&fresh[index], event).payload.byte_len() > 0);
        selected[track] += 1;
    }
    assert!(selected[0] > 0 && selected[2] > 0 && unselected > 0);
}

fn event_key(output: &Output) -> (String, Vec<u8>) {
    let delivery = output.delivery();
    match output.event() {
        RawEvent::Matroska(Event::Envelope(ElementEvent::Leaf { header, data })) => {
            (format!("{delivery:?} leaf {header:?}"), data.to_vec())
        }
        RawEvent::Matroska(Event::Envelope(event)) => (format!("{delivery:?} {event:?}"), vec![]),
        RawEvent::Matroska(Event::Configuration(config)) => {
            let mut raw = Vec::new();
            for (i, track) in config.tracks().iter().enumerate() {
                raw.extend_from_slice(&track.number.to_le_bytes());
                raw.extend_from_slice(config.initialization(i).unwrap().data);
            }
            (format!("{delivery:?} config {}", config.revision()), raw)
        }
        RawEvent::Matroska(Event::Document(_)) => (format!("{delivery:?} document"), vec![]),
        RawEvent::Matroska(Event::Packet(packet)) => {
            let p = &packet.packet;
            (
                format!(
                    "{delivery:?} packet {} {:?} {} {}",
                    p.track, p.timing, p.keyframe, p.discard_padding_ns
                ),
                p.data.to_vec(),
            )
        }
        RawEvent::Matroska(Event::Seek(seek)) => (format!("{delivery:?} seek {seek:?}"), vec![]),
        other => panic!("unexpected Matroska fixture event {other:?}"),
    }
}

#[test]
fn ready_handoff_preserves_matroska_fifo_bytes_work_and_pressure() {
    let cancel = AtomicBool::new(false);
    let (mut baseline, baseline_pool) = player(4096);
    let outputs = collect(&mut baseline);
    let max_payload = outputs
        .iter()
        .map(|output| output.event().payload_bytes())
        .max()
        .unwrap();
    let expected: Vec<_> = outputs.iter().map(event_key).collect();
    drop(outputs);
    drop(baseline);
    assert_eq!(baseline_pool.used(), Budget::control_storage_bytes());

    for (quantum, work_units) in [(4096, 1 << 17), (7, 1)] {
        let (mut fast, pool) = player_with_policy(
            INPUT,
            quantum,
            PlayerPolicy {
                slots: 61,
                queued_payload_bytes: max_payload,
                ..PlayerPolicy::default()
            },
        );
        let mut output = Vec::new();
        let mut byte_pressure = false;
        let mut finished = false;
        for _ in 0..500_000 {
            let status = fast
                .advance_ready(&mut WorkBudget::new(work_units), &cancel)
                .unwrap();
            match status {
                Progress::Backpressure { requested, .. } => {
                    byte_pressure |= requested > 1;
                    let queued = fast.queued();
                    assert!(queued > 0);
                    assert!(matches!(
                        fast.advance_ready(&mut WorkBudget::new(0), &cancel)
                            .unwrap(),
                        Progress::Yield
                    ));
                    assert_eq!(fast.queued(), queued);
                }
                Progress::Ended => finished = true,
                _ => continue,
            }
            while let Some(event) = fast.pop() {
                assert!(fast.current(&event));
                output.push(event_key(&event));
            }
            if finished {
                break;
            }
        }
        assert!(finished);
        assert!(byte_pressure);
        assert_eq!(output, expected);
        drop(fast);
        assert_eq!(pool.used(), Budget::control_storage_bytes());
    }
}

#[test]
fn direct_read_preserves_the_queued_prefix_and_all_matroska_events() {
    let cancel = AtomicBool::new(false);
    let (mut baseline, _) = player(4096);
    let expected: Vec<_> = collect(&mut baseline).iter().map(event_key).collect();
    for work_units in [1, 1 << 17] {
        let (mut player, pool) = player(4096);
        for _ in 0..1000 {
            if player.queued() == 2 {
                break;
            }
            player.advance(&mut WorkBudget::new(17), &cancel).unwrap();
        }
        assert_eq!(player.queued(), 2);
        let mut slot = None;
        assert!(matches!(
            player
                .read_output(&mut slot, &mut WorkBudget::new(0), &cancel)
                .unwrap(),
            Progress::Yield
        ));
        assert!(slot.is_none());
        assert_eq!(player.queued(), 2);
        let mut outputs = Vec::new();
        for _ in 0..500_000 {
            let progress = player
                .read_output(&mut slot, &mut WorkBudget::new(work_units), &cancel)
                .unwrap();
            if let Some(output) = &slot {
                assert!(matches!(progress, Progress::Queued));
                assert!(player.current(output));
                let queue = player.queued();
                let mut work = WorkBudget::new(work_units);
                assert!(matches!(
                    player.read_output(&mut slot, &mut work, &cancel).unwrap(),
                    Progress::Backpressure { .. }
                ));
                assert_eq!(work.remaining(), work_units);
                assert_eq!(player.queued(), queue);
                outputs.push(event_key(&slot.take().unwrap()));
            } else if matches!(progress, Progress::Ended) {
                break;
            }
        }
        assert_eq!(player.state(), triergon_playback::State::Ended);
        assert_eq!(outputs, expected);
        drop(player);
        assert_eq!(pool.used(), Budget::control_storage_bytes());
    }
}

#[test]
fn shared_source_blocks_preserve_the_feed_and_keep_the_full_owner_charged() {
    let (mut exact, _) = player(4096);
    let expected: Vec<_> = collect(&mut exact).iter().map(event_key).collect();
    for quantum in [7, 4096] {
        let (mut shared, pool) = player_with_ownership(
            INPUT,
            quantum,
            PlayerPolicy::default(),
            triergon_matroska::typed::PayloadOwnership::SharedBounded,
        );
        let outputs = collect_with_work(&mut shared, 8192);
        assert_eq!(outputs.iter().map(event_key).collect::<Vec<_>>(), expected);
        let mut borrowed = 0;
        let mut copied = 0;
        for output in &outputs {
            if let RawEvent::Matroska(Event::Packet(packet)) = output.event() {
                let data = &packet.packet.data;
                if data.retained_capacity() == INPUT.len() {
                    borrowed += 1;
                } else {
                    assert!(data.retained_capacity() >= data.len());
                    copied += 1;
                }
            }
        }
        assert_eq!(borrowed + copied, 101);
        if quantum == 4096 {
            assert!(borrowed > 0);
        } else {
            assert!(copied > 0);
        }
        drop(shared);
        if borrowed > 0 {
            assert!(pool.used() >= INPUT.len());
        }
        drop(outputs);
        assert_eq!(pool.used(), Budget::control_storage_bytes());
    }
}

#[test]
fn direct_player_defers_queue_storage_and_retries_fifo_pressure() {
    let (mut eager, eager_pool) = player(4096);
    let (engine, source, pool) = matroska_source(
        INPUT,
        4096,
        triergon_matroska::typed::PayloadOwnership::OwnedExact,
    );
    let policy = PlayerPolicy::default();
    let mut direct = Player::new_direct(engine, source, Selection::All, policy).unwrap();
    let queue_charge = policy.slots * std::mem::size_of::<Option<Output>>();
    assert_eq!(eager_pool.used() - pool.used(), queue_charge);

    let cancel = AtomicBool::new(false);
    let blocker = pool.reserve(pool.available()).unwrap();
    let mut work = WorkBudget::new(8192);
    assert!(matches!(
        direct.advance_ready(&mut work, &cancel).unwrap(),
        Progress::Backpressure { requested, available: 0 } if requested == queue_charge
    ));
    assert_eq!(work.spent(), 0);
    assert!(direct.pending_request().is_none());
    drop(blocker);

    let expected = collect(&mut eager);
    let mut slot = None;
    for _ in 0..2048 {
        let _ = direct.read_output(&mut slot, &mut work, &cancel).unwrap();
        if slot.is_some() {
            break;
        }
    }
    assert_eq!(event_key(slot.as_ref().unwrap()), event_key(&expected[0]));
    drop(slot.take());
    assert_eq!(direct.queued(), 0);

    for _ in 0..2048 {
        let _ = direct
            .advance_ready(&mut WorkBudget::new(8192), &cancel)
            .unwrap();
        if direct.queued() != 0 {
            break;
        }
    }
    assert_eq!(direct.queued(), 1);
    assert!(matches!(
        direct
            .read_output(&mut slot, &mut WorkBudget::new(8192), &cancel)
            .unwrap(),
        Progress::Queued
    ));
    assert_eq!(event_key(slot.as_ref().unwrap()), event_key(&expected[1]));
    let rest = collect(&mut direct);
    assert_eq!(
        rest.iter().map(event_key).collect::<Vec<_>>(),
        expected[2..].iter().map(event_key).collect::<Vec<_>>()
    );
    drop((rest, slot, direct));
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}

#[test]
fn matroska_feed_setup_reuses_charged_initialization_under_pool_pressure() {
    for (input, tracks) in [(INPUT, &[0, 1][..]), (HEVC_DOLBY_INPUT, &[0][..])] {
        let (mut player, pool) = player_with_input(input, 4096);
        let outputs = collect(&mut player);
        let configuration = outputs
            .iter()
            .find_map(|output| match output.event() {
                RawEvent::Matroska(Event::Configuration(configuration)) => Some(configuration),
                _ => None,
            })
            .unwrap();
        let pressure = pool.reserve(pool.limit() - pool.used()).unwrap();
        for &track in tracks {
            assert!(!configuration.initialization(track).unwrap().data.is_empty());
            if input == INPUT {
                assert!(MatroskaFeedSetup::new(&pool, ID, configuration, track, LIMITS).is_ok());
            } else {
                assert!(
                    MatroskaHevcDolbySetup::new(&pool, ID, configuration, track, HEVC_DOLBY_LIMITS)
                        .is_ok()
                );
            }
        }
        drop(pressure);
    }
}

#[test]
fn compact_discovery_replays_every_output_and_reduces_prefix_storage() {
    use triergon_playback::matroska_feed::{
        MatroskaCompactDiscovery, MatroskaDiscovery, MatroskaDiscoveryLimits,
        MatroskaDiscoveryStep, MatroskaStartupSource,
    };

    let fixture = include_bytes!("fixtures/mkv-feed/hevc-no-default-duration-discovery.mkv");
    let cancel = AtomicBool::new(false);
    let make_source = || {
        matroska_source(
            fixture,
            7,
            triergon_matroska::typed::PayloadOwnership::OwnedExact,
        )
    };
    let tail = |player: &mut Player<MatroskaStartupSource<ByteSource>>| {
        let mut keys = Vec::new();
        let mut slot = None;
        for _ in 0..100_000 {
            let status = player
                .read_output(&mut slot, &mut WorkBudget::new(1), &cancel)
                .unwrap();
            if let Some(output) = slot.take() {
                assert!(player.current(&output));
                keys.push(event_key(&output));
            }
            if matches!(status, Progress::Ended) {
                return keys;
            }
        }
        panic!("continuation stalled")
    };

    let (engine, source, pool) = make_source();
    let mut full = MatroskaDiscovery::new(
        engine,
        source,
        Selection::All,
        PlayerPolicy::default(),
        MatroskaDiscoveryLimits::default(),
    )
    .unwrap();
    let full_cutoff = (0..100_000)
        .find_map(
            |_| match full.poll(&mut WorkBudget::new(1), &cancel).unwrap() {
                MatroskaDiscoveryStep::Ready(cutoff) => Some(cutoff),
                _ => None,
            },
        )
        .expect("full discovery stalled");
    let mut full = full.finish().unwrap();
    let full_used = pool.used();
    let expected_prefix: Vec<_> = full.prefix.as_slice().iter().map(event_key).collect();
    let expected_tail = tail(&mut full.player);

    let (engine, source, pool) = make_source();
    let mut compact = MatroskaCompactDiscovery::new(
        engine,
        source,
        Selection::All,
        PlayerPolicy::default(),
        MatroskaDiscoveryLimits::default(),
    )
    .unwrap();
    let compact_cutoff = (0..100_000)
        .find_map(
            |_| match compact.poll(&mut WorkBudget::new(1), &cancel).unwrap() {
                MatroskaDiscoveryStep::Ready(cutoff) => Some(cutoff),
                _ => None,
            },
        )
        .expect("compact discovery stalled");
    assert_eq!(compact_cutoff, full_cutoff);
    assert_eq!(compact.buffered_events().len(), expected_prefix.len());
    let mut compact = compact.finish().unwrap();
    assert!(pool.used() < full_used);
    let mut outputs = compact.prefix.into_outputs();
    for expected in expected_prefix {
        let output = outputs.next().unwrap();
        assert!(compact.player.current(&output));
        assert!(output.belongs_to(&pool));
        assert_eq!(event_key(&output), expected);
    }
    assert_eq!(outputs.len(), 0);
    assert_eq!(tail(&mut compact.player), expected_tail);
}

#[test]
fn av_discovery_hands_off_before_pgs_and_delivers_every_late_packet() {
    use triergon_playback::matroska_feed::{
        MatroskaCompactDiscovery, MatroskaDiscoveryCutoff, MatroskaDiscoveryStep,
    };

    let fixture = include_bytes!("fixtures/matroska-subtitle-probe/known-anchor.mkv");
    let (engine, source, _) = matroska_source(
        fixture,
        7,
        triergon_matroska::typed::PayloadOwnership::OwnedExact,
    );
    let cancel = AtomicBool::new(false);
    let mut discovery = MatroskaCompactDiscovery::for_av_feed(
        engine,
        source,
        Selection::All,
        PlayerPolicy::default(),
        Default::default(),
    )
    .unwrap();
    let mut confirmed = false;
    let cutoff = (0..10_000)
        .find_map(|_| {
            let progress = discovery.poll(&mut WorkBudget::new(1), &cancel).unwrap();
            if let MatroskaDiscoveryStep::Ready(cutoff) = progress {
                return Some(cutoff);
            }
            if !confirmed
                && let Some(Event::Packet(packet)) = discovery.buffered_events().last()
                && packet.packet.track == 0
            {
                discovery
                    .confirm_stream_ready(1, packet.configuration.revision())
                    .unwrap();
                confirmed = true;
            }
            None
        })
        .expect("A/V discovery stalled");
    assert!(confirmed);
    assert_eq!(cutoff, MatroskaDiscoveryCutoff::StreamsReady);
    assert!(
        discovery
            .buffered_events()
            .all(|event| { !matches!(event, Event::Packet(packet) if packet.packet.track == 1) })
    );
    let mut discovered = discovery.finish().unwrap();
    let mut keys: Vec<_> = discovered
        .prefix
        .into_outputs()
        .map(|o| event_key(&o))
        .collect();
    let mut output = None;
    let mut ended = false;
    for _ in 0..10_000 {
        let status = discovered
            .player
            .read_output(&mut output, &mut WorkBudget::new(1), &cancel)
            .unwrap();
        if let Some(output) = output.take() {
            keys.push(event_key(&output));
        }
        if matches!(status, Progress::Ended) {
            ended = true;
            break;
        }
    }
    assert!(ended);
    let (mut reference, _) = player_with_input(fixture, 7);
    let reference = collect(&mut reference);
    assert_eq!(reference.iter().filter(|output| {
        matches!(output.event(), RawEvent::Matroska(Event::Packet(packet)) if packet.packet.track == 1)
    }).count(), 5);
    assert_eq!(keys, reference.iter().map(event_key).collect::<Vec<_>>());
}

#[test]
fn matroska_integral_packet_delivery_preserves_fractional_seek_targets() {
    let fixture = include_bytes!("fixtures/mkv-feed/hevc-indexed-seek.mkv");
    let (mut player, _) = player_with_input(fixture, 7);
    let cancel = AtomicBool::new(false);
    collect_with_work(&mut player, 1);

    for target in [
        Time::new(0, 1).unwrap(),
        Time::new(1, 2_000_000_000).unwrap(),
        Time::new(1_334_000_001, 2_000_000_000).unwrap(),
    ] {
        assert!(
            player
                .seek_and_select(
                    TrackKey::Matroska(1),
                    target,
                    Selection::All,
                    &mut WorkBudget::new(1),
                    &cancel,
                )
                .unwrap()
        );
        let mut packets = 0;
        for output in collect_with_work(&mut player, 1) {
            if let RawEvent::Matroska(Event::Packet(packet)) = output.event() {
                let exact = match packet.source_pts().unwrap() {
                    None => Delivery::UnknownTime,
                    Some(pts) if pts < target => Delivery::DecodeOnly { until: target },
                    Some(_) => Delivery::Present,
                };
                assert_eq!(output.delivery(), exact);
                packets += 1;
            }
        }
        assert!(packets > 0);
    }
}

#[test]
fn matroska_dolby_clock_matches_pinned_startup_and_forward_lace_timestamps() {
    use triergon_playback::matroska_feed::MatroskaDolbyTiming;

    let cases: &[(&[u8], usize, u64, &[i64])] = &[
        (
            include_bytes!("fixtures/dolby-clock/laced-unknown-duration.mkv"),
            1,
            32_000_000,
            &[0, 32, 96, 128],
        ),
        (
            include_bytes!("fixtures/dolby-clock/initial-negative-cluster-relative.mkv"),
            2,
            32_000_000,
            &[-32, 0, 64],
        ),
        (
            include_bytes!("fixtures/dolby-clock/two-laced-before-first-known-pts.mkv"),
            3,
            32_000_000,
            &[32, 64, 96, 128],
        ),
        (
            include_bytes!("fixtures/dolby-clock/no-absolute-anchor.mkv"),
            2,
            32_000_000,
            &[0, 32],
        ),
        (
            include_bytes!("fixtures/dolby-clock/damaged-lace-before-valid-anchor.mkv"),
            3,
            32_000_000,
            &[32, 64, 96, 128],
        ),
        (
            include_bytes!("fixtures/dolby-clock/laced-fractional-cadence.mkv"),
            3,
            34_000_000,
            &[52, 86, 120, 154, 188, 222, 256, 290],
        ),
    ];
    for &(source, count, duration_ns, expected) in cases {
        let (mut player, pool) = player_with_input(source, 7);
        let outputs = collect_with_work(&mut player, 1);
        let mut packets = outputs
            .iter()
            .filter(|output| matches!(output.event(), RawEvent::Matroska(Event::Packet(_))));
        let configuration = outputs
            .iter()
            .find_map(|output| match output.event() {
                RawEvent::Matroska(Event::Configuration(configuration)) => Some(configuration),
                _ => None,
            })
            .unwrap();
        let feed = hevc_dolby_setup(&pool, configuration, 0);
        let mut probe = feed
            .native_dolby_clock_probe(&pool, count)
            .unwrap()
            .unwrap();
        for (ordinal, output) in packets.by_ref().take(count).enumerate() {
            let packet = hevc_dolby_frame(&feed, output);
            assert_eq!(probe.push(&packet).unwrap(), ordinal);
            assert_eq!(packet.source.packet.dts_ns, None);
            assert_eq!(&packet.source.configuration.tracks()[0].codec, "A_EAC3");
        }
        let prefix = probe.finish();
        let mut timings = (0..count)
            .map(|at| prefix.timing(at).unwrap())
            .collect::<Vec<_>>();
        assert!(prefix.timing(count).is_err());
        let mut live = prefix.into_live();
        timings.extend(packets.map(|output| {
            let packet = hevc_dolby_frame(&feed, output);
            live.observe(&packet).unwrap()
        }));
        let expected: Vec<_> = expected
            .iter()
            .map(|time| MatroskaDolbyTiming {
                pts_ns: Some(time * 1_000_000),
                dts_ns: Some(time * 1_000_000),
                duration_ns: Some(duration_ns),
            })
            .collect();
        assert_eq!(timings, expected);
    }
}

#[test]
fn matroska_aac_clock_matches_pinned_mixed_rates_from_probe_through_eof() {
    use triergon_playback::matroska_feed::MatroskaAacTiming;

    let fixture = include_bytes!("fixtures/mkv-feed/audio-mixed-discovery.mkv");
    let (mut player, pool) = player_with_input(fixture, 7);
    let outputs = collect_with_work(&mut player, 1);
    let configuration = outputs
        .iter()
        .find_map(|output| match output.event() {
            RawEvent::Matroska(Event::Configuration(configuration)) => Some(configuration),
            _ => None,
        })
        .unwrap();
    let cases: &[(usize, u64, &[i64])] = &[
        (0, 21_000_000, &[-21, 0, 21, 43, 64, 85, 107]),
        (1, 23_000_000, &[-23, 0, 23, 46, 70, 93]),
    ];
    for &(track, duration_ns, expected) in cases {
        let feed = setup(&pool, configuration, track);
        assert!(feed.native_aac_clock_probe(&pool, 4097).is_err());
        let foreign = Budget::new(1 << 20).unwrap();
        assert!(feed.native_aac_clock_probe(&foreign, 2).is_err());
        let mut packets = outputs.iter().filter(|output| {
            matches!(output.event(), RawEvent::Matroska(Event::Packet(packet))
                if packet.packet.track == track)
        });
        let mut probe = feed.native_aac_clock_probe(&pool, 2).unwrap().unwrap();
        for (ordinal, output) in packets.by_ref().take(2).enumerate() {
            let packet = frame(&feed, output);
            assert_eq!(packet.source.packet.dts_ns, None);
            assert_eq!(probe.push(&packet).unwrap(), ordinal);
        }
        let next = frame(&feed, packets.clone().next().unwrap());
        assert!(probe.push(&next).is_err());
        let prefix = probe.finish();
        let mut timings = (0..2)
            .map(|ordinal| prefix.timing(ordinal).unwrap())
            .collect::<Vec<_>>();
        assert!(prefix.timing(2).is_err());
        let mut live = prefix.into_live();
        timings.extend(packets.map(|output| live.observe(&frame(&feed, output)).unwrap()));
        let expected: Vec<_> = expected
            .iter()
            .map(|pts| MatroskaAacTiming {
                pts_ns: Some(pts * 1_000_000),
                dts_ns: Some(pts * 1_000_000),
                duration_ns: Some(duration_ns),
            })
            .collect();
        assert_eq!(timings, expected);
    }
}

#[test]
fn matroska_dolby_source_clock_preserves_damaged_ac3_before_decoder_admission() {
    use triergon_playback::matroska_feed::MatroskaDolbyTiming;

    let fixture = include_bytes!("fixtures/mkv-feed/audio-bad-first-discovery.mkv");
    let (mut player, pool) = player_with_input(fixture, 7);
    let outputs = collect_with_work(&mut player, 1);
    let configuration = outputs
        .iter()
        .find_map(|output| match output.event() {
            RawEvent::Matroska(Event::Configuration(c)) => Some(c),
            _ => None,
        })
        .unwrap();
    assert_eq!(configuration.tracks()[0].codec, "A_AC3");
    let feed = hevc_dolby_setup(&pool, configuration, 0);
    let mut packets = outputs.iter().filter_map(|output| match output.event() {
        RawEvent::Matroska(Event::Packet(packet)) if packet.packet.track == 0 => {
            Some((output, packet))
        }
        _ => None,
    });
    let (first, source) = packets.next().unwrap();
    let raw = source.packet.data.as_ref();
    assert_eq!(raw[0], 0);
    let mut frame = feed.packet(first).unwrap().unwrap();
    let cancel = AtomicBool::new(false);
    assert!(frame.step(&mut WorkBudget::new(4096), &cancel).is_err());

    let mut probe = feed.native_dolby_clock_probe(&pool, 1).unwrap().unwrap();
    assert_eq!(probe.push_source(source).unwrap(), 0);
    let prefix = probe.finish();
    let expected = |pts_ns| MatroskaDolbyTiming {
        pts_ns: Some(pts_ns),
        dts_ns: Some(pts_ns),
        duration_ns: Some(32_000_000),
    };
    assert_eq!(prefix.timing(0).unwrap(), expected(-5_000_000));
    assert_eq!(source.packet.data.as_ref(), raw);
    assert_eq!(source.packet.dts_ns, None);
    let mut live = prefix.into_live();
    for ((_, packet), pts_ns) in packets.zip([27_000_000, 59_000_000, 91_000_000]) {
        assert_eq!(live.observe_source(packet).unwrap(), expected(pts_ns));
    }
}

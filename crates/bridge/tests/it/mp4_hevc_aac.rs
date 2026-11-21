use crate::framing::*;
use crate::support::sequential::*;
use crate::support::*;
use triergon_bridge::aac as aac_bridge;
use triergon_core::{Budget, Bytes, Error as C, InputCursor, SourceCapabilities, Time, WorkBudget};
use triergon_playback::{ByteSource, mp4::Mp4Packet};
use triergon_playback::{
    SourceOwner,
    mp4::{Mp4IndexJob, Mp4Open, OpenLimits, OpenProgress, PlanLimits, ProgressiveLimits},
    probe::SelectedInput,
    *,
};

#[test]
fn real_hevc_mp4_packets_frame_as_annex_b_from_their_source_bytes() {
    let (mut p, b, _, _) = player(
        include_bytes!("../../../playback/tests/fixtures/mp4/encoded-hevc-aac-front.mp4"),
        false,
        ProgressiveLimits::default(),
    );
    let out = collect(&mut p, 1).unwrap();
    let packets: Vec<_> = packets(&out)
        .into_iter()
        .filter(|p| matches!(&p.configuration.description.format, b"hvc1" | b"hev1"))
        .collect();
    assert!(!packets.is_empty());
    let mut session = annex_b_session(&b, packets[0]);
    for packet in packets {
        let result = annex_b(&mut session, packet);
        assert_eq!(
            result.source().bytes.as_ref(),
            packet.source.payload.as_ref()
        );
        assert_eq!(result.source().generation, packet.source.token.generation());
        assert!(
            result.bytes().starts_with(&[0, 0, 0, 1]) || result.bytes().starts_with(&[0, 0, 1])
        );
    }
}

fn aac_fixture_player() -> (Player<ByteSource>, Budget, Bytes, u64) {
    use triergon_isobmff::config::ConfigurationLimits;
    use triergon_isobmff::{CursorStatus, compat::mov::encoded::*};
    let raw = include_bytes!("../../../playback/tests/fixtures/mp4/encoded-avc-aac-front.mp4");
    let b = pool();
    let input = InputCursor::new(
        &b,
        ID,
        SourceCapabilities {
            length: Some(raw.len() as u64),
            random_access: true,
        },
        4096,
    )
    .unwrap();
    let source = ByteSource::new(ID, Bytes::copy_from_slice(&b, raw).unwrap(), 64).unwrap();
    let mut open = Mp4Open::new(
        SelectedInput {
            input,
            source: SourceOwner::new(source),
            format: Format::Mp4,
        },
        OpenLimits::default(),
    )
    .unwrap();
    while !matches!(
        open.advance(&mut WorkBudget::new(1), &off()).unwrap(),
        OpenProgress::Ready
    ) {}
    let mut opened = open.finish().unwrap();
    let track = opened
        .movie
        .tracks()
        .iter()
        .find(|t| t.header().handler == *b"soun")
        .unwrap();
    let mut job = EncodedStreamJob::new(
        track.header(),
        track.table(),
        &opened.input,
        Some(raw[8..12].try_into().unwrap()),
        ConfigurationLimits::default(),
        EncodedStreamProfile,
    )
    .unwrap();
    while job
        .poll(&mut opened.input, &mut WorkBudget::new(1), &off())
        .unwrap()
        != CursorStatus::Ready
    {}
    let catalogue = job.take(&mut opened.input, &off()).unwrap();
    let asc = catalogue.demux().extradata.clone();
    let config = catalogue.configuration_box();
    assert!(asc.shares_storage_with(config.bytes()));
    let relative = (asc.as_ref().as_ptr() as usize)
        .checked_sub(config.bytes().as_ref().as_ptr() as usize)
        .unwrap();
    assert!(relative + asc.len() <= config.bytes().len());
    let offset = config.header().start + relative as u64;
    let mut index = Mp4IndexJob::new(&opened.movie, PlanLimits::default()).unwrap();
    while !index.poll(&mut WorkBudget::new(1), &off()).unwrap() {}
    let plan = index.finish().unwrap();
    let player = Player::new(
        Engine::mp4(opened.input, plan, 1 << 20).unwrap(),
        opened.source,
        Selection::All,
        PlayerPolicy::default(),
    )
    .unwrap();
    (player, b, asc, offset)
}

fn aac_packets() -> (Vec<Mp4Packet>, Budget, Bytes, u64) {
    let (mut player, b, asc, offset) = aac_fixture_player();
    let out = collect(&mut player, 1).unwrap();
    (
        packets(&out)
            .into_iter()
            .filter(|p| p.configuration.description.format == *b"mp4a")
            .cloned()
            .collect(),
        b,
        asc,
        offset,
    )
}

#[test]
fn real_aac_mp4_adts_bridge_uses_source_asc_and_keeps_whole_raw_packet() {
    use triergon_framing::{
        BuildStep,
        aac::{AacAscParser, AacConfigLimits, AacSource},
    };
    let (packets, b, asc, offset) = aac_packets();
    assert!(!packets.is_empty());
    let cfg = packets[0].configuration.clone();
    let mut parser = AacAscParser::new(
        &b,
        AacSource {
            source: ID,
            generation: cfg.revision,
            offset,
        },
        asc,
        AacConfigLimits {
            maximum_record_bytes: 1 << 20,
            maximum_pce_elements: 256,
            maximum_comment_bytes: 255,
        },
    )
    .unwrap();
    let record = loop {
        if let BuildStep::Ready(x) = parser.step(&mut WorkBudget::new(1), &off()).unwrap() {
            break x;
        }
    };
    let core = record.facts().core_frequency.hz;
    let bridge =
        aac_bridge::AacBridge::new(&b, ID, cfg.track.track_id, cfg.revision, record, 8191).unwrap();
    for packet in packets {
        let sample = packet.source.sample.clone();
        let bytes = packet.source.payload.clone();
        let mut job = bridge.packet(packet).unwrap();
        let before = b.used();
        assert!(matches!(
            job.step(&mut WorkBudget::new(0), &off()).unwrap(),
            BuildStep::Yield
        ));
        assert_eq!(b.used(), before);
        let o = loop {
            if let BuildStep::Ready(x) = job.step(&mut WorkBudget::new(1), &off()).unwrap() {
                break x;
            }
        };
        assert_eq!(o.source.source.sample, sample);
        assert_eq!(o.source.source.payload.as_ref(), bytes.as_ref());
        assert_eq!(o.adts.payload_bytes().unwrap().as_ref(), bytes.as_ref());
        assert_eq!(o.adts.constructed_header().coded_frequency_hz(), Some(core));
        assert_eq!(o.adts.payload_source().offset, sample.range.start);
    }
}

#[test]
fn aac_bridge_refuses_configuration_change_before_packet_conversion() {
    use triergon_framing::{
        BuildStep,
        aac::{AacAscParser, AacConfigLimits, AacSource},
    };
    let (mut packets, b, asc, offset) = aac_packets();
    let mut p = packets.remove(0);
    let cfg = p.configuration.clone();
    let mut parser = AacAscParser::new(
        &b,
        AacSource {
            source: ID,
            generation: cfg.revision,
            offset,
        },
        asc,
        AacConfigLimits {
            maximum_record_bytes: 1 << 20,
            maximum_pce_elements: 256,
            maximum_comment_bytes: 255,
        },
    )
    .unwrap();
    let record = loop {
        if let BuildStep::Ready(x) = parser.step(&mut WorkBudget::new(1), &off()).unwrap() {
            break x;
        }
    };
    let bridge =
        aac_bridge::AacBridge::new(&b, ID, cfg.track.track_id, cfg.revision, record, 8191).unwrap();
    p.configuration.revision += 1;
    assert!(matches!(bridge.packet(p), Err(C::Invalid(_))));
}

#[test]
fn mp4_playback_feeds_real_simulator_decoders_across_clock_and_seek() {
    use triergon_core::Rounding;
    use triergon_framing::aac::{AacAscParser, AacConfigLimits, AacSource};
    use triergon_session::{
        AudioCodec, AudioSession, PcmBlock, QueueLimits, SessionState, StreamSelection, VideoCodec,
        VideoSession,
    };

    let (mut player, pool, asc, offset) = aac_fixture_player();
    let cancel = off();
    let mut video = VideoSession::new(QueueLimits::VIDEO);
    let mut audio = AudioSession::new(QueueLimits::AUDIO);
    let selection = StreamSelection {
        video: VideoCodec::H264,
        audio: Some(AudioCodec::Aac),
    };
    selection.load(&mut video, &mut audio, &[]).unwrap();
    let mut reference = None;

    for iteration in 0..2 {
        let outputs = collect(&mut player, 1).unwrap();
        let packets = packets(&outputs);
        assert_eq!(packets.len(), 4);
        let avc = packets
            .iter()
            .find(|p| p.configuration.description.format == *b"avc1")
            .unwrap();
        let aac = &packets
            .iter()
            .find(|p| p.configuration.description.format == *b"mp4a")
            .unwrap()
            .configuration;
        let video_track = TrackKey::Mp4(avc.configuration.track.track_id);
        let mut avc = annex_b_session(&pool, avc);
        let mut aac_setup = AacAscParser::new(
            &pool,
            AacSource {
                source: ID,
                generation: aac.revision,
                offset,
            },
            asc.clone(),
            AacConfigLimits {
                maximum_record_bytes: 4096,
                maximum_pce_elements: 64,
                maximum_comment_bytes: 255,
            },
        )
        .unwrap();
        let asc_record = ready(|work| aac_setup.step(work, &cancel).unwrap());
        let aac_bridge = aac_bridge::AacBridge::new(
            &pool,
            ID,
            aac.track.track_id,
            aac.revision,
            asc_record,
            8191,
        )
        .unwrap();

        for packet in packets {
            let pts = packet
                .source
                .sample
                .source_times(packet.configuration.track.base.require().unwrap())
                .unwrap()
                .cts
                .map(|t| t.to_nanoseconds(Rounding::NearestAway).unwrap());
            match &packet.configuration.description.format {
                b"avc1" => {
                    let unit = annex_b(&mut avc, packet);
                    video.feed(unit.bytes().as_ref(), pts).unwrap();
                }
                b"mp4a" => {
                    let mut job = aac_bridge.packet(packet.clone()).unwrap();
                    let unit = ready(|work| job.step(work, &cancel).unwrap());
                    audio.feed(unit.adts.bytes().as_ref(), pts).unwrap();
                }
                codec => panic!("unexpected fixture codec {codec:?}"),
            }
        }
        video.end_of_stream();
        audio.end_of_stream();
        let mut pcm = PcmBlock::default();
        assert!(!video.step(Some(-1)));
        assert_eq!(audio.pull(Some(-1), 0, &mut pcm), 0);
        assert!(video.step(Some(0)));
        let first = video.latest().unwrap().clone();
        assert_eq!(first.pts, Some(0));
        assert_eq!(first.generation, video.generation());
        assert_eq!(audio.pull(Some(0), 0, &mut pcm), 1);
        assert_eq!(
            (pcm.pts, pcm.sample_rate, pcm.samples.len()),
            (Some(0), 48_000, 2048)
        );
        assert!(!video.step(Some(0)));
        assert_eq!(audio.pull(Some(0), 0, &mut pcm), 0);
        assert_eq!(audio.pull(Some(21_333_333), 0, &mut pcm), 1);
        assert_eq!(pcm.samples.len(), 4096);
        assert!(pcm.samples.iter().any(|&sample| sample != 0));
        assert!(!video.step(Some(39_999_999)));
        assert!(video.step(Some(40_000_000)));
        let second = video.latest().unwrap().clone();
        assert_eq!(second.pts, Some(40_000_000));
        assert!(first.rgba != second.rgba, "fixture pictures must differ");
        assert!(!video.step(Some(40_000_000)));
        assert_eq!(video.state(), SessionState::Ended);
        assert_eq!(audio.state(), SessionState::Ended);

        if iteration == 0 {
            assert!(
                player
                    .seek_and_select(
                        video_track,
                        Time::new(0, 1).unwrap(),
                        Selection::All,
                        &mut WorkBudget::new(1),
                        &cancel
                    )
                    .unwrap()
            );
            assert!(outputs.iter().all(|out| !player.current(out)));
            video.flush();
            audio.stop();
            assert_eq!(video.latest().unwrap().sequence, second.sequence);
            reference = Some((first, second, pcm));
        } else {
            let (old_first, old_second, old_pcm) = reference.take().unwrap();
            assert!(first.generation > old_first.generation);
            assert!(first.sequence > old_second.sequence);
            assert!(
                first.rgba == old_first.rgba,
                "first picture changed after seek"
            );
            assert!(
                second.rgba == old_second.rgba,
                "second picture changed after seek"
            );
            assert_eq!(pcm.sample_rate, old_pcm.sample_rate);
            assert_eq!(pcm.pts, old_pcm.pts);
            assert!(
                pcm.samples == old_pcm.samples,
                "PCM changed after seek reset"
            );
        }
    }
    player.stop();
    video.stop();
    audio.load(None);
    assert_eq!(player.state(), State::Stopped);
    assert_eq!(video.state(), SessionState::Idle);
    assert_eq!(audio.state(), SessionState::Idle);
}

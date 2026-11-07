//! Application-boundary test for an independently muxed H.264/AAC segment.
use std::fmt::Write;
use std::sync::atomic::AtomicBool;

use triergon_core::{
    Budget, Bytes, InputCursor, Rounding, SourceCapabilities, SourceId, Time, TimeBase, WorkBudget,
};
use triergon_mpegts::{
    AssemblyStep, ParseLimits, clock::SourceTimePolicy, compat::pes::RegisteredCarriage,
    psi::DescriptorNamespace,
};
use triergon_playback::{
    ByteSource, ClockChoice, Engine, Player, PlayerPolicy, Progress, RawEvent, Selection, State,
    TrackKey, TsNativeCarriage,
    hls_aac::{HlsAac, HlsAacFrame},
    hls_avc::{AvcRaster, AvcTsAccessUnit, AvcTsLimits, AvcTsReader, AvcTsStep},
    hls_clock::{HlsAacClock, HlsAacPtsOrigin, resolve_hls_aac_clocks},
    hls_segment::{
        HlsFeedUnit, HlsSegmentLimits, HlsSegmentProgress, HlsSegmentReader, HlsSegmentTracks,
    },
    io::SourceOwner,
};

fn payload(out: &mut String, bytes: &[u8]) {
    for byte in bytes {
        write!(out, "{byte:02x}").unwrap();
    }
    out.push('\n');
}

fn ticks(value: Option<impl ToString>) -> String {
    value.map_or_else(|| "-".into(), |v| v.to_string())
}

fn at_90khz(time: Time) -> i128 {
    let n = time.numerator() * 90_000;
    let d = time.denominator() as i128;
    assert_eq!(n % d, 0);
    n / d
}

fn video_row(out: &mut String, unit: &AvcTsAccessUnit, raster: AvcRaster) {
    let bytes = unit.source_bytes().as_ref();
    let pts = unit.clock.and_then(|clock| clock.pts90);
    let dts = unit.clock.and_then(|clock| clock.dts90);
    assert_eq!(unit.raster, raster);
    assert_eq!(unit.feed_spans().next(), Some(bytes));
    write!(
        out,
        "v {} {} {} ",
        ticks(pts),
        ticks(dts),
        u8::from(unit.idr)
    )
    .unwrap();
    payload(out, bytes);
}

fn audio_row(out: &mut String, unit: &HlsAacFrame, clock: &HlsAacClock) {
    let tick = TimeBase::new(1, 90_000).unwrap();
    let rounded_exact = clock.pts.to_i64_ticks(tick, Rounding::NearestAway).unwrap();
    assert!(rounded_exact.abs_diff(clock.ffmpeg90.pts_and_dts) <= 1);
    write!(
        out,
        "a {} {} {} {} {} ",
        ticks(unit.pts.map(at_90khz)),
        ticks(unit.dts.map(at_90khz)),
        clock.ffmpeg90.pts_and_dts,
        clock.ffmpeg90.pts_and_dts,
        clock.ffmpeg90.duration
    )
    .unwrap();
    payload(out, unit.bytes.as_ref());
}

fn drain_video(
    reader: &mut AvcTsReader,
    out: &mut String,
    work: u64,
    end: bool,
    raster: AvcRaster,
) {
    for _ in 0..30_000 {
        match reader
            .step(&mut WorkBudget::new(work), &AtomicBool::new(false))
            .unwrap()
        {
            AvcTsStep::NeedInput if !end => return,
            AvcTsStep::End if end => return,
            AvcTsStep::Yield => {}
            AvcTsStep::Ready(unit) => video_row(out, &unit, raster),
            other => panic!("unexpected AVC poll result {other:?}"),
        }
    }
    panic!("AVC assembler stalled");
}

fn drain_audio(reader: &mut HlsAac, out: &mut Vec<HlsAacFrame>, work: u64, end: bool) {
    for _ in 0..30_000 {
        match reader
            .poll(&mut WorkBudget::new(work), &AtomicBool::new(false))
            .unwrap()
        {
            AssemblyStep::NeedPayload if !end => return,
            AssemblyStep::Finished if end => return,
            AssemblyStep::Yield => {}
            AssemblyStep::Output(unit) => out.push(unit),
            other => panic!("unexpected AAC poll result {other:?}"),
        }
    }
    panic!("AAC assembler stalled");
}

#[test]
fn independent_ts_segment_discovers_and_reassembles_the_ffmpeg_avc_aac_outputs() {
    check_segment(
        include_bytes!("fixtures/hls/bframes-multislice.ts"),
        include_str!("fixtures/hls/bframes-multislice.tsv"),
        31,
        AvcRaster {
            width: 96,
            height: 56,
        },
    );
    check_segment(
        include_bytes!("fixtures/hls/baseline-stereo.ts"),
        include_str!("fixtures/hls/baseline-stereo.tsv"),
        71,
        AvcRaster {
            width: 64,
            height: 48,
        },
    );
}

fn check_segment(raw: &[u8], expected: &str, service: u16, raster: AvcRaster) {
    for (read_quantum, work) in [(1, 1), (4096, 4096)] {
        let pool = Budget::new(8 * 1024 * 1024).unwrap();
        let id = SourceId(7);
        let input = InputCursor::new(
            &pool,
            id,
            SourceCapabilities {
                length: Some(raw.len() as u64),
                random_access: false,
            },
            4096,
        )
        .unwrap();
        let source = ByteSource::new(
            id,
            Bytes::copy_from_slice(&pool, raw).unwrap(),
            read_quantum,
        )
        .unwrap();
        let engine = Engine::ts_native_segment_v1(
            input,
            ParseLimits::default(),
            SourceTimePolicy::Unanchored,
            DescriptorNamespace::Unspecified,
        )
        .unwrap();
        let mut player = Player::new(
            engine,
            SourceOwner::new(source),
            Selection::All,
            PlayerPolicy::default(),
        )
        .unwrap();
        player
            .set_clock_view(ClockChoice::ExplicitNativeProjection)
            .unwrap();
        let mut video = None;
        let mut audio = None;
        let mut video_rows = String::new();
        let mut audio_frames = Vec::new();
        let cancel = AtomicBool::new(false);
        for _ in 0..100_000 {
            let progress = player.advance(&mut WorkBudget::new(work), &cancel).unwrap();
            while let Some(output) = player.pop() {
                match output.into_event() {
                    RawEvent::NativeTsConfiguration(config) => {
                        assert!(
                            matches!(config.track, TrackKey::Ts { program, .. } if program == service)
                        );
                        match config.carriage {
                            TsNativeCarriage::Registered(RegisteredCarriage::Avc) => {
                                assert!(video.is_none());
                                video = Some(
                                    AvcTsReader::new(&pool, config, AvcTsLimits::default())
                                        .unwrap(),
                                );
                            }
                            TsNativeCarriage::Registered(RegisteredCarriage::AacAdts) => {
                                assert!(audio.is_none());
                                audio = Some(HlsAac::new(&pool, config, 8191).unwrap());
                            }
                            other => panic!("unexpected carriage {other:?}"),
                        }
                    }
                    RawEvent::NativeTs { packet, clock } => {
                        let is_video = packet.stream_id == 0xe0;
                        let mut mailbox = Some(packet);
                        if is_video {
                            let reader = video.as_mut().expect("AVC announced before its PES");
                            assert!(reader.push(&mut mailbox, clock).unwrap());
                            drain_video(reader, &mut video_rows, work, false, raster);
                        } else {
                            let reader = audio.as_mut().expect("AAC announced before its PES");
                            assert!(reader.push(&mut mailbox, clock).unwrap());
                            drain_audio(reader, &mut audio_frames, work, false);
                        }
                        assert!(mailbox.is_none());
                    }
                    RawEvent::NativeRefusal { error, .. } => {
                        panic!("unexpected PES refusal {error:?}")
                    }
                    _ => {}
                }
            }
            if matches!(progress, Progress::Ended) {
                break;
            }
        }
        assert_eq!(player.state(), State::Ended);
        let mut video = video.unwrap();
        let mut audio = audio.unwrap();
        video.finish();
        audio.finish();
        drain_video(&mut video, &mut video_rows, work, true, raster);
        drain_audio(&mut audio, &mut audio_frames, work, true);
        let audio_clocks = resolve_hls_aac_clocks(&pool, &audio_frames, 64, &cancel).unwrap();
        assert_eq!(
            audio_clocks
                .as_slice()
                .iter()
                .filter(|clock| clock.origin == HlsAacPtsOrigin::Pes)
                .count(),
            2
        );
        let mut audio_rows = String::new();
        for (frame, clock) in audio_frames.iter().zip(audio_clocks.as_slice()) {
            audio_row(&mut audio_rows, frame, clock);
        }
        assert_eq!(format!("{audio_rows}{video_rows}"), expected);
        drop(audio_clocks);
        drop(audio_frames);
        drop(audio);
        drop(video);
        drop(player);
        assert_eq!(pool.used(), Budget::control_storage_bytes());
    }
}

#[test]
fn complete_segment_preserves_ffmpeg_cross_track_feed_order() {
    for (raw, packets, order, raster) in [
        (
            include_bytes!("fixtures/hls/bframes-multislice.ts").as_slice(),
            include_str!("fixtures/hls/bframes-multislice.tsv"),
            include_str!("fixtures/hls/bframes-multislice.order"),
            AvcRaster {
                width: 96,
                height: 56,
            },
        ),
        (
            include_bytes!("fixtures/hls/baseline-stereo.ts").as_slice(),
            include_str!("fixtures/hls/baseline-stereo.tsv"),
            include_str!("fixtures/hls/baseline-stereo.order"),
            AvcRaster {
                width: 64,
                height: 48,
            },
        ),
        (
            include_bytes!("fixtures/hls/media-00.ts").as_slice(),
            include_str!("fixtures/hls/media-00.tsv"),
            include_str!("fixtures/hls/media-00.order"),
            AvcRaster {
                width: 64,
                height: 48,
            },
        ),
        (
            include_bytes!("fixtures/hls/media-01.ts").as_slice(),
            include_str!("fixtures/hls/media-01.tsv"),
            include_str!("fixtures/hls/media-01.order"),
            AvcRaster {
                width: 64,
                height: 48,
            },
        ),
    ] {
        for (read, work) in [(1, 1), (4096, 4096)] {
            let pool = Budget::new(8 * 1024 * 1024).unwrap();
            let source_id = SourceId(41);
            let input = InputCursor::new(
                &pool,
                source_id,
                SourceCapabilities {
                    length: Some(raw.len() as u64),
                    random_access: false,
                },
                4096,
            )
            .unwrap();
            let source =
                ByteSource::new(source_id, Bytes::copy_from_slice(&pool, raw).unwrap(), read)
                    .unwrap();
            let mut reader = HlsSegmentReader::new(
                input,
                source,
                HlsSegmentTracks::default(),
                HlsSegmentLimits::default(),
            )
            .unwrap();
            let cancel = AtomicBool::new(false);
            let mut ready = false;
            for _ in 0..200_000 {
                assert!(reader.take(&cancel).unwrap().is_none());
                match reader.advance(&mut WorkBudget::new(work), &cancel).unwrap() {
                    HlsSegmentProgress::Ready => {
                        ready = true;
                        break;
                    }
                    HlsSegmentProgress::Yield | HlsSegmentProgress::NeedInput(_) => {}
                    other @ HlsSegmentProgress::Backpressure { .. } => {
                        panic!("unexpected segment progress {other:?}")
                    }
                }
            }
            assert!(ready, "whole segment reader stalled");
            let segment = reader.take(&cancel).unwrap().unwrap();
            assert!(reader.take(&cancel).unwrap().is_none());
            assert_eq!(segment.initial_raster(), raster);
            assert_eq!(segment.access_units().len(), packets.lines().count());
            let mut actual = String::new();
            let mut bytes = 0;
            let mut video_tail = i64::MIN;
            let mut audio_tail = None;
            for unit in segment.access_units() {
                match unit {
                    HlsFeedUnit::Video(unit) => {
                        video_tail = video_tail.max(unit.clock.unwrap().pts90.unwrap());
                        bytes += unit.source_bytes().len();
                        video_row(&mut actual, unit, raster);
                    }
                    HlsFeedUnit::Audio(unit, clock) => {
                        audio_tail = Some(clock.ffmpeg90.pts_and_dts);
                        bytes += unit.bytes.len();
                        audio_row(&mut actual, unit, clock);
                    }
                }
            }
            let mut video = packets.lines().filter(|line| line.starts_with('v'));
            let mut audio = packets.lines().filter(|line| line.starts_with('a'));
            for (lane, row) in order.trim_end().bytes().zip(actual.lines()) {
                let expected = if lane == b'v' {
                    video.next()
                } else {
                    audio.next()
                };
                assert_eq!(Some(row), expected);
            }
            assert_eq!(actual.lines().count(), order.trim_end().len());
            assert_eq!(segment.feed_bytes(), bytes);
            assert_eq!(segment.video_tail90(), video_tail);
            assert_eq!(segment.audio_tail90(), audio_tail);
            drop(reader);
            assert_eq!(segment.access_units().len(), packets.lines().count());
            drop(segment);
            assert_eq!(pool.used(), Budget::control_storage_bytes());
        }
    }
}

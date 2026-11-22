//! A clock-paced audio session drops what the container marks for discarding.
#![cfg(feature = "codecs")]
use triergon::ogg::page::{BEGIN_OF_STREAM, END_OF_STREAM, write_page};
use triergon::{Budget, ContainerReader, MediaKind, PacketInfo, open_blocking};
use triergon_codec::{AudioFrame, Speaker};
use triergon_core::{Rounding, TimeBase};
use triergon_session::{AudioSession, Discard, Error, PcmBlock, QueueLimits, TrackDecoder};

/// Ogg Opus: five 20 ms CELT packets, the supplied pre-skip, and a final granule (which
/// counts the pre-skip) 100 samples before the end of the last packet.
fn opus_file(pre_skip: u16) -> Vec<u8> {
    let mut head = *b"OpusHead\x01\x02\x00\x00\x80\xbb\x00\x00\x00\x00\x00";
    head[10..12].copy_from_slice(&pre_skip.to_le_bytes());
    let tags = b"OpusTags\x04\x00\x00\x00test\x00\x00\x00\x00";
    let mut file = Vec::new();
    write_page(
        BEGIN_OF_STREAM,
        0,
        9,
        0,
        &[head.len() as u8],
        &head,
        &mut file,
    );
    write_page(0, 0, 9, 1, &[tags.len() as u8], tags, &mut file);
    let body = [0xfc, 0xff, 0xfe].repeat(5);
    write_page(
        END_OF_STREAM,
        5 * 960 - 100,
        9,
        2,
        &[3; 5],
        &body,
        &mut file,
    );
    file
}

#[test]
fn an_audio_session_applies_pre_skip_and_end_trim() {
    let (block, expected) = opus_session(312, false);
    assert_eq!(block.pts, None);
    assert_eq!(block.samples.len(), expected);
}

#[test]
fn opus_block_starts_at_the_first_sample_after_pre_skip() {
    for pre_skip in [312, 1272, 2232] {
        let (block, expected) = opus_session(pre_skip, true);
        assert_eq!(block.pts, Some(0), "pre-skip {pre_skip}");
        assert_eq!(block.samples.len(), expected);
    }
}

fn opus_session(pre_skip: u16, clocked: bool) -> (PcmBlock, usize) {
    let pool = Budget::new(16 << 20).unwrap();
    let mut source = open_blocking(std::io::Cursor::new(opus_file(pre_skip)), &pool).unwrap();
    let mut session = AudioSession::new(QueueLimits::AUDIO);
    let mut loaded = false;
    while let Some(packet) = source.next_packet().unwrap() {
        if !loaded {
            let track = source.reader().track(0).unwrap();
            session.load(Some(TrackDecoder::new(track).unwrap().into_decoder()));
            loaded = true;
        }
        let payload = packet.payload().to_vec();
        let pts = packet.pts().filter(|_| clocked).map(|pts| {
            packet
                .time_base()
                .rescale(pts, TimeBase::NANOSECONDS, Rounding::NearestAway)
                .unwrap()
        });
        session
            .feed_discarding(&payload, pts, Discard::of(&packet))
            .unwrap();
    }
    session.end_of_stream();
    let mut block = PcmBlock::default();
    session.pull(None, 0, &mut block);
    assert_eq!(block.sample_rate, 48_000);
    (block, 2 * (5 * 960 - usize::from(pre_skip) - 100))
}

#[test]
fn a_track_decoder_charges_its_configuration_to_the_callers_pool() {
    let pool = Budget::new(16 << 20).unwrap();
    let mut source = open_blocking(std::io::Cursor::new(opus_file(312)), &pool).unwrap();
    source.next_packet().unwrap();
    let track = source.reader().track(0).unwrap();
    let tiny = Budget::new(Budget::control_storage_bytes() + 8).unwrap();
    let refused = TrackDecoder::new_in(track, &tiny).unwrap_err();
    assert!(matches!(refused, Error::Limit { .. }), "{refused:?}");
    let decoder = TrackDecoder::new_in(track, &Budget::new(1 << 20).unwrap()).unwrap();
    assert_eq!(decoder.codec(), triergon_session::AudioCodec::Opus);
}

#[test]
fn a_quicktime_aac_track_preserves_eight_speakers() {
    use Speaker::{
        BackLeft, BackRight, FrontCenter, FrontLeft, FrontRight, LowFrequency, SideLeft, SideRight,
    };
    let fixture = include_bytes!("../../playback/tests/fixtures/mp4/avc-aac-quicktime-7-1.mov");
    let budget = Budget::new(32 << 20).unwrap();
    let mut source = open_blocking(std::io::Cursor::new(fixture), &budget).unwrap();
    let mut packet = source.next_packet().unwrap();
    let reader = source.reader();
    let track = (0..reader.track_count())
        .filter_map(|index| reader.track(index))
        .find(|track| track.kind() == MediaKind::Audio)
        .unwrap();
    let id = track.id();
    let mut decoder = TrackDecoder::aac_access_units_in(track.configuration(), &budget).unwrap();
    let mut frame = AudioFrame::default();
    let mut count = 0;
    while let Some(current) = packet {
        if current.track() == id {
            decoder.decode(&current, &mut frame).unwrap();
            assert_eq!(
                (frame.sample_rate, frame.samples_per_channel),
                (44_100, 1024)
            );
            assert_eq!(
                frame.layout.speakers(),
                &[
                    FrontLeft,
                    FrontRight,
                    FrontCenter,
                    LowFrequency,
                    BackLeft,
                    BackRight,
                    SideLeft,
                    SideRight
                ]
            );
            if count == 20 {
                // Each speaker carries a distinct tone. Checking the decoded samples
                // catches a plane swap even when the reported layout is correct.
                let frequencies = [251.0, 379.0, 503.0, 71.0, 641.0, 769.0, 911.0, 1031.0];
                for (channel, plane) in frame.planes.iter().enumerate() {
                    let power = tone_power(plane, frequencies[channel]);
                    assert!(power > 10.0);
                    for (index, other) in frequencies.into_iter().enumerate() {
                        if index != channel {
                            assert!(power > 4.0 * tone_power(plane, other));
                        }
                    }
                }
            }
            count += 1;
        }
        packet = source.next_packet().unwrap();
    }
    assert_eq!(count, 174);
}

fn tone_power(samples: &[f32], frequency: f64) -> f64 {
    let phase_step = std::f64::consts::TAU * frequency / 44_100.0;
    let (sin, cos) = samples
        .iter()
        .enumerate()
        .fold((0.0, 0.0), |(sin, cos), (index, &sample)| {
            let (s, c) = (f64::from(u32::try_from(index).unwrap()) * phase_step).sin_cos();
            (sin + f64::from(sample) * s, cos + f64::from(sample) * c)
        });
    sin * sin + cos * cos
}

#[test]
fn reset_replays_aac_without_the_previous_packets_overlap() {
    let fixture = include_bytes!("../../playback/tests/fixtures/mp4/avc-aac-quicktime-7-1.mov");
    let budget = Budget::new(32 << 20).unwrap();
    let mut source = open_blocking(std::io::Cursor::new(fixture), &budget).unwrap();
    let first = source.next_packet().unwrap().unwrap();
    let reader = source.reader();
    let track = (0..reader.track_count())
        .filter_map(|index| reader.track(index))
        .find(|track| track.kind() == MediaKind::Audio)
        .unwrap();
    let id = track.id();
    let mut reset = TrackDecoder::aac_access_units_in(track.configuration(), &budget).unwrap();
    let mut fresh = TrackDecoder::aac_access_units_in(track.configuration(), &budget).unwrap();
    let mut packets = std::iter::once(first)
        .chain(std::iter::from_fn(|| source.next_packet().unwrap()))
        .filter(|packet| packet.track() == id)
        .skip(20);
    let packet = packets.next().unwrap();
    let mut frame = AudioFrame::default();
    reset.decode(&packet, &mut frame).unwrap();
    reset.decode(&packets.next().unwrap(), &mut frame).unwrap();
    reset.reset();
    reset.decode(&packet, &mut frame).unwrap();
    let mut expected = AudioFrame::default();
    fresh.decode(&packet, &mut expected).unwrap();
    assert_eq!(frame.samples_per_channel, expected.samples_per_channel);
    assert_eq!(frame.planes, expected.planes);
}

//! A clock-paced audio session drops what the container marks for discarding.
#![cfg(feature = "codecs")]
use triergon::ogg::page::{BEGIN_OF_STREAM, END_OF_STREAM, write_page};
use triergon::{Budget, ContainerReader, MediaKind, PacketInfo, open_blocking};
use triergon_codec::{AudioFrame, Speaker};
use triergon_session::{AudioSession, Discard, PcmBlock, QueueLimits, TrackDecoder};

/// Ogg Opus: five 20 ms CELT packets, a 312-sample pre-skip, and a final granule (which
/// counts the pre-skip) 100 samples before the end of the last packet.
fn opus_file() -> Vec<u8> {
    let head = b"OpusHead\x01\x02\x38\x01\x80\xbb\x00\x00\x00\x00\x00";
    let tags = b"OpusTags\x04\x00\x00\x00test\x00\x00\x00\x00";
    let mut file = Vec::new();
    write_page(
        BEGIN_OF_STREAM,
        0,
        9,
        0,
        &[head.len() as u8],
        head,
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
    let pool = Budget::new(16 << 20).unwrap();
    let mut source = open_blocking(std::io::Cursor::new(opus_file()), &pool).unwrap();
    let mut session = AudioSession::new(QueueLimits::AUDIO);
    let mut loaded = false;
    while let Some(packet) = source.next_packet().unwrap() {
        if !loaded {
            let track = source.reader().track(0).unwrap();
            session.load(Some(TrackDecoder::new(track).unwrap().into_decoder()));
            loaded = true;
        }
        let payload = packet.payload().to_vec();
        session
            .feed_discarding(&payload, None, Discard::of(&packet))
            .unwrap();
    }
    session.end_of_stream();
    let mut block = PcmBlock::default();
    session.pull(None, 0, &mut block);
    assert_eq!(block.sample_rate, 48_000);
    assert_eq!(block.samples.len(), 2 * (5 * 960 - 312 - 100));
}

#[test]
fn a_track_decoder_charges_its_configuration_to_the_callers_pool() {
    let pool = Budget::new(16 << 20).unwrap();
    let mut source = open_blocking(std::io::Cursor::new(opus_file()), &pool).unwrap();
    source.next_packet().unwrap();
    let track = source.reader().track(0).unwrap();
    let tiny = Budget::new(Budget::control_storage_bytes() + 8).unwrap();
    let refused = TrackDecoder::new_in(track, &tiny).unwrap_err();
    assert!(refused.0.starts_with("Limit"), "{}", refused.0);
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
    let mut decoder = TrackDecoder::new_in(track, &budget).unwrap();
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

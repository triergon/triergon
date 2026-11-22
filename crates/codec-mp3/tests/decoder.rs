//! Behaviour through the public API: output shape, budgets, and malformed input.

use triergon_codec::{AudioDecoder, AudioFrame, Budget, Speaker, StorageError};
use triergon_codec_mp3::{DecodeError, Decoder, Limits, frame_header};

#[allow(dead_code)]
#[path = "../../../xtask/src/sha256.rs"]
mod sha256;

/// A frame of `length` bytes with header word `header` and an all-zero body: silence
/// in every layer (no allocation, no Huffman data).
fn silent_frame(header: u32) -> Vec<u8> {
    let length = frame_header(&header.to_be_bytes())
        .unwrap()
        .frame_bytes()
        .unwrap();
    let mut frame = vec![0; length];
    frame[..4].copy_from_slice(&header.to_be_bytes());
    frame
}

/// MPEG-1 Layer III, 128 kbit/s, 44.1 kHz, joint stereo.
const LAYER3: u32 = 0xfffb_9064;
/// MPEG-2 Layer III, 64 kbit/s, 22.05 kHz, mono.
const LAYER3_LSF: u32 = 0xfff3_80c0;
/// MPEG-1 Layer II, 192 kbit/s, 48 kHz, stereo.
const LAYER2: u32 = 0xfffd_a400;
/// MPEG-1 Layer I, 384 kbit/s, 44.1 kHz, dual channel.
const LAYER1: u32 = 0xffff_c080;

#[test]
fn frames_decode_to_their_layer_and_version_shape() {
    let mut decoder = Decoder::new();
    let mut out = AudioFrame::default();
    for (header, samples, rate, speakers) in [
        (
            LAYER3,
            1152,
            44_100,
            &[Speaker::FrontLeft, Speaker::FrontRight][..],
        ),
        (LAYER3_LSF, 576, 22_050, &[Speaker::FrontCenter][..]),
        (
            LAYER2,
            1152,
            48_000,
            &[Speaker::FrontLeft, Speaker::FrontRight][..],
        ),
        (
            LAYER1,
            384,
            44_100,
            &[Speaker::FrontLeft, Speaker::FrontRight][..],
        ),
    ] {
        assert_eq!(
            decoder.decode(&silent_frame(header), &mut out).unwrap(),
            None
        );
        assert_eq!((out.samples_per_channel, out.sample_rate), (samples, rate));
        assert_eq!(out.layout.speakers(), speakers);
        assert!(
            out.planes
                .iter()
                .all(|p| p.len() == samples && p.iter().all(|&s| s == 0.0))
        );
    }
}

#[test]
fn the_default_limit_holds_the_state_and_the_largest_frame_with_room_to_spare() {
    let budget = Budget::new(Limits::default().live_storage_bytes).unwrap();
    let mut decoder = Decoder::new_in(&budget);
    let mut out = AudioFrame::default();
    for header in [LAYER3, LAYER3_LSF, LAYER2, LAYER1] {
        decoder
            .decode_frame(&silent_frame(header), &mut out)
            .unwrap();
    }
    assert!(budget.peak() < budget.limit() / 4, "{}", budget.peak());
}

#[test]
fn a_budget_too_small_is_a_typed_error_that_leaves_the_frame_empty() {
    // What a mono low-sampling-frequency frame (576 samples) charges, the state included.
    let budget = Budget::new(1 << 20).unwrap();
    Decoder::new_in(&budget)
        .decode_frame(&silent_frame(LAYER3_LSF), &mut AudioFrame::default())
        .unwrap();
    let limits = Limits {
        live_storage_bytes: budget.peak(),
    };
    let mut decoder = Decoder::with_limits(limits).unwrap();
    let mut out = AudioFrame::default();
    // Two channels of 1152 samples do not fit beside the state; the decoder stays usable.
    let error = decoder
        .decode_frame(&silent_frame(LAYER3), &mut out)
        .unwrap_err();
    assert!(matches!(
        error,
        DecodeError::Budget(StorageError::Backpressure { .. })
    ));
    assert_eq!(out.samples_per_channel, 0);
    decoder
        .decode_frame(&silent_frame(LAYER3_LSF), &mut out)
        .unwrap();
    // A pool smaller than the state refuses every frame.
    let mut decoder = Decoder::with_limits(Limits {
        live_storage_bytes: 4096,
    })
    .unwrap();
    for header in [LAYER3_LSF, LAYER1] {
        assert!(matches!(
            decoder.decode_frame(&silent_frame(header), &mut out),
            Err(DecodeError::Budget(StorageError::Limit { .. }))
        ));
    }
}

#[test]
fn decoding_resumes_after_a_refused_charge() {
    // Layer II decodes any body, so random bytes give a frame of nonzero samples.
    let mut random = Lcg(3);
    let mut frame = silent_frame(LAYER2);
    for byte in &mut frame[4..] {
        *byte = random.next();
    }
    let budget = Budget::new(Limits::default().live_storage_bytes).unwrap();
    let mut decoder = Decoder::new_in(&budget);
    let mut out = AudioFrame::default();
    let held = budget.reserve(budget.available()).unwrap();
    assert!(matches!(
        decoder.decode_frame(&frame, &mut out),
        Err(DecodeError::Budget(StorageError::Backpressure { .. }))
    ));
    drop(held);
    decoder.decode_frame(&frame, &mut out).unwrap();
    let mut expected = AudioFrame::default();
    Decoder::new().decode_frame(&frame, &mut expected).unwrap();
    assert!(expected.planes[0].iter().any(|&s| s != 0.0));
    assert_eq!(out.planes, expected.planes);
}

#[test]
fn a_shared_budget_carries_the_output_charge_until_the_decoder_drops() {
    let budget = Budget::new(1 << 20).unwrap();
    let idle = budget.used();
    let mut decoder = Decoder::new_in(&budget);
    decoder
        .decode_frame(&silent_frame(LAYER3), &mut AudioFrame::default())
        .unwrap();
    assert!(budget.used() >= idle + 2 * 1152 * size_of::<f32>());
    drop(decoder);
    assert_eq!(budget.used(), idle);
}

#[test]
fn free_format_and_garbage_headers_are_refused() {
    let mut decoder = Decoder::new();
    let mut out = AudioFrame::default();
    let free = [0xff, 0xfb, 0x04, 0xc0, 0, 0, 0, 0];
    assert_eq!(
        decoder.decode_frame(&free, &mut out),
        Err(DecodeError::Unsupported("free-format frame"))
    );
    for au in [&[][..], &[0xff, 0xfb], &[0, 1, 2, 3, 4, 5]] {
        assert_eq!(
            decoder.decode_frame(au, &mut out),
            Err(DecodeError::Invalid("frame header"))
        );
    }
}

// MPEG-1 mono side information, with a single table 1 (1, 0) pair in granule 0.
// Table B.7 encodes that pair as 01 followed by its positive sign bit 0.
fn signal_frame(backstep: u32) -> Vec<u8> {
    let mut frame = silent_frame(0xfffb_90c0);
    let mut position = 32;
    fn bits(frame: &mut [u8], pos: &mut usize, value: u32, count: usize) {
        for shift in (0..count).rev() {
            frame[*pos / 8] |= (((value >> shift) & 1) as u8) << (7 - *pos % 8);
            *pos += 1;
        }
    }
    bits(&mut frame, &mut position, backstep, 9);
    bits(&mut frame, &mut position, 0, 9); // private bits and scfsi
    bits(&mut frame, &mut position, 3, 12); // part2_3_length
    bits(&mut frame, &mut position, 1, 9); // big_values
    bits(&mut frame, &mut position, 210, 8); // global_gain
    bits(&mut frame, &mut position, 0, 5); // scalefac_compress, window switching
    bits(&mut frame, &mut position, 1, 5); // table_select[0]
    frame[21] = 0x40;
    frame
}

#[test]
fn framed_free_format_signal_and_reservoir_match_ordinary_frames() {
    let ordinary = signal_frame(0);
    let mut free = ordinary.clone();
    free[2] &= 0x0f;
    let mut expected = AudioFrame::default();
    let mut out = AudioFrame::default();
    Decoder::new()
        .decode_frame(&ordinary, &mut expected)
        .unwrap();
    let mut decoder = Decoder::new();
    decoder.decode_framed(&free, &mut out).unwrap();
    assert_eq!(out.planes, expected.planes);
    assert_eq!((out.samples_per_channel, out.sample_rate), (1152, 44_100));
    assert_eq!(out.layout.speakers(), &[Speaker::FrontCenter]);
    assert!(out.planes[0].iter().any(|&sample| sample != 0.0));

    let mut first = silent_frame(0xfffb_90c0);
    *first.last_mut().unwrap() = 0x40;
    first[2] &= 0x0f;
    let mut second = signal_frame(1);
    second[2] &= 0x0f;
    second[21] = 0;
    let mut decoder = Decoder::new();
    decoder.decode_framed(&first, &mut out).unwrap();
    decoder.decode_framed(&second, &mut out).unwrap();
    assert_eq!(out.planes, expected.planes);
    decoder.reset();
    decoder.decode_framed(&second, &mut out).unwrap();
    assert!(out.planes[0].iter().all(|&sample| sample == 0.0));
}

#[test]
fn framed_free_format_rejects_bad_extents_and_preserves_controls() {
    let mut decoder = Decoder::new();
    let mut out = AudioFrame::default();
    let mut free = signal_frame(0);
    free[2] &= 0x0f;
    assert!(matches!(
        decoder.decode_framed(&free[..20], &mut out),
        Err(DecodeError::Invalid(_))
    ));
    let mut oversized = free.clone();
    oversized.resize(21 + 1442, 0);
    assert!(matches!(
        decoder.decode_framed(&oversized, &mut out),
        Err(DecodeError::Invalid(_))
    ));
    assert_eq!(out.samples_per_channel, 0);
    let mut layer2 = silent_frame(LAYER2);
    layer2[2] &= 0x0f;
    assert_eq!(
        decoder.decode_framed(&layer2, &mut out),
        Err(DecodeError::Unsupported("free-format frame"))
    );
    let ordinary = signal_frame(0);
    let mut expected = AudioFrame::default();
    Decoder::new()
        .decode_frame(&ordinary, &mut expected)
        .unwrap();
    decoder.decode_framed(&free, &mut out).unwrap();
    assert_eq!(out.planes, expected.planes);
}

#[test]
fn framed_free_format_bounds_crc_lsf_and_backpressure() {
    let mut frame = silent_frame(LAYER3_LSF);
    frame[2] &= 0x0f;
    frame[1] &= !1; // CRC precedes the low-frequency mono side information.
    frame.resize(4 + 2 + 9 + 1441, 0);
    let budget = Budget::new(Limits::default().live_storage_bytes).unwrap();
    let mut decoder = Decoder::new_in(&budget);
    let mut out = AudioFrame::default();
    let held = budget.reserve(budget.available()).unwrap();
    assert!(matches!(
        decoder.decode_framed(&frame, &mut out),
        Err(DecodeError::Budget(StorageError::Backpressure { .. }))
    ));
    drop(held);
    decoder.decode_framed(&frame, &mut out).unwrap();
    assert_eq!((out.samples_per_channel, out.sample_rate), (576, 22_050));
    assert_eq!(out.layout.speakers(), &[Speaker::FrontCenter]);
    assert!(out.planes[0].iter().all(|&sample| sample == 0.0));
    frame.push(0);
    assert!(matches!(
        decoder.decode_framed(&frame, &mut out),
        Err(DecodeError::Invalid(_))
    ));
}

const FREE_FORMAT_MONO: &str = "harness/fixtures/mpegts/free-format-mono.mp3";

fn free_format_frames(bytes: &[u8], channels: usize, mut visit: impl FnMut(&AudioFrame)) -> usize {
    use triergon::PacketInfo;

    let pool = Budget::new(1 << 20).unwrap();
    let mut reader = triergon::open_blocking(std::io::Cursor::new(bytes), &pool).unwrap();
    let mut decoder = Decoder::new_in(&pool);
    let mut frame = AudioFrame::default();
    let mut frames = 0;
    while let Some(packet) = reader.next_packet().unwrap() {
        assert_eq!(frame_header(packet.payload()).unwrap().bitrate_kbps, 0);
        decoder.decode(packet.payload(), &mut frame).unwrap();
        assert_eq!(frame.sample_rate, 44_100);
        assert_eq!(frame.samples_per_channel, 1152);
        assert_eq!(frame.planes.len(), channels);
        visit(&frame);
        frames += 1;
    }
    decoder.flush(&mut frame).unwrap();
    assert_eq!(frame.samples_per_channel, 0);
    frames
}

#[test]
fn free_format_layer_three_through_source_detection() {
    let bytes = include_bytes!("../../../harness/fixtures/mpegts/free-format-mono.mp3");
    let mut signal = false;
    let frames = free_format_frames(bytes, 1, |frame| {
        signal |= frame.planes[0].iter().any(|&value| value != 0.0);
    });
    assert_eq!(frames, 13);
    assert!(signal);
}

#[test]
#[ignore = "requires mpg123 1.33.7 and the pinned FATE input"]
fn free_format_layer_three_matches_mpg123() {
    use std::{path::Path, process::Command};

    let mpg123 = std::env::var_os("TRIERGON_MPG123").unwrap();
    let version = Command::new(&mpg123).arg("--version").output().unwrap();
    assert!(version.status.success());
    assert!(String::from_utf8_lossy(&version.stdout).contains("mpg123 1.33.7"));
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for (source, pin, channels, frames) in [
        (
            FREE_FORMAT_MONO,
            "2d049635dceb38a5dc7c442f272c064b8fdbcebe1c9b218c7b6ea1c72fcfbe6d",
            1,
            13,
        ),
        (
            ".research/fate-samples/mp3-conformance/he_free.bit",
            "b55afc2a21492c4b3035d423f36fe87560ef34f65eb997da60af9b01f300362c",
            2,
            68,
        ),
    ] {
        let path = root.join(source);
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(sha256::hex(&bytes), pin);
        let reference = Command::new(&mpg123)
            .args(["--no-gapless", "--float", "-q", "-s"])
            .arg(&path)
            .output()
            .unwrap();
        assert!(reference.status.success(), "{source}");
        let mut samples = reference.stdout.chunks_exact(4);
        assert!(samples.remainder().is_empty());
        assert_eq!(samples.len(), frames * 1152 * channels);
        let mut worst_rms = 0.0_f64;
        let mut worst_peak = 0.0_f64;
        let count = free_format_frames(&bytes, channels, |frame| {
            let mut squared = [0.0_f64; 2];
            for index in 0..frame.samples_per_channel {
                for (plane, sum) in frame.planes.iter().zip(&mut squared) {
                    let expected = f32::from_ne_bytes(samples.next().unwrap().try_into().unwrap());
                    let difference = f64::from(plane[index]) - f64::from(expected);
                    assert!(difference.is_finite());
                    *sum += difference * difference;
                    worst_peak = worst_peak.max(difference.abs());
                }
            }
            for sum in &squared[..channels] {
                worst_rms = worst_rms.max((sum / frame.samples_per_channel as f64).sqrt());
            }
        });
        assert_eq!(count, frames);
        assert_eq!(samples.len(), 0);
        println!("{source}: {count} frames, max RMS {worst_rms:.3e}, peak {worst_peak:.3e}");
        assert!(worst_rms <= 1e-6 && worst_peak <= 1e-5, "{source}");
    }
}

/// A small deterministic generator for the robustness test.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u8 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) as u8
    }
}

#[test]
fn random_bodies_and_truncations_never_panic_and_leave_the_decoder_usable() {
    let mut random = Lcg(7);
    let mut decoder = Decoder::new();
    let mut out = AudioFrame::default();
    // Every version, layer, bit rate, sample rate, mode and CRC flag.
    for version in [0, 2, 3] {
        for layer in 1..4u32 {
            for bitrate in 1..15u32 {
                for rate in 0..3u32 {
                    for mode in 0..4u32 {
                        let crc = random.next() & 1;
                        let header = 0xffe0_0000
                            | version << 19
                            | layer << 17
                            | u32::from(crc) << 16
                            | bitrate << 12
                            | rate << 10
                            | mode << 6
                            | u32::from(random.next() & 3) << 4;
                        let mut frame = silent_frame(header);
                        for byte in &mut frame[4..] {
                            *byte = random.next();
                        }
                        let _ = decoder.decode(&frame, &mut out);
                        let cut = usize::from(random.next()) % frame.len();
                        let _ = decoder.decode(&frame[..cut], &mut out);
                    }
                }
            }
        }
    }
    decoder.reset();
    decoder.decode(&silent_frame(LAYER3), &mut out).unwrap();
    assert!(out.planes.iter().all(|p| p.iter().all(|&s| s == 0.0)));
}

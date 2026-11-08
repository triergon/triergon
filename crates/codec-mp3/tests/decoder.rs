//! Behaviour through the public API: output shape, budgets, and malformed input.

use triergon_codec::{AudioDecoder, AudioFrame, Budget, Speaker, StorageError};
use triergon_codec_mp3::{DecodeError, Decoder, Limits, frame_header};

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

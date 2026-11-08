use super::*;
use triergon_codec::Speaker;

/// An `OpusHead` for `channels` with mapping `family` (and its table when not 0).
fn head(channels: u8, family: u8, streams: u8, coupled: u8, mapping: &[u8], gain: i16) -> Vec<u8> {
    let mut head = b"OpusHead\x01".to_vec();
    head.push(channels);
    head.extend_from_slice(&312u16.to_le_bytes());
    head.extend_from_slice(&48000u32.to_le_bytes());
    head.extend_from_slice(&gain.to_le_bytes());
    head.push(family);
    if family != 0 {
        head.extend_from_slice(&[streams, coupled]);
        head.extend_from_slice(mapping);
    }
    head
}

/// A CELT-only fullband 20 ms frame of `bytes` pseudo-random bytes, with a TOC.
fn celt_packet(stereo: bool, bytes: usize, seed: &mut u32) -> Vec<u8> {
    let mut packet = vec![0xf8 | (u8::from(stereo) << 2)];
    packet.extend((0..bytes).map(|_| {
        *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (*seed >> 24) as u8
    }));
    packet
}

#[test]
fn a_too_small_budget_is_refused() {
    let config = head(2, 0, 0, 0, &[], 0);
    let error = Decoder::from_configuration(
        &config,
        Limits {
            live_storage_bytes: 1024,
        },
    )
    .unwrap_err();
    assert!(matches!(error, DecodeError::Budget(_)), "{error:?}");

    // Enough for the stream state but not for the output frame.
    let budget = Budget::new(size_of::<StreamDecoder>() + 256).unwrap();
    let idle = budget.used();
    let mut decoder = Decoder::from_configuration_in(&config, &budget).unwrap();
    let mut frame = AudioFrame::default();
    let failure = decoder
        .decode(&celt_packet(true, 100, &mut 1), &mut frame)
        .unwrap_err();
    assert!(failure.0.starts_with("Budget("), "{failure:?}");
    drop(decoder);
    assert_eq!(
        budget.used(),
        idle,
        "the decoder returns its charge when dropped"
    );
}

#[test]
fn headers_map_channels_to_ffmpeg_layouts() {
    use Speaker::*;
    let stereo = Decoder::from_configuration(&head(2, 0, 0, 0, &[], 0), Limits::default()).unwrap();
    assert_eq!(stereo.layout().speakers(), &[FrontLeft, FrontRight]);
    let mono = Decoder::from_configuration(&head(1, 0, 0, 0, &[], 0), Limits::default()).unwrap();
    assert_eq!(mono.layout().speakers(), &[FrontCenter]);
    let surround = head(6, 1, 4, 2, &[0, 4, 1, 2, 3, 5], 0);
    let surround = Decoder::from_configuration(&surround, Limits::default()).unwrap();
    assert_eq!(surround.layout().mask(), 0x3f);
    let unordered =
        Decoder::from_configuration(&head(3, 255, 2, 1, &[0, 1, 2], 0), Limits::default()).unwrap();
    assert_eq!(unordered.layout().channels(), 3);
    assert_eq!(unordered.layout().mask(), 0);
    assert!(matches!(
        Decoder::from_configuration(&head(4, 2, 2, 2, &[0, 1, 2, 3], 0), Limits::default()),
        Err(DecodeError::Unsupported(_))
    ));
    assert!(matches!(
        Decoder::from_configuration(
            &head(9, 1, 9, 0, &[0, 1, 2, 3, 4, 5, 6, 7, 8], 0),
            Limits::default()
        ),
        Err(DecodeError::Invalid(_))
    ));
    assert!(Decoder::from_configuration(b"OpusTags", Limits::default()).is_err());
}

#[test]
fn multistream_packets_fill_every_mapped_plane_and_silence_the_rest() {
    // Two streams: a coupled pair and a mono stream; plane 1 takes no stream.
    let config = head(4, 255, 2, 1, &[0, 255, 2, 1], 0);
    let mut decoder = Decoder::from_configuration(&config, Limits::default()).unwrap();
    let mut seed = 7;
    let first = celt_packet(true, 60, &mut seed);
    let second = celt_packet(false, 40, &mut seed);
    // Self-delimited first stream: TOC, the frame length, the frame.
    let mut packet = vec![first[0], (first.len() - 1) as u8];
    packet.extend_from_slice(&first[1..]);
    packet.extend_from_slice(&second);
    let mut frame = AudioFrame::default();
    for _ in 0..3 {
        decoder.decode(&packet, &mut frame).unwrap();
    }
    assert_eq!((frame.samples_per_channel, frame.planes.len()), (960, 4));
    assert!(frame.planes[1].iter().all(|&v| v == 0.0));
    for plane in [0, 2, 3] {
        assert!(
            frame.planes[plane].iter().any(|&v| v != 0.0),
            "plane {plane}"
        );
    }
    // Streams of different durations are refused.
    let mut uneven = packet.clone();
    let last = uneven.len() - second.len();
    uneven[last] = 0xf0; // 10 ms
    assert!(decoder.decode(&uneven, &mut frame).is_err());
}

#[test]
fn the_output_gain_scales_every_sample() {
    let mut seed = 3;
    let packets: Vec<Vec<u8>> = (0..4).map(|_| celt_packet(true, 120, &mut seed)).collect();
    let decode = |gain: i16| {
        let mut decoder =
            Decoder::from_configuration(&head(2, 0, 0, 0, &[], gain), Limits::default()).unwrap();
        let mut frame = AudioFrame::default();
        let mut out = Vec::new();
        for packet in &packets {
            decoder.decode(packet, &mut frame).unwrap();
            out.extend_from_slice(&frame.planes[0]);
        }
        out
    };
    // +6.02 dB in Q7.8 doubles the amplitude.
    let (plain, louder) = (decode(0), decode(1541));
    for (a, b) in plain.iter().zip(&louder) {
        assert!((b - 2.0 * a).abs() <= 1e-3 * a.abs() + 1e-9, "{a} {b}");
    }
}

#[test]
fn malformed_packets_never_panic_and_the_decoder_recovers() {
    let configs = [
        head(1, 0, 0, 0, &[], 0),
        head(2, 0, 0, 0, &[], 0),
        head(3, 1, 2, 1, &[0, 2, 1], 0),
    ];
    let mut seed = 11u32;
    let mut next = || {
        seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
        (seed >> 16) as u8
    };
    for config in &configs {
        let mut decoder = Decoder::from_configuration(config, Limits::default()).unwrap();
        let mut frame = AudioFrame::default();
        for round in 0..3000 {
            let len = [0, 1, 2, 3, 5, 17, 60, 200, 1300][round % 9];
            let packet: Vec<u8> = (0..len).map(|_| next()).collect();
            if decoder.decode(&packet, &mut frame).is_ok() {
                assert!(frame.samples_per_channel <= 5760 + 32);
                for plane in &frame.planes {
                    assert!(
                        plane[..frame.samples_per_channel]
                            .iter()
                            .all(|v| v.is_finite())
                    );
                }
            }
        }
        decoder.reset();
        // A silent CELT frame for every stream still decodes.
        let streams = usize::from(config[9] > 2) + 1;
        let mut packet = [0xf8, 0].repeat(streams - 1);
        packet.push(0xf8);
        decoder.decode(&packet, &mut frame).unwrap();
        assert_eq!(frame.samples_per_channel, 960);
    }
}

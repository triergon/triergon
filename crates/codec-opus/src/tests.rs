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

/// Decoders that decode `before`, one of which is refused `refused`, decode `next` to
/// the same samples: a refusal changes no state.
fn assert_refusal_keeps_state(config: &[u8], before: &[&[u8]], refused: &[u8], next: &[&[u8]]) {
    let mut decoders = [Decoder::new(config).unwrap(), Decoder::new(config).unwrap()];
    let mut frame = AudioFrame::default();
    let mut outputs = [Vec::new(), Vec::new()];
    for (index, decoder) in decoders.iter_mut().enumerate() {
        for packet in before {
            decoder.decode(packet, &mut frame).unwrap();
        }
        if index == 1 {
            assert!(decoder.decode(refused, &mut frame).is_err());
        }
        for packet in next {
            decoder.decode(packet, &mut frame).unwrap();
            let samples = frame
                .planes
                .iter()
                .flat_map(|p| &p[..frame.samples_per_channel]);
            outputs[index].extend(samples.map(|v| v.to_bits()));
            outputs[index].push(decoder.final_range());
        }
    }
    assert!(
        outputs[0] == outputs[1],
        "a refused packet changed the decoder"
    );
}

/// Prefix the self-delimiting length to a one-frame packet.
fn delimited(packet: &[u8]) -> Vec<u8> {
    let mut out = vec![packet[0], (packet.len() - 1) as u8];
    out.extend_from_slice(&packet[1..]);
    out
}

#[test]
fn a_too_small_budget_is_refused_and_decoding_resumes() {
    let config = head(2, 0, 0, 0, &[], 0);
    let error = Decoder::with_limits(
        &config,
        Limits {
            live_storage_bytes: 1024,
        },
    )
    .unwrap_err();
    assert!(matches!(error, DecodeError::Budget(_)), "{error:?}");

    // A pool another user leaves too little of for the output frame.
    let budget = Budget::new(1 << 20).unwrap();
    let idle = budget.used();
    let mut decoder = Decoder::new_in(&config, &budget).unwrap();
    let other = budget.reserve(budget.available() - 256).unwrap();
    let mut frame = AudioFrame::default();
    let mut seed = 1;
    let packets: Vec<Vec<u8>> = (0..3).map(|_| celt_packet(true, 100, &mut seed)).collect();
    let failure = decoder.decode(&packets[0], &mut frame).unwrap_err();
    assert!(failure.0.starts_with("Budget("), "{failure:?}");

    // Once the pool has room, decoding goes on as in a decoder that was never refused.
    drop(other);
    let mut fresh = Decoder::new(&config).unwrap();
    let mut expected = AudioFrame::default();
    for packet in &packets {
        fresh.decode(packet, &mut expected).unwrap();
        decoder.decode(packet, &mut frame).unwrap();
        assert_eq!(frame.planes, expected.planes);
    }
    drop(decoder);
    assert_eq!(
        budget.used(),
        idle,
        "the decoder returns its charge when dropped"
    );
}

#[test]
fn the_default_limit_holds_the_largest_configuration() {
    // 93 output channels (the most a layout holds) from 255 streams.
    let mapping: Vec<u8> = (0..93).collect();
    let config = head(93, 255, 255, 0, &mapping, 0);
    let mut decoder = Decoder::new(&config).unwrap();
    // 120 ms in every stream: six empty 20 ms CELT frames, each a silence frame.
    let mut packet = [0xfb, 6, 0].repeat(254);
    packet.extend_from_slice(&[0xfb, 6]);
    let mut frame = AudioFrame::default();
    decoder.decode(&packet, &mut frame).unwrap();
    assert_eq!((frame.planes.len(), frame.samples_per_channel), (93, 5760));
}

#[test]
fn a_multistream_packet_with_a_malformed_last_stream_changes_nothing() {
    let config = head(4, 255, 2, 1, &[0, 255, 2, 1], 0);
    let mut seed = 5;
    let packet = |seed: &mut u32| {
        let mut packet = delimited(&celt_packet(true, 60, seed));
        packet.extend_from_slice(&celt_packet(false, 40, seed));
        packet
    };
    let before = [packet(&mut seed), packet(&mut seed)];
    let next = [packet(&mut seed), packet(&mut seed)];
    let first = delimited(&celt_packet(true, 60, &mut seed));
    // Code 1 needs an even payload (R3).
    let refused = [&first[..], &[0xf9, 1, 2, 3]].concat();
    let before = before.each_ref().map(|p| &p[..]);
    let next = next.each_ref().map(|p| &p[..]);
    assert_refusal_keeps_state(&config, &before, &refused, &next);
    // Streams of different durations are refused alike.
    let mut uneven = refused.clone();
    let last = first.len();
    uneven.truncate(last);
    uneven.extend_from_slice(&celt_packet(false, 40, &mut seed));
    uneven[last] = 0xf0; // 10 ms
    assert_refusal_keeps_state(&config, &before, &uneven, &next);
}

#[test]
fn a_hybrid_frame_whose_redundant_frame_does_not_fit_changes_nothing() {
    // Fullband 20 ms stereo hybrid frames of pseudo-random bytes. After its SILK layer,
    // the frame from this seed signals a redundant frame larger than what is left.
    let toc = 15 << 3 | 4;
    let hybrid = |seed: &mut u32| [&[toc][..], &celt_packet(true, 48, seed)[1..]].concat();
    let refused_frame = hybrid(&mut 3_371_809_081)[1..].to_vec();
    let config = head(2, 0, 0, 0, &[], 0);
    let mut frame = AudioFrame::default();
    let failure = Decoder::new(&config)
        .unwrap()
        .decode(&[&[toc][..], &refused_frame].concat(), &mut frame)
        .unwrap_err();
    assert!(failure.0.contains("redundant frame size"), "{failure:?}");
    let mut seed = 9;
    let packets: Vec<Vec<u8>> = (0..5).map(|_| hybrid(&mut seed)).collect();
    // A decodable first frame, then the refused one (code 2).
    let first = &packets[0][1..];
    let refused = [&[toc | 2, first.len() as u8][..], first, &refused_frame].concat();
    let before = [&packets[1][..], &packets[2][..]];
    let next = [&packets[3][..], &packets[4][..]];
    assert_refusal_keeps_state(&config, &before, &refused, &next);
}

#[test]
fn headers_map_channels_to_ffmpeg_layouts() {
    use Speaker::*;
    let stereo = Decoder::new(&head(2, 0, 0, 0, &[], 0)).unwrap();
    assert_eq!(stereo.layout().speakers(), &[FrontLeft, FrontRight]);
    let mono = Decoder::new(&head(1, 0, 0, 0, &[], 0)).unwrap();
    assert_eq!(mono.layout().speakers(), &[FrontCenter]);
    let surround = head(6, 1, 4, 2, &[0, 4, 1, 2, 3, 5], 0);
    let surround = Decoder::new(&surround).unwrap();
    assert_eq!(surround.layout().mask(), 0x3f);
    let unordered = Decoder::new(&head(3, 255, 2, 1, &[0, 1, 2], 0)).unwrap();
    assert_eq!(unordered.layout().channels(), 3);
    assert_eq!(unordered.layout().mask(), 0);
    assert!(matches!(
        Decoder::new(&head(4, 2, 2, 2, &[0, 1, 2, 3], 0)),
        Err(DecodeError::Unsupported(_))
    ));
    assert!(matches!(
        Decoder::new(&head(9, 1, 9, 0, &[0, 1, 2, 3, 4, 5, 6, 7, 8], 0)),
        Err(DecodeError::Invalid(_))
    ));
    assert!(Decoder::new(b"OpusTags").is_err());
    let channels: Vec<u8> = (0..94).collect();
    assert!(matches!(
        Decoder::new(&head(94, 255, 94, 0, &channels, 0)),
        Err(DecodeError::Unsupported(_))
    ));
}

#[test]
fn multistream_packets_fill_every_mapped_plane_and_silence_the_rest() {
    // Two streams: a coupled pair and a mono stream; plane 1 takes no stream.
    let config = head(4, 255, 2, 1, &[0, 255, 2, 1], 0);
    let mut decoder = Decoder::new(&config).unwrap();
    let mut seed = 7;
    // Self-delimited first stream: TOC, the frame length, the frame.
    let mut packet = delimited(&celt_packet(true, 60, &mut seed));
    packet.extend_from_slice(&celt_packet(false, 40, &mut seed));
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
}

#[test]
fn the_output_gain_scales_every_sample() {
    let mut seed = 3;
    let packets: Vec<Vec<u8>> = (0..4).map(|_| celt_packet(true, 120, &mut seed)).collect();
    let decode = |gain: i16| {
        let mut decoder = Decoder::new(&head(2, 0, 0, 0, &[], gain)).unwrap();
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
        let mut decoder = Decoder::new(config).unwrap();
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

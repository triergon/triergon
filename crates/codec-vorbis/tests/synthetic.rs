//! Decoder behaviour on authored streams: budgets, malformed packets, recovery.

use triergon_codec::{AudioDecoder, AudioFrame, Budget};
use triergon_codec_vorbis::{DecodeError, Decoder, Limits};

/// LSB-first bit packing (Vorbis I section 2).
#[derive(Default)]
struct Bits {
    bytes: Vec<u8>,
    at: usize,
}

impl Bits {
    fn put(&mut self, value: u64, width: u32) -> &mut Self {
        for bit in 0..width {
            if self.at.is_multiple_of(8) {
                self.bytes.push(0);
            }
            if value >> bit & 1 != 0 {
                *self.bytes.last_mut().unwrap() |= 1 << (self.at % 8);
            }
            self.at += 1;
        }
        self
    }
}

fn identification(channels: u8, sizes: u8) -> Vec<u8> {
    let mut id = b"\x01vorbis".to_vec();
    id.extend_from_slice(&0u32.to_le_bytes());
    id.push(channels);
    id.extend_from_slice(&48_000u32.to_le_bytes());
    id.extend_from_slice(&[0; 12]);
    id.push(sizes);
    id.push(1);
    id
}

fn comment() -> Vec<u8> {
    let mut c = b"\x03vorbis".to_vec();
    c.extend_from_slice(&0u32.to_le_bytes());
    c.extend_from_slice(&0u32.to_le_bytes());
    c.push(1);
    c
}

/// A setup with one two-entry VQ codebook (values 0 and 1), one floor 1 without
/// partitions, one type 2 residue over the whole spectrum with one class coding pass 0,
/// one mapping coupling channels 0 and 1, and a short and a long mode.
fn setup(channels: u32, residue_end: u64) -> Vec<u8> {
    let mut b = Bits::default();
    b.put(0, 8); // one codebook
    b.put(0x56_4342, 24).put(1, 16).put(2, 24); // sync, dimensions 1, 2 entries
    b.put(0, 1).put(0, 1).put(0, 5).put(0, 5); // unordered, dense, lengths 1 and 1
    b.put(1, 4); // lookup type 1
    b.put(0, 32); // minimum 0
    b.put(768 << 21 | 0x10_0000, 32); // delta 1.0
    b.put(0, 4).put(0, 1); // 1-bit values, no sequence
    b.put(0, 1).put(1, 1); // multiplicands 0, 1
    b.put(0, 6).put(0, 16); // one time placeholder
    b.put(0, 6).put(1, 16); // one floor, type 1
    b.put(0, 5).put(0, 2).put(7, 4); // no partitions, multiplier 1, range bits 7
    b.put(0, 6).put(2, 16); // one residue, type 2
    b.put(0, 24).put(residue_end, 24).put(1, 24); // begin, end, partition size 2
    b.put(0, 6).put(0, 8); // one classification, classbook 0
    b.put(1, 3).put(0, 1); // cascade: pass 0
    b.put(0, 8); // pass 0 book
    b.put(0, 6).put(0, 16); // one mapping, type 0
    b.put(0, 1); // one submap
    if channels > 1 {
        let bits = 32 - (channels - 1).leading_zeros();
        b.put(1, 1).put(0, 8).put(0, bits).put(1, bits);
    } else {
        b.put(0, 1);
    }
    b.put(0, 2).put(0, 8).put(0, 8).put(0, 8); // reserved, submap 0: floor 0, residue 0
    b.put(1, 6); // two modes
    b.put(0, 1).put(0, 16).put(0, 16).put(0, 8);
    b.put(1, 1).put(0, 16).put(0, 16).put(0, 8);
    b.put(1, 1); // framing
    let mut packet = b"\x05vorbis".to_vec();
    packet.extend_from_slice(&b.bytes);
    packet
}

/// An audio packet in mode `mode` with every channel's floor at level `y` and the
/// residue entries given as bits.
fn audio(channels: u32, mode: u64, y: u64, residue: &[u64]) -> Vec<u8> {
    let mut b = Bits::default();
    b.put(0, 1).put(mode, 1);
    if mode == 1 {
        b.put(1, 1).put(1, 1);
    }
    for _ in 0..channels {
        b.put(1, 1).put(y, 8).put(y, 8);
    }
    for &bit in residue {
        b.put(bit, 1);
    }
    b.bytes
}

/// The three headers Xiph-laced, as a configuration.
fn laced(identification: &[u8], comment: &[u8], setup: &[u8]) -> Vec<u8> {
    let mut laced = vec![2];
    for header in [identification, comment] {
        laced.extend(std::iter::repeat_n(255, header.len() / 255));
        laced.push((header.len() % 255) as u8);
    }
    laced.extend_from_slice(identification);
    laced.extend_from_slice(comment);
    laced.extend_from_slice(setup);
    laced
}

fn configuration(channels: u8) -> Vec<u8> {
    laced(
        &identification(channels, 0x76),
        &comment(),
        &setup(u32::from(channels), 64),
    )
}

fn decoder(channels: u8, limits: Limits) -> Result<Decoder, DecodeError> {
    Decoder::with_limits(&configuration(channels), limits)
}

#[test]
fn packets_yield_the_span_between_block_centres() {
    let mut decoder = decoder(2, Limits::default()).unwrap();
    let mut frame = AudioFrame::default();
    let mut lengths = Vec::new();
    for mode in [0, 0, 1, 1, 0] {
        decoder
            .decode(&audio(2, mode, 200, &[1; 32]), &mut frame)
            .unwrap();
        lengths.push(frame.samples_per_channel);
        assert!(frame.planes.iter().all(|p| p.iter().all(|v| v.is_finite())));
    }
    // 64 and 128 sample blocks: nothing, 16 + 16, 16 + 32, 32 + 32, 32 + 16.
    assert_eq!(lengths, [0, 32, 48, 64, 48]);
    assert_eq!(frame.layout.channels(), 2);
    assert!(frame.planes[0].iter().any(|&v| v != 0.0));
}

#[test]
fn configuration_is_the_laced_headers() {
    let decoder = Decoder::new(&configuration(1)).unwrap();
    assert_eq!(decoder.sample_rate(), 48_000);
    assert_eq!(decoder.layout().channels(), 1);
    let (id, c, s) = (identification(1, 0x76), comment(), setup(1, 64));
    let budget = Budget::new(1 << 20).unwrap();
    let packets = Decoder::from_headers_in(&id, &c, &s, &budget).unwrap();
    assert_eq!(packets.layout(), decoder.layout());
    assert!(matches!(
        Decoder::new(&s),
        Err(DecodeError::Invalid("Vorbis configuration"))
    ));
}

#[test]
fn a_small_budget_is_refused() {
    let tiny = Limits {
        live_storage_bytes: 1024,
    };
    assert!(matches!(decoder(2, tiny), Err(DecodeError::Budget(_))));
    // A shared pool carries the decoder's charge while it lives.
    let budget = Budget::new(1 << 20).unwrap();
    let eight = laced(&identification(8, 0xd6), &comment(), &setup(8, 64));
    let decoder = Decoder::new_in(&eight, &budget).unwrap();
    let used = budget.used();
    // Eight channels' spectra, overlaps and floors of 4096-sample blocks.
    assert!(used > 8 * 3 * 4 * 1024, "{used}");
    drop(decoder);
    assert_eq!(
        budget.used(),
        Budget::control_storage_bytes(),
        "dropping the decoder returns its charge"
    );
}

#[test]
fn decoding_resumes_after_the_output_frame_is_refused() {
    let budget = Budget::new(256 << 10).unwrap();
    let mut decoder = Decoder::new_in(&configuration(2), &budget).unwrap();
    let mut reference = Decoder::new(&configuration(2)).unwrap();
    let mut frame = AudioFrame::default();
    let mut expected = AudioFrame::default();
    let packet = |mode| audio(2, mode, 200, &[1; 32]);
    decoder.decode(&packet(1), &mut frame).unwrap();
    reference.decode(&packet(1), &mut expected).unwrap();
    // Another holder takes the rest of the pool: the next frame cannot be charged.
    let held = budget.reserve(budget.available()).unwrap();
    assert!(decoder.decode(&packet(1), &mut frame).is_err());
    drop(held);
    // The refused packet left the overlap as it was.
    for mode in [1, 0, 1] {
        decoder.decode(&packet(mode), &mut frame).unwrap();
        reference.decode(&packet(mode), &mut expected).unwrap();
        assert_eq!(frame.samples_per_channel, expected.samples_per_channel);
        assert_eq!(frame.planes, expected.planes);
    }
}

#[test]
fn malformed_packets_fail_without_disturbing_the_decoder() {
    let mut decoder = decoder(2, Limits::default()).unwrap();
    let mut frame = AudioFrame::default();
    let good = audio(2, 1, 180, &[1, 0, 1, 1]);
    decoder.decode(&good, &mut frame).unwrap();
    assert!(decoder.decode(&[], &mut frame).is_err(), "empty packet");
    assert!(
        decoder.decode(&[1], &mut frame).is_err(),
        "header packet type"
    );
    // Truncated in the floor or residue: decodes as silence or partial residue.
    for len in 1..good.len() {
        let _ = decoder.decode(&good[..len], &mut frame);
    }
    let mut seed = 0x1234_5678u32;
    for _ in 0..2000 {
        let len = (seed >> 24) as usize % 40;
        let packet: Vec<u8> = (0..len)
            .map(|_| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (seed >> 16) as u8
            })
            .collect();
        let _ = decoder.decode(&packet, &mut frame);
    }
    decoder.decode(&good, &mut frame).unwrap();
    assert!(frame.samples_per_channel > 0);
}

#[test]
fn reset_drops_the_overlap() {
    let mut decoder = decoder(1, Limits::default()).unwrap();
    let mut frame = AudioFrame::default();
    decoder
        .decode(&audio(1, 0, 200, &[1; 8]), &mut frame)
        .unwrap();
    decoder
        .decode(&audio(1, 0, 200, &[1; 8]), &mut frame)
        .unwrap();
    assert_eq!(frame.samples_per_channel, 32);
    decoder.reset();
    decoder
        .decode(&audio(1, 0, 200, &[1; 8]), &mut frame)
        .unwrap();
    assert_eq!(frame.samples_per_channel, 0);
}

#[test]
fn malformed_setups_are_rejected() {
    let id = identification(2, 0x76);
    let mut setup = setup(2, 64);
    let budget = Budget::new(Limits::default().live_storage_bytes).unwrap();
    for len in 0..setup.len() {
        assert!(Decoder::from_headers_in(&id, &comment(), &setup[..len], &budget).is_err());
    }
    // Break the codebook sync pattern.
    setup[8] ^= 0xff;
    assert!(Decoder::from_headers_in(&id, &comment(), &setup, &budget).is_err());
    let mut seed = 99u32;
    for _ in 0..500 {
        let mut setup = self::setup(2, 64);
        for _ in 0..3 {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let at = 7 + (seed >> 8) as usize % (setup.len() - 7);
            setup[at] ^= 1 << (seed % 8);
        }
        if let Ok(mut decoder) = Decoder::from_headers_in(&id, &comment(), &setup, &budget) {
            let mut frame = AudioFrame::default();
            for mode in [0, 1, 0] {
                let _ = decoder.decode(&audio(2, mode, 100, &[1, 1, 0, 1]), &mut frame);
            }
        }
    }
}

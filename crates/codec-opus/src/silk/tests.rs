//! Whole-stream tests of the SILK layer: malformed input, and a harness that decodes the
//! SILK-only packets of an Ogg Opus file for comparison with a reference decoder.

use super::*;

/// An Opus frame: TOC fields and the frame bytes (RFC 6716 section 3).
struct OpusFrame<'a> {
    config: u8,
    stereo: bool,
    data: &'a [u8],
}

/// Split an Opus packet into its frames (RFC 6716 section 3.2).
fn split_packet(packet: &[u8]) -> Option<Vec<OpusFrame<'_>>> {
    let (&toc, mut rest) = packet.split_first()?;
    let config = toc >> 3;
    let stereo = toc & 4 != 0;
    fn length(rest: &mut &[u8]) -> Option<usize> {
        let (&first, tail) = rest.split_first()?;
        if first < 252 {
            *rest = tail;
            return Some(usize::from(first));
        }
        let (&second, tail) = tail.split_first()?;
        *rest = tail;
        Some(usize::from(second) * 4 + usize::from(first))
    }
    let mut sizes = Vec::new();
    match toc & 3 {
        0 => sizes.push(rest.len()),
        1 => {
            if rest.len() % 2 != 0 {
                return None;
            }
            sizes = vec![rest.len() / 2; 2];
        }
        2 => {
            let first = length(&mut rest)?;
            sizes = vec![first, rest.len().checked_sub(first)?];
        }
        _ => {
            let (&count, tail) = rest.split_first()?;
            rest = tail;
            let frames = usize::from(count & 63);
            if frames == 0 {
                return None;
            }
            let mut padding = 0;
            if count & 64 != 0 {
                loop {
                    let (&p, tail) = rest.split_first()?;
                    rest = tail;
                    padding += if p == 255 { 254 } else { usize::from(p) };
                    if p != 255 {
                        break;
                    }
                }
            }
            if count & 128 != 0 {
                for _ in 0..frames - 1 {
                    sizes.push(length(&mut rest)?);
                }
                let used: usize = sizes.iter().sum();
                sizes.push(rest.len().checked_sub(padding)?.checked_sub(used)?);
            } else {
                let total = rest.len().checked_sub(padding)?;
                if total % frames != 0 {
                    return None;
                }
                sizes = vec![total / frames; frames];
            }
        }
    }
    let mut frames = Vec::new();
    for size in sizes {
        if size > rest.len() {
            return None;
        }
        let (data, tail) = rest.split_at(size);
        frames.push(OpusFrame {
            config,
            stereo,
            data,
        });
        rest = tail;
    }
    Some(frames)
}

/// The packets of the first logical stream of an Ogg file.
fn ogg_packets(file: &[u8]) -> Vec<Vec<u8>> {
    let mut packets = Vec::new();
    let mut partial = Vec::new();
    let mut at = 0;
    while at + 27 <= file.len() && &file[at..at + 4] == b"OggS" {
        let segments = usize::from(file[at + 26]);
        let table = &file[at + 27..at + 27 + segments];
        let mut body = at + 27 + segments;
        for &lace in table {
            partial.extend_from_slice(&file[body..body + usize::from(lace)]);
            body += usize::from(lace);
            if lace < 255 {
                packets.push(std::mem::take(&mut partial));
            }
        }
        at = body;
    }
    packets
}

/// Decode the SILK layer of `packets` (after the two header packets) the way
/// FFmpeg's decoder sequences its resampler: flushed when the internal rate changes, when a
/// CELT-only frame arrives (which is replaced by silence) and at the end. Returns 48 kHz
/// interleaved output and the internal-rate output.
fn decode_stream(packets: &[Vec<u8>]) -> (usize, Vec<f32>, Vec<f32>, usize) {
    let channels = usize::from(packets[0][9]);
    let mut decoder = SilkDecoder::new();
    let mut output = Vec::new();
    let mut internal = Vec::new();
    let mut skipped = 0;
    let mut planes = vec![vec![0.0f32; 2880 + 24]; channels];
    let mut rate = None;
    let push = |planes: &[Vec<f32>], len: usize, output: &mut Vec<f32>| {
        for i in 0..len {
            for plane in planes {
                output.push(plane[i]);
            }
        }
    };
    for packet in &packets[2..] {
        let Some(frames) = split_packet(packet) else {
            skipped += 1;
            continue;
        };
        for frame in frames {
            let config = usize::from(frame.config);
            let duration = if config < 12 {
                [480, 960, 1920, 2880][config % 4]
            } else if config < 16 {
                [480, 960][config % 2]
            } else {
                [120, 240, 480, 960][config % 4]
            };
            // Hybrid frames start with a wideband SILK layer; CELT is not decoded here.
            let frame_rate = match config {
                0..12 => Some([8000, 12000, 16000][config / 4]),
                12..16 => Some(16000),
                _ => None,
            };
            if rate.is_some() && frame_rate != rate {
                let mut out: Vec<&mut [f32]> = planes.iter_mut().map(|p| &mut p[..]).collect();
                let flushed = decoder.flush(&mut out);
                push(&planes, flushed, &mut output);
            }
            rate = frame_rate;
            let Some(frame_rate) = frame_rate else {
                skipped += 1;
                decoder.reset();
                for plane in &mut planes {
                    plane.fill(0.0);
                }
                push(&planes, duration, &mut output);
                continue;
            };
            let mut rc = RangeDecoder::new(frame.data);
            let mut out: Vec<&mut [f32]> = planes.iter_mut().map(|p| &mut p[..duration]).collect();
            let result = decoder.decode(
                &mut rc,
                frame_rate,
                duration,
                1 + usize::from(frame.stereo),
                &mut out,
            );
            let Ok(produced) = result else {
                panic!("{result:?}");
            };
            push(&planes, produced, &mut output);
            // Internal-rate output, always as two channels.
            for i in 0..decoder.internal_len {
                internal.push(decoder.internal[0][i]);
                internal.push(decoder.internal[usize::from(frame.stereo)][i]);
            }
        }
    }
    let mut out: Vec<&mut [f32]> = planes.iter_mut().map(|p| &mut p[..]).collect();
    let flushed = decoder.flush(&mut out);
    push(&planes, flushed, &mut output);
    (channels, output, internal, skipped)
}

/// Decode `SILK_OGG` into `SILK_OUT` (48 kHz, interleaved f32le) and, if set,
/// `SILK_INTERNAL` (the internal-rate signal after stereo unmixing).
#[test]
#[ignore = "needs an Ogg Opus file named by SILK_OGG"]
fn decode_ogg_file() {
    let (Ok(input), Ok(output)) = (std::env::var("SILK_OGG"), std::env::var("SILK_OUT")) else {
        return;
    };
    let file = std::fs::read(input).expect("SILK_OGG readable");
    let packets = ogg_packets(&file);
    let (channels, samples, internal, skipped) = decode_stream(&packets);
    let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
    std::fs::write(output, bytes).expect("SILK_OUT writable");
    if let Ok(path) = std::env::var("SILK_INTERNAL") {
        let bytes: Vec<u8> = internal.iter().flat_map(|s| s.to_le_bytes()).collect();
        std::fs::write(path, bytes).expect("SILK_INTERNAL writable");
    }
    eprintln!(
        "channels {channels} samples {} skipped_frames {skipped}",
        samples.len() / channels
    );
}

#[test]
fn malformed_frames_never_panic() {
    let mut decoder = SilkDecoder::new();
    let mut state = 0x1234_5678u32;
    let mut left = [0.0f32; 2880];
    let mut right = [0.0f32; 2880];
    for round in 0..3000 {
        let len = (round * 7) % 300;
        let data: Vec<u8> = (0..len)
            .map(|_| {
                state = state.wrapping_mul(1664525).wrapping_add(1013904223);
                (state >> 24) as u8
            })
            .collect();
        let rate = [8000, 12000, 16000][round % 3];
        let duration = [480, 960, 1920, 2880][(round / 3) % 4];
        let channels = 1 + (round / 12) % 2;
        let mut rc = RangeDecoder::new(&data);
        let mut out: [&mut [f32]; 2] = [&mut left[..duration], &mut right[..duration]];
        let result = decoder.decode(&mut rc, rate, duration, channels, &mut out[..1 + round % 2]);
        assert!(result.is_ok());
        // The interpolation filters have a worst-case gain below 1.91 for clamped input.
        let worst = left[..duration].iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(worst.is_finite() && worst <= 1.91, "{worst}");
    }
}

#[test]
fn invalid_parameters_are_refused() {
    let mut decoder = SilkDecoder::new();
    let mut plane = [0.0f32; 960];
    let mut rc = RangeDecoder::new(&[0; 8]);
    let mut out: [&mut [f32]; 1] = [&mut plane];
    assert!(decoder.decode(&mut rc, 24000, 960, 1, &mut out).is_err());
    assert!(decoder.decode(&mut rc, 16000, 240, 1, &mut out).is_err());
    assert!(decoder.decode(&mut rc, 16000, 960, 3, &mut out).is_err());
    assert!(decoder.decode(&mut rc, 16000, 480, 1, &mut out).is_err());
}

#[test]
fn packet_splitting_follows_the_frame_codes() {
    assert_eq!(split_packet(&[0x08, 1, 2, 3]).map(|f| f.len()), Some(1));
    assert_eq!(split_packet(&[0x09, 1, 2, 3, 4]).map(|f| f.len()), Some(2));
    assert_eq!(
        split_packet(&[0x0a, 1, 9, 3, 4]).map(|f| f[1].data.len()),
        Some(2)
    );
    assert_eq!(
        split_packet(&[0x0b, 0x03, 1, 2, 3]).map(|f| f.len()),
        Some(3)
    );
}

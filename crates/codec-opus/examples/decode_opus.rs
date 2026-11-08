#![forbid(unsafe_code)]
//! Decode an Opus file to interleaved f32le, planes in FFmpeg's channel order.
//!
//! usage: decode_opus INPUT OUTPUT.f32 [--channels N]
//!
//! INPUT is an RFC 6716 test vector (`.bit`: per packet a 4-byte big-endian length, the
//! 4-byte final range and the packet; `--channels` sets the output channels, default 2,
//! and the final ranges are checked against the decoder's) or any container the
//! `triergon` facade reads (Matroska/WebM, Ogg), whose first Opus track is decoded. The
//! decoder's whole output is written, without the pre-skip or other container trims.
//! Prints one JSON line to stderr: format, packet and sample counts, errors and the last
//! error.

use std::io::Write;
use std::process::ExitCode;

use triergon::{Budget, CodecId, ContainerReader, MediaKind, PacketInfo};
use triergon_codec::{AudioDecoder, AudioFrame};
use triergon_codec_opus::{Decoder, Limits};

/// The packets and final ranges of an RFC 6716 test vector.
fn read_vector(path: &str) -> std::io::Result<Vec<(u32, Vec<u8>)>> {
    let data = std::fs::read(path)?;
    let mut packets = Vec::new();
    let mut rest = &data[..];
    while let [a, b, c, d, r0, r1, r2, r3, ..] = *rest {
        let len = u32::from_be_bytes([a, b, c, d]) as usize;
        let range = u32::from_be_bytes([r0, r1, r2, r3]);
        let packet = rest
            .get(8..8 + len)
            .ok_or_else(|| std::io::Error::other("truncated test vector"))?;
        packets.push((range, packet.to_vec()));
        rest = &rest[8 + len..];
    }
    Ok(packets)
}

/// The output file and what has been decoded into it.
struct Output {
    out: std::io::BufWriter<std::fs::File>,
    frames: usize,
    samples: usize,
    errors: usize,
    last_error: String,
    /// Packets decoded with a final range check, mismatches and the first mismatch.
    checked: usize,
    range_mismatches: usize,
    first_mismatch: Option<usize>,
    interleaved: Vec<u8>,
}

impl Output {
    /// Decode `packet`, checking the decoder's final range against `range` if given.
    fn decode(
        &mut self,
        decoder: &mut Decoder,
        frame: &mut AudioFrame,
        packet: &[u8],
        range: Option<u32>,
    ) -> std::io::Result<()> {
        if let Some(range) = range {
            self.checked += 1;
            if let Err(error) = decoder.decode(packet, frame) {
                self.fail(error.0);
                return Ok(());
            }
            if range != decoder.final_range() {
                self.range_mismatches += 1;
                self.first_mismatch.get_or_insert(self.checked - 1);
            }
        } else if let Err(error) = decoder.decode(packet, frame) {
            self.fail(error.0);
            return Ok(());
        }
        self.write(frame)
    }

    fn fail(&mut self, error: String) {
        self.errors += 1;
        self.last_error = error;
    }

    fn write(&mut self, frame: &AudioFrame) -> std::io::Result<()> {
        self.frames += 1;
        self.samples += frame.samples_per_channel;
        self.interleaved.clear();
        for i in 0..frame.samples_per_channel {
            for plane in &frame.planes {
                self.interleaved.extend_from_slice(&plane[i].to_le_bytes());
            }
        }
        self.out.write_all(&self.interleaved)
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let (Some(input), Some(output)) = (args.get(1), args.get(2)) else {
        eprintln!("usage: decode_opus INPUT OUTPUT.f32 [--channels N]");
        return ExitCode::from(2);
    };
    let options = &args[3..];
    let channels = options
        .iter()
        .position(|a| a == "--channels")
        .and_then(|i| options.get(i + 1)?.parse().ok())
        .unwrap_or(2);
    let mut out = match std::fs::File::create(output) {
        Ok(file) => Output {
            out: std::io::BufWriter::new(file),
            frames: 0,
            samples: 0,
            errors: 0,
            last_error: String::new(),
            checked: 0,
            range_mismatches: 0,
            first_mismatch: None,
            interleaved: Vec::new(),
        },
        Err(error) => {
            eprintln!("{output}: {error}");
            return ExitCode::FAILURE;
        }
    };
    let vector = std::path::Path::new(input)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("bit"));
    let decoded = if vector {
        decode_vector(input, channels, &mut out)
    } else {
        decode_container(input, &mut out)
    };
    let decoder = match decoded {
        Ok(decoder) => decoder,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(error) = out.out.flush() {
        eprintln!("{output}: {error}");
        return ExitCode::FAILURE;
    }
    let layout = decoder.layout();
    let ranges = if vector {
        let first = out
            .first_mismatch
            .map_or("null".into(), |i: usize| i.to_string());
        format!(
            ",\"range_mismatches\":{},\"first_range_mismatch\":{first}",
            out.range_mismatches
        )
    } else {
        String::new()
    };
    eprintln!(
        "{{\"sample_rate\":48000,\"channels\":{},\"layout\":\"{}\",\"frames\":{},\"samples_per_channel\":{},\"errors\":{},\"last_error\":{:?}{ranges}}}",
        layout.channels(),
        // ffprobe's name for a layout of unspecified order.
        if layout.mask() == 0 {
            "unknown".into()
        } else {
            layout.name()
        },
        out.frames,
        out.samples,
        out.errors,
        out.last_error,
    );
    ExitCode::SUCCESS
}

/// Decode a test vector with an `OpusHead` of `channels` at 48 kHz.
fn decode_vector(path: &str, channels: u8, out: &mut Output) -> Result<Decoder, String> {
    let packets = read_vector(path).map_err(|e| format!("{path}: {e}"))?;
    let mut head = b"OpusHead\x01".to_vec();
    head.push(channels);
    head.extend_from_slice(&[0, 0, 0x80, 0xbb, 0, 0, 0, 0, 0]);
    let mut decoder = Decoder::from_configuration(&head, Limits::default())
        .map_err(|e| format!("configuration: {e}"))?;
    let mut frame = AudioFrame::default();
    for (range, packet) in &packets {
        out.decode(&mut decoder, &mut frame, packet, Some(*range))
            .map_err(|e| e.to_string())?;
    }
    drain(&mut decoder, &mut frame, out)?;
    Ok(decoder)
}

/// Decode the first Opus track of a container.
fn decode_container(path: &str, out: &mut Output) -> Result<Decoder, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("{path}: {e}"))?;
    let pool = Budget::new(256 << 20).map_err(|e| e.to_string())?;
    let mut source = triergon::open_blocking(std::io::BufReader::new(file), &pool)
        .map_err(|e| format!("{path}: {e}"))?;
    let mut decoder: Option<(triergon::TrackId, Decoder)> = None;
    let mut frame = AudioFrame::default();
    loop {
        let packet = match source.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
            Err(error) => {
                out.fail(format!("read: {error}"));
                break;
            }
        };
        if decoder.is_none() {
            let reader = source.reader();
            let Some(track) = (0..reader.track_count())
                .filter_map(|i| reader.track(i))
                .find(|t| t.kind() == MediaKind::Audio && t.codec() == CodecId::Opus)
            else {
                continue;
            };
            let mut head = Vec::new();
            triergon_framing::canonical_opus_configuration(track.configuration(), &mut head)
                .map_err(|e| format!("configuration: {e}"))?;
            let built = Decoder::from_configuration(&head, Limits::default())
                .map_err(|e| format!("configuration: {e}"))?;
            decoder = Some((track.id(), built));
        }
        let Some((track, decoder)) = decoder.as_mut() else {
            continue;
        };
        if packet.track() == *track {
            out.decode(decoder, &mut frame, packet.payload(), None)
                .map_err(|e| e.to_string())?;
        }
    }
    let (_, mut decoder) = decoder.ok_or_else(|| format!("{path}: no Opus track"))?;
    drain(&mut decoder, &mut frame, out)?;
    Ok(decoder)
}

/// Write whatever the decoder still holds back.
fn drain(decoder: &mut Decoder, frame: &mut AudioFrame, out: &mut Output) -> Result<(), String> {
    if decoder.flush(frame).is_ok() && frame.samples_per_channel > 0 {
        out.write(frame).map_err(|e| e.to_string())?;
    }
    Ok(())
}

//! Decode an Opus file to interleaved f32le, planes in FFmpeg's channel order.
//!
//! usage: decode_opus INPUT OUTPUT.f32 [--pre-skip] [--channels N]
//!
//! INPUT is Matroska/WebM (`.mka`, `.mkv`, `.webm`; the first Opus track), Ogg (`.opus`,
//! `.ogg`; the first logical stream) or an RFC 6716 test vector (`.bit`: per packet a
//! 4-byte big-endian length, the 4-byte final range and the packet; `--channels` sets
//! the output channels, default 2, and the final ranges are checked against the
//! decoder's). The decoder's
//! whole output is written unless `--pre-skip` drops the header's pre-skip from the
//! start. Prints one JSON line to stderr: format, packet and sample counts, errors and
//! the last error.

use std::io::Write;
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;

use triergon_codec::{AudioDecoder, AudioFrame, ChannelLayout};
use triergon_codec_opus::{Decoder, Limits};

/// The identification header and packets of the input.
struct Input {
    head: Vec<u8>,
    packets: Vec<Vec<u8>>,
    /// Final range per packet (test vectors only).
    ranges: Option<Vec<u32>>,
}

fn read_input(path: &str, channels: u8) -> Result<Input, Box<dyn std::error::Error>> {
    let extension = path
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    match extension.as_str() {
        "bit" => {
            let data = std::fs::read(path)?;
            let (mut packets, mut ranges) = (Vec::new(), Vec::new());
            let mut rest = &data[..];
            while rest.len() >= 8 {
                let len = u32::from_be_bytes(rest[..4].try_into()?) as usize;
                ranges.push(u32::from_be_bytes(rest[4..8].try_into()?));
                let packet = rest.get(8..8 + len).ok_or("truncated test vector")?;
                packets.push(packet.to_vec());
                rest = &rest[8 + len..];
            }
            let mut head = b"OpusHead\x01".to_vec();
            head.push(channels);
            head.extend_from_slice(&[0, 0, 0x80, 0xbb, 0, 0, 0, 0, 0]);
            Ok(Input {
                head,
                packets,
                ranges: Some(ranges),
            })
        }
        "opus" | "ogg" => {
            let packets = triergon_ogg::read_packets(&std::fs::read(path)?)?;
            let first = packets
                .iter()
                .find(|p| p.data.starts_with(b"OpusHead"))
                .ok_or("no Opus stream")?;
            let serial = first.serial;
            let mut stream = packets
                .into_iter()
                .filter(|p| p.serial == serial)
                .map(|p| p.data);
            let head = stream.next().ok_or("no Opus stream")?;
            // The comment header.
            let _ = stream.next();
            Ok(Input {
                head,
                packets: stream.collect(),
                ranges: None,
            })
        }
        _ => {
            use triergon_matroska::{Limits, typed::BlockingReader};
            let file = std::io::BufReader::new(std::fs::File::open(path)?);
            let mut reader = BlockingReader::open_seekable(file, Limits::default())?;
            let track = reader
                .tracks()
                .iter()
                .position(|t| &*t.codec == "A_OPUS")
                .ok_or("no Opus track")?;
            let head = reader.tracks()[track].codec_private.to_vec();
            let cancel = AtomicBool::new(false);
            let mut packets = Vec::new();
            while let Some(packet) = reader.next_typed_packet(&cancel)? {
                if packet.packet.track == track {
                    packets.push(packet.packet.data.to_vec());
                }
            }
            Ok(Input {
                head,
                packets,
                ranges: None,
            })
        }
    }
}

/// The name ffprobe prints for `layout`.
fn layout_name(layout: &ChannelLayout) -> String {
    const NAMES: [(&str, &str); 8] = [
        ("mono", "FC"),
        ("stereo", "FL+FR"),
        ("3.0", "FL+FR+FC"),
        ("quad", "FL+FR+BL+BR"),
        ("5.0", "FL+FR+FC+BL+BR"),
        ("5.1", "FL+FR+FC+LFE+BL+BR"),
        ("6.1", "FL+FR+FC+LFE+BC+SL+SR"),
        ("7.1", "FL+FR+FC+LFE+BL+BR+SL+SR"),
    ];
    if layout.mask() == 0 {
        return "unknown".into();
    }
    let speakers: Vec<&str> = layout.speakers().iter().map(|s| s.name()).collect();
    let speakers = speakers.join("+");
    NAMES
        .iter()
        .find(|(_, s)| *s == speakers)
        .map_or(speakers.clone(), |(name, _)| (*name).into())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let (Some(input), Some(output)) = (args.get(1), args.get(2)) else {
        eprintln!("usage: decode_opus INPUT OUTPUT.f32 [--pre-skip] [--channels N]");
        return ExitCode::from(2);
    };
    let options = &args[3..];
    let apply_pre_skip = options.iter().any(|a| a == "--pre-skip");
    let channels = options
        .iter()
        .position(|a| a == "--channels")
        .and_then(|i| options.get(i + 1)?.parse().ok())
        .unwrap_or(2);
    let input = match read_input(input, channels) {
        Ok(input) => input,
        Err(error) => {
            eprintln!("{input}: {error}");
            return ExitCode::FAILURE;
        }
    };
    let mut decoder = match Decoder::from_configuration(&input.head, Limits::default()) {
        Ok(decoder) => decoder,
        Err(error) => {
            eprintln!("configuration: {error}");
            return ExitCode::FAILURE;
        }
    };
    let mut out = match std::fs::File::create(output) {
        Ok(file) => std::io::BufWriter::new(file),
        Err(error) => {
            eprintln!("{output}: {error}");
            return ExitCode::FAILURE;
        }
    };
    let mut skip = if apply_pre_skip {
        usize::from(u16::from_le_bytes([input.head[10], input.head[11]]))
    } else {
        0
    };

    let (mut frames, mut samples, mut errors, mut range_mismatches) =
        (0usize, 0usize, 0usize, 0usize);
    let mut first_mismatch = None;
    let mut last_error = String::new();
    let mut frame = AudioFrame::default();
    let mut interleaved = Vec::new();
    // The packets, then whatever the decoder still holds back.
    let count = input.packets.len();
    for index in 0..=count {
        if index == count {
            if decoder.flush(&mut frame).is_err() || frame.samples_per_channel == 0 {
                break;
            }
        } else if let Err(error) = decoder.decode(&input.packets[index], &mut frame) {
            errors += 1;
            last_error = error.0;
            continue;
        }
        frames += 1;
        if let Some(ranges) = input.ranges.as_ref().filter(|_| index < count)
            && ranges.get(index) != Some(&decoder.final_range())
        {
            range_mismatches += 1;
            first_mismatch.get_or_insert(index);
        }
        let n = frame.samples_per_channel;
        let first = skip.min(n);
        skip -= first;
        interleaved.clear();
        for i in first..n {
            for plane in &frame.planes {
                interleaved.extend_from_slice(&plane[i].to_le_bytes());
            }
        }
        samples += n - first;
        if let Err(error) = out.write_all(&interleaved) {
            eprintln!("{output}: {error}");
            return ExitCode::FAILURE;
        }
    }
    if let Err(error) = out.flush() {
        eprintln!("{output}: {error}");
        return ExitCode::FAILURE;
    }
    let layout = decoder.layout();
    let ranges = input
        .ranges
        .map(|_| {
            let first = first_mismatch.map_or("null".into(), |i: usize| i.to_string());
            format!(",\"range_mismatches\":{range_mismatches},\"first_range_mismatch\":{first}")
        })
        .unwrap_or_default();
    eprintln!(
        "{{\"sample_rate\":48000,\"channels\":{},\"layout\":\"{}\",\"frames\":{frames},\"samples_per_channel\":{samples},\"errors\":{errors},\"last_error\":{:?}{ranges}}}",
        layout.channels(),
        layout_name(&layout),
        last_error,
    );
    ExitCode::SUCCESS
}

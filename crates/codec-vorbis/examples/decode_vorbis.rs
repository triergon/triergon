//! Decode an Ogg Vorbis file to interleaved f32le, planes in FFmpeg's channel order.
//!
//! usage: decode_vorbis INPUT OUTPUT.f32 [--untrimmed] [--per-packet]
//!
//! Decodes the first logical stream and the streams chained after it the way FFmpeg
//! does: a link whose headers repeat the current ones continues the same decoder, so
//! its first packet laps the previous link's last block; a link with new headers starts
//! a new decoder; a link that changes the channel count or sample rate ends decoding,
//! as FFmpeg's Ogg demuxer refuses it. By default the last packet of each link is
//! trimmed to the link's final granule position; `--untrimmed` keeps the decoder's
//! whole output. Prints one JSON line to stderr: the first link's format, packet and
//! sample counts, errors and the last error.

use std::io::Write;
use std::process::ExitCode;

use triergon_codec::{AudioDecoder, AudioFrame, Budget, ChannelLayout};
use triergon_codec_vorbis::{Decoder, Limits};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let (Some(input), Some(output)) = (args.get(1), args.get(2)) else {
        eprintln!("usage: decode_vorbis INPUT OUTPUT.f32 [--untrimmed] [--per-packet]");
        return ExitCode::from(2);
    };
    let flag = |name: &str| args[3..].iter().any(|a| a == name);
    let (trim, per_packet) = (!flag("--untrimmed"), flag("--per-packet"));
    let packets = match std::fs::read(input).map(|data| triergon_ogg::read_packets(&data)) {
        Ok(Ok(packets)) => packets,
        Ok(Err(error)) => {
            eprintln!("{input}: {error}");
            return ExitCode::FAILURE;
        }
        Err(error) => {
            eprintln!("{input}: {error}");
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

    let mut first: Option<(u32, ChannelLayout)> = None;
    let (mut decoded, mut samples, mut errors) = (0usize, 0usize, 0usize);
    let mut last_error = String::new();
    let mut frame = AudioFrame::default();
    let mut interleaved = Vec::new();
    let mut current: Option<(Decoder, [&[u8]; 3], Budget)> = None;
    let mut peak = 0;
    for chain in chains(&packets) {
        let continues = current.as_ref().is_some_and(|(_, headers, _)| {
            headers[0] == chain.headers[0] && headers[2] == chain.headers[2]
        });
        if !continues {
            let [identification, comment, setup] = chain.headers;
            let budget = match Budget::new(Limits::default().live_storage_bytes) {
                Ok(budget) => budget,
                Err(error) => {
                    eprintln!("{error}");
                    return ExitCode::FAILURE;
                }
            };
            match Decoder::from_headers_in(identification, comment, setup, &budget) {
                Ok(decoder) => {
                    let format = (decoder.sample_rate(), decoder.layout());
                    if first.is_some_and(|first| first != format) {
                        errors += 1;
                        last_error = "chained stream changes the sample rate or channels".into();
                        break;
                    }
                    first = Some(format);
                    current = Some((decoder, chain.headers, budget));
                }
                Err(error) => {
                    errors += 1;
                    last_error = error.to_string();
                    current = None;
                    continue;
                }
            }
        }
        let Some((decoder, _, budget)) = current.as_mut() else {
            continue;
        };
        // Granule position of the end of this link's output so far; the output of the
        // link's first packet precedes position 0.
        let mut position = None;
        for packet in chain.audio {
            if let Err(error) = decoder.decode(&packet.data, &mut frame) {
                errors += 1;
                last_error = error.0;
                continue;
            }
            decoded += 1;
            let mut len = frame.samples_per_channel;
            let end = position.map_or(0, |position| position + len as i64);
            position = Some(end);
            if let (true, true, Some(granule)) = (trim, packet.end_of_stream, packet.granule) {
                len -= (end - granule).clamp(0, len as i64) as usize;
            }
            if per_packet {
                eprintln!(
                    "{{\"serial\":{},\"bytes\":{},\"samples\":{},\"kept\":{len},\"position\":{end},\"granule\":{}}}",
                    packet.serial,
                    packet.data.len(),
                    frame.samples_per_channel,
                    packet
                        .granule
                        .map_or_else(|| "null".to_string(), |g| g.to_string()),
                );
            }
            samples += len;
            interleaved.clear();
            for i in 0..len {
                for plane in &frame.planes {
                    interleaved.extend_from_slice(&plane[i].to_le_bytes());
                }
            }
            if let Err(error) = out.write_all(&interleaved) {
                eprintln!("{output}: {error}");
                return ExitCode::FAILURE;
            }
        }
        peak = peak.max(budget.peak());
    }
    if let Err(error) = out.flush() {
        eprintln!("{output}: {error}");
        return ExitCode::FAILURE;
    }
    let (rate, layout) = first.unwrap_or((0, ChannelLayout::EMPTY));
    eprintln!(
        "{{\"sample_rate\":{rate},\"channels\":{},\"layout\":\"{}\",\"frames\":{decoded},\"samples_per_channel\":{samples},\"errors\":{errors},\"last_error\":\"{}\",\"storage_peak\":{peak}}}",
        layout.channels(),
        layout_name(&layout),
        last_error.replace('"', "'")
    );
    ExitCode::SUCCESS
}

/// One logical Vorbis stream: its three headers and its audio packets.
struct Chain<'a> {
    headers: [&'a [u8]; 3],
    audio: Vec<&'a triergon_ogg::SlicePacket>,
}

/// The first Vorbis stream and the streams chained after it, in file order. A stream
/// starting while the current one has not ended is multiplexed with it and skipped.
fn chains(packets: &[triergon_ogg::SlicePacket]) -> Vec<Chain<'_>> {
    let mut chains: Vec<Chain<'_>> = Vec::new();
    let mut current: Option<(u32, bool)> = None;
    let mut headers: Vec<&[u8]> = Vec::new();
    for packet in packets {
        let ours = current.is_some_and(|(serial, _)| serial == packet.serial);
        if !ours {
            let open = current.is_some_and(|(_, ended)| !ended);
            if open || !packet.data.starts_with(b"\x01vorbis") {
                continue;
            }
            current = Some((packet.serial, false));
            headers.clear();
        }
        if packet.end_of_stream {
            current = current.map(|(serial, _)| (serial, true));
        }
        if headers.len() < 3 {
            headers.push(&packet.data);
            if let [identification, comment, setup] = headers[..] {
                chains.push(Chain {
                    headers: [identification, comment, setup],
                    audio: Vec::new(),
                });
            }
            continue;
        }
        if let Some(chain) = chains.last_mut() {
            chain.audio.push(packet);
        }
    }
    chains
}

/// FFmpeg's name for a channel layout (as ffprobe prints it).
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
        // Beyond eight channels FFmpeg reports an unspecified order.
        return "unknown".to_string();
    }
    let speakers: Vec<&str> = layout.speakers().iter().map(|s| s.name()).collect();
    let speakers = speakers.join("+");
    NAMES
        .iter()
        .find(|(_, decomposition)| *decomposition == speakers)
        .map_or_else(
            || format!("{} channels ({speakers})", layout.channels()),
            |(name, _)| (*name).to_string(),
        )
}

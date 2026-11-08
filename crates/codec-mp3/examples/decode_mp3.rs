#![forbid(unsafe_code)]
//! Decode the first MPEG audio track of a file to interleaved f32le, each frame with its
//! own channels. The file (a raw MPEG audio stream, or a container) is read through the
//! `triergon` facade; the decoder's whole output is written, without container trims.
//!
//! usage: decode_mp3 INPUT OUTPUT.f32 [--reset-at N]
//!
//! `--reset-at N` resets the decoder before access unit `N` (counted from 0), as a
//! seek would.
//!
//! Prints one JSON line to stderr: sample rate, channels and layout of the first frame,
//! frames and samples decoded, errors and the last error.

use std::io::Write;

use triergon::{Budget, CodecId, ContainerReader, MediaKind, PacketInfo, TrackInfo};
use triergon_codec::{AudioDecoder, AudioFrame};
use triergon_codec_mp3::Decoder;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let reset_at = match args.as_slice() {
        [_, _, _] => None,
        [_, _, _, flag, n] if flag == "--reset-at" => n.parse::<usize>().ok(),
        _ => {
            eprintln!("usage: decode_mp3 INPUT OUTPUT.f32 [--reset-at N]");
            std::process::exit(2);
        }
    };
    let mut output =
        std::io::BufWriter::new(std::fs::File::create(&args[2]).expect("create output"));
    let mut decoder = Decoder::new();
    let mut track = None;
    let mut frame = AudioFrame::default();
    let (mut index, mut frames, mut samples, mut errors) = (0usize, 0usize, 0usize, 0usize);
    let mut first: Option<(u32, usize)> = None;
    let mut last_error = String::new();
    let mut bytes = Vec::new();
    // A source the facade does not recognise is reported as an error, with no output.
    let mut source = std::fs::File::open(&args[1])
        .map_err(|e| e.to_string())
        .and_then(|file| {
            let pool = Budget::new(256 << 20).map_err(|e| e.to_string())?;
            triergon::open_blocking(std::io::BufReader::new(file), &pool).map_err(|e| e.to_string())
        })
        .map_err(|error| {
            errors += 1;
            last_error = error;
        })
        .ok();
    while let Some(source) = source.as_mut() {
        let packet = match source.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
            Err(error) => {
                errors += 1;
                last_error = format!("read: {error}");
                break;
            }
        };
        if track.is_none() {
            let reader = source.reader();
            track = (0..reader.track_count())
                .filter_map(|i| reader.track(i))
                .find(|t| t.kind() == MediaKind::Audio && t.codec() == CodecId::MpegAudio)
                .map(TrackInfo::id);
        }
        if track != Some(packet.track()) {
            continue;
        }
        if Some(index) == reset_at {
            decoder.reset();
        }
        index += 1;
        match decoder.decode(packet.payload(), &mut frame) {
            Ok(_) => {
                frames += 1;
                samples += frame.samples_per_channel;
                first.get_or_insert((frame.sample_rate, frame.planes.len()));
                bytes.clear();
                for i in 0..frame.samples_per_channel {
                    for plane in &frame.planes {
                        bytes.extend_from_slice(&plane[i].to_le_bytes());
                    }
                }
                output.write_all(&bytes).expect("write output");
            }
            Err(error) => {
                errors += 1;
                last_error = error.0;
            }
        }
    }
    output.flush().expect("flush output");
    let (rate, channels) = first.unwrap_or((0, 0));
    let layout = match channels {
        1 => "mono",
        2 => "stereo",
        _ => "",
    };
    eprintln!(
        "{{\"sample_rate\":{rate},\"channels\":{channels},\"layout\":\"{layout}\",\"frames\":{frames},\"samples_per_channel\":{samples},\"errors\":{errors},\"last_error\":\"{}\"}}",
        last_error.replace('"', "'")
    );
}

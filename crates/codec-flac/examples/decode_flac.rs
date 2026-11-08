#![forbid(unsafe_code)]
//! Decode the first FLAC track of a file to interleaved f32le, planes in FFmpeg's
//! channel order. The container (native FLAC, Ogg, Matroska) is read through the
//! `triergon` facade; the decoder's whole output is written, without container trims.
//!
//! usage: decode_flac INPUT OUTPUT.f32 [--reject-crc] [--per-frame]
//!
//! Prints one JSON line to stderr: the first frame's format, frame and sample counts,
//! errors and the last error. `--per-frame` first prints one line per frame.

use std::io::Write;
use std::process::ExitCode;

use triergon::{Budget, CodecId, ContainerReader, MediaKind, PacketInfo};
use triergon_codec_flac::{AudioFrame, ChannelLayout, CrcPolicy, Decoder};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let (Some(input), Some(output)) = (args.get(1), args.get(2)) else {
        eprintln!("usage: decode_flac INPUT OUTPUT.f32 [--reject-crc] [--per-frame]");
        return ExitCode::from(2);
    };
    let flag = |name: &str| args[3..].iter().any(|a| a == name);
    let per_frame = flag("--per-frame");
    let policy = if flag("--reject-crc") {
        CrcPolicy::Reject
    } else {
        CrcPolicy::Ignore
    };
    let opened = std::fs::File::open(input)
        .map_err(|e| e.to_string())
        .and_then(|file| {
            let pool = Budget::new(256 << 20).map_err(|e| e.to_string())?;
            triergon::open_blocking(std::io::BufReader::new(file), &pool).map_err(|e| e.to_string())
        });
    let mut source = match opened {
        Ok(source) => source,
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

    let mut decoder: Option<(triergon::TrackId, Decoder)> = None;
    let mut frame = AudioFrame::default();
    let mut first = None;
    let (mut index, mut decoded, mut samples, mut errors) = (0usize, 0usize, 0usize, 0usize);
    let mut last_error = String::new();
    let mut interleaved = Vec::new();
    loop {
        let packet = match source.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
            Err(error) => {
                errors += 1;
                last_error = format!("read: {error}");
                break;
            }
        };
        if decoder.is_none() {
            let reader = source.reader();
            let Some(track) = (0..reader.track_count())
                .filter_map(|i| reader.track(i))
                .find(|t| t.kind() == MediaKind::Audio && t.codec() == CodecId::Flac)
            else {
                continue;
            };
            let built = Budget::new(track.configuration().len() + 64)
                .map_err(|e| e.to_string())
                .and_then(|pool| {
                    let mut configuration = triergon_codec::BudgetVec::new(&pool);
                    triergon_framing::canonical_flac_configuration(
                        track.configuration(),
                        &mut configuration,
                    )
                    .map(|()| configuration)
                    .map_err(|e| e.to_string())
                })
                .and_then(|configuration| Decoder::new(&configuration).map_err(|e| e.to_string()));
            match built {
                Ok(built) => decoder = Some((track.id(), built.with_crc_policy(policy))),
                Err(error) => {
                    eprintln!("{input}: {error}");
                    return ExitCode::FAILURE;
                }
            }
        }
        let Some((track, decoder)) = decoder.as_mut() else {
            continue;
        };
        if packet.track() != *track {
            continue;
        }
        match decoder.decode_float(packet.payload(), &mut frame) {
            Ok(info) => {
                if per_frame {
                    eprintln!(
                        "{{\"frame\":{index},\"sample_rate\":{},\"layout\":\"{}\",\"samples\":{}}}",
                        info.sample_rate,
                        info.layout.name(),
                        info.samples_per_channel
                    );
                }
                first.get_or_insert((info.sample_rate, info.layout));
                decoded += 1;
                samples += frame.samples_per_channel;
                interleaved.clear();
                for i in 0..frame.samples_per_channel {
                    for plane in &frame.planes {
                        interleaved.extend_from_slice(&plane[i].to_le_bytes());
                    }
                }
                if let Err(error) = out.write_all(&interleaved) {
                    eprintln!("{output}: {error}");
                    return ExitCode::FAILURE;
                }
            }
            Err(error) => {
                if per_frame {
                    eprintln!("{{\"frame\":{index},\"error\":\"{error}\"}}");
                }
                errors += 1;
                last_error = error.to_string();
            }
        }
        index += 1;
    }
    if let Err(error) = out.flush() {
        eprintln!("{output}: {error}");
        return ExitCode::FAILURE;
    }
    let (rate, layout) = first.unwrap_or((0, ChannelLayout::EMPTY));
    eprintln!(
        "{{\"sample_rate\":{rate},\"channels\":{},\"layout\":\"{}\",\"frames\":{decoded},\"samples_per_channel\":{samples},\"errors\":{errors},\"last_error\":\"{last_error}\"}}",
        layout.channels(),
        if first.is_some() {
            layout.name()
        } else {
            String::new()
        },
    );
    ExitCode::SUCCESS
}

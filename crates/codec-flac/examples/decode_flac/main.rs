//! Decode a native FLAC file to interleaved f32le, planes in FFmpeg's channel order.
//!
//! usage: decode_flac INPUT OUTPUT.f32 [--reject-crc] [--per-frame]
//!
//! Prints one JSON line to stderr: the first frame's format, frame and sample counts,
//! errors and the last error. `--per-frame` first prints one line per frame.

use std::io::Write;
use std::process::ExitCode;

use triergon_codec_flac::{AudioFrame, ChannelLayout, CrcPolicy, Decoder, Limits};

mod native;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let (Some(input), Some(output)) = (args.get(1), args.get(2)) else {
        eprintln!("usage: decode_flac INPUT OUTPUT.f32 [--reject-crc] [--per-frame]");
        return ExitCode::from(2);
    };
    let data = match std::fs::read(input) {
        Ok(data) => data,
        Err(error) => {
            eprintln!("{input}: {error}");
            return ExitCode::FAILURE;
        }
    };
    let flag = |name: &str| args[3..].iter().any(|a| a == name);
    let per_frame = flag("--per-frame");
    let policy = if flag("--reject-crc") {
        CrcPolicy::Reject
    } else {
        CrcPolicy::Ignore
    };
    let (configuration, frames) = native::split_header(&data);
    let mut decoder = match Decoder::from_configuration(configuration, Limits::default()) {
        Ok(decoder) => decoder.with_crc_policy(policy),
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

    let mut frame = AudioFrame::default();
    let mut first = None;
    let (mut decoded, mut samples, mut errors) = (0usize, 0usize, 0usize);
    let mut last_error = String::new();
    let mut interleaved = Vec::new();
    for (index, au) in native::frames(frames).enumerate() {
        match decoder.decode_float(au, &mut frame) {
            Ok(info) => {
                if per_frame {
                    eprintln!(
                        "{{\"frame\":{index},\"sample_rate\":{},\"layout\":\"{}\",\"samples\":{}}}",
                        info.sample_rate,
                        layout_name(&info.layout),
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
    }
    if let Err(error) = out.flush() {
        eprintln!("{output}: {error}");
        return ExitCode::FAILURE;
    }
    let (rate, layout) = first.unwrap_or_default();
    eprintln!(
        "{{\"sample_rate\":{rate},\"channels\":{},\"layout\":\"{}\",\"frames\":{decoded},\"samples_per_channel\":{samples},\"errors\":{errors},\"last_error\":\"{last_error}\"}}",
        layout.channels(),
        layout_name(&layout),
    );
    ExitCode::SUCCESS
}

/// The name ffprobe prints for `layout`.
fn layout_name(layout: &ChannelLayout) -> String {
    const NAMES: [(&str, &str); 24] = [
        ("mono", "FC"),
        ("stereo", "FL+FR"),
        ("2.1", "FL+FR+LFE"),
        ("3.0", "FL+FR+FC"),
        ("3.0(back)", "FL+FR+BC"),
        ("4.0", "FL+FR+FC+BC"),
        ("quad", "FL+FR+BL+BR"),
        ("quad(side)", "FL+FR+SL+SR"),
        ("3.1", "FL+FR+FC+LFE"),
        ("5.0", "FL+FR+FC+BL+BR"),
        ("5.0(side)", "FL+FR+FC+SL+SR"),
        ("4.1", "FL+FR+FC+LFE+BC"),
        ("5.1", "FL+FR+FC+LFE+BL+BR"),
        ("5.1(side)", "FL+FR+FC+LFE+SL+SR"),
        ("6.0", "FL+FR+FC+BC+SL+SR"),
        ("6.0(front)", "FL+FR+FLC+FRC+SL+SR"),
        ("3.1.2", "FL+FR+FC+LFE+TFL+TFR"),
        ("hexagonal", "FL+FR+FC+BL+BR+BC"),
        ("6.1", "FL+FR+FC+LFE+BC+SL+SR"),
        ("6.1(back)", "FL+FR+FC+LFE+BL+BR+BC"),
        ("6.1(front)", "FL+FR+LFE+FLC+FRC+SL+SR"),
        ("7.0", "FL+FR+FC+BL+BR+SL+SR"),
        ("7.0(front)", "FL+FR+FC+FLC+FRC+SL+SR"),
        ("7.1", "FL+FR+FC+LFE+BL+BR+SL+SR"),
    ];
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

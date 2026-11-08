//! Decode a raw MPEG audio file to interleaved f32le, each frame with its own channels.
//!
//! usage: decode_mp3 INPUT OUTPUT.f32 [--reset-at N]
//!
//! `--reset-at N` resets the decoder before access unit `N` (counted from 0), as a
//! seek would.
//!
//! Prints one JSON line to stderr: sample rate, channels and layout of the first frame,
//! frames and samples decoded, errors and the last error.

use std::io::Write;

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
    let data = std::fs::read(&args[1]).expect("read input");
    let mut output =
        std::io::BufWriter::new(std::fs::File::create(&args[2]).expect("create output"));
    let mut decoder = Decoder::new();
    let mut frame = AudioFrame::default();
    let (mut frames, mut samples, mut errors) = (0usize, 0usize, 0usize);
    let mut first: Option<(u32, usize)> = None;
    let mut last_error = String::new();
    let mut bytes = Vec::new();
    for (index, au) in split::frames(&data).enumerate() {
        if Some(index) == reset_at {
            decoder.reset();
        }
        match decoder.decode(au, &mut frame) {
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

/// Frame splitting of a raw MPEG audio file, as FFmpeg's mp3 demuxer delivers it: an
/// ID3v2 tag at the start and an ID3v1 tag at the end are skipped, and a Xing, Info or
/// VBRI frame at the start is dropped. A reader crate replaces this.
mod split {
    use triergon_codec_mp3::frame_header;
    use triergon_framing::{MpegAudioHeader, mpeg_info_frame};

    /// Bytes of an ID3v2 tag at the start of `data`.
    fn id3v2_length(data: &[u8]) -> usize {
        match data {
            [b'I', b'D', b'3', _, _, flags, size @ ..] if size.len() >= 4 => {
                let size = size[..4]
                    .iter()
                    .fold(0usize, |n, &b| n << 7 | usize::from(b & 0x7f));
                10 + size + if flags & 0x10 != 0 { 10 } else { 0 }
            }
            _ => 0,
        }
    }

    /// The access units of `data`. Bytes that do not start a frame of the stream are
    /// skipped; a free-format frame (whose length the header does not give) or a
    /// truncated last frame runs to the end of the data.
    pub(super) fn frames(data: &[u8]) -> impl Iterator<Item = &[u8]> {
        let mut data = &data[id3v2_length(data).min(data.len())..];
        if data.len() >= 128 && data[data.len() - 128..].starts_with(b"TAG") {
            data = &data[..data.len() - 128];
        }
        let mut position = 0;
        let mut stream: Option<MpegAudioHeader> = None;
        core::iter::from_fn(move || {
            loop {
                let rest = data.get(position..).filter(|rest| rest.len() >= 4)?;
                let Some(header) =
                    frame_header(rest).filter(|h| stream.is_none_or(|s| s.same_stream(h)))
                else {
                    position += 1;
                    continue;
                };
                let length = header.frame_bytes().unwrap_or(rest.len()).min(rest.len());
                let frame = &rest[..length];
                position += length;
                if stream.is_none() {
                    stream = Some(header);
                    if mpeg_info_frame(frame, &header).is_some() {
                        continue;
                    }
                }
                return Some(frame);
            }
        })
    }
}

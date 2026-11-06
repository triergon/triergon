//! Decode an ADTS AAC elementary stream to interleaved f32le (planes in FFmpeg native
//! order). Prints one JSON line to stderr: rate, channels, layout, frames.
//!
//! usage: decode IN.aac OUT.f32
use std::io::Write;
use triergon_codec_aac::{AudioFrame, Decoder, adts, layout_name};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: decode IN.aac OUT.f32");
        std::process::exit(2);
    }
    let data = std::fs::read(&args[1]).expect("read input");
    let mut out = std::io::BufWriter::new(std::fs::File::create(&args[2]).expect("create output"));
    // skip an ID3v2 tag
    let mut pos = 0usize;
    while data.len() >= pos + 10 && &data[pos..pos + 3] == b"ID3" {
        let sz = ((data[pos + 6] as usize & 0x7f) << 21)
            | ((data[pos + 7] as usize & 0x7f) << 14)
            | ((data[pos + 8] as usize & 0x7f) << 7)
            | (data[pos + 9] as usize & 0x7f);
        let footer = if data[pos + 5] & 0x10 != 0 { 10 } else { 0 };
        pos += 10 + sz + footer;
    }
    let mut dec = Decoder::new();
    let mut frame = AudioFrame::default();
    let (mut frames, mut errors, mut samples) = (0u64, 0u64, 0u64);
    let mut last: Option<(u32, String, usize)> = None;
    let mut changes = 0u32;
    let mut buf: Vec<u8> = Vec::new();
    while let Some((off, h)) = adts::find_frame(&data, pos) {
        let end = (off + h.frame_length).min(data.len());
        let au = &data[off..end];
        pos = end;
        match dec.decode(au, &mut frame) {
            Ok(()) => {
                frames += 1;
                let key = (
                    frame.sample_rate,
                    layout_name(&frame.layout),
                    frame.planes.len(),
                );
                if last.as_ref() != Some(&key) {
                    if last.is_some() {
                        changes += 1;
                        eprintln!(
                            "format change at frame {frames}: {} Hz {} ({} ch)",
                            key.0, key.1, key.2
                        );
                    }
                    last = Some(key);
                }
                let n = frame.samples_per_channel;
                samples += n as u64;
                buf.clear();
                for i in 0..n {
                    for p in &frame.planes {
                        buf.extend_from_slice(&p[i].to_le_bytes());
                    }
                }
                out.write_all(&buf).expect("write");
            }
            Err(e) => {
                errors += 1;
                eprintln!("frame at byte {off}: {e}");
            }
        }
    }
    out.flush().expect("flush");
    let (rate, layout, ch) = last.unwrap_or((0, String::new(), 0));
    eprintln!(
        "{{\"sample_rate\":{rate},\"channels\":{ch},\"layout\":\"{layout}\",\"frames\":{frames},\"samples_per_channel\":{samples},\"errors\":{errors},\"format_changes\":{changes}}}"
    );
}

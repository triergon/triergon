//! Development aid: write the QMF slots this decoder hands to Parametric Stereo, for the
//! table-recovery probes (`harness/codecs/aac/ps`). Per ADTS frame: 38 rows of 64 complex
//! bands as little-endian f32 (re, im), or zeros when PS did not run.
//!
//! usage: ps_input IN.aac OUT.f32
use std::io::Write;
use triergon_codec_aac::{AudioFrame, Decoder, adts};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let data = std::fs::read(&args[1]).expect("input");
    let mut out = std::io::BufWriter::new(std::fs::File::create(&args[2]).expect("output"));
    let mut decoder = Decoder::new();
    let mut frame = AudioFrame::default();
    let mut pos = 0;
    let empty = vec![0f32; 38 * 64 * 2];
    while let Some((off, header)) = adts::find_frame(&data, pos) {
        let end = (off + header.frame_length).min(data.len());
        pos = end;
        let input = match decoder.decode(&data[off..end], &mut frame) {
            Ok(()) if !decoder.ps_input().is_empty() => decoder.ps_input(),
            _ => &empty,
        };
        for v in input {
            out.write_all(&v.to_le_bytes()).expect("write");
        }
    }
}

//! Development timing aid: decode an ADTS file N times in-process (no output) and print the
//! fastest and median pass in milliseconds.
//!
//! usage: bench IN.aac [PASSES]
use std::time::Instant;
use triergon_codec_aac::{AudioFrame, Decoder, adts};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let data = std::fs::read(&args[1]).expect("read input");
    let passes: usize = args.get(2).map_or(10, |s| s.parse().expect("passes"));
    let mut frames = Vec::new();
    let mut pos = 0;
    while let Some((off, h)) = adts::find_frame(&data, pos) {
        let end = (off + h.frame_length).min(data.len());
        frames.push(&data[off..end]);
        pos = end;
    }
    let mut dec = Decoder::new();
    let mut frame = AudioFrame::default();
    let mut times = Vec::new();
    let mut check = 0f64;
    for _ in 0..passes {
        dec.reset();
        let t = Instant::now();
        for au in &frames {
            if dec.decode(au, &mut frame).is_ok() {
                check += frame.planes.first().map_or(0.0, |p| p[p.len() / 2] as f64);
            }
        }
        times.push(t.elapsed().as_secs_f64() * 1e3);
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!(
        "{{\"frames\":{},\"min_ms\":{:.2},\"median_ms\":{:.2},\"check\":{check:.6}}}",
        frames.len(),
        times[0],
        times[times.len() / 2]
    );
}

//! Development timing aid: decode an elementary stream
//! `reps` times in-process and report wall time per pass.
//!
//! usage: bench <input> [reps]

use std::time::Instant;

use triergon_codec_ac3::{AudioFrame, Decoder, access_unit_len};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let data = std::fs::read(&args[1]).expect("read input");
    let reps: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(10);
    // Pre-split access units so the timing covers decoding only.
    let mut aus = Vec::new();
    let mut pos = 0;
    while pos + 8 <= data.len() {
        match access_unit_len(&data[pos..]) {
            Ok(l) if pos + l <= data.len() => {
                aus.push((pos, l));
                pos += l;
            }
            Ok(_) => break,
            Err(_) => pos += 1,
        }
    }
    let mut frame = AudioFrame::default();
    let mut best = f64::MAX;
    let mut samples = 0;
    let mut checksum = 0f64;
    for _ in 0..reps {
        let mut dec = Decoder::new();
        let t = Instant::now();
        samples = 0;
        for &(p, l) in &aus {
            frame.clear();
            if dec.decode(&data[p..p + l], &mut frame).is_ok() {
                samples += frame.samples_per_channel;
                checksum += frame.planes[0][0] as f64;
            }
        }
        best = best.min(t.elapsed().as_secs_f64());
    }
    eprintln!(
        "{} AUs, {} samples/ch, best of {reps}: {:.3} ms ({:.1}x realtime) [checksum {checksum:.3}]",
        aus.len(),
        samples,
        best * 1e3,
        samples as f64 / 48000.0 / best
    );
}

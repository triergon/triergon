//! Development aid: print per-frame parse statistics of an ADTS stream.
use triergon_codec_aac::{AudioFrame, Decoder, adts};
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let data = std::fs::read(&a[1]).unwrap();
    let from: usize = a.get(2).map_or(0, |s| s.parse().unwrap());
    let to: usize = a.get(3).map_or(usize::MAX, |s| s.parse().unwrap());
    let mut pos = 0;
    let mut dec = Decoder::new();
    dec.enable_stats();
    let mut f = AudioFrame::default();
    let mut k = 0usize;
    while let Some((off, h)) = adts::find_frame(&data, pos) {
        let end = (off + h.frame_length).min(data.len());
        pos = end;
        let r = dec.decode(&data[off..end], &mut f);
        if k >= from && k <= to {
            println!(
                "{k}: {:?} rate{} ch{} n{} {}",
                r.err(),
                f.sample_rate,
                f.planes.len(),
                f.samples_per_channel,
                dec.stats()
            );
        }
        k += 1;
    }
}

//! Robustness smoke test: decode bit-flipped / truncated / spliced copies of the given Annex-B
//! files and report any panic (the decoder must return errors, never panic).
//! Usage: fuzz [--iters N] FILE...
use std::panic::{AssertUnwindSafe, catch_unwind};
use triergon_codec_h264::{AnnexBNalIter, Decoder};

fn decode(data: &[u8]) {
    let mut dec = Decoder::new();
    for nal in AnnexBNalIter::new(data) {
        let _ = dec.send_nal(nal, None);
        while dec.receive().is_some() {}
    }
    dec.flush();
    while dec.receive().is_some() {}
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut iters = 20usize;
    if args.len() > 1 && args[0] == "--iters" {
        iters = args[1].parse().unwrap();
        args.drain(..2);
    }
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    let mut rnd = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    std::panic::set_hook(Box::new(|_| {}));
    let mut panics = 0;
    for path in &args {
        let orig = std::fs::read(path).unwrap();
        for it in 0..iters {
            let mut d = orig.clone();
            match it % 4 {
                0 => {
                    // Random bit flips past the first 64 bytes (keep parameter sets mostly intact).
                    let n = 1 + (rnd() % 16) as usize;
                    for _ in 0..n {
                        let p = 64 + (rnd() as usize) % (d.len().saturating_sub(64).max(1));
                        if p < d.len() {
                            d[p] ^= 1 << (rnd() % 8);
                        }
                    }
                }
                1 => {
                    // Truncate.
                    let l = (rnd() as usize) % d.len();
                    d.truncate(l);
                }
                2 => {
                    // Random byte overwrite burst.
                    let p = (rnd() as usize) % d.len();
                    for k in 0..(rnd() % 64) as usize {
                        if p + k < d.len() {
                            d[p + k] = rnd() as u8;
                        }
                    }
                }
                _ => {
                    // Splice: drop a random range.
                    let a = (rnd() as usize) % d.len();
                    let b = (a + (rnd() as usize) % 4096).min(d.len());
                    d.drain(a..b);
                }
            }
            if catch_unwind(AssertUnwindSafe(|| decode(&d))).is_err() {
                panics += 1;
                let out = format!(
                    "fuzz_crash_{}_{}.264",
                    std::path::Path::new(path)
                        .file_name()
                        .unwrap()
                        .to_string_lossy(),
                    it
                );
                std::fs::write(&out, &d).unwrap();
                eprintln!("PANIC {path} iter {it} -> saved {out}");
            }
        }
    }
    eprintln!("done, {panics} panics");
    if panics > 0 {
        std::process::exit(1);
    }
}

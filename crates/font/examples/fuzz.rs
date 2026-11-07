//! Robustness smoke test: parse and read bit-flipped, truncated, overwritten and
//! spliced copies of the given font files, and report any panic (the reader must
//! return errors, never panic). Build it in the dev profile so arithmetic overflow
//! is checked too.
//!
//! Usage: fuzz [--iters N] FILE...

#[path = "../tests/support/mod.rs"]
mod support;

use std::panic::{AssertUnwindSafe, catch_unwind};
use triergon_font::Limits;

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut iters = 20usize;
    if args.len() > 1 && args[0] == "--iters" {
        iters = args[1].parse().expect("iteration count");
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
    let limits = Limits {
        work: 1 << 20,
        ..Limits::default()
    };
    let mut panics = 0;
    for path in &args {
        let original = std::fs::read(path).expect("font file");
        if catch_unwind(AssertUnwindSafe(|| support::exercise(&original, limits))).is_err() {
            panics += 1;
            eprintln!("PANIC {path} unmodified");
        }
        for it in 0..iters {
            let mut d = original.clone();
            match it % 4 {
                0 => {
                    // Bit flips anywhere: directories and headers are small targets.
                    for _ in 0..1 + rnd() % 16 {
                        let p = (rnd() as usize) % d.len();
                        d[p] ^= 1 << (rnd() % 8);
                    }
                }
                1 => {
                    let l = (rnd() as usize) % d.len();
                    d.truncate(l);
                }
                2 => {
                    let p = (rnd() as usize) % d.len();
                    for k in 0..(rnd() % 64) as usize {
                        if p + k < d.len() {
                            d[p + k] = rnd() as u8;
                        }
                    }
                }
                _ => {
                    let a = (rnd() as usize) % d.len();
                    let b = (a + (rnd() as usize) % 4096).min(d.len());
                    d.drain(a..b);
                }
            }
            if catch_unwind(AssertUnwindSafe(|| support::exercise(&d, limits))).is_err() {
                panics += 1;
                let name = std::path::Path::new(path).file_name().expect("file name");
                let out = format!("fuzz_crash_{}_{it}", name.to_string_lossy());
                std::fs::write(&out, &d).expect("crash file");
                eprintln!("PANIC {path} iter {it} -> saved {out}");
            }
        }
    }
    eprintln!("done, {panics} panics");
    if panics > 0 {
        std::process::exit(1);
    }
}

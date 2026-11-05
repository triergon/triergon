//! Synthetic DTS core stream generator for oracle experiments (development tooling).
//!
//! usage: synth SCENARIO OUT.dts
//! Writes a raw core elementary stream (and for codebook-recovery scenarios a sidecar
//! `OUT.dts.meta` text file). Scenarios live in `tests/support/scenarios.rs`.
#[path = "../tests/support/scenarios.rs"]
mod scenarios;
#[path = "../tests/support/synth.rs"]
mod synth;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: synth SCENARIO OUT.dts");
        std::process::exit(2);
    }
    let mut meta = String::new();
    let Some(bytes) = scenarios::scenarios(&args[1], &mut meta) else {
        eprintln!("unknown scenario {}", args[1]);
        std::process::exit(2);
    };
    std::fs::write(&args[2], &bytes).expect("write");
    if !meta.is_empty() {
        std::fs::write(format!("{}.meta", args[2]), meta).expect("write meta");
    }
}

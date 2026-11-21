//! Packet and access-unit measurements over the public MPEG-TS readers.
#![forbid(unsafe_code)]

mod ts;
mod ts_au;
mod ts_input;
mod ts_matched;

use std::process::ExitCode;

mod runner {
    pub use triergon_harness_runner::{Invocation, Refusal, Result, Totals, measure};

    pub const MAX_INPUT: u64 = 64 * 1024 * 1024;
}

fn dispatch(args: &[String]) -> runner::Result<()> {
    let mode = args.first().map_or("", String::as_str);
    if let Some(kind) = ts_matched::Kind::for_mode(mode) {
        ts_matched::run(kind, args)
    } else if mode.starts_with(ts_au::PREFIX) {
        ts_au::run(args)
    } else if mode.starts_with(ts::PREFIX) {
        ts::run(args)
    } else {
        Err(
            "usage: transport-worker PREFIX{fold,bench,memory} FILE ...; prefixes: ts-au-, ts-, ts-seek-, ts-audio-seek-, ts-free-audio-"
                .into(),
        )
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match dispatch(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("transport-worker: {error}");
            ExitCode::from(2)
        }
    }
}

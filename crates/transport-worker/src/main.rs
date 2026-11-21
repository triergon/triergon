//! Packet and access-unit measurements over the public MPEG-TS readers.
#![deny(unsafe_code)]
#![cfg_attr(not(test), no_main)]

mod ts;
mod ts_au;
mod ts_dts_hd;
mod ts_input;
mod ts_matched;

use std::process::ExitCode;

mod runner {
    pub use triergon_harness_runner::{Invocation, Refusal, Result, Totals, measure};

    pub const MAX_INPUT: u64 = 64 * 1024 * 1024;
}

fn dispatch(args: &[String]) -> runner::Result<()> {
    let mode = args.first().map_or("", String::as_str);
    if mode.starts_with(ts_dts_hd::PREFIX) {
        ts_dts_hd::run(args)
    } else if let Some(kind) = ts_matched::Kind::for_mode(mode) {
        ts_matched::run(kind, args)
    } else if mode.starts_with(ts_au::PREFIX) {
        ts_au::run(args)
    } else if mode.starts_with(ts::PREFIX) {
        ts::run(args)
    } else {
        Err(
            "usage: transport-worker PREFIX{fold,bench,memory} FILE ...; prefixes: ts-au-, ts-, ts-seek-, ts-audio-seek-, ts-free-audio-, ts-dts-hd-"
                .into(),
        )
    }
}

#[cfg_attr(test, allow(dead_code))]
fn worker_main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match dispatch(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("transport-worker: {error}");
            ExitCode::from(2)
        }
    }
}

/// C's entry point, the one exception to the worker's `deny(unsafe_code)`
/// (`triergon-worker-entry`).
#[cfg(not(test))]
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
extern "C" fn main(
    _argc: std::ffi::c_int,
    _argv: *const *const std::ffi::c_char,
) -> std::ffi::c_int {
    triergon_worker_entry::enter(worker_main)
}

//! Whole-stream VVC access-unit grouping for the differential harness.
#![deny(unsafe_code)]
#![cfg_attr(not(test), no_main)]

use std::process::ExitCode;
use triergon_framing::compat;
use triergon_harness_protocol::digest::{fold, fold_bytes};
use triergon_harness_runner::{Invocation, Result, Totals, measure};

const INPUT_LIMIT: usize = 16 * 1024 * 1024;

fn group(data: &[u8]) -> Result<Totals> {
    let mut totals = Totals::default();
    for unit in compat::vvc::access_units(data, INPUT_LIMIT)? {
        let unit = unit?;
        if unit.range.start as u64 != totals.bytes {
            return Err("access units do not cover the complete input in order".into());
        }
        fold(&mut totals.digest, unit.range.start as u64);
        fold(&mut totals.digest, unit.bytes.len() as u64);
        fold_bytes(&mut totals.digest, unit.bytes);
        totals.packets += 1;
        totals.bytes += unit.bytes.len() as u64;
    }
    if totals.bytes != data.len() as u64 {
        return Err("access units do not cover the complete input".into());
    }
    Ok(totals)
}

fn run(args: &[String]) -> Result<()> {
    let invocation = Invocation::parse(args, "vvc-au-", "", 0..=0, false)?;
    let data = invocation.input(INPUT_LIMIT as u64)?;
    measure(&invocation, "vvc-annex-b-access-units", None, || {
        group(&data)
    })
}

#[cfg_attr(test, allow(dead_code))]
fn worker_main() -> ExitCode {
    let args: Vec<_> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
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

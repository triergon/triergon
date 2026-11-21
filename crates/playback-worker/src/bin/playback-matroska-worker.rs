//! Playback measurement worker with the Matroska runners alone: Matroska startup and
//! seek, and Matroska audio clocks. Its reference build demuxes Matroska alone; the
//! playback engine it links still holds the MP4 and TS backends.
#![deny(unsafe_code)]
#![cfg_attr(not(test), no_main)]
use playback_worker::runner;
use std::process::ExitCode;

#[cfg_attr(test, allow(dead_code))]
fn worker_main() -> ExitCode {
    runner::main("playback-matroska-worker", &runner::MATROSKA_RUNNERS)
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

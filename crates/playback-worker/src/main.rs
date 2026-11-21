//! Playback measurement worker with every runner family; serialization and startup
//! are outside the timed loop.
#![deny(unsafe_code)]
#![cfg_attr(not(test), no_main)]
use playback_worker::runner;
use std::process::ExitCode;

#[cfg_attr(test, allow(dead_code))]
fn worker_main() -> ExitCode {
    runner::main("playback-worker", &runner::RUNNERS)
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

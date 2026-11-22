//! Whole packet-framing operations for the differential harness.
#![deny(unsafe_code)]
#![cfg_attr(not(test), no_main)]

#[path = "../../../harness/features/packet-framing/worker.rs"]
mod worker;

#[cfg_attr(test, allow(dead_code))]
fn worker_main() -> std::process::ExitCode {
    worker::main(std::env::args_os())
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

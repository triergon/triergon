//! Decoder measurement worker with every operation: the session decoders' decode
//! operations and the container readers' `packets` and `seek` commands.
#![deny(unsafe_code)]
#![cfg_attr(not(test), no_main)]

use std::process::ExitCode;

#[cfg_attr(test, allow(dead_code))]
fn worker_main() -> ExitCode {
    codec_worker::main("codec-worker", codec_worker::Operations::All);
    ExitCode::SUCCESS
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

//! Decoder measurement worker with the AAC decode operations alone: `audio-aac` and
//! the access-unit operations. The gate build builds it with the AAC decoder's `simd`
//! feature, by the pinned nightly; every other operation stays with `codec-worker`.
#![deny(unsafe_code)]
#![cfg_attr(not(test), no_main)]

use std::process::ExitCode;

#[cfg_attr(test, allow(dead_code))]
fn worker_main() -> ExitCode {
    codec_worker::main("codec-aac-worker", codec_worker::Operations::Aac);
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

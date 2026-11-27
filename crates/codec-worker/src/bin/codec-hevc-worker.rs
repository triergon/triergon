//! HEVC decoding and letterboxing with the nightly SIMD profile.
#![deny(unsafe_code)]
#![cfg_attr(not(test), no_main)]

use std::process::ExitCode;

#[cfg_attr(test, allow(dead_code))]
fn worker_main() -> ExitCode {
    if !std::env::args_os().nth(1).is_some_and(|mode| {
        matches!(
            mode.to_str(),
            Some("video-hevc-fold" | "video-hevc-raw" | "video-hevc-bench" | "video-hevc-memory")
        )
    }) {
        eprintln!(
            "usage: codec-hevc-worker video-hevc-{{fold|raw|bench|memory}} FILE [OUT|ITERATIONS]"
        );
        return ExitCode::from(2);
    }
    codec_worker::main("codec-hevc-worker", codec_worker::Operations::All);
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

//! Differential worker for playback views.
#![deny(unsafe_code)]
#![cfg_attr(not(test), no_main)]

mod bridge;
mod hls;
mod player;
mod probe;
mod source_view;
mod subtitles;

use playback_worker::runner;
use std::ffi::OsString;
use std::process::ExitCode;
use triergon_harness_runner::dispatch;

const RUNNERS: &[(&str, runner::Run)] = &[
    ("views-smoke-", smoke),
    ("probe-lifecycle-", probe::run_lifecycle),
    ("probe-", probe::run),
    ("source-view-", source_view::run),
    ("mp4-seek-", player::run),
    ("catalogue-", player::run),
    ("delivery-", player::run),
    ("select-", player::run),
    ("mp4-index-", player::run),
    ("ts-pes-open-", player::run),
    ("hls-views-", hls::run),
    ("access-units-", bridge::run),
    ("mp4-bridges-", bridge::run),
    ("mkv-discovery-", bridge::run),
    ("text-bridge-", subtitles::run),
    ("text-control-", subtitles::run_text_control),
    ("subtitle-bridge-", subtitles::run),
];

/// `views-smoke-`, the table workers' smoke operation.
fn smoke(args: &[String]) -> runner::Result<()> {
    let args: Vec<OsString> = args.iter().map(OsString::from).collect();
    dispatch(&args, &[("views-smoke-", triergon_harness_runner::smoke)])
}

#[cfg_attr(test, allow(dead_code))]
fn worker_main() -> ExitCode {
    runner::main("playback-views-worker", RUNNERS)
}

/// C's entry point, the worker's one exception to `deny(unsafe_code)`.
#[cfg(not(test))]
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
extern "C" fn main(
    _argc: std::ffi::c_int,
    _argv: *const *const std::ffi::c_char,
) -> std::ffi::c_int {
    triergon_worker_entry::enter(worker_main)
}

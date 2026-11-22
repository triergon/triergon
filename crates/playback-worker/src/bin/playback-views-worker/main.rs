//! Differential worker for playback views.
#![deny(unsafe_code)]
#![cfg_attr(not(test), no_main)]

mod bridge;
mod hls;
#[cfg(not(test))]
#[allow(
    dead_code,
    reason = "Playback views share the packet fold for packet operations."
)]
#[path = "../../packet_fold.rs"]
mod packet_fold;
mod player;
mod probe;
mod source_view;
mod subtitles;

use playback_worker::runner::{self, Invocation, MAX_INPUT, Mode, Totals, measure};
use std::process::ExitCode;
use triergon_harness_protocol::digest::fold;

const RUNNERS: &[(&str, runner::Run)] = &[
    ("views-smoke-", smoke),
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
    ("subtitle-bridge-", subtitles::run),
];

fn smoke(args: &[String]) -> runner::Result<()> {
    let invocation = Invocation::parse(args, "views-smoke-", "", 0..=0, true)?;
    let input = invocation.input(MAX_INPUT)?;
    measure(&invocation, "views-smoke", None, || {
        let (mut a, mut b) = (1_u32, 0_u32);
        for &byte in &input {
            a = (a + u32::from(byte)) % 65521;
            b = (a + b) % 65521;
        }
        let size = input.len() as u64;
        let checksum = u64::from(b << 16 | a);
        let mut digest = 0;
        for word in [3, size, checksum] {
            fold(&mut digest, word);
        }
        if invocation.mode == Mode::Trace {
            println!("3 {size} {checksum}");
        }
        Ok(Totals {
            packets: 1,
            bytes: 0,
            digest,
            refusal: None,
        })
    })
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

//! Differential worker for transport views.
#![deny(unsafe_code)]
#![cfg_attr(not(test), no_main)]

mod catalogue;
mod facts;
mod mux;
mod packets;
#[path = "../../../../framing/examples/framing_views/protocol.rs"]
mod protocol;
mod reader;
mod source;

use std::process::ExitCode;

const OPERATIONS: &[(&str, protocol::Prepare)] = &[
    ("views-smoke-", protocol::smoke),
    ("ts-catalogue-", catalogue::operation),
    ("ts-psi-", catalogue::operation),
    ("ts-video-descriptors-", catalogue::operation),
    ("ts-packets-", packets::operation),
    ("ts-source-", source::operation),
    ("ts-pes-assembler-", source::operation),
    ("ts-observer-", source::operation),
    ("ts-facts-", facts::operation),
    ("ts-recovery-", facts::operation),
    ("ts-reader-views-", reader::operation),
    ("ts-reader-container-", reader::operation),
    ("ts-epoch-index-seek-", reader::operation),
    ("ts-bounded-time-", reader::operation),
    ("ts-mux-", mux::operation),
];

#[cfg_attr(test, allow(dead_code))]
fn worker_main() -> ExitCode {
    protocol::main(std::env::args_os(), OPERATIONS)
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

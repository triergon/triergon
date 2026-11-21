//! Differential worker for isobmff views.
#![deny(unsafe_code)]
#![cfg_attr(not(test), no_main)]

#[path = "isobmff_views/index.rs"]
mod index;
#[path = "isobmff_views/packets.rs"]
mod packets;
#[path = "../../framing/examples/framing_views/protocol.rs"]
mod protocol;
#[path = "isobmff_views/segment_index.rs"]
mod segment_index;
#[path = "isobmff_views/trace.rs"]
mod trace;

const OPERATIONS: &[(&str, protocol::Prepare)] = &[
    ("views-smoke-", protocol::smoke),
    ("qt-pcm-", packets::operation),
    ("qt-sound-", packets::operation),
    ("mov-pcm-", packets::operation),
    ("mov-recover-", packets::operation),
    ("dts-entry-", packets::operation),
    ("ddts-", packets::operation),
    ("mov-encoded-", packets::operation),
    ("mov-track-seek-", index::operation),
    ("source-seek-", index::operation),
    ("index-", segment_index::operation),
    ("mov-trace-", trace::operation),
    ("index-landings-", trace::operation),
];

#[cfg_attr(test, allow(dead_code))]
fn worker_main() -> std::process::ExitCode {
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

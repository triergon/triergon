//! Differential worker for matroska views.
#![deny(unsafe_code)]
#![cfg_attr(not(test), no_main)]

#[path = "matroska_views/extensions.rs"]
mod extensions;
#[path = "matroska_views/metadata.rs"]
mod metadata;
#[path = "../../framing/examples/framing_views/protocol.rs"]
mod protocol;
#[path = "matroska_views/side_data.rs"]
mod side_data;
// Use the shared C entry point without adding a dependency to the library package.
#[path = "../../worker-entry/src/lib.rs"]
pub mod triergon_worker_entry;

const OPERATIONS: &[(&str, protocol::Prepare)] = &[
    ("views-smoke-", protocol::smoke),
    ("mkv-side-data-", side_data::operation),
    ("mkv-codec-state-", side_data::operation),
    ("mkv-track-extensions-", extensions::operation),
    ("mkv-attachments-", metadata::operation),
    ("mkv-metadata-", metadata::operation),
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

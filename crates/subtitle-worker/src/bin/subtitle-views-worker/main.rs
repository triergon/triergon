//! Differential worker for subtitle views.
#![deny(unsafe_code)]
#![cfg_attr(not(test), no_main)]

mod carriage;
mod charset;
mod dvb;
mod dvd;
mod pgs;
#[path = "../../../../framing/examples/framing_views/protocol.rs"]
mod protocol;
mod text;
mod tx3g;

const OPERATIONS: &[(&str, protocol::Prepare)] = &[
    ("views-smoke-", protocol::smoke),
    ("text-extract-", text::operation),
    ("renderer-context-", text::operation),
    ("charset-", charset::operation),
    ("charset-packets-", charset::operation),
    ("tx3g-config-", tx3g::operation),
    ("tx3g-semantics-", tx3g::operation),
    ("dvb-source-", dvb::operation),
    ("vobsub-lines-", dvd::operation),
    ("pgs-segments-", pgs::operation),
    ("carriage-", carriage::operation),
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

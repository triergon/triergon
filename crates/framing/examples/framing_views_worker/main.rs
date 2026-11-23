//! Differential worker for framing views.
#![deny(unsafe_code)]
#![cfg_attr(not(test), no_main)]

mod envelope;

mod adts;
mod config_views;
mod dts;
mod nal;
mod obu;

use triergon_harness_runner::{Prepare, serve, smoke};

const OPERATIONS: &[(&str, Prepare)] = &[
    ("views-smoke-", smoke),
    ("nal-units-", nal::operation),
    ("nal-config-", nal::operation),
    ("hvcc-annexb-", nal::operation),
    ("obu-", obu::operation),
    ("obu-wire-", obu::operation),
    ("adts-", adts::operation),
    ("adts-wire-", adts::operation),
    ("config-", config_views::operation),
    ("hvcc-prefix-", config_views::operation),
    ("vpcc-", config_views::operation),
    ("asc-", config_views::operation),
    ("eac3-bsi-", config_views::operation),
    ("dts-core-", dts::operation),
    ("dts-header-", dts::operation),
    ("dts-packed-", dts::operation),
    ("dts-unpack-", dts::operation),
    ("dts-parser-", dts::operation),
];

#[cfg_attr(test, allow(dead_code))]
fn worker_main() -> std::process::ExitCode {
    serve(std::env::args_os(), OPERATIONS)
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

//! Differential worker for play views.
#![deny(unsafe_code)]
#![cfg_attr(not(test), no_main)]

mod facade;
#[path = "../../../../framing/examples/framing_views/protocol.rs"]
mod protocol;
mod raw;

use std::process::ExitCode;

const OPERATIONS: &[(&str, protocol::Prepare)] = &[
    ("views-smoke-", protocol::smoke),
    ("play-", raw::operation),
    ("facade-probe-", facade::operation),
    ("any-ts-au-", facade::operation),
    ("ts-au-", facade::operation),
    ("any-seek-", facade::operation),
    ("ts-seek-", facade::operation),
];

#[cfg_attr(test, allow(dead_code))]
fn worker_main() -> ExitCode {
    let args: Vec<_> = std::env::args_os().collect();
    if let [_, command, arguments @ ..] = args.as_slice()
        && command == "play-raw"
    {
        return match raw::command(arguments) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("{error}");
                ExitCode::from(2)
            }
        };
    }
    protocol::main(args, OPERATIONS)
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

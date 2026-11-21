#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![warn(
    missing_docs,
    missing_debug_implementations,
    unreachable_pub,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::unwrap_used,
    clippy::expect_used
)]

use std::ffi::c_int;
use std::io::{self, Write};
use std::panic;
use std::process::ExitCode;

/// Exit status of a worker that panicked, as from Rust's own entry point.
const PANIC_STATUS: c_int = 101;

/// Runs a worker from its C `main` and returns the exit status for it.
///
/// Standard output and standard error are flushed after `run` returns or panics; a
/// worker whose output could not be flushed fails with status 1 unless it already
/// failed. `std::env::args` still holds the command line.
pub fn enter(run: fn() -> ExitCode) -> c_int {
    let result = panic::catch_unwind(run);
    let flushed = io::stdout().flush().is_ok() & io::stderr().flush().is_ok();
    match result {
        Ok(code) if code == ExitCode::SUCCESS && !flushed => 1,
        Ok(code) => status(code),
        Err(_) => PANIC_STATUS,
    }
}

/// The status `code` carries. `ExitCode` has no stable integer accessor; on the
/// workers' platforms it holds one byte, the one whose `ExitCode` it equals.
fn status(code: ExitCode) -> c_int {
    (0..=u8::MAX)
        .find(|&byte| ExitCode::from(byte) == code)
        .map_or(1, c_int::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_byte_status_round_trips() {
        for byte in 0..=u8::MAX {
            assert_eq!(status(ExitCode::from(byte)), c_int::from(byte));
        }
        assert_eq!(status(ExitCode::SUCCESS), 0);
        assert_eq!(status(ExitCode::FAILURE), 1);
    }

    #[test]
    fn a_panic_ends_with_rusts_panic_status() {
        let previous = panic::take_hook();
        panic::set_hook(Box::new(|_| {}));
        let code = enter(|| panic!("worker failed"));
        panic::set_hook(previous);
        assert_eq!(code, PANIC_STATUS);
        assert_eq!(enter(|| ExitCode::from(2)), 2);
        assert_eq!(enter(|| ExitCode::SUCCESS), 0);
    }
}

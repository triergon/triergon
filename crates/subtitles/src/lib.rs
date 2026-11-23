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

pub mod cc;
pub mod context;
pub mod dvb;
pub mod dvd;
pub mod pgs;
pub mod text;

pub mod compat;
#[cfg(feature = "reference-trace")]
pub mod reference_trace;

mod canvas;
mod teletext;

use std::sync::atomic::{AtomicBool, Ordering};

/// `Error::Cancelled` once `cancel` is set.
fn cancelled(cancel: &AtomicBool) -> triergon_core::Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(triergon_core::Error::Cancelled)
    } else {
        Ok(())
    }
}

/// Microseconds to milliseconds, rounded to nearest with ties away from zero;
/// exact for every `i64`.
fn milliseconds(us: i64) -> i64 {
    // At most 2^63 + 500 before the division, below 2^54 after it.
    let ms = ((us.unsigned_abs() + 500) / 1000) as i64;
    if us < 0 { -ms } else { ms }
}

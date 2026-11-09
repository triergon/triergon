#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![warn(
    missing_debug_implementations,
    unreachable_pub,
    clippy::print_stdout,
    clippy::print_stderr
)]
// Pending: `missing_docs`, `clippy::unwrap_used`, `clippy::expect_used`.

pub mod compat;
#[cfg(feature = "reference-trace")]
pub mod reference_trace;

pub mod dvb;
pub mod text;

pub mod context;

pub mod cc;
pub mod dvd;
pub mod pgs;
pub(crate) mod teletext;

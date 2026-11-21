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

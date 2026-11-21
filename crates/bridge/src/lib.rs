#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![warn(
    missing_debug_implementations,
    unreachable_pub,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::unwrap_used,
    clippy::expect_used
)]
// Pending: `missing_docs`.
pub mod aac;
pub mod av1;
pub mod bitmap;
pub mod caption;
pub mod compat;
pub mod matroska;
pub mod mp4_dvd;
pub mod text;
pub mod ts;

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
pub mod aac;
pub mod access;
pub mod av1;
pub mod bitmap;
pub mod caption;
pub mod compat;
pub mod matroska;
pub mod mp4_dvd;
pub mod text;
pub mod ts;

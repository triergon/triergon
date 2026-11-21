#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![warn(clippy::print_stdout, clippy::print_stderr, clippy::unwrap_used)]
// Pending: `missing_docs`, `missing_debug_implementations`, `unreachable_pub`, `clippy::expect_used`.
pub mod aac;
pub mod avc;
pub mod clock;
mod dolby;
mod hevc;
mod pes;
pub mod progressive;
pub mod segment;

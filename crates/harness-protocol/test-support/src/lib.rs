//! Read comparison documents and verify the hashes of their inputs.
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
#![cfg_attr(
    test,
    allow(clippy::unwrap_used, reason = "tests assert authored inputs parse")
)]

pub mod json;
pub mod sha256;

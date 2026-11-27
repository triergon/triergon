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
pub mod avc;
pub mod clock;
mod dolby;
mod hevc;
pub mod packet;
mod pes;
mod pmt;
pub mod progressive;
pub mod segment;

// Identity of the elementary bytes assembled from PES payloads.
const ELEMENTARY_SOURCE: triergon_core::SourceId = triergon_core::SourceId(0);

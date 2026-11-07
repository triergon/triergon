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

mod checked;
pub mod crc;
mod rbsp;
mod reader;

pub use checked::{BitCursor, BitError, BitSource, bits_at};
pub use rbsp::Rbsp;
pub use reader::{BitReader, Cached, CachedBitReader, Direct, Reader, Window};

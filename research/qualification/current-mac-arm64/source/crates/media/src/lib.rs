//! Bounded compressed-media parsing. Experimental; no module is release-qualified.
#![forbid(unsafe_code)]

mod content_encoding;
mod ebml;
mod error;
pub mod matroska;
pub mod time;
pub mod timing;

pub use error::{Error, Result};

/// Explicit per-reader limits. Input and packets are streamed, never buffered as a file.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub metadata_bytes: usize,
    pub block_bytes: usize,
    pub tracks: usize,
    pub elements: u64,
    pub scan_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            metadata_bytes: 1024 * 1024,
            block_bytes: 16 * 1024 * 1024,
            tracks: 64,
            elements: 1_000_000,
            scan_bytes: 64 * 1024 * 1024,
        }
    }
}

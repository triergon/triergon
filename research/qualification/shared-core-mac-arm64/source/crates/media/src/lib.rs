//! Bounded compressed-media parsing. Experimental; no module is release-qualified.
#![forbid(unsafe_code)]

pub mod colour;
mod content_encoding;
mod cues;
mod ebml;
mod error;
pub mod incremental;
pub mod matroska;
pub mod metadata;
pub mod storage;
pub mod time;
pub mod timing;
pub mod zlib;

pub use error::{Error, Result};

/// Explicit per-reader limits. Input and packets are streamed, never buffered as a file.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Cumulative live library-owned capacity, including caller-retained aliases.
    /// Explicit `*_in` constructors can share a caller-provided core pool instead.
    pub live_storage_bytes: usize,
    pub metadata_bytes: usize,
    pub block_bytes: usize,
    pub tracks: usize,
    /// Cumulative segment-metadata source/arena storage, including validation workspace.
    pub metadata_storage_bytes: usize,
    pub metadata_nodes: usize,
    pub metadata_depth: usize,
    pub attachment_bytes: usize,
    pub elements: u64,
    /// Maximum bytes consumed in one bounded input/seek scan phase.
    pub scan_bytes: u64,
    pub index_entries: usize,
    pub decompression_ratio: usize,
    pub decompression_work: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            live_storage_bytes: 128 * 1024 * 1024,
            metadata_bytes: 1024 * 1024,
            block_bytes: 16 * 1024 * 1024,
            tracks: 64,
            metadata_storage_bytes: 16 * 1024 * 1024,
            metadata_nodes: 4096,
            metadata_depth: 32,
            attachment_bytes: 8 * 1024 * 1024,
            elements: 1_000_000,
            scan_bytes: 64 * 1024 * 1024,
            index_entries: 16384,
            decompression_ratio: 1024,
            decompression_work: 128 * 1024 * 1024,
        }
    }
}

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
pub mod colour;
pub mod compat;
mod configuration;
mod container;
mod content_encoding;
mod ebml;
mod element;
mod hdr10_plus;
pub mod metadata;
mod packet;
mod reader;
#[cfg(feature = "reference-trace")]
pub mod reference_trace;
mod seek;
mod step;
mod storage;
pub mod timing;
mod track;
mod zlib;

pub use configuration::{CodecState, Configuration, Initialization, InitializationOrigin};
pub use element::{ElementEvent, ElementHeader};
pub use hdr10_plus::{
    Hdr10Plus, Hdr10PlusGeometry, Hdr10PlusPeakGrid, Hdr10PlusPercentile, Hdr10PlusToneCurve,
    Hdr10PlusWindow,
};
pub use packet::block_group::{
    AdditionView, BlockGroupSource, ExtensionScope, ExtensionView, Presence, SourcePosition,
    StateView,
};
pub use packet::{Packet, PacketData, TypedPacket};
pub use reader::{BlockingReader, Event, Reader};
pub use seek::{SeekReceipt, SeekRequest};
pub use step::ParseStep;
pub use storage::{PayloadOwnership, SharedBytes, SharedText};
pub use track::extensions::{MappingExtension, MappingView, TrackExtensions, TrackExtensionsJob};
pub use track::{LegacyAacProfile, Track};
pub use triergon_core::{Error, Result};

/// Explicit per-reader limits. Input and packets are streamed, never buffered as a file.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Cumulative live library-owned capacity, including caller-retained aliases.
    /// Explicit `*_in` constructors can share a caller-provided core pool instead.
    pub live_storage_bytes: usize,
    /// Bytes of all structural metadata held since the reader started or was reset:
    /// the EBML header and level-one metadata elements, attachment payloads excepted.
    /// Also bounds each TrackEntry and decoded CodecPrivate.
    pub metadata_bytes: usize,
    /// Bytes of one SimpleBlock or BlockGroup, and of all decoded frames of one lace.
    pub block_bytes: usize,
    /// TrackEntry elements per Segment.
    pub tracks: usize,
    /// Cumulative segment-metadata source/arena storage, including validation workspace.
    pub metadata_storage_bytes: usize,
    /// Nodes of one segment-metadata document; also the descriptors (additions,
    /// references, mappings) kept for one BlockGroup or TrackEntry.
    pub metadata_nodes: usize,
    /// Nesting depth of a segment-metadata document, capped at 64.
    pub metadata_depth: usize,
    /// Attachment payload bytes of all Attachments together.
    pub attachment_bytes: usize,
    /// Element headers read between resets and seeks.
    pub elements: u64,
    /// Maximum bytes consumed in one bounded input/seek scan phase.
    pub scan_bytes: u64,
    /// CueTrackPositions entries kept from Cues.
    pub index_entries: usize,
    /// Most decoded bytes per compressed byte of one zlib-encoded value.
    pub decompression_ratio: usize,
    /// Work units (blocks, symbols, checksum chunks) for one zlib-encoded value.
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

#[cfg(test)]
#[path = "../tests/support/deflate.rs"]
mod deflate_kit;
#[cfg(test)]
#[path = "../tests/support/ebml.rs"]
mod ebml_kit;
#[cfg(test)]
mod tests_internal;

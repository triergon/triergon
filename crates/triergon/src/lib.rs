//! Triergon: bounded, pure-Rust media parsing.
//!
//! This facade is the consumer boundary. It re-exports
//! the shared core vocabulary, opens a source as an [`AnyReader`] chosen from
//! its first bytes, and exposes each format crate behind its own Cargo feature.
//! Generic code (the harness, format tests) keeps using the concrete readers.
//!
//! Experimental.
#![forbid(unsafe_code)]

mod any;

pub use any::{AnyPacket, AnyReader, Probe, probe};
pub use triergon_core::container::{
    ColourDescription, ColourRange, ContainerReader, ContentLight, Event, MasteringDisplay,
    MediaKind, Origin, PacketInfo, RandomAccess, ReaderStep, SeekMethod, SeekPolicy, SeekReceipt,
    SeekRequest, Sourced, TrackId, TrackInfo, Trim,
};
pub use triergon_core::{
    Backpressure, Budget, Bytes, Error, InputRequest, InputToken, InputWindow, Result,
    SourceCapabilities, SourceFailure, SourceId, SourceLocation, Step, Time, TimeBase, WorkBudget,
};

#[cfg(feature = "framing")]
pub use triergon_framing as framing;
#[cfg(feature = "isobmff")]
pub use triergon_isobmff as isobmff;
#[cfg(feature = "matroska")]
pub use triergon_matroska as matroska;
#[cfg(feature = "mpegts")]
pub use triergon_mpegts as mpegts;
#[cfg(feature = "subtitles")]
pub use triergon_subtitles as subtitles;

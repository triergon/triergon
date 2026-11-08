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

mod any;

pub use any::{AnyOther, AnyPacket, AnyReader, Probe, open_blocking, probe};
pub use triergon_core::blocking::Blocking;
pub use triergon_core::container::{
    CodecId, ColourDescription, ColourRange, ContainerReader, ContentLight, Event,
    MasteringDisplay, MediaKind, Origin, PacketInfo, RandomAccess, ReaderStep, SeekMethod,
    SeekPolicy, SeekReceipt, SeekRequest, Sourced, TrackId, TrackInfo, Trim,
};
pub use triergon_core::{
    Backpressure, Budget, Bytes, Error, InputRequest, InputToken, InputWindow, Result,
    SourceCapabilities, SourceFailure, SourceId, SourceLocation, Step, Time, TimeBase, WorkBudget,
};

#[cfg(feature = "elementary")]
pub use triergon_elementary as elementary;
#[cfg(feature = "framing")]
pub use triergon_framing as framing;
#[cfg(feature = "isobmff")]
pub use triergon_isobmff as isobmff;
#[cfg(feature = "matroska")]
pub use triergon_matroska as matroska;
#[cfg(feature = "mpegts")]
pub use triergon_mpegts as mpegts;
#[cfg(feature = "ogg")]
pub use triergon_ogg as ogg;
#[cfg(feature = "subtitles")]
pub use triergon_subtitles as subtitles;

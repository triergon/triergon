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
pub mod compat;
mod engine;
mod event;
pub mod mp4;
pub mod pes_feed;
mod player;
pub mod probe;
pub mod pump;
mod source;

pub use engine::{Engine, Track};
pub use event::{DemuxTimes, EngineStep, Event, Packet, TsEvent};
pub use player::{
    ClockChoice, Delivery, Output, OutputScope, Player, PlayerPolicy, Progress, SeekIntent,
    Selection, State, TrackKey, TrackKind,
};
pub use source::{ByteSource, Source, SourceOwner, SourceReply};
pub use triergon_core::{Error, Result};

/// Matroska playback beside the reader's own events.
pub mod matroska {
    pub use crate::player::MatroskaReplayOutput as ReplayOutput;
}

/// The container format of an input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// Matroska or WebM.
    Matroska,
    /// MP4 or QuickTime.
    Mp4,
    /// An MPEG transport stream in this packet format.
    Ts(triergon_mpegts::PacketFormat),
}

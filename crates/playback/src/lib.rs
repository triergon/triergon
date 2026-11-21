#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![warn(clippy::print_stdout, clippy::print_stderr, clippy::unwrap_used)]
// Pending: `missing_docs`, `missing_debug_implementations`, `unreachable_pub`, `clippy::expect_used`.
pub mod compat;
mod engine;
mod event;
pub mod io;
pub mod mp4_open;
pub mod mp4_plan;
pub mod pes_feed;
mod player;
mod sample_sort;
pub use engine::Engine;
pub use event::{
    EngineStep, Event, Mp4EmptyPacket, Mp4Event, Mp4MissingSample, Mp4Packet, Mp4ShortPacket,
    TsEvent,
};
pub use player::{
    ClockChoice, Delivery, MatroskaReplayOutput, Mp4DemuxSeekCommand, Mp4DemuxSeekJob,
    Mp4DemuxSeekRequest, Mp4RangeOpen, Mp4RangePlayer, Mp4RangeSeek, Mp4RangeSeekJob,
    Mp4SyncSeekCommand, Mp4SyncSeekJob, Mp4SyncSeekRequest, Output, OutputScope, Player,
    PlayerPolicy, Progress, SeekIntent, Selection, State, TrackKey, TrackKind,
};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Matroska,
    Mp4,
    Ts(triergon_mpegts::PacketFormat),
}
pub use triergon_core::{Error, Result};

pub use engine::mp4::progressive;
pub mod pump;

pub mod input;
pub use input::{ByteSource, Source, SourceReply};

pub mod catalogue;

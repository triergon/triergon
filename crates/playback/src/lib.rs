#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![warn(clippy::print_stdout, clippy::print_stderr, clippy::unwrap_used)]
// Pending: `missing_docs`, `missing_debug_implementations`, `unreachable_pub`, `clippy::expect_used`.
pub mod compat;
mod engines;
pub mod io;
mod mp4_native_replay;
pub mod mp4_open;
pub mod mp4_plan;
pub mod pes_feed;
mod player;
mod sample_sort;
pub use engines::{
    BackendStep, Engine, Mp4Event, Mp4NativeEmptyPacket, Mp4NativeMissingSample,
    Mp4NativeShortPacket, Mp4Packet, RawEvent, TsEvent, TsNativeCarriage, TsNativeConfiguration,
    TsNativePrivateAudioSelection, TsNativeSelection, TsNativeTrackSelection,
};
pub use player::{
    ClockChoice, Delivery, MatroskaReplayOutput, Mp4NativeSeekCommand, Mp4NativeSeekJob,
    Mp4NativeSeekRequest, Mp4RangeOpen, Mp4RangePlayer, Mp4RangeSeek, Mp4RangeSeekJob,
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

pub mod progressive;
pub mod pump;

pub mod native_startup;

pub mod input;
pub use input::{ByteSource, Source, SourceReply};

pub mod catalogue;

mod ts_payload;

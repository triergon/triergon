//! Typed playback composition over source-owned container, packet and subtitle jobs.
//! Experimental; no HTTP, platform sink, device or numerical qualification.
#![forbid(unsafe_code)]
mod engines;
pub mod io;
mod mp4_native_replay;
pub mod mp4_open;
pub mod mp4_plan;
pub mod nal_bridge;
mod player;
mod sample_sort;
pub use engines::{
    BackendStep, Engine, Mp4Event, Mp4NativeEmptyPacket, Mp4NativeMissingSample,
    Mp4NativeShortPacket, Mp4Packet, RawEvent, TsEvent, TsNativeCarriage, TsNativeConfiguration,
    TsNativePrivateAudioSelection, TsNativeSelection, TsNativeTrackSelection,
};
pub use player::{
    ClockChoice, Delivery, Mp4NativeSeekCommand, Mp4NativeSeekJob, Mp4NativeSeekRequest,
    Mp4RangeOpen, Mp4RangePlayer, Mp4RangeSeek, Mp4RangeSeekJob, Mp4SyncSeekCommand,
    Mp4SyncSeekJob, Mp4SyncSeekRequest, Output, Player, PlayerPolicy, Progress, SeekIntent,
    Selection, State, TrackKey, TrackKind,
};
use triergon_core::Error as CoreError;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Matroska,
    Mp4,
    Ts(triergon_mpegts::PacketFormat),
}
#[derive(Debug)]
pub enum Error {
    Core(CoreError),
    Matroska(triergon::Error),
    Closed,
}
pub type Result<T> = std::result::Result<T, Error>;
impl From<CoreError> for Error {
    fn from(e: CoreError) -> Self {
        Self::Core(e)
    }
}
impl From<triergon::Error> for Error {
    fn from(e: triergon::Error) -> Self {
        match e {
            triergon::Error::Core(c) => Self::Core(c),
            other => Self::Matroska(other),
        }
    }
}

pub mod progressive;
pub mod pump;
pub mod raw_video;

pub mod native_startup;

pub mod aac_bridge;
pub mod av1_bridge;
pub mod hevc_bridge;
pub mod matroska_feed;
pub mod text_bridge;

pub mod bitmap_bridge;
pub mod dvb_ts;

pub mod input;
pub use input::{ByteSource, Source, SourceReply};

pub mod catalogue;

pub mod hls_aac;
pub mod hls_avc;
pub mod hls_clock;
pub mod hls_segment;

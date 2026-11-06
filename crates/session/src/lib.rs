#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![warn(clippy::print_stdout, clippy::print_stderr, clippy::unwrap_used)]
// Pending: `missing_docs`, `missing_debug_implementations`, `unreachable_pub`, `clippy::expect_used`.

pub mod audio;
pub mod load;
pub mod mix;
pub mod queue;
pub mod video;

pub use audio::{AudioSession, PcmBlock};
pub use load::{LoadSelection, select};
pub use queue::{FeedError, QueueLimits};
pub use triergon_codec::{AudioCodec, AudioDecoder, VideoCodec, VideoDecoder};
pub use video::{Published, VideoSession};

/// The pure-Rust decoder for `codec`.
#[cfg(feature = "codecs")]
pub fn video_decoder(codec: VideoCodec) -> Box<dyn VideoDecoder> {
    match codec {
        VideoCodec::H264 => Box::new(triergon_codec_h264::Decoder::new()),
        VideoCodec::Hevc => Box::new(triergon_codec_hevc::Decoder::new()),
    }
}

/// The pure-Rust decoder for `codec`.
#[cfg(feature = "codecs")]
pub fn audio_decoder(codec: AudioCodec) -> Box<dyn AudioDecoder> {
    match codec {
        AudioCodec::Ac3 | AudioCodec::Eac3 => Box::new(triergon_codec_ac3::Decoder::new()),
        AudioCodec::Aac => Box::new(triergon_codec_aac::Decoder::new()),
        AudioCodec::Dts => Box::new(triergon_codec_dca::Decoder::new()),
    }
}

/// Why a session is not producing output right now. A starved session waits
/// for input; an ended one has drained its decoder after an explicit end of
/// stream; a failed one stays failed until the next load/flush/stop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionState {
    /// No session (nothing loaded, or the stream named no codec for this lane).
    Idle,
    /// Accepting input; output follows the clock.
    Running,
    /// Every queued access unit is decoded and every picture/sample delivered.
    Starved,
    /// `end_of_stream` was called and all decoded output has been delivered.
    Ended,
    /// The decoder rejected the stream; the message names the reason.
    Failed(String),
}

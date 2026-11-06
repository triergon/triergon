//! In-process desktop playback sessions.
//!
//! The session layer owns bounded input queues, decoder lifecycle, timestamps
//! through reordering, clock-gated publication, generations, flush/seek/stop,
//! decoded end of stream and bounded shutdown. Decoding itself is delegated to
//! the pure-Rust codec crates through the [`triergon_codec`] traits (the
//! `codecs` feature provides [`video_decoder`] and [`audio_decoder`]); output
//! devices and GPU upload stay with the caller.
//!
//! Nothing here spawns a process, links a native library or falls back to one.
#![forbid(unsafe_code)]

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

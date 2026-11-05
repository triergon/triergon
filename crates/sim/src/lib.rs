//! In-process desktop-simulator playback sessions (D4 / O61).
//!
//! Streem's simulator pipes video and audio access units to external `ffmpeg`
//! children (`sim_video.rs`, `sim_audio.rs`). This crate is the replacement's
//! session layer: it owns bounded input queues, decoder lifecycle, timestamps
//! through reordering, clock-gated publication, generations, flush/seek/stop,
//! decoded end of stream and bounded shutdown. Decoding itself is delegated to
//! the pure-Rust codec crates through the [`codec`] traits; output devices and
//! GPU upload stay with the consumer adapter (see `research/d4/ADAPTER-CONTRACT.md`).
//!
//! Nothing here spawns a process, links a native library or falls back to one.
#![forbid(unsafe_code)]

#[cfg(feature = "codecs")]
pub mod adapters;
pub mod audio;
pub mod codec;
pub mod load;
pub mod mix;
pub mod queue;
pub mod scale;
pub mod video;

pub use audio::{AudioSession, PcmBlock};
pub use codec::{AudioCodec, VideoCodec};
pub use load::{LoadSelection, select};
pub use queue::{FeedError, QueueLimits};
pub use video::{Published, VideoSession};

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

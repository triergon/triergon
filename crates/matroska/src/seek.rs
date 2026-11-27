//! Seeking: requests, receipts and the Cues index they are planned from.
pub(crate) mod cue_validation;
pub(crate) mod cues;
pub(crate) mod epoch;
pub(crate) mod plan;

/// The outcome of a completed seek; times are source presentation times in nanoseconds.
#[derive(Clone, Copy, Debug)]
pub struct SeekReceipt {
    /// The target the seek was asked for.
    pub requested_ns: i64,
    /// Presentation time of the video keyframe the seek landed on.
    pub actual_ns: i64,
    /// Source bytes the seek read to find and verify its landing, metadata included.
    pub scanned_bytes: u64,
    /// Whether Cues located the landing, rather than a bounded scan.
    pub used_index: bool,
    /// Decode returned packets, but suppress presentation until this source time.
    pub discard_until_ns: i64,
    /// The largest track SeekPreRoll subtracted from the target; 0 unless this was
    /// a playback seek.
    pub preroll_ns: u64,
    /// The complete first Cluster is replayed. Consumers may apply one-time
    /// stream initialization; this fact comes from source position, not PTS zero.
    pub replays_from_start: bool,
}
#[derive(Clone, Copy, Debug)]
pub(crate) struct Cue {
    pub ticks: u64,
    pub track: u64,
    pub cluster: u64,
    pub relative: Option<u64>,
    pub block: Option<u64>,
    pub codec_state: u64,
}

/// A seek on one video track of a [`Reader`](crate::Reader).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SeekRequest {
    /// Index of the video track in [`Configuration::tracks`](crate::Configuration::tracks).
    pub track: usize,
    /// Target presentation time in nanoseconds.
    pub requested_ns: i64,
    /// Whether this is a playback seek, as
    /// [`Reader::seek_for_playback`](crate::Reader::seek_for_playback) starts, rather than
    /// [`Reader::seek_previous`](crate::Reader::seek_previous).
    pub playback_preroll: bool,
}

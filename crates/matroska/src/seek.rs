//! Seeking: requests, receipts and the Cues index they are planned from.
pub(crate) mod cue_validation;
pub(crate) mod cues;
pub(crate) mod epoch;
pub(crate) mod plan;

#[derive(Clone, Copy, Debug)]
pub struct SeekReceipt {
    pub requested_ns: i64,
    pub actual_ns: i64,
    pub scanned_bytes: u64,
    pub used_index: bool,
    /// Decode returned packets, but suppress presentation until this source time.
    pub discard_until_ns: i64,
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
    pub track: usize,
    pub requested_ns: i64,
    pub playback_preroll: bool,
}

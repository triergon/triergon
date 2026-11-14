//! Comparison-only projections, compiled with the `reference-trace` feature.
//!
//! These report values in the form FFmpeg's public API exposes them, so tests
//! can compare them with recorded reference output. They are not behaviour a
//! consumer selects outside a comparison, and they are absent from default
//! builds. Their reference revision is the one named on the matching
//! [`compat`](crate::compat) family.

/// Text carriage in the form of FFmpeg's demuxer API.
///
/// [`Session`](carriage::Session) publishes one immutable
/// [`Snapshot`](carriage::Snapshot) per input: FFmpeg's stream and packet
/// fields with its codec identifiers, return codes and millisecond document
/// timing, which keeps zero or negative intervals, and the reference's UTF-16
/// SubRip line-entry byte behaviour. `recover_pending` returns inputs after
/// cancellation or a resource failure.
///
/// Reference: FFmpeg git revision `d32b387f` on the 9.0 branch.
pub mod carriage {
    pub use crate::text::carriage::*;
}

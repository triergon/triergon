//! Comparison-only projections, compiled with the `reference-trace` feature.
//!
//! These report values in the form FFmpeg's public API exposes them, or label
//! the receiver view that produced an output, so tests can compare them with
//! recorded reference output. They are not behaviour a consumer selects outside
//! a comparison, and they are absent from default builds. With the feature enabled,
//! [`dvb::Decoder::receiver_failure`](crate::dvb::Decoder::receiver_failure),
//! the `view_profile()` and `reference_revision()` labels of
//! [`compat::dvb::Palette`](crate::compat::dvb::Palette) and
//! [`compat::dvb::Publication`](crate::compat::dvb::Publication) are also available. Their reference revision is the one named on the matching
//! [`compat`](crate::compat) family.

/// DVB receiver labels and the receiver's refusal of a truncated packet.
pub mod dvb {
    pub use crate::dvb::receiver::{RECEIVER_VIEW_PROFILE, REFERENCE_REVISION, ReceiverFailure};
}

/// The PGS receiver projection of strict source display sets.
///
/// [`Projector`](pgs::Projector) projects each strict
/// [`DisplaySet`](crate::pgs::DisplaySet) onto the rectangles FFmpeg's public
/// API returns for it, keeping the source set alongside. A
/// [`ProjectedPacket`](pgs::ProjectedPacket) groups the sets of one packet, of
/// which the receiver reports only the first.
pub mod pgs {
    pub use crate::pgs::projector::{
        Canvas, ProjectedDisplay, ProjectedPacket, ProjectedRect, ProjectionStatus, Projector,
        RasterOrigin,
    };
}

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

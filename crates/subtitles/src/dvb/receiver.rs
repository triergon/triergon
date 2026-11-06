//! Receiver view of a DVB display set, kept apart from the source facts.
use triergon_core::{BudgetVec, Bytes, Rounding, Time, TimeBase};

/// Comparison label of the receiver view; see [`crate::reference_trace::dvb`].
#[cfg(feature = "reference-trace")]
pub const RECEIVER_VIEW_PROFILE: &str = "d32-default-explicit-CLUT-DVB-receiver-v1";
/// The FFmpeg revision the receiver view reproduces.
#[cfg(feature = "reference-trace")]
pub const REFERENCE_REVISION: &str = "d32b387f2b0a484599d4587d651891f0c63c4238";

/// Which view a [`Decoder`](super::Decoder) publishes. `Source` is the default;
/// `Receiver` adds the [`compat::dvb`](crate::compat::dvb) presentation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReceiverView {
    Source,
    Receiver,
}

/// Source validation stays the default. Omission is only an explicit native
/// projection of a region with no raster, never geometry repair or clipping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GeometryPolicy {
    ValidateSource,
    OmitUnrendered,
}

/// Native emits no rectangle. The original source geometry and any strict
/// validation error are retained rather than relabelled as a conforming region.
#[derive(Debug)]
pub struct OmittedRegion {
    pub id: u8,
    pub source_address: super::Point,
    pub source_size: super::Size,
    pub original_composition: Bytes,
    pub source_error: Option<triergon_core::Error>,
}

#[derive(Debug)]
pub struct Palette {
    pub rgba: Bytes,
    pub words: BudgetVec<u32>,
    /// Source provenance only. Native defaults/automatic colours are a separate
    /// constructed view; this flag never certifies general native equality.
    pub all_entries_explicit: bool,
}

/// Comparison labels. Every receiver palette carries the same two values.
#[cfg(feature = "reference-trace")]
impl Palette {
    /// The receiver view's profile label, [`RECEIVER_VIEW_PROFILE`].
    pub fn view_profile(&self) -> &'static str {
        RECEIVER_VIEW_PROFILE
    }
    /// The reproduced FFmpeg revision, [`REFERENCE_REVISION`].
    pub fn reference_revision(&self) -> &'static str {
        REFERENCE_REVISION
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublicationReason {
    EndOfDisplay,
    CompletePrefix,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Publication {
    pub packet_sequence: u64,
    /// Relative to the codec packet when a native PES projection is attached
    /// to the event, or to the declared decoder input otherwise.
    pub consumed_prefix_bytes: usize,
    pub reason: PublicationReason,
    /// The packet that triggered publication can differ from the first packet
    /// of the display set. Keep its configured source clock separately.
    pub packet_clock: Option<(super::SourceClock, super::PacketTiming)>,
    pub context_canvas: Option<super::Size>,
    /// Only an exactly representable sampled source instant is assigned here.
    /// Unknown/nonintegral/overflow clocks remain explicitly unresolved.
    pub exact_pts_microseconds: Option<i64>,
    pub start_display_ms: u32,
    pub end_display_ms: u32,
}

/// Comparison labels. Every publication carries the same two values.
#[cfg(feature = "reference-trace")]
impl Publication {
    /// The receiver view's profile label, [`RECEIVER_VIEW_PROFILE`].
    pub fn view_profile(&self) -> &'static str {
        RECEIVER_VIEW_PROFILE
    }
    /// The reproduced FFmpeg revision, [`REFERENCE_REVISION`].
    pub fn reference_revision(&self) -> &'static str {
        REFERENCE_REVISION
    }
}

/// The receiver's refusal of a packet whose remaining bytes are too few for a
/// segment header: the packet, the offset and length of the unread tail, and
/// FFmpeg's `AVERROR_INVALIDDATA`.
#[cfg(feature = "reference-trace")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReceiverFailure {
    pub packet_sequence: u64,
    pub packet_offset: usize,
    pub code: i32,
    pub unconsumed_bytes: usize,
}

pub(crate) fn exact_microseconds(pts: Option<Time>) -> Option<i64> {
    let time = pts?;
    let base = TimeBase::new(1, 1_000_000).ok()?;
    let ticks = time.to_ticks(base, Rounding::TowardZero).ok()?;
    if Time::from_ticks(ticks, base).ok()? != time {
        return None;
    }
    i64::try_from(ticks).ok()
}

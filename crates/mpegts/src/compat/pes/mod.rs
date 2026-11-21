//! The demuxer's PES packets and clock.
//!
//! [`DemuxObserver`] cuts the transport packets of one selected PID
//! ([`DemuxScope`]) into the packets FFmpeg's `mpegts` demuxer emits, beside the
//! strict source view: finite PES at their declared length, unbounded PES at the
//! next start, and damaged or headerless ones as the demuxer does. A
//! [`DemuxPacket`] may therefore be partial, and it says why it ended
//! ([`DemuxBoundary`]) and what it holds ([`DemuxPayloadKind`]). Its constructors
//! select the carriage: [`RegisteredCarriage`] for AVC, HEVC and AAC,
//! [`PrivateAudioCarriage`] for AC-3 and E-AC-3, and
//! [`DvbSubtitlesDescriptors`] for DVB subtitles.
//! [`TransportFaultRecovery`] adds opt-in extraction from finite TS188 AVC
//! carriers with TEI or scrambling. Its [`TransportObservation::Faulted`]
//! result retains the actual source diagnostic and opaque bytes separately
//! from validated source PES.
//! [`PesFaultPolicy`] separately opts into isolated timing-header faults and
//! fully checked rejected clock fields. [`PesFaultPacket`] retains the source
//! refusal; [`DiagnosticTimeView`] reports optional encoded compatibility time.
//! A source-signalled PCR/DI transition needs [`TransportFaultRecovery::confirm_source_timeline`].
//! Opt-in terminal transport handling needs real core/framer EOF and source
//! Finished; [`TransportTailConfirmation`] retains the excluded source fault.
//!
//! [`PesCutter`] cuts one PID of any stream type into the packets the demuxer
//! hands a decoder ([`CutPes`]): each starts at a unit start and ends at its
//! declared length, the next unit start or the end of the input, keeps its raw
//! timestamps and the program clock it started under, and is never empty.
//! [`pcr_bounded_pts`] is the demuxer's teletext PTS, bounded by that clock
//! ([`CutPes::pcr_bounded_pts`] for a cut packet).
//!
//! [`DemuxClock`] places a packet's timestamps in the demuxer's 33-bit interval,
//! fixed by the first timestamp it sees, and returns a [`DemuxTimeView`] beside
//! the raw source values ([`DemuxClock::place`] for raw values alone).
//!
//! Reference: FFmpeg git revision `d32b387f` on the 9.0 branch.

pub(crate) mod clock;
pub(crate) mod completion;
pub(crate) mod cutter;
pub(crate) mod fault;
pub(crate) mod observer;

pub use clock::{
    ClockOrigin, DemuxClock, DemuxProfile, DemuxTimeView, DiagnosticTimeView, EncodedTimeView,
    TimestampOrigin,
};
/// Control-only completion from complete physical carrier coverage and
/// actual source EOF. The confirmation never creates a validated PES scope
/// or accepts media from a rejected carrier.
pub use completion::{
    ControlCarrierKind, ControlPatFact, NoMediaConfirmation, PacketCompletion, PhysicalCarrier,
    PhysicalNullRun,
};
pub use cutter::{CutPes, PesCutter, pcr_bounded_pts};
pub use fault::{
    MalformedInputCompletion, MalformedInputFact, MalformedInputKind, MalformedSourceDisposition,
    MalformedSourceEvent, NoPacketsConfirmation,
};
pub use fault::{
    PesFaultCarrier, PesFaultHeader, PesFaultKind, PesFaultPacket, PesFaultPolicy,
    TransportFaultHeader, TransportFaultKind, TransportFaultPacket, TransportFaultRecovery,
    TransportObservation, TransportTailConfirmation,
};
pub use observer::{
    CompactDemuxPacket, CompactPayload, DemuxBoundary, DemuxObserver, DemuxPacket,
    DemuxPayloadKind, DemuxScope, DemuxSpan, DemuxSpanView, DemuxSpanViews,
    DvbSubtitlesDescriptors, PayloadCursor, PrivateAudioCarriage, PrivateAudioDescriptors,
    ReadyContinuation, RegisteredCarriage, SourceContext, selected_track_kind,
};

/// How to handle the reserved bits of a program clock reference.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PcrPolicy {
    #[default]
    Strict,
    /// Retain the original bits while accepting a valid PCR base and
    /// extension. Other adaptation-field checks still apply.
    IgnoreReservedBits,
}

/// Reporting of unframed payload in a private-audio carriage at logical EOF.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UnframedAudioPolicy {
    /// Require the source namespace before admitting a private-audio framer.
    #[default]
    Strict,
    /// With unspecified namespace, admit one finite private-audio PES per
    /// uninterrupted run when its payload has no AC-3 or E-AC-3 syncword
    /// and does not end with the first byte of one.
    /// The observation preserves the track's source codec and namespace.
    ReportAtEof,
}

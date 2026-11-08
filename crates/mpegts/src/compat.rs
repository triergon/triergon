//! Reference-compatible policies.
//!
//! The types at the crate root follow H.222.0 and report every source fact. Where
//! FFmpeg's demuxer cuts packets, positions or timestamps differently and a consumer
//! needs FFmpeg's result, the policies here reproduce that demuxer for the input.
//! They are fed from the same transport packets and never rewrite source facts. Each
//! module is one policy family; its documentation names the FFmpeg build it
//! reproduces.
//!
//! Projections that exist only to compare traces with FFmpeg are not policies. They
//! are compiled only with the `reference-trace` feature.

pub mod pes {
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
    pub use crate::demux_clock::{
        ClockOrigin, DemuxClock, DemuxProfile, DemuxTimeView, DiagnosticTimeView, EncodedTimeView,
        TimestampOrigin,
    };
    pub use crate::demux_pes::{
        CompactDemuxPacket, CompactPayload, CutPes, DemuxBoundary, DemuxObserver, DemuxPacket,
        DemuxPayloadKind, DemuxScope, DemuxSpan, DemuxSpanView, DemuxSpanViews,
        DvbSubtitlesDescriptors, PayloadCursor, PesCutter, PrivateAudioCarriage,
        PrivateAudioDescriptors, ReadyContinuation, RegisteredCarriage, SourceContext,
        pcr_bounded_pts, selected_track_kind,
    };
    pub use crate::demux_transport::{
        PesFaultCarrier, PesFaultHeader, PesFaultKind, PesFaultPacket, PesFaultPolicy,
        TransportFaultHeader, TransportFaultKind, TransportFaultPacket, TransportFaultRecovery,
        TransportObservation,
    };
    /// Control-only completion from complete physical carrier coverage and
    /// actual source EOF. The confirmation never creates a validated PES scope
    /// or accepts media from a rejected carrier.
    pub use crate::packet_completion::{
        ControlCarrierKind, ControlPatFact, NoMediaConfirmation, PacketCompletion, PhysicalCarrier,
        PhysicalNullRun,
    };
}

pub mod stream {
    //! The demuxer's streams.
    //!
    //! [`ProgramReader`] reads the PAT and PMT sections the demuxer accepts and
    //! reports each [`Program`] with its elementary streams. [`codec`] is the
    //! codec the demuxer opens for a PMT entry, [`is_video`] whether it opens a
    //! video stream, and [`configuration`] the decoder configuration it derives
    //! from the entry's DVB subtitling or teletext descriptor. The standard
    //! readings are [`TransportStream`] and [`ElementaryStream::codec`].
    //!
    //! Reference: FFmpeg git revision `d32b387f` on the 9.0 branch.
    //!
    //! [`TransportStream`]: crate::TransportStream
    //! [`ElementaryStream::codec`]: crate::psi::ElementaryStream::codec
    pub use crate::demux_stream::{Program, ProgramReader, codec, configuration, is_video};
}

pub mod input {
    //! The demuxer's start position.
    //!
    //! FFmpeg's demuxer reports packet positions against the carrier grid it finds at
    //! the start of the input. [`InputBootstrap`] inspects the initial window for that
    //! grid without consuming input. The [`InputContext`] it produces binds a
    //! [`DemuxObserver`](super::pes::DemuxObserver) to the same positions through
    //! [`bind_input`](super::pes::DemuxObserver::bind_input).
    //!
    //! Reference: FFmpeg git revision `d32b387f` on the 9.0 branch.
    pub use crate::demux_input::{InputBootstrap, InputContext, InputStep};
}

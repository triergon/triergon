//! Comparison-only projections, compiled with the `reference-trace` feature.
//!
//! These report what FFmpeg's demuxer does with an input, so the harness can compare
//! traces byte for byte. They are not behaviour a consumer selects outside a
//! comparison, and they are absent from default builds. With the feature enabled,
//! [`DemuxProfile`](crate::compat::pes::DemuxProfile) also names the reference
//! build and its options, and these are also available:
//! [`DemuxObserver::profile`](crate::compat::pes::DemuxObserver::profile),
//! [`DemuxObserver::operation`](crate::compat::pes::DemuxObserver::operation),
//! [`PrivateAudioCarriage::label`](crate::compat::pes::PrivateAudioCarriage::label),
//! [`DemuxClock::project_encoded_header`](crate::compat::pes::DemuxClock::project_encoded_header),
//! [`DemuxClock::project_diagnostic_header`](crate::compat::pes::DemuxClock::project_diagnostic_header),
//! [`InputContext::basis`](crate::compat::input::InputContext::basis) and
//! [`ElementaryStream::dvb_single_service_view`](crate::psi::ElementaryStream::dvb_single_service_view).

pub mod recovery {
    //! The demuxer's packets from rejected carriers.
    //!
    //! [`RecoveryObserver`] wraps a
    //! [`DemuxObserver`](crate::compat::pes::DemuxObserver) and also assembles the
    //! packets FFmpeg emits for carriers the source view rejects
    //! ([`RecoveredPacket`], [`DiagnosticPacket`]). It follows one selected program
    //! through PAT and PMT changes ([`PatSelection`], [`PmtBindingJob`]).
    //!
    //! Reference: FFmpeg git revision `d32b387f` on the 9.0 branch.
    pub use crate::demux_clock::{DiagnosticTimeView, EncodedTimeView};
    pub use crate::demux_recovery::{
        DemuxObservation, DiagnosticCarrier, DiagnosticHeader, DiagnosticPacket, EncodedHeader,
        PatRoute, PatSelection, PmtBinding, PmtBindingJob, RecoveredPacket, RecoveryObserver,
        RejectedPolicy, SelectedFault,
    };
}

pub mod catalogue {
    //! The demuxer's programs and streams.
    //!
    //! [`Catalogue`] projects strict PAT and PMT events onto FFmpeg's program and
    //! stream order, its absent-PCR value and language tags.
    //! [`PsiCompatibilityPolicies`] adds the tables the demuxer accepts and the
    //! source view does not, and [`PsiCompatibilityEvidence`] records each one with
    //! its source status.
    //!
    //! Reference: FFmpeg git revision `d32b387f` on the 9.0 branch.
    pub use crate::demux_catalogue::{
        Catalogue, CatalogueCodec, CatalogueEvidence, Program, PsiCompatibility,
        PsiCompatibilityEvidence, PsiCompatibilityPolicies, PsiCompatibilityRecord,
        PsiCompatibilitySourceStatus, Stream,
    };
}

pub mod packet_completion {
    //! Whether an input held any media.
    //!
    //! [`PacketCompletion`] classifies every physical carrier of an input with no
    //! selected PES scope and confirms that none carried media
    //! ([`NoMediaConfirmation`]), which is when FFmpeg's demuxer ends without a
    //! packet.
    //!
    //! Reference: FFmpeg git revision `d32b387f` on the 9.0 branch.
    pub use crate::packet_completion::{
        ControlCarrierKind, ControlPatFact, NoMediaConfirmation, PacketCompletion, PhysicalCarrier,
        PhysicalNullRun,
    };
}

pub mod dvb {
    //! The demuxer's codec parameters for one DVB subtitling service.
    //!
    //! [`ElementaryStream::dvb_single_service_view`](crate::psi::ElementaryStream::dvb_single_service_view)
    //! returns the five extradata bytes FFmpeg derives from a stream that signals
    //! exactly one service in one descriptor.
    //!
    //! Reference: FFmpeg git revision `d32b387f` on the 9.0 branch.
    pub use crate::psi::dvb::{DvbSingleServiceView, DvbViewProfile};
}

pub mod input {
    //! Which carrier offset the demuxer reports positions from.
    //!
    //! Reference: FFmpeg git revision `d32b387f` on the 9.0 branch.
    pub use crate::demux_input::PositionBasis;
}

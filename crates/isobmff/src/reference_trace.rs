//! Comparison-only projections, compiled with the `reference-trace` feature.
//!
//! These report values in the form FFmpeg's public API exposes them, or model how
//! FFmpeg's input layer reaches a result, so the harness can compare traces byte for
//! byte. They are not behaviour a consumer selects outside a comparison, and they are
//! absent from default builds.
//!
//! With the feature enabled,
//! [`DemuxProfile::WORKER_SHA256`](crate::compat::mov::packets::DemuxProfile::WORKER_SHA256),
//! [`RunRecoveryProfile::REFERENCE_WORKER_SHA256`](crate::compat::mov::recovery::RunRecoveryProfile::REFERENCE_WORKER_SHA256),
//! [`DescriptionRecoveryTable`](crate::compat::mov::recovery::DescriptionRecoveryTable)'s
//! transport cursors,
//! [`DescriptionEvent::modeled_transport_read`](crate::compat::mov::recovery::DescriptionEvent::modeled_transport_read)
//! and [`RecoveryFacts::receiver_cause`](crate::compat::mov::encoded::RecoveryFacts::receiver_cause)
//! are also available.

pub mod mov {
    //! Projections of FFmpeg's MOV demuxer. The reference revision is the one named
    //! on [`compat::mov`](crate::compat::mov).

    pub mod transport {
        //! Models of FFmpeg's buffered reader during open.
        //!
        //! [`RawVideoTransportModel`] and [`RootWalkTransportModel`] work out the
        //! read windows FFmpeg can hold after walking a file's top-level boxes, as a
        //! set of possible states ([`ModeledOpenState`]). Passed to
        //! [`DescriptionRecoveryTable`](crate::compat::mov::recovery::DescriptionRecoveryTable),
        //! they let a recovered first packet past the physical end report the
        //! end-of-file FFmpeg reaches ([`ModeledInitialRead`]).
        pub use crate::raw_video_transport::{
            EntryRevisit, ModeledInitialRead, ModeledOpenState, RawVideoTransportBuilder,
            RawVideoTransportModel, RawVideoTransportProfile,
        };
        pub use crate::root_walk_transport::{
            RootWalkTransportBuilder, RootWalkTransportModel, RootWalkTransportProfile,
        };
    }

    pub mod operation {
        //! The failure of a first packet read.
        //!
        //! When every supported open window needs an absolute seek past the physical
        //! source, FFmpeg's read fails. [`InitialOperationJob`] detects that case and
        //! reports it as a [`RejectedPacketSeek`], with FFmpeg's return code.
        pub use crate::rejected_packet_seek::{
            InitialOperation, InitialOperationJob, RejectedPacketSeek, RejectedPacketSeekProfile,
        };
    }

    pub mod sample_entry {
        //! Sample entry outcomes during open.
        //!
        //! [`SampleEntryHeaderJob`] reports the entries FFmpeg's open rejects, with its
        //! return code, and the configuration box it installs. [`RawVisualEntriesJob`]
        //! applies that to a track's raw visual entries and fails on a rejected one.
        //! [`PrivateConfigurationJob`] derives the private bytes FFmpeg publishes for a
        //! raw-video entry.
        pub use crate::private_catalogue::{
            PrivateCatalogueProfile, PrivateConfigCause, PrivateConfiguration,
            PrivateConfigurationJob,
        };
        pub use crate::sample_entry_header::{
            HeaderCause, HeaderRejection, RawVisualEntriesJob, SampleEntryHeader,
            SampleEntryHeaderJob, SampleEntryHeaderProfile,
        };
    }

    pub mod fragment_clock {
        //! The fragment start clock during open.
        //!
        //! [`FragmentClockJob`] folds a fragment's durations onto FFmpeg's signed clock
        //! and reports a fragment whose clock it refuses, with its return code.
        //! [`check_signed_decode_runs`] refuses the fragments of a file whose decode run
        //! ends past that clock.
        pub use crate::fragment_open_clock::{
            FragmentClockJob, FragmentClockProfile, FragmentClockResult, check_signed_decode_runs,
        };
    }

    pub mod receiver {
        //! Packet times as FFmpeg's public API returns them.
        //!
        //! The API moves timestamps near the top of the signed range to a relative
        //! value. [`ReceiverClock`] reports that value beside the demuxer's field and
        //! [`ReceiverOrigin`] says which of the two it is.
        pub use crate::receiver_clock::{
            ReceiverClock, ReceiverClockProfile, ReceiverOrigin, ReceiverPacketTimes,
        };
    }

    pub mod diagnostics {
        //! FFmpeg's log lines for a raw-video entry.
        //!
        //! [`PrivateDiagnosticJob`] reports the messages and return code FFmpeg produces
        //! while walking the entry's children.
        pub use crate::private_diagnostics::{
            DiagnosticKind, PrivateDiagnosticJob, PrivateDiagnostics, ReceiverDiagnostic,
        };
    }

    pub mod encoded {
        //! Labels of the encoded recovery view.
        pub use crate::encoded_recovery::RecoveryReceiverCause;
    }
}

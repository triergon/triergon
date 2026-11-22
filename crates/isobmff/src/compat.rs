//! Opt-in policies that reproduce how FFmpeg orders, times and recovers MOV packets.
//! They work over the parsed tables and preserve source samples. Trace projections
//! are available with the `reference-trace` feature.

pub mod mov {
    //! FFmpeg MOV packet, seek and stream metadata policies. Each packet occurrence
    //! pairs the source sample with the demuxer's packet fields.
    //!
    //! Reference: the `mov` demuxer with `+noparse+nofillin`, FFmpeg git revision
    //! `d32b387f` on the 9.0 branch.

    pub(crate) mod clocks;
    pub(crate) mod cursor;
    pub(crate) mod edit_list;
    pub(crate) mod encoded_configs;
    pub(crate) mod encoded_contexts;
    pub(crate) mod encoded_dependency;
    pub(crate) mod encoded_edits;
    pub(crate) mod encoded_metadata;
    pub(crate) mod encoded_recovery;
    pub(crate) mod encoded_recovery_context;
    pub(crate) mod encoded_stream;
    pub(crate) mod field_recovery;
    pub(crate) mod front_movie;
    pub(crate) mod index;
    pub(crate) mod pcm_packets;
    pub(crate) mod physical_read;
    pub(crate) mod view;

    pub mod clock {
        //! Exact and stored packet times, track time bases and the clocks used
        //! by [`seekable_head_precedes`] to compare packets across tracks.
        pub use super::clocks::{
            ClockField, MediaClock, MediaClockCause, SchedulingClock, SourceTimeBase,
        };
        pub use super::cursor::{ClockOrigin, PredecessorClock, seekable_head_precedes};
    }

    pub mod packets {
        //! [`PacketCursor`] reproduces the demuxer's packet order across tracks.
        //! Use [`PacketProjector`] for a single sample and [`StreamView`] for stream metadata.
        pub use super::cursor::{
            DemuxProfile, DemuxTrack, PacketCursor, PacketLimits, PacketOccurrence, ResumePoint,
        };
        pub use super::view::{PacketProjector, PacketView, StreamView};
    }

    pub mod edits {
        //! Edit-list clocks.
        //!
        //! [`EditCursor`] repeats, trims and shifts one track's samples as the demuxer
        //! applies its edit list. Encoded tracks go through [`EncodedEditJob`], which
        //! scans the track's metadata once and returns an [`EncodedEditPlan`] for
        //! [`PacketCursor::new_encoded_regular`](super::packets::PacketCursor::new_encoded_regular)
        //! or [`PacketCursor::new_front_with_encoded_edits`](super::packets::PacketCursor::new_front_with_encoded_edits).
        pub use super::cursor::EditIdentity;
        pub use super::edit_list::{DurationBasis, EditCursor, EditLimits, EditOccurrence};
        pub use super::encoded_edits::{
            EncodedEditContext, EncodedEditJob, EncodedEditLimits, EncodedEditPlan,
            EncodedEditProfile, fragment_aac::FragmentAacTrimJob,
        };
        pub use super::view::ProjectedEdit;
    }

    pub mod seek {
        //! Demux index entries and seek plans for one track or all tracks.
        //! [`RangeSeekJob`] starts from a packet cursor's recorded checkpoints.
        pub use super::cursor::RangeSeekJob;
        pub use super::index::{
            GlobalSeekJob, GlobalSeekPlan, IndexCursor, IndexEntry, IndexLimits, SeekCursor,
            SeekJob, SeekLanding, SeekLimits, SeekPlan,
        };
    }

    pub mod front {
        //! Forward-only movies whose metadata comes first.
        //!
        //! [`PacketCursor::new_front_source`](super::packets::PacketCursor::new_front_source)
        //! and [`new_front_regular`](super::packets::PacketCursor::new_front_regular)
        //! read a nonseekable input from its start. [`FrontTrack`] keeps one track's
        //! clock across [`FrontFragment`]s, and a [`FrontSample`] names the
        //! [`FrontReadBoundary`] up to which its payload can be read.
        pub use super::front_movie::{FrontFragment, FrontReadBoundary, FrontSample, FrontTrack};
    }

    pub mod encoded {
        //! Admission of encoded sample entries.
        //!
        //! [`EncodedStreamJob`] reads one sample entry and returns an [`EncodedStream`]:
        //! the demuxer's stream view, the source description and the metadata it reads
        //! from the movie. Its constructors select what it admits:
        //! [`EncodedStreamProfile`] one description, [`EncodedConfigProfile`] one of
        //! several, [`EncodedMetadataProfile`] QuickTime sound entries and legacy
        //! metadata, [`EncodedContextProfile`] the receiver's reading of a `wave` box.
        //! [`EncodedRecoveryProfile`] adds the `roll` and `prol` groups of the table.
        //!
        //! [`EncodedInitial`] and [`EncodedConfigSequence`] follow the configuration of a
        //! track with more than one description.
        pub use super::encoded_configs::{
            EncodedConfigProfile, EncodedConfigSequence, EncodedConfigStep, EncodedCreation,
            EncodedInitial,
        };
        pub use super::encoded_contexts::{
            EncodedContextProfile, SourceProfileFault, WaveObservation,
        };
        pub use super::encoded_metadata::{
            DependedOn, DependencyView, EncodedCreationSource, EncodedMetadataProfile,
            EncodedSelection, EncodedSource, HandlerSource, LanguageCause, LanguageSource,
            SoundV1Source,
        };
        pub use super::encoded_recovery::{
            DependencyCause, DependencyObservation, EncodedRecoveryProfile, RecoveryFacts,
            RecoveryGroup,
        };
        pub use super::encoded_recovery_context::RecoveryObservation;
        pub use super::encoded_stream::{
            EncodedEntryKind, EncodedStream, EncodedStreamJob, EncodedStreamProfile,
        };
    }

    pub mod recovery {
        //! Malformed-MOV recovery.
        //!
        //! Strict parsing refuses these files; the types here read what the demuxer
        //! reads from them and keep the strict error beside the result. Their entry
        //! points take an explicit profile token.
        //!
        //! - Field recovery. [`RecoveryTrackHeader`] accepts a zero timescale and
        //!   [`PartialSampleReader`] or
        //!   [`SampleReader::poll_recovery`](crate::payload::SampleReader::poll_recovery)
        //!   return the part of a sample that exists ([`ShortSample`]).
        //! - Run recovery. [`RunRecoveryBuilder`] accepts overlong timing and sync
        //!   tables. [`RunRecoveryTable`] keeps the raw tables with their
        //!   [`RunCoverage`] and reads the usable prefix through
        //!   [`RecoveryPacketCursor`] and [`RecoveryIndexCursor`].
        //! - Direct recovery. [`RunRecoveryBuilder::new_direct`] also accepts a zero
        //!   first description, and [`DirectPacketCursor`] reports packets declared
        //!   past the physical end of the file.
        //! - Description recovery. [`DescriptionRecoveryBuilder`] accepts invalid
        //!   description rows. [`DescriptionPacketCursor`] reports a
        //!   [`DescriptionEvent`] for each declaration, including those the demuxer
        //!   drops from its catalogue, and [`PhysicalReader`] reads an event's bytes at
        //!   the physical end of the file.
        pub use super::field_recovery::{
            PartialSampleReader, PayloadBounds, RecoveredPayload, RecoverySampleExtent,
            RecoverySampleRead, RecoveryTrackHeader, ShortSample,
        };
        pub use super::physical_read::{
            PhysicalPayload, PhysicalRead, PhysicalReadProfile, PhysicalReader,
        };
        pub use crate::table::recovery::{
            DescriptionAssociation, DescriptionContext, DescriptionEmission, DescriptionEvent,
            DescriptionIndexCursor, DescriptionIndexEvent, DescriptionPacketCursor,
            DescriptionRead, DescriptionRecoveryBuilder, DescriptionRecoveryProfile,
            DescriptionRecoveryTable, DescriptionRowDiagnostic, DirectAssociation, DirectContext,
            DirectIndexCursor, DirectIndexEntry, DirectOccurrence, DirectPacketCursor, DirectRead,
            DirectRecoveryProfile, InitialDescriptionDiagnostic, NativeIndexEntry,
            RecoveryIndexCursor, RecoveryIndexEntry, RecoveryOccurrence, RecoveryPacketCursor,
            RecoverySourceCursor, RunCoverage, RunKind, RunRecoveryBuilder, RunRecoveryProfile,
            RunRecoveryTable, SyncDiagnostic,
        };
    }

    pub mod pcm {
        //! [`PcmCursor`] groups QuickTime version 0 PCM samples into demux packets.
        //! [`PcmReader`] reads the grouped payloads.
        pub use super::pcm_packets::{
            PcmCursor, PcmLimits, PcmPacket, PcmPayload, PcmProfile, PcmReader, PcmStream,
        };
    }
}

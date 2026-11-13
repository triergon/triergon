//! Reference-compatible policies.
//!
//! The types at the crate root follow the specifications and report every source
//! fact. Where FFmpeg's MOV demuxer orders, times or recovers packets differently and
//! a consumer needs FFmpeg's result, the policies here reproduce that demuxer for the
//! input. They read the same parsed tables and never rewrite source facts. Each
//! module is one policy family; its documentation names the FFmpeg build it
//! reproduces.
//!
//! Projections that exist only to compare traces with FFmpeg are not policies. They
//! are compiled only with the `reference-trace` feature.

pub mod mov {
    //! FFmpeg's MOV demuxer.
    //!
    //! Each occurrence pairs the untouched source sample with the packet the demuxer
    //! emits for it. [`packets`] walks tracks in the demuxer's order, [`clock`] holds
    //! its signed clocks, [`edits`] applies edit lists, [`seek`] builds its index and
    //! seeks, [`front`] reads a forward-only input, [`encoded`] admits encoded sample
    //! entries, [`recovery`] keeps malformed files readable and [`pcm`] groups
    //! QuickTime PCM samples into packets.
    //!
    //! Reference: the `mov` demuxer with `+noparse+nofillin`, FFmpeg git revision
    //! `d32b387f` on the 9.0 branch.

    pub mod clock {
        //! The demuxer's signed clocks.
        //!
        //! [`ClockField`] is a packet time as the demuxer stores it, including its
        //! null sentinel, beside the exact source tick. [`MediaClock`] is a track's
        //! stream clock, with [`MediaClockCause`] saying why it differs from the
        //! declared timescale. [`ClockOrigin`] says where a packet's decode time came
        //! from and [`SchedulingClock`] orders heads across tracks;
        //! [`seekable_head_precedes`] compares two heads as the demuxer does.
        pub use crate::mov_clock::{
            ClockField, MediaClock, MediaClockCause, SchedulingClock, SourceTimeBase,
        };
        pub use crate::mov_cursor::{ClockOrigin, PredecessorClock, seekable_head_precedes};
    }

    pub mod packets {
        //! The demuxer's packet sequence.
        //!
        //! [`PacketCursor`] walks the selected [`DemuxTrack`]s in the order the demuxer
        //! emits them and yields a [`PacketOccurrence`] for each packet: the source
        //! sample, the [`PacketView`] the demuxer reports and their clocks. A
        //! [`StreamView`] is a track's catalogue entry and [`PacketProjector`] projects
        //! one source sample on its own.
        pub use crate::mov_cursor::{
            DemuxProfile, DemuxTrack, PacketCursor, PacketLimits, PacketOccurrence, ResumePoint,
        };
        pub use crate::mov_edits::CursorStatus;
        pub use crate::mov_view::{PacketProjector, PacketView, StreamView};
    }

    pub mod edits {
        //! Edit-list clocks.
        //!
        //! [`EditCursor`] repeats, trims and shifts one track's samples as the demuxer
        //! applies its edit list. Encoded tracks go through [`EncodedEditJob`], which
        //! scans the track's metadata once and returns an [`EncodedEditPlan`] for
        //! [`PacketCursor::new_encoded_regular`](super::packets::PacketCursor::new_encoded_regular)
        //! or [`PacketCursor::new_front_with_encoded_edits`](super::packets::PacketCursor::new_front_with_encoded_edits).
        pub use crate::encoded_edits::{
            EncodedEditContext, EncodedEditJob, EncodedEditLimits, EncodedEditPlan,
            EncodedEditProfile, fragment_aac::FragmentAacTrimJob,
        };
        pub use crate::mov_cursor::EditIdentity;
        pub use crate::mov_edits::{DurationBasis, EditCursor, EditLimits, EditOccurrence};
        pub use crate::mov_view::ProjectedEdit;
    }

    pub mod seek {
        //! The demuxer's index and seek.
        //!
        //! [`IndexCursor`] builds one track's index. [`SeekJob`] seeks one track and
        //! [`GlobalSeekJob`] all of them; both return a plan whose [`SeekLanding`] says
        //! which entry was selected. [`RangeSeekJob`] does the same from a packet
        //! cursor that recorded checkpoints.
        pub use crate::mov_cursor::RangeSeekJob;
        pub use crate::mov_index::{
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
        pub use crate::mov_front::{FrontFragment, FrontReadBoundary, FrontSample, FrontTrack};
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
        pub use crate::encoded_configs::{
            EncodedConfigProfile, EncodedConfigSequence, EncodedConfigStep, EncodedCreation,
            EncodedInitial,
        };
        pub use crate::encoded_contexts::{
            EncodedContextProfile, SourceProfileFault, WaveObservation,
        };
        pub use crate::encoded_metadata::{
            CreationCause, DependedOn, DependencyView, EncodedCreationSource,
            EncodedMetadataProfile, EncodedSelection, EncodedSource, HandlerSource, LanguageCause,
            LanguageSource, SoundV1Source,
        };
        pub use crate::encoded_recovery::{
            DependencyCause, DependencyObservation, EncodedRecoveryProfile, RecoveryFacts,
            RecoveryGroup,
        };
        pub use crate::encoded_recovery_context::RecoveryObservation;
        pub use crate::encoded_stream::{
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
        //!   [`SampleReader::poll_recovery`](crate::SampleReader::poll_recovery)
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
        pub use crate::metadata::RecoveryTrackHeader;
        pub use crate::packet::{RecoverySampleExtent, RecoverySampleRead, ShortSample};
        pub use crate::physical_read::{
            PhysicalPayload, PhysicalRead, PhysicalReadProfile, PhysicalReader,
        };
        pub use crate::recovery_payload::{PartialSampleReader, PayloadBounds, RecoveredPayload};
        pub use crate::regular::{
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
        //! QuickTime PCM packets.
        //!
        //! [`PcmStream`] admits one track of [`QtSoundDescriptionV0`](crate::QtSoundDescriptionV0)
        //! samples. [`PcmCursor`] groups them into the [`PcmPacket`]s the demuxer emits,
        //! which do not match the source samples, and [`PcmReader`] reads one packet's
        //! bytes as a [`PcmPayload`].
        pub use crate::mov_pcm::{
            PcmCursor, PcmLimits, PcmPacket, PcmPayload, PcmProfile, PcmReader, PcmStream,
        };
    }
}

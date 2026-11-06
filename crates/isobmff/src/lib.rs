//! Independent bounded ISO-derived MOV/MP4 syntax and source sample indexes.
//!
//! Experimental seam, not a complete demuxer.
//! Receives charged metadata bytes and absolute coordinates from the shared core;
//! owns no I/O, transport tokens, allocator, codec adaptation or presentation clock.
#![forbid(unsafe_code)]

mod boxes;
mod dts_description;
pub(crate) use dts_description::DtsCoreDescription;
mod fields;
mod fragment;
mod fragment_time;
mod input_boxes;
mod interleave;
mod metadata;
mod native_clock;
mod native_front;
pub use native_front::{
    Mov9FrontFragmentV1, Mov9FrontReadBoundaryV1, Mov9FrontSampleV1, Mov9FrontTrackV1,
};
mod native_cursor;
mod native_edits;
mod native_operation_v5;
mod native_transport_v3;
mod native_view;
pub use native_operation_v5::{
    Mov9InitialOperationJobV5, Mov9InitialOperationV5, PinnedMovD32RejectedPacketSeekV5,
};
pub use native_transport_v3::{
    Mov9V3EntryRevisit, Mov9V3TransportBuilder, Mov9V3TransportModel,
    PinnedMovD32SmallRawVideoTransportV3,
};
pub(crate) use native_transport_v3::{Mov9V3ModeledInitialRead, Mov9V3ModeledOpenState};
mod packet;
mod physical_read_v4;
mod recovery_payload;
pub use physical_read_v4::{Mov9PhysicalReadV4, Mov9PhysicalReaderV4, PinnedMovD32PhysicalReadV4};
mod regular;
pub use recovery_payload::{Mov9PartialSampleReader, Mov9PayloadBounds};
mod sidx;
mod sound_v0;
mod tables;
pub use sound_v0::{
    QtPcmEncodingV0, QtPcmSampleReaderV0, QtPcmSourcePacketV0, QtSoundDescriptionV0,
};

pub use boxes::{BoxChildren, BoxHeader, BoxStep, BoxView, HeaderRead};
pub use fragment::{FragmentBuilder, FragmentContext, FragmentIndex, ImplicitBase, Trex};
pub(crate) use fragment::{FragmentCursor, FragmentLocator};
pub use fragment_time::FragmentDecodeTime;
pub use input_boxes::{BoxEvent, BoxReader};
pub use interleave::{
    ForwardRegularMdatV1, InterleaveStatus, PhysicalInterleaveLimits, PhysicalSampleCursor,
    SourceTrack, TrackSampleCursor,
};
pub use metadata::{
    EditEntry, EditList, EntryLayout, Mov9RecoveryTrackHeader, MovieHeader, SampleDescription,
    TrackHeader,
};
pub use native_clock::{
    Mov9ClockField, Mov9MediaClock, Mov9MediaClockCause, Mov9SchedulingClock, SourceTimeBase,
};
pub(crate) use native_cursor::Mov9PredecessorClock;
pub use native_cursor::{
    Mov9ClockOrigin, Mov9PacketCursor, Mov9PacketLimits, Mov9PacketOccurrence, Mov9Profile,
    Mov9RangeSeekJob, Mov9ResumePoint, Mov9Track, mov9_seekable_head_precedes,
};
pub use native_edits::{Mov9CursorStatus, Mov9DurationBasis, Mov9EditCursor, Mov9EditLimits};
pub use native_view::{Mov9Edit, Mov9PacketView, Mov9StreamView, Mov9View};
pub use packet::{
    Mov9SampleExtent, Mov9SampleRead, Mov9ShortSample, SampleReader, SourcePacket, TerminalMdat,
};
pub(crate) use regular::Mov9RecoveryV3PacketCursor;
pub(crate) use regular::SampleLocator;
pub use regular::{
    Mov9RecoveryV2Association, Mov9RecoveryV2Context, Mov9RecoveryV2Read, Mov9RunKind,
    Mov9RunRecoveryBuilder, Mov9RunRecoveryTable, PinnedMovD32RegularRecoveryV1,
    PinnedMovD32RegularRecoveryV2, SampleCursor, SampleTable, SampleTableBuilder,
    SampleTablePolicy, SyncSamplePolicy, TimingRunPolicy, TimingRunSurplus,
};
pub use regular::{
    Mov9RecoveryV3Association, Mov9RecoveryV3Context, Mov9RecoveryV3Emission, Mov9RecoveryV3Event,
    Mov9RecoveryV3Read, Mov9RunRecoveryV3Builder, Mov9RunRecoveryV3Table,
    PinnedMovD32RegularRecoveryV3,
};
pub use sidx::{IndexStep, SegmentIndex};
use std::ops::Range;
use triergon_core::{Error, Result, Time, TimeBase};

/// Format-policy counts; live capacity is exclusively owned/accounted by core.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_boxes: u32,
    pub max_records: u32,
    pub max_descriptions: u32,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_boxes: 1_000_000,
            max_records: 1_000_000,
            max_descriptions: 256,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuildStatus {
    Yield,
    Ready,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    ContainerSync,
    NonSync,
    Unknown,
}

/// Versioned wire value: sign interpretation never erases the original bits.
/// Decoding these fields does not resolve the remaining v1 box/profile gaps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompositionOffset {
    version: u8,
    raw_bits: u32,
}
impl CompositionOffset {
    pub fn from_wire(version: u8, raw_bits: u32) -> Result<Self> {
        if version > 1 {
            return Err(Error::Unsupported("composition offset version"));
        }
        Ok(Self { version, raw_bits })
    }
    pub fn version(self) -> u8 {
        self.version
    }
    pub fn raw_bits(self) -> u32 {
        self.raw_bits
    }
    pub fn signed_value(self) -> Option<i32> {
        (self.version == 1).then(|| i32::from_be_bytes(self.raw_bits.to_be_bytes()))
    }
    pub fn ticks(self) -> i128 {
        match self.signed_value() {
            Some(value) => i128::from(value),
            None => i128::from(self.raw_bits),
        }
    }
}

/// Source sample facts. Decode deltas are not a promise of display duration.
/// Configuration index is not an I/O transaction or a decoder/config epoch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sample {
    pub ordinal: u64,
    pub description_index: u32,
    pub range: Range<u64>,
    pub dts_ticks: Option<i128>,
    pub cts_ticks: Option<i128>,
    pub duration_ticks: u32,
    /// None means absent; Some preserves even an explicitly encoded zero.
    pub composition_offset: Option<CompositionOffset>,
    pub access: Access,
    pub partial_sync: bool,
    pub dependency: Option<u8>,
    pub fragment_flags: Option<u32>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceTimes {
    pub dts: Option<Time>,
    pub cts: Option<Time>,
    pub decode_delta: Time,
}
impl Sample {
    pub fn is_container_sync(&self) -> bool {
        self.access == Access::ContainerSync
    }
    /// Uses the parent's exact seconds representation. Unknown fragment origin
    /// remains unknown; rounding/native compatibility are separate operations.
    pub fn source_times(&self, base: TimeBase) -> Result<SourceTimes> {
        Ok(SourceTimes {
            dts: self
                .dts_ticks
                .map(|t| Time::from_ticks(t, base))
                .transpose()?,
            cts: self
                .cts_ticks
                .map(|t| Time::from_ticks(t, base))
                .transpose()?,
            decode_delta: Time::from_ticks(i128::from(self.duration_ticks), base)?,
        })
    }
    /// The caller supplies the actual self-contained mdat extent, not just a file length.
    pub fn require_within(&self, media: Range<u64>) -> Result<()> {
        if self.range.start > self.range.end
            || media.start > media.end
            || self.range.start < media.start
            || self.range.end > media.end
        {
            return Err(Error::Invalid("sample outside media extent"));
        }
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum SampleStep {
    Sample(Sample),
    Yield,
    End,
}

pub(crate) fn policy(count: u32, maximum: u32) -> Result<()> {
    if count > maximum {
        return Err(Error::Limit {
            requested: usize::try_from(count).map_err(|_| Error::Overflow)?,
            maximum: usize::try_from(maximum).map_err(|_| Error::Overflow)?,
        });
    }
    Ok(())
}

pub(crate) fn add_offset(base: u64, delta: i32) -> Result<u64> {
    let end = i128::from(base)
        .checked_add(i128::from(delta))
        .ok_or(Error::Overflow)?;
    u64::try_from(end).map_err(|_| Error::Overflow)
}

// Source-only seek/index API. Native views remain independently named above.
mod source_seek;
mod tfra;
pub use source_seek::{SourceSeekLimits, SourceSeekPlan, SourceSeekPlanner, SourceSeekRequest};
pub use tfra::{
    FragmentRandomAccessStep, MovieFragmentRandomAccessTail, TrackFragmentRandomAccess,
};

// Source-only movie targeting/recovery; pinned native views above stay separate.
mod movie_seek;
mod recovery;
pub use movie_seek::{
    CompositionTarget, MovieEditLocator, MovieEmptyEdit, MoviePoint, MovieSeekClocks,
    MovieSeekLimits, MovieSeekResult, MovieSeekStatus, MovieSourcePoint, MovieSourceSeekContext,
    MovieSourceSeekPlanner, SourceCompositionLookup,
};
pub use recovery::{
    RecoveryDefinitionOrigin, RecoveryDescription, RecoveryDescriptions, RecoveryKind,
    RecoveryLimits, RecoveryLookup, RecoveryMappingOrigin, RecoveryMappings, RecoveryScope,
    RecoverySelection, RecoverySyntax,
};

// Pinned native index/seek view; source clocks/indices remain unchanged.
mod native_index;
pub use native_index::{
    Mov9NativeGlobalSeekJob, Mov9NativeGlobalSeekPlan, Mov9NativeIndexCursor, Mov9NativeIndexEntry,
    Mov9NativeIndexLimits, Mov9NativeSeekJob, Mov9NativeSeekLimits, Mov9NativeSeekPlan,
    Mov9NativeSeekReceipt,
};

mod native_transport_v4;
pub use native_transport_v4::{
    Mov9V4RootWalkTransportBuilder, Mov9V4RootWalkTransportModel, PinnedMovD32RootWalkTransportV4,
};

mod native_open_clock_v6;
pub use native_open_clock_v6::{
    Mov9FragmentClockJobV6, Mov9FragmentClockResultV6, PinnedMovD32FragmentClockV6,
};

mod native_pcm_v1;
pub use native_pcm_v1::{
    Mov9PcmCursorV1, Mov9PcmLimitsV1, Mov9PcmPacketV1, Mov9PcmPayloadV1, Mov9PcmReaderV1,
    Mov9PcmStreamV1, PinnedMovD32PcmV1,
};

mod native_public_receiver_v6;
pub use native_public_receiver_v6::{
    Mov9PublicClockV6, Mov9PublicPacketTimesV6, Mov9ReceiverOriginV6, PinnedMovD32PublicReceiverV6,
};

mod native_header_v7;
pub(crate) use native_header_v7::Mov9SampleEntryHeaderV7;
pub use native_header_v7::{Mov9SampleEntryHeaderJobV7, PinnedMovD32SampleEntryHeaderV7};

mod native_encoded_v1;
pub use native_encoded_v1::{
    AacConfiguration, AacConfigurationJob, AacConfigurationStep, ConfigurationLimits,
    EncodedEntryKindV1, Mov9EncodedLimitsV1, Mov9EncodedStreamJobV1, Mov9EncodedStreamV1,
    PinnedMovD32EncodedRegularV1, check_dts_core_description,
};

mod native_encoded_edits_v2;
pub use native_encoded_edits_v2::{
    Mov9EncodedEditContextV2, Mov9EncodedEditJobV2, Mov9EncodedEditLimitsV2, Mov9EncodedEditPlanV2,
    PinnedMovD32EncodedEditsV2,
};

mod native_encoded_configs_v3;
pub use native_encoded_configs_v3::{
    Mov9EncodedConfigSequenceV3, Mov9EncodedConfigStepV3, Mov9EncodedCreationV3,
    Mov9EncodedInitialV3, PinnedMovD32EncodedRegularV3,
};

mod native_encoded_metadata_v4;
pub use native_encoded_metadata_v4::{
    Mov9DependedOnV4, Mov9EncodedCreationV4, Mov9EncodedSelectionV4, PinnedMovD32EncodedRegularV4,
};
pub(crate) use native_encoded_metadata_v4::{
    Mov9DependencyViewV4, Mov9EncodedSourceV4, Mov9SoundV1SourceV4,
};

mod native_encoded_dependency_v4;

mod native_private_catalogue_v8;
pub use native_private_catalogue_v8::{
    Mov9PrivateConfigCauseV8, Mov9PrivateConfigurationJobV8, Mov9PrivateConfigurationV8,
    PinnedMovD32OpaquePrivateCatalogueV8,
};

mod native_encoded_recovery_v5;
pub(crate) use native_encoded_recovery_v5::Mov9RecoveryFactsV5;
pub use native_encoded_recovery_v5::{
    Mov9DependencyCauseV5, Mov9DependencyObservationV5, Mov9RecoveryReceiverCauseV5,
    PinnedMovD32EncodedRecoveryV5,
};

mod native_context_reports_v7;
pub use native_context_reports_v7::PinnedMovD32EncodedContextsV7;
pub(crate) use native_context_reports_v7::{Mov9SourceProfileFaultV7, Mov9WaveObservationV7};

mod native_recovery_context_v7;
pub(crate) use native_recovery_context_v7::Mov9RecoveryObservationV7;

mod native_private_diagnostics_v7;
pub use native_private_diagnostics_v7::Mov9PrivateDiagnosticJobV7;

pub use fragment::OwnedFragmentCursor;

#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![warn(
    missing_debug_implementations,
    unreachable_pub,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::unwrap_used
)]
// Pending: `missing_docs`, `clippy::expect_used`.
// Pending: these rewrites change the generated code of measured hot paths and are
// adopted with an A/B timing run.
#![allow(
    clippy::assigning_clones,
    clippy::checked_conversions,
    clippy::map_unwrap_or,
    clippy::redundant_closure_for_method_calls,
    clippy::ref_option,
    clippy::unnested_or_patterns
)]
mod boxes;
pub mod compat;
mod dts_description;
pub(crate) use dts_description::DtsCoreDescription;
mod fields;
mod fragment;
mod fragment_time;
mod input_boxes;
mod interleave;
mod metadata;
mod mov_clock;
mod mov_front;
pub(crate) use mov_front::FrontTrack;
mod mov_cursor;
mod mov_edits;
mod mov_view;
#[cfg(feature = "reference-trace")]
mod raw_video_transport;
#[cfg(feature = "reference-trace")]
pub(crate) use raw_video_transport::{
    EntryRevisit, ModeledInitialRead, ModeledOpenState, RawVideoTransportModel,
};
mod packet;
mod physical_read;
mod recovery_payload;
#[cfg(feature = "reference-trace")]
mod rejected_packet_seek;
pub(crate) use physical_read::PhysicalReadProfile;
mod qt_sound;
#[cfg(feature = "reference-trace")]
pub mod reference_trace;
mod regular;
mod sidx;
mod tables;
pub use qt_sound::{
    QtPcmEncodingV0, QtPcmSampleReaderV0, QtPcmSourcePacketV0, QtSoundDescriptionV0,
};

pub use boxes::{BoxChildren, BoxHeader, BoxStep, BoxView, HeaderRead};
pub use fragment::{FragmentBuilder, FragmentContext, FragmentIndex, ImplicitBase, Trex};
pub(crate) use fragment::{FragmentCursor, FragmentLocator};
pub use fragment_time::FragmentDecodeTime;
pub use input_boxes::{BoxEvent, BoxReader, MediaDataPolicy};
pub use interleave::{
    ForwardRegularMdat, InterleaveStatus, PhysicalInterleaveLimits, PhysicalSampleCursor,
    SourceTrack, TrackSampleCursor,
};
pub(crate) use metadata::RecoveryTrackHeader;
pub use metadata::{EditEntry, EditList, EntryLayout, MovieHeader, SampleDescription, TrackHeader};
pub(crate) use mov_clock::{ClockField, MediaClock, SchedulingClock};
pub(crate) use mov_cursor::PredecessorClock;
pub(crate) use mov_cursor::{
    ClockOrigin, DemuxProfile, DemuxTrack, PacketCursor, PacketLimits, PacketOccurrence,
    ResumePoint,
};
pub(crate) use mov_edits::{CursorStatus, DurationBasis, EditCursor, EditLimits};
pub(crate) use mov_view::{PacketProjector, PacketView, ProjectedEdit, StreamView};
pub use packet::{SampleReader, SourcePacket, TerminalMdat};
#[cfg(feature = "reference-trace")]
pub(crate) use regular::{
    DescriptionContext, DescriptionEmission, DescriptionPacketCursor, DescriptionRecoveryTable,
};
pub(crate) use regular::{DescriptionEvent, DescriptionRead};
pub(crate) use regular::{RunRecoveryProfile, SampleLocator};
pub use regular::{
    SampleCursor, SampleTable, SampleTableBuilder, SampleTablePolicy, SyncSamplePolicy,
    TimingRunPolicy, TimingRunSurplus,
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
    /// remains unknown; rounding/demux compatibility are separate operations.
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

// Source-only seek/index API. Demux views remain independently named above.
mod source_seek;
mod tfra;
pub use source_seek::{SourceSeekLimits, SourceSeekPlan, SourceSeekPlanner, SourceSeekRequest};
pub use tfra::{
    FragmentRandomAccessStep, MovieFragmentRandomAccessTail, TrackFragmentRandomAccess,
};

// Source-only movie targeting/recovery; reference demux views above stay separate.
mod movie_seek;
mod opus_seek;
mod recovery;
pub use movie_seek::{
    CompositionTarget, MovieEditLocator, MovieEmptyEdit, MoviePoint, MovieSeekClocks,
    MovieSeekLimits, MovieSeekResult, MovieSeekStatus, MovieSourcePoint, MovieSourceSeekContext,
    MovieSourceSeekPlanner, SourceCompositionLookup,
};
pub use opus_seek::OpusRecoveryLanding;
pub use recovery::{
    RecoveryDefinitionOrigin, RecoveryDescription, RecoveryDescriptions, RecoveryKind,
    RecoveryLimits, RecoveryLookup, RecoveryMappingOrigin, RecoveryMappings, RecoveryScope,
    RecoverySelection, RecoverySyntax,
};

// Reference demux index/seek view; source clocks/indices remain unchanged.
mod mov_index;
pub(crate) use mov_index::{IndexCursor, IndexEntry, IndexLimits, SeekLanding, SeekLimits};

#[cfg(feature = "reference-trace")]
mod root_walk_transport;
#[cfg(feature = "reference-trace")]
pub(crate) use root_walk_transport::{RootWalkTransportModel, RootWalkTransportProfile};

#[cfg(feature = "reference-trace")]
mod fragment_open_clock;

mod mov_pcm;

#[cfg(feature = "reference-trace")]
mod receiver_clock;

#[cfg(feature = "reference-trace")]
mod sample_entry_header;
#[cfg(feature = "reference-trace")]
pub(crate) use sample_entry_header::{
    SampleEntryHeader, SampleEntryHeaderJob, SampleEntryHeaderProfile,
};

mod encoded_stream;
pub use encoded_stream::{
    AacConfiguration, AacConfigurationJob, AacConfigurationStep, ConfigurationLimits,
    ObjectTypeScan, check_dts_core_description,
};
pub(crate) use encoded_stream::{EncodedEntryKind, EncodedStream};

mod encoded_edits;
pub(crate) use encoded_edits::{EncodedEditPlan, EncodedEditProfile};

mod encoded_configs;
pub(crate) use encoded_configs::EncodedConfigProfile;

mod encoded_metadata;
pub(crate) use encoded_metadata::{DependencyView, EncodedSource, SoundV1Source};
pub(crate) use encoded_metadata::{EncodedMetadataProfile, EncodedSelection};

mod encoded_dependency;

#[cfg(feature = "reference-trace")]
mod private_catalogue;

mod encoded_recovery;
pub(crate) use encoded_recovery::RecoveryFacts;
pub(crate) use encoded_recovery::{DependencyObservation, EncodedRecoveryProfile};

mod encoded_contexts;
pub(crate) use encoded_contexts::EncodedContextProfile;
pub(crate) use encoded_contexts::{SourceProfileFault, WaveObservation};

mod encoded_recovery_context;
pub(crate) use encoded_recovery_context::RecoveryObservation;

#[cfg(feature = "reference-trace")]
mod private_diagnostics;

pub use fragment::OwnedFragmentCursor;

pub mod open;

mod container;
pub use container::{Packet, Reader, ReaderEvent, ReaderLimits, Track};

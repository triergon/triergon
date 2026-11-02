//! Independent bounded ISO-derived MOV/MP4 syntax and source sample indexes.
//!
//! Experimental seam, not a complete demuxer or a qualified M03 implementation.
//! Receives charged metadata bytes and absolute coordinates from the shared core;
//! owns no I/O, transport tokens, allocator, codec adaptation or presentation clock.
#![forbid(unsafe_code)]

mod boxes;
mod fields;
mod fragment;
mod fragment_time;
mod input_boxes;
mod interleave;
mod metadata;
mod native_clock;
mod native_cursor;
mod native_edits;
mod native_view;
mod packet;
mod regular;
mod sidx;
mod tables;

pub use boxes::{BoxChildren, BoxHeader, BoxStep, BoxView, HeaderRead};
pub use fragment::{
    FragmentBuilder, FragmentContext, FragmentCursor, FragmentIndex, FragmentLocator, ImplicitBase,
    Trex,
};
pub use fragment_time::FragmentDecodeTime;
pub use input_boxes::{BoxEvent, BoxReader};
pub use interleave::{
    InterleaveStatus, InterleavedSample, PhysicalInterleaveLimits, PhysicalSampleCursor,
    SourceTrack, TrackSampleCursor,
};
pub use metadata::{EditEntry, EditList, EntryLayout, MovieHeader, SampleDescription, TrackHeader};
pub use native_clock::{Mov9ClockField, Mov9MediaClock, Mov9MediaClockCause, Mov9SchedulingClock};
pub use native_cursor::{
    Mov9ClockOrigin, Mov9EditIdentity, Mov9PacketCursor, Mov9PacketLimits, Mov9PacketOccurrence,
    Mov9PredecessorClock, Mov9Profile, Mov9ResumePoint, Mov9Track,
};
pub use native_edits::{
    Mov9CursorStatus, Mov9DurationBasis, Mov9EditCursor, Mov9EditLimits, Mov9Occurrence,
};
pub use native_view::{Mov9Edit, Mov9PacketView, Mov9StreamView, Mov9View};
pub use packet::{SampleReader, SourcePacket};
pub use regular::{SampleCursor, SampleLocator, SampleTable, SampleTableBuilder};
pub use sidx::{IndexStep, SegmentIndex, SegmentIndexCursor, SegmentReference};
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
pub use source_seek::{
    SourceSeekLimits, SourceSeekPlan, SourceSeekPlanner, SourceSeekReceipt, SourceSeekRequest,
    SourceSeekSamples,
};
pub use tfra::{
    FragmentRandomAccessCursor, FragmentRandomAccessEntry, FragmentRandomAccessStep,
    MovieFragmentRandomAccessTail, TrackFragmentRandomAccess,
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
    RecoveryDefinitionOrigin, RecoveryDescription, RecoveryDescriptionLookup, RecoveryDescriptions,
    RecoveryKind, RecoveryLimits, RecoveryLookup, RecoveryMappingOrigin, RecoveryMappings,
    RecoveryScope, RecoverySelection, RecoverySyntax,
};

// Pinned native index/seek view; source clocks/indices remain unchanged.
mod native_index;
pub use native_index::{
    Mov9NativeGlobalSeekJob, Mov9NativeGlobalSeekPlan, Mov9NativeIndexCursor, Mov9NativeIndexEntry,
    Mov9NativeIndexLimits, Mov9NativeSeekCursor, Mov9NativeSeekJob, Mov9NativeSeekLimits,
    Mov9NativeSeekPlan, Mov9NativeSeekReceipt,
};

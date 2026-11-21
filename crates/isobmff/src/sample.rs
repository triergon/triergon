//! Source sample facts shared by the table, fragment and payload readers.
use std::ops::Range;
use triergon_core::{Error, Result, Time, TimeBase};

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
        require_range_within(&self.range, media)
    }
}

/// [`Sample::require_within`] for a sample's byte range.
pub(crate) fn require_range_within(range: &Range<u64>, media: Range<u64>) -> Result<()> {
    if range.start > range.end
        || media.start > media.end
        || range.start < media.start
        || range.end > media.end
    {
        return Err(Error::Invalid("sample outside media extent"));
    }
    Ok(())
}

/// Consecutive samples of one track whose bytes are contiguous: the first
/// one's ordinal, how many there are, the bytes they cover together and, for
/// fragment samples that state composition offsets, the least of those read as
/// signed 32-bit values.
#[derive(Clone, Debug)]
pub(crate) struct SampleSpan {
    pub(crate) first: u64,
    pub(crate) count: u64,
    pub(crate) range: Range<u64>,
    pub(crate) fragment: bool,
    pub(crate) offset_minimum: Option<i32>,
}

/// One step of a walk over sample spans.
#[derive(Debug)]
pub(crate) enum SpanStep {
    Span(SampleSpan),
    Yield,
    End,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SampleStep {
    Sample(Sample),
    Yield,
    End,
}

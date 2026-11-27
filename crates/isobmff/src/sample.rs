use std::ops::Range;
use triergon_core::{Error, Result, Time, TimeBase};

/// Whether the container marks a sample as a sync sample (a random-access point).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// Marked: listed in `stss`, any sample of a table without `stss`, or a fragment
    /// sample whose `sample_is_non_sync_sample` flag is clear. A container fact, not proof
    /// that a decoder can start there.
    ContainerSync,
    /// Not marked.
    NonSync,
    /// Not known, because a recovery cursor set aside the sync table that would say.
    Unknown,
}

/// A composition offset as `ctts` or `trun` stores it: the box version and the 32-bit
/// field. Version 0 reads the field as unsigned and version 1 as signed; the stored bits
/// stay available either way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompositionOffset {
    version: u8,
    raw_bits: u32,
}
impl CompositionOffset {
    /// The offset from a version 0 or 1 box; `Error::Unsupported` for a later version.
    pub fn from_wire(version: u8, raw_bits: u32) -> Result<Self> {
        if version > 1 {
            return Err(Error::Unsupported("composition offset version"));
        }
        Ok(Self { version, raw_bits })
    }
    /// Version of the box the offset came from: 0 (unsigned) or 1 (signed).
    pub fn version(self) -> u8 {
        self.version
    }
    /// The 32-bit field exactly as stored.
    pub fn raw_bits(self) -> u32 {
        self.raw_bits
    }
    /// The field read as signed, for a version 1 box; `None` for version 0.
    pub fn signed_value(self) -> Option<i32> {
        (self.version == 1).then(|| i32::from_be_bytes(self.raw_bits.to_be_bytes()))
    }
    /// The offset in ticks of the media timescale, read as the box version says.
    pub fn ticks(self) -> i128 {
        match self.signed_value() {
            Some(value) => i128::from(value),
            None => i128::from(self.raw_bits),
        }
    }
}

/// One sample as the container declares it: where its bytes are, its clocks in ticks of
/// the media timescale and its access flags. The duration is the decode delta, not a
/// display duration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sample {
    /// 1-based position in the track; fragment samples continue after the regular table.
    pub ordinal: u64,
    /// 1-based index of the sample's entry in `stsd`, from `stsc` or the fragment defaults.
    pub description_index: u32,
    /// Absolute file byte range of the payload.
    pub range: Range<u64>,
    /// Decode time in ticks of the media timescale; `None` for a fragment sample with
    /// neither `tfdt` nor a caller-supplied decode origin.
    pub dts_ticks: Option<i128>,
    /// Composition time in media ticks: the decode time plus the composition offset, if
    /// any; `None` when the decode time is unknown.
    pub cts_ticks: Option<i128>,
    /// Decode delta in media ticks, from `stts` or the fragment sample duration.
    pub duration_ticks: u32,
    /// The stated composition offset; `None` without `ctts` or the `trun` offset field, so
    /// an explicit zero stays distinct from no offset.
    pub composition_offset: Option<CompositionOffset>,
    /// Whether the container marks the sample as a sync sample.
    pub access: Access,
    /// Listed in the QuickTime partial sync table `stps`; `false` for fragment samples.
    pub partial_sync: bool,
    /// The sample's `sdtp` byte of dependency flags; `None` without `sdtp` and for
    /// fragment samples.
    pub dependency: Option<u8>,
    /// The fragment `sample_flags` word that applies, from `trun`, `tfhd` or `trex`;
    /// `None` for samples of the regular table.
    pub fragment_flags: Option<u32>,
}
/// A sample's clocks as exact core [`Time`] values; see [`Sample::source_times`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceTimes {
    /// Decode time; `None` when unknown.
    pub dts: Option<Time>,
    /// Composition time; `None` when unknown.
    pub cts: Option<Time>,
    /// The decode delta, which is not a display duration.
    pub decode_delta: Time,
}
impl Sample {
    /// Whether [`access`](Self::access) is [`Access::ContainerSync`].
    pub fn is_container_sync(&self) -> bool {
        self.access == Access::ContainerSync
    }
    /// Converts ticks using the track's media time base. Unknown times stay `None`;
    /// `Error::Overflow` if a clock cannot be represented exactly.
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
    /// Checks that the byte range lies inside `media`, the payload extent of the `mdat`
    /// holding it; `Error::Invalid` otherwise or when either range is reversed.
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

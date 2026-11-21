//! Field recovery, as FFmpeg's MOV demuxer reads malformed fields: a track header whose
//! `mdhd` timescale is zero, and the part of a sample that exists before the physical end
//! of the file. The declared values stay beside what is read.
use crate::{
    Sample,
    boxes::BoxView,
    movie::TrackHeader,
    payload::{SampleReader, SourcePacket, TerminalMdat},
    table::recovery::RunRecoveryProfile,
};
use std::{ops::Range, sync::atomic::AtomicBool};
use triergon_core::{
    Bytes, Error, InputCursor, InputToken, Result, SourceId, SourceLocation, Step, TimeBase,
    WorkBudget,
};

/// A track's headers as FFmpeg's MOV demuxer reads them, accepting a zero `mdhd` timescale.
/// The source header keeps the zero: only
/// [`demux_projection_base`](Self::demux_projection_base) substitutes a usable time base,
/// and source ticks still cannot be converted to seconds.
#[derive(Debug)]
pub struct RecoveryTrackHeader {
    source: TrackHeader,
}
impl RecoveryTrackHeader {
    /// Parses as [`TrackHeader::parse`] does, except that a zero `mdhd` timescale is
    /// accepted.
    pub fn parse(
        tkhd: &BoxView,
        mdhd: &BoxView,
        hdlr: &BoxView,
        _profile: RunRecoveryProfile,
    ) -> Result<Self> {
        Ok(Self {
            source: TrackHeader::parse_inner(tkhd, mdhd, hdlr, true)?,
        })
    }
    /// The header as stated in the file; its `timescale` may be zero.
    pub fn source(&self) -> &TrackHeader {
        &self.source
    }
    /// Returns the header as stated in the file.
    pub fn into_source(self) -> TrackHeader {
        self.source
    }
    /// The time base of FFmpeg's packets for the track: 1/timescale of `mdhd`, or 1/1 when
    /// the timescale is zero.
    pub fn demux_projection_base(&self) -> Result<TimeBase> {
        TimeBase::new(1, u64::from(self.source.timescale.max(1)))
    }
    /// Whether the `mdhd` timescale is nonzero, so that source ticks convert to seconds.
    pub fn source_clock_valid(&self) -> bool {
        self.source.timescale != 0
    }
    /// The `Error::Invalid` strict parsing returns for a zero timescale; `None` when the
    /// timescale is nonzero.
    pub fn source_clock_error(&self) -> Option<Error> {
        (!self.source_clock_valid()).then_some(Error::Invalid("zero media/movie timescale"))
    }
}

/// The bytes of a sample that exist before its source ends, at least one, from
/// [`SampleReader::poll_recovery`]. The declared sample and the place the source ended are
/// kept apart.
#[derive(Clone, Debug)]
pub struct ShortSample {
    /// The sample as the table declares it, its range unchanged.
    pub declared: Sample,
    /// The bytes read before the source ended: a nonempty prefix of the declared range.
    pub payload: Bytes,
    /// Where the source ended, as the input reported it.
    pub end: SourceLocation,
    /// The input token of the read.
    pub token: InputToken,
}

/// The result of [`SampleReader::poll_recovery`].
#[derive(Debug)]
pub enum RecoverySampleRead {
    /// The whole declared sample was read.
    Complete(SourcePacket),
    /// The source ended inside the sample after at least one byte.
    Short(ShortSample),
}

/// The media extent a recovery read is checked against, for
/// [`SampleReader::new_recovery`].
#[derive(Debug)]
pub enum RecoverySampleExtent {
    /// A finite `mdat` payload extent in absolute file offsets; the sample must lie inside
    /// it.
    Finite(Range<u64>),
    /// A terminal size-zero `mdat`; the sample must start at or after its payload.
    Terminal(TerminalMdat),
}

/// A sample's declared byte range beside the part of it that exists before a known source
/// length.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PayloadBounds {
    /// The sample's declared byte range, in absolute file offsets.
    pub declared: Range<u64>,
    /// The part of `declared` before the source length; `None` when the sample starts at or
    /// past it, except that an empty sample exactly at the end is available.
    pub available: Option<Range<u64>>,
    pub(crate) known_source_length: u64,
    /// Whether `declared` ends past the source length.
    pub incomplete_at_known_eof: bool,
}
impl PayloadBounds {
    /// Bounds `sample` by a source of `known_length` bytes. Fails with `Error::Invalid` when
    /// the sample leaves `declared_media`, and with `Error::Limit` when its declared size
    /// exceeds `maximum_packet`.
    pub fn from_source(
        sample: &Sample,
        declared_media: Range<u64>,
        known_length: u64,
        maximum_packet: usize,
        _profile: RunRecoveryProfile,
    ) -> Result<Self> {
        sample.require_within(declared_media)?;
        // The limit is on the original declaration, before any physical bound.
        let declared_len =
            usize::try_from(sample.range.end - sample.range.start).map_err(|_| Error::Overflow)?;
        if declared_len > maximum_packet {
            return Err(Error::Limit {
                requested: declared_len,
                maximum: maximum_packet,
            });
        }
        let available = if sample.range.start < known_length
            || (sample.range.is_empty() && sample.range.start == known_length)
        {
            Some(sample.range.start..sample.range.end.min(known_length))
        } else {
            None
        };
        Ok(Self {
            declared: sample.range.clone(),
            available,
            known_source_length: known_length,
            incomplete_at_known_eof: sample.range.end > known_length,
        })
    }
}
/// The bytes a [`PartialSampleReader`] read: the available part of the declared sample.
#[derive(Clone, Debug)]
pub struct RecoveredPayload {
    /// The sample as the table declares it, its range unchanged.
    pub source_sample: Sample,
    /// Its declared and available ranges.
    pub bounds: PayloadBounds,
    /// The bytes of the available range.
    pub payload: Bytes,
    /// The source the bytes came from.
    pub source: SourceId,
    /// The input token of the read.
    pub token: InputToken,
}
/// Reads the part of a sample that exists before the known end of the source, through a
/// [`SampleReader`] over that part; the result keeps the declared sample. A sample with no
/// bytes before the end is refused when the reader is made, not read as an empty packet.
/// If the source ends earlier than its stated length, the read fails as a `SampleReader`'s
/// does.
#[derive(Debug)]
pub struct PartialSampleReader {
    inner: SampleReader,
    source_sample: Sample,
    bounds: PayloadBounds,
}
impl PartialSampleReader {
    /// Starts the read. Fails with `Error::Unsupported` when the input's length is unknown,
    /// with `Error::Invalid` when the sample leaves `declared_media` or has no bytes before
    /// the end, and with `Error::Limit` when its declared size exceeds `maximum_packet`.
    pub fn new(
        input: &mut InputCursor,
        sample: Sample,
        declared_media: Range<u64>,
        maximum_packet: usize,
        profile: RunRecoveryProfile,
    ) -> Result<Self> {
        let known_length = input.capabilities().length.ok_or(Error::Unsupported(
            "partial MOV payload requires known physical length",
        ))?;
        let bounds = PayloadBounds::from_source(
            &sample,
            declared_media,
            known_length,
            maximum_packet,
            profile,
        )?;
        let available = bounds
            .available
            .clone()
            .ok_or(Error::Invalid("no MOV payload bytes at known EOF"))?;
        let mut physical_read = sample.clone();
        physical_read.range = available.clone();
        let inner = SampleReader::new(input, physical_read, available, maximum_packet)?;
        Ok(Self {
            inner,
            source_sample: sample,
            bounds,
        })
    }
    /// Reads the available bytes.
    pub fn poll(
        &mut self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<RecoveredPayload>> {
        Ok(match self.inner.poll(input, work, cancel)? {
            Step::Ready(packet) => Step::Ready(RecoveredPayload {
                source_sample: self.source_sample.clone(),
                bounds: self.bounds.clone(),
                payload: packet.payload,
                source: packet.source,
                token: packet.token,
            }),
            Step::NeedInput(r) => Step::NeedInput(r),
            Step::Yield => Step::Yield,
            Step::End(e) => Step::End(e),
        })
    }
}

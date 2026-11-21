//! Field recovery: a track header with a zero timescale, and the part of a sample
//! that exists before the physical end of the file.
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

/// Raw source clock remains zero/invalid. Only the explicitly labelled demux
/// projection has a fallback base; source_seconds must still reject that scale.
#[derive(Debug)]
pub struct RecoveryTrackHeader {
    source: TrackHeader,
}
impl RecoveryTrackHeader {
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
    pub fn source(&self) -> &TrackHeader {
        &self.source
    }
    pub fn into_source(self) -> TrackHeader {
        self.source
    }
    pub fn demux_projection_base(&self) -> Result<TimeBase> {
        TimeBase::new(1, u64::from(self.source.timescale.max(1)))
    }
    pub fn source_clock_valid(&self) -> bool {
        self.source.timescale != 0
    }
    pub fn source_clock_error(&self) -> Option<Error> {
        (!self.source_clock_valid()).then_some(Error::Invalid("zero media/movie timescale"))
    }
}

/// A positive, physically observed prefix of an incomplete declared sample.
/// The declaration and the source-bound EOF remain separate facts.
#[derive(Clone, Debug)]
pub struct ShortSample {
    pub declared: Sample,
    pub payload: Bytes,
    pub end: SourceLocation,
    pub token: InputToken,
}

#[derive(Debug)]
pub enum RecoverySampleRead {
    Complete(SourcePacket),
    Short(ShortSample),
}

#[derive(Debug)]
pub enum RecoverySampleExtent {
    Finite(Range<u64>),
    Terminal(TerminalMdat),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PayloadBounds {
    pub declared: Range<u64>,
    pub available: Option<Range<u64>>,
    pub(crate) known_source_length: u64,
    pub incomplete_at_known_eof: bool,
}
impl PayloadBounds {
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
#[derive(Clone, Debug)]
pub struct RecoveredPayload {
    pub source_sample: Sample,
    pub bounds: PayloadBounds,
    pub payload: Bytes,
    pub source: SourceId,
    pub token: InputToken,
}
/// Uses the normal core-bound SampleReader for the actual range transaction.
/// The temporary physical-read range is never returned as a declared sample.
/// None availability must be observed separately as demux EOF, not an empty
/// valid-looking packet. Early transport EOF/failure still propagates unchanged.
#[derive(Debug)]
pub struct PartialSampleReader {
    inner: SampleReader,
    source_sample: Sample,
    bounds: PayloadBounds,
}
impl PartialSampleReader {
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

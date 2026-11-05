//! Explicit pinned MOV physical-EOF payload observation. Raw declared ranges
//! and source syntax remain distinct from the available, never padded, bytes.
use crate::{PinnedMovD32RegularRecoveryV1, Sample, SampleReader};
use std::{ops::Range, sync::atomic::AtomicBool};
use triergon_core::{Bytes, Error, InputCursor, InputToken, Result, SourceId, Step, WorkBudget};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mov9PayloadBounds {
    pub declared: Range<u64>,
    pub available: Option<Range<u64>>,
    pub known_source_length: u64,
    pub incomplete_at_known_eof: bool,
}
impl Mov9PayloadBounds {
    pub fn from_source(
        sample: &Sample,
        declared_media: Range<u64>,
        known_length: u64,
        maximum_packet: usize,
        _profile: PinnedMovD32RegularRecoveryV1,
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
pub struct Mov9RecoveredPayload {
    pub source_sample: Sample,
    pub bounds: Mov9PayloadBounds,
    pub payload: Bytes,
    pub source: SourceId,
    pub token: InputToken,
}
/// Uses the normal core-bound SampleReader for the actual range transaction.
/// The temporary physical-read range is never returned as a declared sample.
/// None availability must be observed separately as native EOF, not an empty
/// valid-looking packet. Early transport EOF/failure still propagates unchanged.
#[derive(Debug)]
pub struct Mov9PartialSampleReader {
    inner: SampleReader,
    source_sample: Sample,
    bounds: Mov9PayloadBounds,
}
impl Mov9PartialSampleReader {
    pub fn new(
        input: &mut InputCursor,
        sample: Sample,
        declared_media: Range<u64>,
        maximum_packet: usize,
        profile: PinnedMovD32RegularRecoveryV1,
    ) -> Result<Self> {
        let known_length = input.capabilities().length.ok_or(Error::Unsupported(
            "partial MOV payload requires known physical length",
        ))?;
        let bounds = Mov9PayloadBounds::from_source(
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
    ) -> Result<Step<Mov9RecoveredPayload>> {
        Ok(match self.inner.poll(input, work, cancel)? {
            Step::Ready(packet) => Step::Ready(Mov9RecoveredPayload {
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

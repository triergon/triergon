//! One indexed, self-contained source sample through the shared input contract.
//! No I/O, demux clock inference, configuration epoch or second allocation pool.
use crate::Sample;
use safe_media_core::{
    BudgetVec, Bytes, Error, InputCursor, InputToken, Result, SourceId, Step, WorkBudget,
};
use std::{ops::Range, sync::atomic::AtomicBool};

#[derive(Clone, Debug)]
pub struct SourcePacket {
    pub sample: Sample,
    pub payload: Bytes,
    pub source: SourceId,
    pub token: InputToken,
}

/// A bounded range transaction. Construction checks/reserves before changing the
/// input transaction. A freeze backpressure retry retains all completed bytes.
#[derive(Debug)]
pub struct SampleReader {
    sample: Sample,
    source: SourceId,
    token: InputToken,
    position: u64,
    length: usize,
    buffer: Option<BudgetVec<u8>>,
    failed: Option<Error>,
}
impl SampleReader {
    pub fn new(
        input: &mut InputCursor,
        sample: Sample,
        media_extent: Range<u64>,
        maximum_packet: usize,
    ) -> Result<Self> {
        sample.require_within(media_extent)?;
        if input
            .capabilities()
            .length
            .is_some_and(|end| sample.range.end > end)
        {
            return Err(Error::Invalid("sample exceeds source length"));
        }
        let length =
            usize::try_from(sample.range.end - sample.range.start).map_err(|_| Error::Overflow)?;
        if length > maximum_packet {
            return Err(Error::Limit {
                requested: length,
                maximum: maximum_packet,
            });
        }
        let mut buffer = BudgetVec::new(input.budget());
        buffer.try_reserve_exact(length)?;
        // Internal I/O transaction only; core preserves parser generation.
        if input.position() != sample.range.start || input.capabilities().random_access {
            input.request_range(sample.range.start)?;
        }
        Ok(Self {
            position: sample.range.start,
            sample,
            source: input.source(),
            token: input.token(),
            length,
            buffer: Some(buffer),
            failed: None,
        })
    }
    pub fn poll(
        &mut self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<SourcePacket>> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = self.poll_inner(input, work, cancel);
        if let Err(error) = result {
            if !matches!(error, Error::Backpressure { .. }) {
                self.failed = Some(error);
            }
        }
        result
    }
    fn poll_inner(
        &mut self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<SourcePacket>> {
        // Even a zero-byte sample or a completed buffer must observe cancellation
        // and transport failure before producing a packet.
        input.read_into(&mut [], work, cancel)?;
        if self.source != input.source() {
            return Err(Error::SourceMismatch);
        }
        let current = input.token();
        if current.generation() != self.token.generation() {
            return Err(Error::StaleGeneration {
                expected: self.token.generation(),
                received: current.generation(),
            });
        }
        if current.request() != self.token.request() {
            return Err(Error::StaleRequest {
                expected: self.token.request(),
                received: current.request(),
            });
        }
        if self.token != current {
            return Err(Error::ForeignPool);
        }
        if self.position != input.position() {
            return Err(Error::Invalid("input changed outside sample reader"));
        }
        loop {
            let buffer = self
                .buffer
                .as_mut()
                .ok_or(Error::Invalid("sample reader completed"))?;
            if buffer.len() == self.length {
                if !work.try_charge(1) {
                    return Ok(Step::Yield);
                }
                let buffer = self
                    .buffer
                    .take()
                    .ok_or(Error::Invalid("missing sample buffer"))?;
                let payload = match buffer.freeze() {
                    Ok(bytes) => bytes,
                    Err(failure) => {
                        self.buffer = Some(failure.buffer);
                        return Err(failure.error);
                    }
                };
                return Ok(Step::Ready(SourcePacket {
                    sample: self.sample.clone(),
                    payload,
                    source: self.source,
                    token: self.token.clone(),
                }));
            }
            match input.read_span(self.length - buffer.len(), work, cancel)? {
                Step::Ready(span) => {
                    // Full packet capacity was reserved before consuming input.
                    buffer.try_extend_from_slice(span.as_ref())?;
                    self.position = input.position();
                }
                Step::NeedInput(request) => return Ok(Step::NeedInput(request)),
                Step::Yield => return Ok(Step::Yield),
                Step::End(_) => return Err(Error::Truncated),
            }
        }
    }
}

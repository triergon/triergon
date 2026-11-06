use crate::{NalFacts, NalUnit};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result, WorkBudget};

/// A work-bounded operation with no external input requests. `Ready` occurs
/// exactly once; subsequent calls return `Complete` unless cancelled/failed.
#[derive(Debug)]
pub enum BuildStep<T> {
    Yield,
    Ready(T),
    Complete,
}

#[derive(Clone, Debug)]
pub struct AnnexBOutput {
    bytes: Bytes,
    source_facts: NalFacts,
}

impl AnnexBOutput {
    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }
    pub const fn source_facts(&self) -> NalFacts {
        self.source_facts
    }
}

/// Emits exactly 00 00 00 01 followed by the original NAL bytes. No parameter
/// sets, access-unit delimiters, emulation-prevention bytes or padding are added
/// inside the NAL. A four-byte prefix includes the Annex-B zero_byte so callers
/// need not guess whether this NAL starts an access unit.
pub struct AnnexBWriter {
    pool: Budget,
    input: Option<NalUnit>,
    facts: NalFacts,
    output: Option<BudgetVec<u8>>,
    total: usize,
    copied: usize,
    reserved: bool,
    complete: bool,
    failure: Option<Error>,
}

impl AnnexBWriter {
    pub fn new(pool: &Budget, input: NalUnit, maximum_output_bytes: usize) -> Result<Self> {
        if !input.bytes().belongs_to(pool) {
            return Err(Error::ForeignPool);
        }
        let total = input.bytes().len().checked_add(4).ok_or(Error::Overflow)?;
        if total > maximum_output_bytes {
            return Err(Error::Limit {
                requested: total,
                maximum: maximum_output_bytes,
            });
        }
        Ok(Self {
            pool: pool.clone(),
            facts: input.facts(),
            input: Some(input),
            output: None,
            total,
            copied: 0,
            reserved: false,
            complete: false,
            failure: None,
        })
    }

    /// Charges one unit per output byte copied and one per reserve/freeze
    /// attempt. A failed freeze retains the completed builder for retry.
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<AnnexBOutput>> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if cancel.load(Ordering::Relaxed) {
            self.stop(Error::Cancelled);
            return Err(Error::Cancelled);
        }
        let result = self.step_inner(work, cancel);
        if let Err(error) = result
            && !matches!(error, Error::Backpressure { .. } | Error::Allocation)
        {
            self.stop(error);
        }
        result
    }

    fn stop(&mut self, error: Error) {
        self.failure = Some(error);
        self.input = None;
        self.output = None;
    }

    fn step_inner(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<AnnexBOutput>> {
        if self.complete {
            return Ok(BuildStep::Complete);
        }
        if !self.reserved {
            if !work.try_charge(1) {
                return Ok(BuildStep::Yield);
            }
            if self.output.is_none() {
                self.output = Some(BudgetVec::new(&self.pool));
            }
            self.output
                .as_mut()
                .ok_or(Error::Invalid("missing Annex B builder"))?
                .try_reserve_exact(self.total)?;
            self.reserved = true;
        }
        while self.copied < self.total {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let available = if self.copied < 4 {
                4 - self.copied
            } else {
                (self.total - self.copied).min(4096)
            };
            let count =
                usize::try_from(work.take(u64::try_from(available).map_err(|_| Error::Overflow)?))
                    .map_err(|_| Error::Overflow)?;
            if count == 0 {
                return Ok(BuildStep::Yield);
            }
            let output = self
                .output
                .as_mut()
                .ok_or(Error::Invalid("missing Annex B builder"))?;
            if self.copied < 4 {
                output.try_extend_from_slice(&[0, 0, 0, 1][self.copied..self.copied + count])?;
            } else {
                let input = self
                    .input
                    .as_ref()
                    .ok_or(Error::Invalid("missing Annex B input"))?;
                let start = self.copied - 4;
                output.try_extend_from_slice(&input.bytes()[start..start + count])?;
            }
            self.copied += count;
        }
        if !work.try_charge(1) {
            return Ok(BuildStep::Yield);
        }
        let output = self
            .output
            .take()
            .ok_or(Error::Invalid("missing Annex B builder"))?;
        let bytes = match output.freeze() {
            Ok(bytes) => bytes,
            Err(rejected) => {
                self.output = Some(rejected.buffer);
                return Err(rejected.error);
            }
        };
        self.input = None;
        self.complete = true;
        Ok(BuildStep::Ready(AnnexBOutput {
            bytes,
            source_facts: self.facts,
        }))
    }
}

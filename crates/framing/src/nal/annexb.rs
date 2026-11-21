use crate::BuildStep;
use crate::nal::{NalFacts, NalUnit};
use crate::{freeze_whole, retryable};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result, WorkBudget};

/// The four-byte start code written before every NAL unit; a three-byte code
/// is its last three bytes.
pub(crate) const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// One NAL unit behind a four-byte start code, as [`AnnexBWriter`] writes it.
#[derive(Clone, Debug)]
pub struct AnnexBOutput {
    bytes: Bytes,
    source_facts: NalFacts,
}

impl AnnexBOutput {
    /// `00 00 00 01` followed by the NAL unit's bytes.
    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }
    /// Where the NAL unit lay in its source and how it was framed there.
    pub const fn source_facts(&self) -> NalFacts {
        self.source_facts
    }
}

/// Emits exactly 00 00 00 01 followed by the original NAL bytes. No parameter
/// sets, access-unit delimiters, emulation-prevention bytes or padding are added
/// inside the NAL. A four-byte prefix includes the Annex-B `zero_byte` so callers
/// need not guess whether this NAL starts an access unit.
#[derive(Debug)]
pub struct AnnexBWriter {
    facts: NalFacts,
    total: usize,
    copied: usize,
    reserved: bool,
    state: State,
}

/// Where a write stands. The NAL unit and the output are held only while it is
/// written.
#[derive(Debug)]
enum State {
    Writing {
        input: NalUnit,
        output: BudgetVec<u8>,
    },
    Complete,
    Failed(Error),
}

impl AnnexBWriter {
    /// A writer for `input`, whose bytes are charged to `pool`; the output is charged
    /// to it too. Fails with [`Error::ForeignPool`] when `input` is charged to another
    /// pool; [`Error::Limit`] when the NAL unit and its start code exceed
    /// `maximum_output_bytes`.
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
            facts: input.facts(),
            total,
            copied: 0,
            reserved: false,
            state: State::Writing {
                input,
                output: BudgetVec::new(pool),
            },
        })
    }

    /// Charges one unit per output byte copied and one per reserve/freeze attempt. A
    /// failed freeze retains the completed builder for retry. Fails with
    /// [`Error::Cancelled`] when `cancel` is set. [`Error::Backpressure`] and
    /// [`Error::Allocation`] are retryable: the writer keeps its progress. Every other
    /// error is final and returned again by each later call.
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<AnnexBOutput>> {
        if let State::Failed(error) = self.state {
            return Err(error);
        }
        if cancel.load(Ordering::Relaxed) {
            self.state = State::Failed(Error::Cancelled);
            return Err(Error::Cancelled);
        }
        let result = self.write(work, cancel);
        if let Err(error) = result
            && !retryable(error)
        {
            self.state = State::Failed(error);
        }
        result
    }

    fn write(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<AnnexBOutput>> {
        let State::Writing { input, output } = &mut self.state else {
            return Ok(BuildStep::Complete);
        };
        if !self.reserved {
            if !work.try_charge(1) {
                return Ok(BuildStep::Yield);
            }
            output.try_reserve_exact(self.total)?;
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
            if self.copied < 4 {
                output.try_extend_from_slice(&START_CODE[self.copied..self.copied + count])?;
            } else {
                let start = self.copied - 4;
                output.try_extend_from_slice(&input.bytes()[start..start + count])?;
            }
            self.copied += count;
        }
        if !work.try_charge(1) {
            return Ok(BuildStep::Yield);
        }
        let bytes = freeze_whole(output)?;
        self.state = State::Complete;
        Ok(BuildStep::Ready(AnnexBOutput {
            bytes,
            source_facts: self.facts,
        }))
    }
}

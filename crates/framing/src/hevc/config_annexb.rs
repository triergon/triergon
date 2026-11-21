//! Every NAL unit of an `hvcC` record in record order, as Annex B. The record stays
//! available; unknown arrays, duplicates and mismatched declarations are written as
//! they are.
use crate::nal::annexb::START_CODE;
use crate::retryable;
use crate::{BuildStep, hevc::HvcConfiguration};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result, WorkBudget};

/// The NAL units of an `hvcC` record as one Annex B buffer, with the record they
/// were written from.
#[derive(Debug)]
pub struct HvcAnnexBOutput {
    bytes: Bytes,
    configuration: HvcConfiguration,
}
impl HvcAnnexBOutput {
    /// Every NAL unit of the record in record order, each after a four-byte start
    /// code. Empty for a record without NAL units.
    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }
    /// The record the bytes were written from, as it was given to the writer.
    pub fn configuration(&self) -> &HvcConfiguration {
        &self.configuration
    }
}

enum Phase {
    Count,
    Reserve,
    Copy,
    Freeze,
}

/// Where a conversion stands. The record and the output are held only while it is
/// written.
#[expect(
    clippy::large_enum_variant,
    reason = "one write per writer; boxing it would allocate outside the pool"
)]
enum State {
    Writing(Writing),
    Complete,
    Failed(Error),
}

struct Writing {
    input: HvcConfiguration,
    output: BudgetVec<u8>,
}

/// Writes every NAL unit of an `hvcC` record as Annex B, in record order and
/// whatever type its array declares: each after a four-byte start code, into one
/// buffer charged to the pool.
pub struct HvcAnnexBWriter {
    pool: Budget,
    maximum: usize,
    total: usize,
    index: usize,
    nal_offset: usize,
    phase: Phase,
    state: State,
}
impl std::fmt::Debug for HvcAnnexBWriter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let failure = match self.state {
            State::Failed(error) => Some(error),
            State::Writing(_) | State::Complete => None,
        };
        formatter
            .debug_struct("HvcAnnexBWriter")
            .field("maximum", &self.maximum)
            .field("total", &self.total)
            .field("index", &self.index)
            .field("failure", &failure)
            .finish_non_exhaustive()
    }
}
impl HvcAnnexBWriter {
    /// Starts writing the NAL units of `input` into a buffer of at most
    /// `maximum_output_bytes` bytes charged to `pool`. Fails when the record belongs to
    /// another pool.
    pub fn new(
        pool: &Budget,
        input: HvcConfiguration,
        maximum_output_bytes: usize,
    ) -> Result<Self> {
        if !input.source_bytes().belongs_to(pool) {
            return Err(Error::ForeignPool);
        }
        Ok(Self {
            pool: pool.clone(),
            maximum: maximum_output_bytes,
            total: 0,
            index: 0,
            nal_offset: 0,
            phase: Phase::Count,
            state: State::Writing(Writing {
                input,
                output: BudgetVec::new(pool),
            }),
        })
    }
    /// Advances by at most the units in `work` and returns the output once. Sizing
    /// the output costs one unit for each NAL unit and writing it one for each byte.
    ///
    /// Fails when `cancel` is set, the output would exceed `maximum_output_bytes`, or
    /// the pool's limit is below the record and the output together. Every later call
    /// returns the same error. A pool that lacks the room only until other storage is
    /// released is `Backpressure` or `Allocation` instead, and the call can be
    /// repeated.
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<HvcAnnexBOutput>> {
        let result = self.write(work, cancel);
        if let Err(error) = result
            && !retryable(error)
        {
            self.state = State::Failed(error);
        }
        result
    }
    /// Writes for as long as `work` lasts, and publishes the output once it is
    /// frozen.
    fn write(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<HvcAnnexBOutput>> {
        let Writing { input, output } = match &mut self.state {
            State::Writing(writing) => writing,
            State::Complete => return Ok(BuildStep::Complete),
            State::Failed(error) => return Err(*error),
        };
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            match self.phase {
                Phase::Count => {
                    if !work.try_charge(1) {
                        return Ok(BuildStep::Yield);
                    }
                    if let Some(nal) = input.nal(self.index) {
                        let total = self
                            .total
                            .checked_add(4)
                            .and_then(|n| n.checked_add(nal.source_bytes().len()))
                            .ok_or(Error::Overflow)?;
                        if total > self.maximum {
                            return Err(Error::Limit {
                                requested: total,
                                maximum: self.maximum,
                            });
                        }
                        self.total = total;
                        self.index += 1;
                    } else {
                        let minimum = Budget::control_storage_bytes()
                            .checked_add(input.retained_storage_bytes()?)
                            .and_then(|n| n.checked_add(self.total))
                            .and_then(|n| n.checked_add(Bytes::owner_overhead_bytes()))
                            .ok_or(Error::Overflow)?;
                        if minimum > self.pool.limit() {
                            return Err(Error::Limit {
                                requested: minimum,
                                maximum: self.pool.limit(),
                            });
                        }
                        self.index = 0;
                        self.phase = Phase::Reserve;
                    }
                }
                Phase::Reserve => {
                    if !work.try_charge(1) {
                        return Ok(BuildStep::Yield);
                    }
                    output.try_reserve_exact(self.total)?;
                    self.phase = Phase::Copy;
                }
                Phase::Copy => {
                    let Some(nal) = input.nal(self.index) else {
                        self.phase = Phase::Freeze;
                        continue;
                    };
                    let end = nal
                        .source_bytes()
                        .len()
                        .checked_add(4)
                        .ok_or(Error::Overflow)?;
                    if self.nal_offset == end {
                        if !work.try_charge(1) {
                            return Ok(BuildStep::Yield);
                        }
                        self.index += 1;
                        self.nal_offset = 0;
                        continue;
                    }
                    let available = if self.nal_offset < 4 {
                        4 - self.nal_offset
                    } else {
                        (end - self.nal_offset).min(4096)
                    };
                    let count = usize::try_from(
                        work.take(u64::try_from(available).map_err(|_| Error::Overflow)?),
                    )
                    .map_err(|_| Error::Overflow)?;
                    if count == 0 {
                        return Ok(BuildStep::Yield);
                    }
                    if self.nal_offset < 4 {
                        output.try_extend_from_slice(
                            &START_CODE[self.nal_offset..self.nal_offset + count],
                        )?;
                    } else {
                        let start = self.nal_offset - 4;
                        output.try_extend_from_slice(&nal.source_bytes()[start..start + count])?;
                    }
                    self.nal_offset += count;
                }
                Phase::Freeze => {
                    if !work.try_charge(1) {
                        return Ok(BuildStep::Yield);
                    }
                    if output.len() != self.total {
                        return Err(Error::Invalid("hvcC framing byte count"));
                    }
                    let State::Writing(Writing { input, output }) =
                        std::mem::replace(&mut self.state, State::Complete)
                    else {
                        unreachable!("the writer is writing")
                    };
                    return match output.freeze() {
                        Ok(bytes) => Ok(BuildStep::Ready(HvcAnnexBOutput {
                            bytes,
                            configuration: input,
                        })),
                        Err(refused) => {
                            self.state = State::Writing(Writing {
                                input,
                                output: refused.value,
                            });
                            Err(refused.error)
                        }
                    };
                }
            }
        }
    }
}

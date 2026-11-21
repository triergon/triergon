//! Complete ordered hvcC NAL framing. Source configuration remains available;
//! unknown arrays, duplicates and mismatches are never filtered or repaired.
use crate::nal::annexb::START_CODE;
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
    Complete,
}
/// Writes every NAL unit of an `hvcC` record as Annex B, in record order and
/// whatever type its array declares: each after a four-byte start code, into one
/// buffer charged to the pool.
pub struct HvcAnnexBWriter {
    pool: Budget,
    input: Option<HvcConfiguration>,
    output: Option<BudgetVec<u8>>,
    maximum: usize,
    total: usize,
    index: usize,
    nal_offset: usize,
    phase: Phase,
    failure: Option<Error>,
}
impl std::fmt::Debug for HvcAnnexBWriter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HvcAnnexBWriter")
            .field("maximum", &self.maximum)
            .field("total", &self.total)
            .field("index", &self.index)
            .field("failure", &self.failure)
            .finish_non_exhaustive()
    }
}
impl HvcAnnexBWriter {
    /// Starts writing the NAL units of `input` into a buffer of at most
    /// `maximum_output_bytes` bytes charged to `pool`.
    ///
    /// # Errors
    ///
    /// When the record belongs to another pool.
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
            input: Some(input),
            output: None,
            maximum: maximum_output_bytes,
            total: 0,
            index: 0,
            nal_offset: 0,
            phase: Phase::Count,
            failure: None,
        })
    }
    /// Advances by at most the units in `work` and returns the output once, as
    /// [`BuildStep`] describes. Sizing the output costs one unit for each NAL unit
    /// and writing it one for each byte.
    ///
    /// # Errors
    ///
    /// When `cancel` is set, the output would exceed `maximum_output_bytes`, or the
    /// pool's limit is below the record and the output together. Every later call
    /// returns the same error. A pool that lacks the room only until other storage
    /// is released is `Backpressure` or `Allocation` instead, and the call can be
    /// repeated.
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<HvcAnnexBOutput>> {
        if let Some(e) = self.failure {
            return Err(e);
        }
        let result = self.advance(work, cancel);
        if let Err(e) = result
            && !matches!(e, Error::Backpressure { .. } | Error::Allocation)
        {
            self.failure = Some(e);
            self.input = None;
            self.output = None;
        }
        result
    }
    fn advance(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<HvcAnnexBOutput>> {
        loop {
            if matches!(self.phase, Phase::Complete) {
                return Ok(BuildStep::Complete);
            }
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let input = self
                .input
                .as_ref()
                .ok_or(Error::Invalid("missing hvcC framing input"))?;
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
                    if self.output.is_none() {
                        self.output = Some(BudgetVec::new(&self.pool));
                    }
                    self.output
                        .as_mut()
                        .ok_or(Error::Invalid("missing hvcC framing builder"))?
                        .try_reserve_exact(self.total)?;
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
                    let out = self
                        .output
                        .as_mut()
                        .ok_or(Error::Invalid("missing hvcC framing builder"))?;
                    if self.nal_offset < 4 {
                        out.try_extend_from_slice(
                            &START_CODE[self.nal_offset..self.nal_offset + count],
                        )?;
                    } else {
                        let start = self.nal_offset - 4;
                        out.try_extend_from_slice(&nal.source_bytes()[start..start + count])?;
                    }
                    self.nal_offset += count;
                }
                Phase::Freeze => {
                    if !work.try_charge(1) {
                        return Ok(BuildStep::Yield);
                    }
                    let out = self
                        .output
                        .take()
                        .ok_or(Error::Invalid("missing hvcC framing builder"))?;
                    if out.len() != self.total {
                        return Err(Error::Invalid("hvcC framing byte count"));
                    }
                    let bytes = match out.freeze() {
                        Ok(b) => b,
                        Err(e) => {
                            self.output = Some(e.value);
                            return Err(e.error);
                        }
                    };
                    let configuration = self
                        .input
                        .take()
                        .ok_or(Error::Invalid("missing hvcC framing input"))?;
                    self.phase = Phase::Complete;
                    return Ok(BuildStep::Ready(HvcAnnexBOutput {
                        bytes,
                        configuration,
                    }));
                }
                Phase::Complete => return Ok(BuildStep::Complete),
            }
        }
    }
}

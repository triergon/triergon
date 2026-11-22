//! Shared lower bound for materializing a validated PES-backed unit.
use super::{Piece, table_storage};
use crate::reader::PayloadSource;
use crate::reader::model::{PacketData, Shared};
use crate::{Pes, PesSpan, TraceEvent};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result, WorkBudget};

// Count each allocation pinned by this PES once, regardless of its slice count.
// Other pool reservations can still cause retryable backpressure.
pub(super) struct Storage {
    minimum: usize,
    span: usize,
    prior: usize,
}

impl Storage {
    pub(super) fn new(pes: &Pes, pieces: &BudgetVec<Piece>, copy: Option<usize>) -> Result<Self> {
        let minimum = Budget::control_storage_bytes()
            .checked_add(table_storage(pieces)?)
            .and_then(|n| n.checked_add(table_storage(&pes.header_source_ranges).ok()?))
            .and_then(|n| n.checked_add(pes.spans().len().checked_mul(size_of::<PesSpan>())?))
            .and_then(|n| n.checked_add(pes.header.retained_storage()))
            .and_then(|n| n.checked_add(Shared::<TraceEvent>::allocation_bytes().ok()?))
            .and_then(|n| n.checked_add(Shared::<PacketData>::allocation_bytes().ok()?))
            .and_then(|n| n.checked_add(size_of::<PayloadSource>()))
            .and_then(|n| match copy {
                None => Some(n),
                Some(size) => n
                    .checked_add(size)?
                    .checked_add(Bytes::owner_overhead_bytes()),
            })
            .ok_or(Error::Overflow)?;
        Ok(Self {
            minimum,
            span: 0,
            prior: 0,
        })
    }

    pub(super) fn check_limit(&self, pool: &Budget) -> Result<()> {
        if self.minimum > pool.limit() {
            return Err(Error::Limit {
                requested: self.minimum,
                maximum: pool.limit(),
            });
        }
        Ok(())
    }

    pub(super) fn step(
        &mut self,
        pes: &Pes,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        while let Some(span) = pes.spans().get(self.span) {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if !work.try_charge(1) {
                return Ok(false);
            }
            let bytes = span.backing_bytes();
            if self.prior == self.span {
                self.minimum = self
                    .minimum
                    .checked_add(bytes.retained_storage_bytes())
                    .ok_or(Error::Overflow)?;
            } else if !pes.spans()[self.prior]
                .backing_bytes()
                .shares_storage_with(bytes)
            {
                self.prior += 1;
                continue;
            }
            self.span += 1;
            self.prior = 0;
        }
        Ok(true)
    }
}

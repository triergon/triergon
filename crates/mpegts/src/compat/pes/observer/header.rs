//! The PES header of the packet being cut.
use super::{Arena, DemuxObserver};
use crate::pes::{HEADER_MAX, parse_header, parse_timed_header};
use crate::{SourceRange, commit_range, limit, prepare_range};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Error, Result, WorkBudget};

impl DemuxObserver {
    fn finish_header(&mut self) -> Result<()> {
        let header = parse_header(&self.budget, &self.raw_header[..self.header_used])?;
        if header.stream_id != self.scope.stream_id {
            return Err(Error::Unsupported(
                "native PES stream-id context unobserved",
            ));
        }
        self.body_limit = self
            .declared_total
            .map(|total| {
                total
                    .checked_sub(self.header_used)
                    .ok_or(Error::Invalid("PES header exceeds length"))
            })
            .transpose()?;
        self.header = Some(header);
        Ok(())
    }

    /// Read a timestamp-only header from one carrier when its whole validation
    /// fits. The ordinary staged parser handles incomplete or invalid headers.
    pub(super) fn complete_timed_header(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        let pending = self
            .pending
            .as_ref()
            .ok_or(Error::Invalid("native PES input missing"))?;
        let payload = pending
            .payload
            .as_ref()
            .ok_or(Error::Invalid("native PES payload missing"))?;
        let bytes = &payload[pending.position..];
        let Some((header, declared)) = parse_timed_header(bytes, self.limits.pes_bytes, work)
        else {
            return Ok(false);
        };
        if header.stream_id != self.scope.stream_id || (declared.is_none() && self.finite_private())
        {
            return Ok(false);
        }
        let used = header.as_bytes().len();
        let charge = used as u64 + 3;
        let extent = SourceRange {
            entity: self.scope.entity,
            offset: pending
                .carrier
                .payload_source_offset
                .checked_add(pending.position as u64)
                .ok_or(Error::Overflow)?,
            bytes: used,
        };
        extent.end()?;
        if !self.reserve(Arena::Header, work, cancel)? || !work.try_charge(charge) {
            return Ok(false);
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        debug_assert!(self.header_ranges.is_empty());
        self.header_ranges
            .try_push(extent)
            .map_err(|error| error.error)?;
        self.header_used = used;
        self.header_target = used;
        self.declared_total = declared;
        self.body_limit = declared.map(|total| total - used);
        self.header = Some(header);
        self.pending
            .as_mut()
            .ok_or(Error::Invalid("native PES input missing"))?
            .position += used;
        Ok(true)
    }

    pub(super) fn poll_header(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        if self.header_used == 0 && self.complete_timed_header(work, cancel)? {
            return Ok(true);
        }
        self.poll_staged_header(work, cancel)
    }

    /// Reads a header byte range at a time, for the headers one carrier does not
    /// hold whole or that the whole-header path refuses. Kept apart from that path.
    #[inline(never)]
    fn poll_staged_header(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<bool> {
        if self.header_used == self.header_target {
            if !work.try_charge(1) {
                return Ok(false);
            }
            match self.header_used {
                6 => {
                    if self.raw_header[..3] != [0, 0, 1] {
                        return Err(Error::Invalid("PES start code"));
                    }
                    if self.raw_header[3] != self.scope.stream_id {
                        return Err(Error::Unsupported(
                            "native PES stream-id context unobserved",
                        ));
                    }
                    let length =
                        usize::from(u16::from_be_bytes([self.raw_header[4], self.raw_header[5]]));
                    if length == 0 && self.finite_private() {
                        return Err(self.finite_private_error(
                            "native private audio unbounded PES unobserved",
                            "native DVB unbounded PES unobserved",
                        ));
                    }
                    if length != 0 {
                        let total = length + 6;
                        if total > self.limits.pes_bytes {
                            return Err(limit(total, self.limits.pes_bytes));
                        }
                        if total < 9 {
                            return Err(Error::Invalid("PES optional-header length"));
                        }
                        self.declared_total = Some(total);
                    }
                    self.header_target = 9;
                }
                9 => {
                    self.header_target = 9 + usize::from(self.raw_header[8]);
                    if self.header_target > self.limits.pes_bytes {
                        return Err(limit(self.header_target, self.limits.pes_bytes));
                    }
                    if self
                        .declared_total
                        .is_some_and(|total| self.header_target > total)
                    {
                        return Err(Error::Invalid("PES header exceeds length"));
                    }
                    if self.header_target == 9 {
                        self.finish_header()?;
                    }
                }
                _ => self.finish_header()?,
            }
            return Ok(true);
        }
        let pending = self
            .pending
            .as_ref()
            .ok_or(Error::Invalid("native PES input missing"))?;
        let payload = pending
            .payload
            .as_ref()
            .ok_or(Error::Invalid("native PES payload missing"))?;
        if pending.position == payload.len() {
            self.pending = None;
            return Ok(true);
        }
        let extent = SourceRange {
            entity: self.scope.entity,
            offset: pending
                .carrier
                .payload_source_offset
                .checked_add(pending.position as u64)
                .ok_or(Error::Overflow)?,
            bytes: 1,
        };
        let merge = self.header_ranges.as_slice().last().is_some_and(|last| {
            last.entity == extent.entity && last.end().ok() == Some(extent.offset)
        });
        if !merge && !self.reserve(Arena::Header, work, cancel)? {
            return Ok(false);
        }
        if !work.try_charge(1) {
            return Ok(false);
        }
        let pending = self
            .pending
            .as_ref()
            .ok_or(Error::Invalid("native PES input missing"))?;
        let payload = pending
            .payload
            .as_ref()
            .ok_or(Error::Invalid("native PES payload missing"))?
            .as_ref();
        let count = (self.header_target - self.header_used)
            .min(payload.len() - pending.position)
            .min(work.remaining().min(HEADER_MAX as u64) as usize + 1);
        let bytes = &payload[pending.position..pending.position + count];
        let extent = SourceRange {
            bytes: count,
            ..extent
        };
        let merge = prepare_range(&mut self.header_ranges, extent, self.limits.source_spans)?;
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        commit_range(&mut self.header_ranges, extent, merge)?;
        self.raw_header[self.header_used..self.header_used + count].copy_from_slice(bytes);
        work.take(count as u64 - 1);
        self.header_used += count;
        self.pending
            .as_mut()
            .ok_or(Error::Invalid("native PES input missing"))?
            .position += count;
        Ok(true)
    }
}

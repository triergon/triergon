//! The start and body of the packet being cut.
use super::{Arena, DEFAULT_PACKET_CAP, DemuxBoundary, DemuxObserver, DemuxSpan};
use crate::{SourceRange, limit};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Error, Result, WorkBudget};

impl DemuxObserver {
    pub(super) fn poll_start(&mut self, work: &mut WorkBudget) -> Result<bool> {
        let pending = self
            .pending
            .as_ref()
            .ok_or(Error::Invalid("PES observer input missing"))?;
        let has_payload = pending
            .payload
            .as_ref()
            .is_some_and(|bytes| !bytes.is_empty());
        if !pending.counter_checked {
            if !work.try_charge(1) {
                return Ok(false);
            }
            let counter = pending.carrier.continuity_counter;
            let gap = self
                .previous_counter
                .is_some_and(|previous| counter != ((previous + u8::from(has_payload)) & 15));
            if gap {
                self.counter_gap(
                    pending.carrier.payload_unit_start,
                    pending.carrier.discontinuity,
                    has_payload,
                )?;
            }
            self.previous_counter = Some(counter);
            self.pending
                .as_mut()
                .ok_or(Error::Invalid("PES observer input missing"))?
                .counter_checked = true;
        }
        if !has_payload {
            self.pending = None;
            return Ok(true);
        }
        let start = self
            .pending
            .as_ref()
            .ok_or(Error::Invalid("PES observer input missing"))?
            .carrier
            .payload_unit_start;
        if start && self.active {
            self.schedule(DemuxBoundary::NextPesStart, false, false)?;
            return Ok(true);
        }
        if start {
            if !work.try_charge(1) {
                return Ok(false);
            }
            self.begin_pes()?;
        } else if !self.active {
            return Err(Error::Unsupported(
                "PES continuation without an active packet",
            ));
        }
        self.pending
            .as_mut()
            .ok_or(Error::Invalid("PES observer input missing"))?
            .started = true;
        Ok(true)
    }

    #[inline(always)]
    fn counter_gap(&mut self, start: bool, discontinuity: bool, has_payload: bool) -> Result<()> {
        if self.finite_private() {
            return Err(self.finite_private_error(
                "continuity counter damage in private audio PES",
                "continuity counter damage in DVB subtitle PES",
            ));
        }
        if !start && discontinuity {
            return Err(Error::Unsupported(
                "continuation has a continuity counter break and a discontinuity indicator",
            ));
        }
        if !has_payload {
            return Err(Error::Unsupported(
                "continuity counter changed on a carrier without payload",
            ));
        }
        // Mark the packet before a possible pre-append flush.
        if !start && self.active {
            self.corrupt = true;
        }
        Ok(())
    }

    pub(super) fn poll_body(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<bool> {
        let pending = self
            .pending
            .as_ref()
            .ok_or(Error::Invalid("PES observer input missing"))?;
        if pending.body_drained {
            self.pending = None;
            return Ok(true);
        }
        let payload = pending
            .payload
            .as_ref()
            .ok_or(Error::Invalid("PES observer payload missing"))?;
        let available = payload.len() - pending.position;
        if available == 0 {
            self.pending = None;
            return Ok(true);
        }
        let mut count = available;
        if let Some(limit) = self.body_limit {
            let remaining = limit.checked_sub(self.payload_len()).ok_or(Error::Invalid(
                "PES buffer already exceeds the declared payload length",
            ))?;
            if available > remaining {
                let Some(in_bound) =
                    self.finite_overrun(pending.carrier.payload_unit_start, remaining)?
                else {
                    return Ok(true);
                };
                count = in_bound;
            }
        }
        let next = self
            .payload_len()
            .checked_add(count)
            .ok_or(Error::Overflow)?;
        let retained = self.header_used.checked_add(next).ok_or(Error::Overflow)?;
        if retained > self.limits.pes_bytes {
            return Err(limit(retained, self.limits.pes_bytes));
        }
        if next >= DEFAULT_PACKET_CAP {
            return Err(Error::Unsupported(
                "PES payload reaches the assembly size cap",
            ));
        }
        if !self.reserve(Arena::Payload, work, cancel)? || !work.try_charge(1) {
            return Ok(false);
        }
        let pending = self
            .pending
            .as_mut()
            .ok_or(Error::Invalid("PES observer input missing"))?;
        let payload = pending
            .payload
            .as_ref()
            .ok_or(Error::Invalid("PES observer payload missing"))?;
        let start = pending.position;
        let end = start.checked_add(count).ok_or(Error::Overflow)?;
        payload
            .as_ref()
            .get(start..end)
            .ok_or(Error::Invalid("byte span range"))?;
        let source = SourceRange {
            entity: self.scope.entity,
            offset: pending
                .carrier
                .payload_source_offset
                .checked_add(start as u64)
                .ok_or(Error::Overflow)?,
            bytes: count,
        };
        let transport = SourceRange {
            entity: self.scope.entity,
            offset: pending.carrier.source_offset,
            bytes: pending.source.len(),
        };
        source.end()?;
        transport.end()?;
        let complete = end == payload.len();
        let next_position = if count < available {
            payload.len()
        } else {
            end
        };
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        // The arena has a free slot and all ranges are checked. Keep the
        // pending carrier until the same poll boundary after moving its payload.
        let bytes = if complete {
            pending
                .payload
                .take()
                .ok_or(Error::Invalid("PES observer payload missing"))?
                .into_slice(start..end)?
        } else {
            payload.slice(start..end)?
        };
        let span = DemuxSpan {
            bytes,
            source,
            transport,
            context: pending.context,
        };
        self.spans.try_push(span).map_err(|error| error.error)?;
        // A bounded <=184-byte admission per work unit, not a per-yield span.
        pending.position = next_position;
        pending.body_drained = complete;
        self.add_spans(1);
        self.set_payload_len(next);
        Ok(true)
    }

    #[inline(always)]
    fn finite_overrun(&mut self, start: bool, remaining: usize) -> Result<Option<usize>> {
        if self.finite_private() {
            return Err(self.finite_private_error(
                "private audio payload exceeds the declared PES length",
                "DVB subtitle payload exceeds the declared PES length",
            ));
        }
        if start {
            // Admit only the declared PES bytes and preserve the raw carrier.
            return Ok(Some(remaining));
        }
        self.schedule(DemuxBoundary::FiniteOverrun, true, true)?;
        Ok(None)
    }
}

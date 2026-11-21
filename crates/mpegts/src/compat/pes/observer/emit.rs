//! Storage growth, packet scheduling and emission.
use super::{
    Arena, CompactDemuxPacket, CompactPayload, DemuxBoundary, DemuxObserver, DemuxPayloadKind,
    DemuxSpan, Emission, Growth,
};
use crate::pes::HEADER_MAX;
use crate::{AssemblyStep, SourceRange, limit};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Error, Result, SourceLocation, WorkBudget};

impl DemuxObserver {
    /// Keeps a failure that ends the observer, so that every later poll reports it.
    #[cold]
    pub(super) fn keep_fault(&mut self, error: Error) {
        if !crate::retryable(error) {
            self.fault = Some(error);
        }
    }

    /// Prepay allocation/move work across small polls, then reserve before any
    /// logical byte/extent is committed. A pressure retry retains paid work and
    /// the original mailbox, not a partially published result.
    pub(super) fn reserve(
        &mut self,
        arena: Arena,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        let (len, capacity, item_bytes) = match arena {
            Arena::Header => (
                self.header_ranges.len(),
                self.header_ranges.capacity(),
                size_of::<SourceRange>(),
            ),
            Arena::Payload => (
                self.span_count(),
                self.spans.capacity(),
                size_of::<DemuxSpan>(),
            ),
            Arena::OutputHeader => (
                self.output_header.len(),
                self.output_header.capacity(),
                size_of::<SourceRange>(),
            ),
        };
        let maximum = if arena == Arena::Payload {
            self.limits.source_spans
        } else {
            self.limits.source_spans.min(HEADER_MAX)
        };
        if len >= maximum {
            return Err(limit(len.saturating_add(1), maximum));
        }
        if capacity > len {
            return Ok(true);
        }
        if self.growth.is_none() {
            self.growth = Some(Growth::plan(arena, len, maximum, item_bytes)?);
        }
        let growth = self
            .growth
            .as_mut()
            .ok_or(Error::Invalid("native PES growth missing"))?;
        if growth.arena != arena {
            return Err(Error::Invalid("native PES growth phase"));
        }
        let paid = work.take(growth.remaining);
        growth.remaining -= paid;
        if growth.remaining != 0 {
            return Ok(false);
        }
        let target = growth.target;
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        match arena {
            Arena::Header => self.header_ranges.try_reserve_exact(target - len)?,
            Arena::Payload => self.spans.try_reserve_exact(target - self.spans.len())?,
            Arena::OutputHeader => self.output_header.try_reserve_exact(target - len)?,
        }
        self.growth = None;
        Ok(true)
    }

    pub(super) fn begin_pes(&mut self) -> Result<()> {
        let pending = self
            .pending
            .as_ref()
            .ok_or(Error::Invalid("native PES input missing"))?;
        let local_position = if let Some(context) = &self.demux_input {
            context.position(
                self.scope.entity,
                pending.carrier.source_offset,
                pending.source.len(),
                pending.carrier.prefix_bytes,
            )?
        } else {
            i64::try_from(
                pending
                    .carrier
                    .source_offset
                    .checked_add(pending.carrier.prefix_bytes as u64)
                    .ok_or(Error::Overflow)?,
            )
            .map_err(|_| Error::Overflow)?
        };
        self.position =
            self.media_position(u64::try_from(local_position).map_err(|_| Error::Overflow)?)?;
        self.origin = Some(SourceLocation {
            source: self.scope.entity,
            offset: pending.carrier.source_offset,
        });
        self.active = true;
        self.raw_header = [0; HEADER_MAX];
        self.header_used = 0;
        self.header_target = 6;
        self.header = None;
        self.declared_total = None;
        self.body_limit = None;
        self.time_header_applies = true;
        self.corrupt = false;
        self.header_ranges.clear();
        self.spans.clear();
        self.reset_payload_extent();
        self.output_header.clear();
        Ok(())
    }

    pub(super) fn schedule(
        &mut self,
        boundary: DemuxBoundary,
        continuation: bool,
        forced_corrupt: bool,
    ) -> Result<()> {
        let header = self.header.as_ref().ok_or(Error::Unsupported(
            "native interrupted PES header unobserved",
        ))?;
        if self.finite_private()
            && (boundary != DemuxBoundary::FiniteLength
                || continuation
                || forced_corrupt
                || self.corrupt
                || self.body_limit != Some(self.payload_len()))
        {
            return Err(self.finite_private_error(
                "native private audio incomplete/recovery boundary unobserved",
                "native DVB incomplete/recovery boundary unobserved",
            ));
        }
        if self.payload_len() == 0 {
            return Err(Error::Unsupported("native empty PES emission unobserved"));
        }
        let incomplete = self
            .body_limit
            .is_some_and(|expected| expected != self.payload_len());
        let kind = if !self.time_header_applies {
            DemuxPayloadKind::HeaderlessContinuation
        } else if header.declared_packet_length == 0 {
            DemuxPayloadKind::Unbounded
        } else if incomplete {
            DemuxPayloadKind::DeclaredIncomplete
        } else {
            DemuxPayloadKind::DeclaredComplete
        };
        self.emission = Some(Emission {
            boundary,
            kind,
            flags: if self.corrupt || forced_corrupt || incomplete {
                2
            } else {
                0
            },
            continue_without_header: continuation,
        });
        Ok(())
    }

    pub(super) fn poll_emit(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<AssemblyStep<CompactDemuxPacket>> {
        while self.output_header.len() < self.header_ranges.len() {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if !self.reserve(Arena::OutputHeader, work, cancel)? || !work.try_charge(1) {
                return Ok(AssemblyStep::Yield);
            }
            let range = self.header_ranges.as_slice()[self.output_header.len()];
            self.output_header
                .try_push(range)
                .map_err(|error| error.error)?;
        }
        if !work.try_charge(1) {
            return Ok(AssemblyStep::Yield);
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let emission = self
            .emission
            .ok_or(Error::Invalid("native PES emission missing"))?;
        let origin = self
            .origin
            .ok_or(Error::Invalid("native PES origin missing"))?;
        let header = if emission.continue_without_header {
            self.header.clone()
        } else {
            self.header.take()
        }
        .ok_or(Error::Invalid("native PES causal header missing"))?;
        let span_count = self.span_count();
        let packet = CompactDemuxPacket {
            profile: self.profile,
            program: self.scope.program,
            pid: self.scope.pid,
            source_origin: origin,
            source_epochs: self.scope.epochs,
            position: self.position,
            flags: emission.flags,
            duration90_raw: 0,
            stream_id: header.stream_id,
            boundary: emission.boundary,
            payload_kind: emission.kind,
            source_header: header,
            header_source_ranges: self.output_header.take(),
            time_header_applies: self.time_header_applies,
            payload_len: self.payload_len(),
            payload: CompactPayload::new(self.spans.take(), span_count, self.groups_payload_runs()),
        };
        self.reset_payload_extent();
        self.corrupt = false;
        self.emission = None;
        if emission.continue_without_header {
            self.active = true;
            self.time_header_applies = false;
            self.body_limit = None;
        } else {
            self.active = false;
        }
        Ok(AssemblyStep::Output(packet))
    }
}

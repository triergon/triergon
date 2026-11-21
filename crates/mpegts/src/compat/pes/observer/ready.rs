//! Continuations appended without allocation: single carriers and payload runs.
use super::{
    Arena, DEFAULT_PACKET_CAP, DemuxObserver, DemuxSpan, Growth, ListGrowth, ReadyContinuation,
    SourceContext,
};
use crate::transport::carrier::{
    PLAIN_PAYLOAD_BYTES, PlainContinuation, TS_PAYLOAD_START, TS188_BYTES,
};
use crate::{SourceRange, TransportPacket};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Bytes, Error, Result, SourceId, SourceLocation, WorkBudget};

impl DemuxObserver {
    fn body_fits(&self, counter: u8, next: usize) -> bool {
        self.active
            && self.header.is_some()
            && self.body_limit.is_none_or(|limit| next <= limit)
            && next < DEFAULT_PACKET_CAP
            && self.header_used + next <= self.limits.pes_bytes
            && self.previous_counter.map(|value| (value + 1) & 15) == Some(counter)
    }

    fn ready_body(&self, counter: u8, next: usize) -> bool {
        self.span_count() < self.spans.capacity().min(self.limits.source_spans)
            && self.body_fits(counter, next)
    }

    /// Append an ordinary TS188 continuation when it fits the existing arena.
    /// Unhandled leaves the carrier, observer and budget ready for normal polling.
    pub fn append_ready_continuation(
        &mut self,
        packet: &mut TransportPacket,
        context: SourceContext,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ReadyContinuation> {
        let Some(next) = self.ready_continuation(packet, context, None, work, cancel)? else {
            return Ok(ReadyContinuation::Unhandled);
        };
        let bytes = packet
            .payload
            .take()
            .ok_or(Error::Invalid("native PES payload missing"))?;
        self.append_carrier_payload(packet, context, bytes, next)
    }

    /// Append from a carrier whose full transport event is still needed.
    /// The payload keeps its original charge and the event remains intact.
    pub fn append_ready_borrowed_continuation(
        &mut self,
        packet: &TransportPacket,
        context: SourceContext,
        input: &triergon_core::InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ReadyContinuation> {
        let Some(next) = self.ready_continuation(packet, context, Some(input), work, cancel)?
        else {
            return Ok(ReadyContinuation::Unhandled);
        };
        let bytes = packet
            .payload
            .as_ref()
            .ok_or(Error::Invalid("native PES payload missing"))?
            .clone();
        self.append_carrier_payload(packet, context, bytes, next)
    }

    fn ready_continuation(
        &self,
        packet: &TransportPacket,
        context: SourceContext,
        input: Option<&triergon_core::InputCursor>,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<usize>> {
        let Some(payload) = packet.payload.as_ref() else {
            return Ok(None);
        };
        let count = payload.len();
        let next = self
            .payload_len()
            .checked_add(count)
            .ok_or(Error::Overflow)?;
        if packet.payload_unit_start
            || packet.discontinuity_indicator()
            || (packet.prefix.is_some() && (input.is_none() || self.demux_input.is_none()))
            || count == 0
            || !self.ready_body(packet.continuity_counter, next)
            || self
                .input_error(packet, context, false, input, false)
                .is_some()
            || cancel.load(Ordering::Relaxed)
            || !work.try_charge(2)
        {
            return Ok(None);
        }
        Ok(Some(next))
    }

    fn append_carrier_payload(
        &mut self,
        packet: &TransportPacket,
        context: SourceContext,
        bytes: Bytes,
        next: usize,
    ) -> Result<ReadyContinuation> {
        let count = bytes.len();
        self.append_ready_span(
            DemuxSpan {
                bytes,
                source: SourceRange {
                    entity: self.scope.entity,
                    offset: packet.payload_source_offset,
                    bytes: count,
                },
                transport: SourceRange {
                    entity: self.scope.entity,
                    offset: packet.source_offset,
                    bytes: packet.source.len(),
                },
                context,
            },
            packet.continuity_counter,
            next,
        )
    }

    fn accepts_plain_source(
        &self,
        source: &Bytes,
        pid: u16,
        entity: Option<SourceId>,
        offset: u64,
        context: SourceContext,
    ) -> bool {
        self.validate_input(None).is_ok()
            && !self.finite_private()
            && source.belongs_to(&self.budget)
            && self.pending.is_none()
            && self.emission.is_none()
            && !self.ended
            && self.fault.is_none()
            && pid == self.scope.pid
            && entity == Some(self.scope.entity)
            && context.epochs == self.scope.epochs
            && self.follows_input(context, offset)
    }

    pub(crate) fn append_plain_continuation(
        &mut self,
        packet: &mut PlainContinuation,
        context: SourceContext,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ReadyContinuation> {
        let Some(source) = packet.source.as_ref() else {
            return Ok(ReadyContinuation::Unhandled);
        };
        let start = packet.payload_start();
        let count = source.len() - start;
        let counter = packet.counter();
        let next = self
            .payload_len()
            .checked_add(count)
            .ok_or(Error::Overflow)?;
        // Framing proved the layout, clear transport and nonempty payload.
        // The observer still checks its own binding, order and arena capacity.
        if !self.ready_body(counter, next)
            || !self.accepts_plain_source(
                source,
                packet.pid(),
                packet.entity,
                packet.offset,
                context,
            )
            || packet.offset.checked_add(source.len() as u64).is_none()
            || cancel.load(Ordering::Relaxed)
            || !work.try_charge(2)
        {
            return Ok(ReadyContinuation::Unhandled);
        }
        let width = source.len();
        // Move the charged storage directly into the payload view.
        let bytes = packet
            .source
            .take()
            .ok_or(Error::Invalid("missing continuation source"))?
            .into_slice(start..width)?;
        self.append_ready_span(
            DemuxSpan {
                bytes,
                source: SourceRange {
                    entity: self.scope.entity,
                    offset: packet.offset + start as u64,
                    bytes: count,
                },
                transport: SourceRange {
                    entity: self.scope.entity,
                    offset: packet.offset,
                    bytes: width,
                },
                context,
            },
            counter,
            next,
        )
    }

    /// A run stops before a completion. If the arena is full, plan the same
    /// growth the first continuation would require in the ordinary observer.
    pub(crate) fn payload_run_capacity(
        &self,
        source: &Bytes,
        pid: u16,
        counter: u8,
        start: SourceLocation,
        context: SourceContext,
    ) -> (usize, Option<Growth>) {
        if !self.body_fits(counter, self.payload_len() + PLAIN_PAYLOAD_BYTES)
            || !self.accepts_plain_source(source, pid, Some(start.source), start.offset, context)
        {
            return (0, None);
        }
        let limit = self
            .body_limit
            .map_or(DEFAULT_PACKET_CAP - 1, |end| {
                end.saturating_sub(1).min(DEFAULT_PACKET_CAP - 1)
            })
            .min(self.limits.pes_bytes - self.header_used);
        let len = self.span_count();
        let slots = self.spans.capacity().min(self.limits.source_spans) - len;
        let growth = if slots == 0 && len < self.limits.source_spans && self.growth.is_none() {
            Growth::plan(
                Arena::Payload,
                len,
                self.limits.source_spans,
                size_of::<DemuxSpan>(),
            )
            .ok()
        } else {
            None
        };
        let slots = growth.as_ref().map_or(slots, |plan| plan.target - len);
        (
            (limit.saturating_sub(self.payload_len()) / PLAIN_PAYLOAD_BYTES).min(slots),
            growth,
        )
    }

    /// Keep the old arena intact until the cursor commits the run.
    pub(crate) fn prepare_payload_run_growth(
        &self,
        growth: &Growth,
    ) -> Option<ListGrowth<DemuxSpan>> {
        self.spans.prepare_growth(growth.target)
    }

    /// Commit preflighted carriers from one shared window. The arena already
    /// has room for every span, including any spans moved from a full arena.
    pub(crate) fn append_payload_run(
        &mut self,
        bytes: Bytes,
        start: SourceLocation,
        context: SourceContext,
        first_counter: u8,
        growth: Option<ListGrowth<DemuxSpan>>,
        mut record_last: impl FnMut(&[u8]),
    ) -> Result<()> {
        if let Some(growth) = growth {
            self.spans.commit_growth(growth)?;
        }
        let count = bytes.len() / TS188_BYTES;
        debug_assert!(count >= 2 && bytes.len().is_multiple_of(TS188_BYTES));
        debug_assert!(self.span_count() + count <= self.spans.capacity());
        if self.groups_payload_runs() {
            let at = (count - 1) * TS188_BYTES;
            record_last(&bytes.as_ref()[at..at + TS188_BYTES]);
            self.spans
                .try_push(DemuxSpan {
                    bytes,
                    source: SourceRange {
                        entity: start.source,
                        offset: start.offset + TS_PAYLOAD_START as u64,
                        bytes: count * PLAIN_PAYLOAD_BYTES,
                    },
                    transport: SourceRange {
                        entity: start.source,
                        offset: start.offset,
                        bytes: count * TS188_BYTES,
                    },
                    context,
                })
                .map_err(|error| error.error)?;
        } else {
            // Clone each preceding view and move the original into the final span.
            let mut views = std::iter::repeat_n(bytes, count);
            self.spans.try_extend_with(count, |index| {
                let at = index * TS188_BYTES;
                let offset = start.offset + at as u64;
                let bytes = views.next().expect("one view per admitted carrier");
                if index + 1 == count {
                    // Capacity is reserved before this closure runs. Record the
                    // complete final carrier before narrowing its original view.
                    record_last(&bytes.as_ref()[at..at + TS188_BYTES]);
                }
                DemuxSpan {
                    bytes: bytes
                        .into_slice(at + TS_PAYLOAD_START..at + TS188_BYTES)
                        .expect("payload range inside complete carrier"),
                    source: SourceRange {
                        entity: start.source,
                        offset: offset + TS_PAYLOAD_START as u64,
                        bytes: PLAIN_PAYLOAD_BYTES,
                    },
                    transport: SourceRange {
                        entity: start.source,
                        offset,
                        bytes: TS188_BYTES,
                    },
                    context: SourceContext {
                        source_event_ordinal: context.source_event_ordinal + 2 * index as u64,
                        ..context
                    },
                }
            })?;
        }
        self.add_spans(count);
        let last = (count - 1) as u64;
        self.record_ready_body(
            (
                context.source_event_ordinal + 2 * last,
                start.offset + last * TS188_BYTES as u64,
            ),
            first_counter.wrapping_add(last as u8) & 15,
            self.payload_len() + count * PLAIN_PAYLOAD_BYTES,
        );
        Ok(())
    }

    fn append_ready_span(
        &mut self,
        span: DemuxSpan,
        counter: u8,
        next: usize,
    ) -> Result<ReadyContinuation> {
        let position = (span.context.source_event_ordinal, span.transport.offset);
        self.spans.try_push(span).map_err(|error| error.error)?;
        self.add_spans(1);
        self.record_ready_body(position, counter, next);
        Ok(if self.body_limit == Some(next) {
            ReadyContinuation::Complete
        } else {
            ReadyContinuation::Buffered
        })
    }

    fn record_ready_body(&mut self, position: (u64, u64), counter: u8, next: usize) {
        self.last_input = Some(position);
        self.entity_first = false;
        self.previous_counter = Some(counter);
        self.set_payload_len(next);
    }
}

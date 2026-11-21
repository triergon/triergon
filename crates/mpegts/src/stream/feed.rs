//! Delivery of validated PES continuations to a feed.
use super::{Route, TransportStream};
use crate::SourceEpochs;
use crate::compat::pes::{DemuxObserver, ReadyContinuation, SourceContext};
use crate::pes::PesAssembler;
use crate::transport::carrier::{PLAIN_PAYLOAD_BYTES, TS188_BYTES, payload_only_continuation};
use crate::transport::framer::FramedCarrier;
use crate::transport::{Continuity, TransportFramer};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Error, InputCursor, Result, SourceLocation, WorkBudget};

/// Disposition of a validated PES body carrier offered directly to a feed.
#[derive(Debug)]
pub enum FeedContinuation {
    Emit,
    Buffered,
    Yield,
}

/// A validated continuation offered to a feed's native observer.
#[derive(Debug)]
pub struct FeedCarrier<'a>(pub(super) &'a mut FramedCarrier);

impl FeedCarrier<'_> {
    pub fn pid(&self) -> u16 {
        self.0.pid()
    }

    /// Transfer the payload when the observer can append it without allocation.
    /// An unhandled carrier remains available for ordinary delivery.
    pub fn append_ready_continuation(
        self,
        observer: &mut DemuxObserver,
        context: SourceContext,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ReadyContinuation> {
        match self.0 {
            FramedCarrier::Packet(packet) => {
                observer.append_ready_continuation(packet, context, work, cancel)
            }
            FramedCarrier::Plain(packet) => {
                observer.append_plain_continuation(packet, context, work, cancel)
            }
        }
    }
}

/// An ordinary carrier or a shared-window prefix offered to a progressive feed.
#[derive(Debug)]
pub enum FeedItem<'a> {
    Carrier(FeedCarrier<'a>),
    PayloadRun(FeedPayloadRun<'a>),
}

// Per carrier: input, header inspection, source PES, native append and event.
pub(super) const PAYLOAD_RUN_WORK: u64 =
    TS188_BYTES as u64 + 2 + (PLAIN_PAYLOAD_BYTES as u64 + 1) + 2 + 1;

fn matches_payload_header(ts: &[u8], pid: u16, counter: u8) -> bool {
    let expected = 0x4700_0010 | (u32::from(pid) << 8) | u32::from(counter);
    // Transport priority is the only bit ignored by the continuation check.
    ts.first_chunk::<4>()
        .is_some_and(|header| u32::from_be_bytes(*header) & !0x0020_0000 == expected)
}

/// A same-PID continuation prefix that may be appended to a native observer.
#[derive(Debug)]
pub struct FeedPayloadRun<'a> {
    input: &'a mut InputCursor,
    framer: &'a mut TransportFramer,
    continuity: &'a mut Continuity,
    assembler: &'a mut PesAssembler,
    omissions: &'a mut u64,
    cancel: &'a AtomicBool,
    epochs: SourceEpochs,
    pid: u16,
    counter: u8,
    maximum: usize,
}

impl FeedPayloadRun<'_> {
    pub fn pid(&self) -> u16 {
        self.pid
    }

    /// Accept a run of at least two carriers, up to the caller's event limit.
    /// False leaves input, work, observer and source state ready for the
    /// ordinary carrier path.
    pub fn append_ready(
        self,
        observer: &mut DemuxObserver,
        context: SourceContext,
        maximum: usize,
        work: &mut WorkBudget,
    ) -> Result<bool> {
        if context.epochs != self.epochs || context.byte_run_epoch.is_some() {
            return Ok(false);
        }
        let Some(carrier) = self.input.buffered_owner() else {
            return Ok(false);
        };
        let start = SourceLocation {
            source: self.input.source(),
            offset: self.input.position(),
        };
        let (slots, growth) =
            observer.payload_run_capacity(carrier, self.pid, self.counter, start, context);
        let growth_work = growth.as_ref().map_or(0, |plan| plan.remaining);
        let work_limit = work
            .remaining()
            .saturating_sub(1)
            .saturating_sub(growth_work)
            / PAYLOAD_RUN_WORK;
        let maximum = maximum
            .min(self.maximum)
            .min(usize::try_from(work_limit).unwrap_or(usize::MAX))
            .min(slots);
        if maximum < 2 {
            return Ok(false);
        }

        // The first two headers were checked before routing the run. Finish
        // the probe inside this window, stopping before the first exception.
        let mut carriers = 2;
        for ts in self
            .input
            .buffered()
            .chunks_exact(TS188_BYTES)
            .take(maximum)
            .skip(2)
        {
            if self.cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let expected = self.counter.wrapping_add(carriers as u8) & 15;
            if !matches_payload_header(ts, self.pid, expected) {
                break;
            }
            carriers += 1;
        }
        let events = 2 * carriers as u64;
        let Some(omissions) = self.omissions.checked_add(events) else {
            return Ok(false);
        };
        if context.source_event_ordinal.checked_add(events).is_none() {
            return Ok(false);
        }
        if self.cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let replacement = if let Some(growth) = growth {
            let Some(replacement) = observer.prepare_payload_run_growth(&growth) else {
                return Ok(false);
            };
            Some(replacement)
        } else {
            None
        };

        // The cursor charges the input bytes. The remaining checks and span
        // writes have already been bounded and need no allocation.
        let bytes = self
            .framer
            .read_payload_run(self.input, carriers, work, self.cancel)?;
        let charged = work
            .try_charge(growth_work + (PAYLOAD_RUN_WORK - TS188_BYTES as u64) * carriers as u64);
        debug_assert!(charged);
        observer.append_payload_run(bytes, start, context, self.counter, replacement, |last| {
            self.continuity.record_payload_run(self.counter, last);
        })?;
        self.assembler
            .record_body_carriers(carriers, carriers * PLAIN_PAYLOAD_BYTES);
        *self.omissions = omissions;
        Ok(true)
    }
}

impl TransportStream {
    pub(super) fn validate_feed_continuation(
        &mut self,
        carrier: &FramedCarrier,
        work: &mut WorkBudget,
    ) -> Result<bool> {
        let pid = carrier.pid();
        if self.feed_omissions.is_none() || (self.pcr_pid == Some(pid) && self.clock_pending) {
            return Ok(false);
        }
        let Route::Pes(index) = self.route else {
            return Ok(false);
        };
        let (counter, payload_len) = match carrier {
            FramedCarrier::Packet(packet) => {
                if packet.payload_unit_start
                    || packet.discontinuity_indicator()
                    || packet.transport_error
                    || packet.scrambling != 0
                    || (self.pcr_pid == Some(pid)
                        && packet.adaptation.as_ref().is_some_and(|a| a.pcr.is_some()))
                {
                    return Ok(false);
                }
                let Some(payload) = packet.payload.as_ref() else {
                    return Ok(false);
                };
                (packet.continuity_counter, payload.len())
            }
            FramedCarrier::Plain(packet) => (packet.counter(), 188 - packet.payload_start()),
        };
        let route = &mut self.routes.as_mut_slice()[index];
        let continuity = route
            .continuity
            .as_mut()
            .ok_or(Error::Invalid("PES continuity missing"))?;
        if payload_len == 0 || !continuity.expects_payload_counter(counter) {
            return Ok(false);
        }
        let assembler = route
            .assembler
            .as_mut()
            .ok_or(Error::Invalid("PES route missing"))?;
        let Some(completed) = assembler.validate_body_carrier(payload_len, work)? else {
            return Ok(false);
        };
        match carrier {
            FramedCarrier::Packet(packet) => continuity.record_continuation(packet),
            FramedCarrier::Plain(packet) => {
                let source = packet
                    .source
                    .as_ref()
                    .ok_or(Error::Invalid("missing continuation source"))?;
                continuity.record_plain_continuation(source.as_ref(), counter);
            }
        }
        if self.pcr_pid == Some(pid) {
            self.clock_signal = false;
        }
        // Transport is observed first in the full trace. Count continuity
        // only when the consumer asks for the next source event.
        self.deferred_feed_events = 1 + u64::from(completed);
        Ok(true)
    }

    pub(super) fn try_feed_payload_run(
        &mut self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        consume: &mut impl FnMut(
            FeedItem<'_>,
            u64,
            SourceEpochs,
            &mut WorkBudget,
        ) -> Result<FeedContinuation>,
    ) -> Result<bool> {
        if work.remaining() <= 2 * PAYLOAD_RUN_WORK
            || input.buffered().len() < 2 * TS188_BYTES
            || !self.framer.retained_payload_window(input)
        {
            return Ok(false);
        }
        let buffered = input.buffered();
        let Some((pid, counter)) = payload_only_continuation(&buffered[..TS188_BYTES]) else {
            return Ok(false);
        };
        if !matches_payload_header(
            &buffered[TS188_BYTES..2 * TS188_BYTES],
            pid,
            (counter + 1) & 15,
        ) || (self.pcr_pid == Some(pid) && self.clock_pending)
        {
            return Ok(false);
        }
        let Route::Pes(index) = self.route_for_carrier(pid) else {
            return Ok(false);
        };
        let epochs = self.source_epochs();
        let route = &mut self.routes.as_mut_slice()[index];
        let (Some(continuity), Some(assembler), Some(omissions)) = (
            route.continuity.as_mut(),
            route.assembler.as_mut(),
            self.feed_omissions.as_mut(),
        ) else {
            return Ok(false);
        };
        let maximum = assembler.body_run_capacity(PLAIN_PAYLOAD_BYTES);
        if maximum < 2 || !continuity.expects_payload_counter(counter) {
            return Ok(false);
        }
        let before = *omissions;
        let result = consume(
            FeedItem::PayloadRun(FeedPayloadRun {
                input,
                framer: &mut self.framer,
                continuity,
                assembler,
                omissions,
                cancel,
                epochs,
                pid,
                counter,
                maximum,
            }),
            before,
            epochs,
            work,
        );
        let accepted = *omissions != before;
        if accepted {
            self.entity_end = None;
            self.route = Route::Pes(index);
            if self.pcr_pid == Some(pid) {
                self.clock_signal = false;
            }
        }
        result?;
        Ok(accepted)
    }
}

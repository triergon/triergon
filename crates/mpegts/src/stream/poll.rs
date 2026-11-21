//! The entry points and the carrier loop.
use super::event::{CarrierObservation, CompactTraceEvent, FaultKind, TraceEvent, TraceFault};
use super::feed::{FeedCarrier, FeedContinuation, FeedItem};
use super::held::Counted;
use super::tables::PsiWork;
use super::{Held, Route, Stage, TransportStream};
use crate::SourceEpochs;
use crate::pes::Pes;
use crate::transport::carrier::{PlainContinuation, TransportObservation};
use crate::transport::framer::FramedCarrier;
use crate::transport::{ContinuityOutcome, TransportFramer, TransportPacket};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Error, InputCursor, Result, SourceLocation, Step, WorkBudget};

// Preparing, beginning and publishing one source event.
const OBSERVED_EVENT_WORK: u64 = 3;

enum ObservedContinuation {
    Fallback,
    Retained,
    Completed(Pes),
}

// Select the returned event type before entering the shared parser loop. Ordinary
// feeds never construct a compact event just to unpack its source event again.
trait PollOutput: Sized {
    const COMPACT: bool = false;

    fn event(event: TraceEvent) -> Self;

    fn null_run(
        _framer: &mut TransportFramer,
        _input: &mut InputCursor,
        _work: &mut WorkBudget,
        _cancel: &AtomicBool,
    ) -> Result<Option<Self>> {
        Ok(None)
    }
}

impl PollOutput for TraceEvent {
    fn event(event: Self) -> Self {
        event
    }
}

impl PollOutput for CompactTraceEvent {
    const COMPACT: bool = true;

    fn event(event: TraceEvent) -> Self {
        Self::Event(event)
    }

    fn null_run(
        framer: &mut TransportFramer,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<Self>> {
        Ok(framer.try_null_run(input, work, cancel)?.map(Self::NullRun))
    }
}

impl TransportStream {
    #[expect(
        clippy::expect_used,
        reason = "an error return here measured with the span refusal: +0.24% (seek split-PES +0.72%)"
    )]
    fn retain_observed_continuation(
        &mut self,
        carrier: &mut PlainContinuation,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        observe: &mut impl FnMut(CarrierObservation<'_>),
    ) -> Result<ObservedContinuation> {
        let Route::Pes(index) = self.route else {
            return Ok(ObservedContinuation::Fallback);
        };
        let pid = carrier.pid();
        let clock = self.pcr_pid == Some(pid);
        if (clock && self.clock_pending) || self.collector.has_pending() {
            return Ok(ObservedContinuation::Fallback);
        }
        let route = &mut self.routes.as_mut_slice()[index];
        let continuity = route
            .continuity
            .as_mut()
            .ok_or(Error::Invalid("PES continuity missing"))?;
        if !continuity.expects_payload_counter(carrier.counter()) {
            return Ok(ObservedContinuation::Fallback);
        }
        let assembler = route
            .assembler
            .as_mut()
            .ok_or(Error::Invalid("PES route missing"))?;
        let Some(bytes) = carrier.source.as_ref() else {
            return Ok(ObservedContinuation::Fallback);
        };
        let start = carrier.payload_start();
        let location = SourceLocation {
            source: carrier.entity.ok_or(Error::SourceMismatch)?,
            offset: carrier.offset + start as u64,
        };
        let grant = work.remaining().saturating_sub(2 * OBSERVED_EVENT_WORK + 1);
        if !assembler.can_retain_body_carrier(bytes, start, location.offset, grant)
            || cancel.load(Ordering::Relaxed)
        {
            return Ok(ObservedContinuation::Fallback);
        }

        work.take(OBSERVED_EVENT_WORK);
        observe(CarrierObservation::Transport(
            TransportObservation::continuation(carrier, bytes),
        ));
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        work.take(1);
        continuity.record_plain_continuation(bytes.as_ref(), carrier.counter());
        work.take(OBSERVED_EVENT_WORK);
        observe(CarrierObservation::Continuity {
            pid,
            outcome: ContinuityOutcome::Continuous,
        });
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if clock {
            self.clock_signal = false;
        }
        let bytes = carrier.source.take().expect("observed carrier source");
        let len = bytes.len();
        assembler.append_body_carrier(bytes.into_slice(start..len)?, location, work);
        Ok(assembler.take_complete_body()?.map_or(
            ObservedContinuation::Retained,
            ObservedContinuation::Completed,
        ))
    }

    /// Reports a carrier the framer rejected and discards every partial table
    /// and PES, which the lost bytes may belong to.
    #[cold]
    fn framing_fault(&mut self, input: &InputCursor, error: Error) -> TraceEvent {
        let location = SourceLocation {
            source: input.source(),
            offset: self
                .framer
                .pending_or_last_packet_offset()
                .unwrap_or_else(|| input.position().saturating_sub(self.format.bytes() as u64)),
        };
        let rejected = self.framer.take_rejection();
        self.framer.discard();
        self.pat.assembler.discard();
        if let Some(pmt) = self.pmt.as_mut() {
            pmt.assembler.discard();
        }
        for route in self.routes.as_mut_slice() {
            if let Some(assembler) = route.assembler.as_mut() {
                assembler.discard(route.epochs);
            }
        }
        TraceEvent::Diagnostic(TraceFault {
            kind: FaultKind::Transport,
            pid: None,
            location,
            error,
            rejected,
        })
    }

    /// Advances the stream within `work` and returns at most one event, in source order.
    /// `NeedInput` asks for more bytes, `Yield` means `work` is spent, and `End` reports
    /// the end of the input entity; call
    /// [`end_of_source`](Self::end_of_source) there when it is the logical end of media.
    ///
    /// Source faults are [`TraceEvent::Diagnostic`] events, not errors. Fails with
    /// `Cancelled` when `cancel` is set, with `Invalid` after
    /// [`TraceEvent::Finished`], and with the budget's and the input cursor's errors.
    /// A stream built as a feed omits some events and counts them; see
    /// [`take_feed_omissions`](Self::take_feed_omissions).
    pub fn poll(
        &mut self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<TraceEvent>> {
        // Select the full trace before entering the per-carrier loop.
        if self.feed_omissions.is_none() {
            return self.poll_inner::<TraceEvent, false, false, false, false>(
                input,
                work,
                cancel,
                |_, _, _, _| Ok(FeedContinuation::Emit),
                |_| (),
            );
        }
        self.poll_feed(input, work, cancel, &mut |_, _, _, _| {
            Ok(FeedContinuation::Emit)
        })
    }

    /// Offer stable PES continuations to a feed before publishing a trace event.
    /// The callback receives the number of preceding events omitted in this poll
    /// window and the current source epochs. Buffered consumes the carrier,
    /// and Yield consumes it and returns control to the feed. Both advance
    /// the omitted count. Emit and errors must leave the carrier unchanged.
    /// After an error, the carrier is retained for ordinary transport delivery.
    pub fn poll_feed(
        &mut self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        consume: &mut impl FnMut(
            &mut TransportPacket,
            u64,
            SourceEpochs,
            &mut WorkBudget,
        ) -> Result<FeedContinuation>,
    ) -> Result<Step<TraceEvent>> {
        self.poll_feed_inner::<false, false>(input, work, cancel, |item, omitted, epochs, work| {
            let FeedItem::Carrier(carrier) = item else {
                unreachable!("literal carrier delivery");
            };
            match carrier.0 {
                FramedCarrier::Packet(packet) => consume(packet, omitted, epochs, work),
                FramedCarrier::Plain(_) => unreachable!("literal carrier delivery"),
            }
        })
    }

    /// Like poll_feed, with compact carriers for ordinary progressive TS188
    /// continuations. Emit and errors retain the full transport event.
    pub fn poll_feed_carriers(
        &mut self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        consume: &mut impl FnMut(
            FeedCarrier<'_>,
            u64,
            SourceEpochs,
            &mut WorkBudget,
        ) -> Result<FeedContinuation>,
    ) -> Result<Step<TraceEvent>> {
        let mut single = |item: FeedItem<'_>, omitted, epochs, work: &mut WorkBudget| {
            let FeedItem::Carrier(carrier) = item else {
                unreachable!("single carrier delivery");
            };
            consume(carrier, omitted, epochs, work)
        };
        if self.feed_validated_carriers {
            self.poll_feed_inner::<true, false>(input, work, cancel, &mut single)
        } else {
            self.poll_feed_inner::<false, false>(input, work, cancel, &mut single)
        }
    }

    /// Like poll_feed_carriers, with optional shared-window runs for progressive
    /// feeds. A run is accepted by FeedPayloadRun::append_ready. The returned
    /// FeedContinuation applies to ordinary carriers.
    pub fn poll_feed_with_payload_runs(
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
    ) -> Result<Step<TraceEvent>> {
        if self.feed_validated_carriers {
            self.poll_feed_inner::<true, true>(input, work, cancel, consume)
        } else {
            self.poll_feed_inner::<false, false>(input, work, cancel, consume)
        }
    }

    // A stream runs one of the two variants, so neither is inlined into the
    // other's caller: the one that runs stays contiguous.
    #[inline(never)]
    fn poll_feed_inner<const DIRECT: bool, const RUNS: bool>(
        &mut self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        consume: impl FnMut(
            FeedItem<'_>,
            u64,
            SourceEpochs,
            &mut WorkBudget,
        ) -> Result<FeedContinuation>,
    ) -> Result<Step<TraceEvent>> {
        self.poll_inner::<TraceEvent, true, DIRECT, RUNS, false>(
            input,
            work,
            cancel,
            consume,
            |_| (),
        )
    }

    pub(crate) fn poll_observing<const OBSERVING: bool>(
        &mut self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        observe: impl FnMut(CarrierObservation<'_>),
    ) -> Result<Step<TraceEvent>> {
        self.poll_inner::<TraceEvent, false, false, false, OBSERVING>(
            input,
            work,
            cancel,
            |_, _, _, _| Ok(FeedContinuation::Emit),
            observe,
        )
    }

    /// Like [`poll`](Self::poll), returning each run of null packets as one
    /// [`CompactTraceEvent::NullRun`] instead of one transport event per packet. On a
    /// feed stream, events are omitted and counted as by `poll`.
    pub fn poll_compact(
        &mut self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<CompactTraceEvent>> {
        self.poll_inner::<CompactTraceEvent, true, false, false, false>(
            input,
            work,
            cancel,
            |_, _, _, _| Ok(FeedContinuation::Emit),
            |_| (),
        )
    }

    /// The carrier a feed consumer refused, for publication as a transport event.
    #[cold]
    fn take_refused(&mut self) -> Result<TransportPacket> {
        let held = self
            .held
            .take()
            .ok_or(Error::Invalid("TS packet missing"))?;
        Ok(held.packet)
    }

    /// The discontinuity event owed after [`TransportStream::discontinuity`].
    #[cold]
    fn take_boundary(&mut self) -> TraceEvent {
        self.boundary_event = false;
        TraceEvent::Discontinuity {
            epochs: self.source_epochs(),
        }
    }

    fn poll_inner<
        T: PollOutput,
        const FEED: bool,
        const DIRECT: bool,
        const RUNS: bool,
        const OBSERVING: bool,
    >(
        &mut self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        mut consume: impl FnMut(
            FeedItem<'_>,
            u64,
            SourceEpochs,
            &mut WorkBudget,
        ) -> Result<FeedContinuation>,
        mut observe: impl FnMut(CarrierObservation<'_>),
    ) -> Result<Step<T>> {
        let ready = |event| Step::Ready(T::event(event));
        if self.finished {
            return Err(Error::Invalid("TS stream already finished"));
        }
        'events: loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if FEED && self.deferred_feed_events != 0 {
                if matches!(
                    self.held,
                    Some(Held {
                        stage: Stage::Refused,
                        ..
                    })
                ) {
                    if work.remaining() == 0 {
                        return Ok(Step::Yield);
                    }
                    return Ok(ready(TraceEvent::Transport(self.take_refused()?)));
                }
                self.omit_feed_events(self.deferred_feed_events)?;
                self.deferred_feed_events = 0;
            }
            if self.boundary_event {
                return Ok(ready(self.take_boundary()));
            }
            if !matches!(self.psi, PsiWork::Idle) || self.collector.has_pending() {
                if let Some(event) = self.poll_jobs(work, cancel)? {
                    return Ok(ready(event));
                }
                if self.psi.blocks_carriers() {
                    return Ok(Step::Yield);
                }
            } else if FEED && cancel.load(Ordering::Relaxed) {
                // Feed omissions may have advanced since the loop's first check.
                return Err(Error::Cancelled);
            }
            if work.remaining() == 0 {
                return Ok(Step::Yield);
            }
            if let Some(held) = self.held.as_mut() {
                match held.stage {
                    Stage::Continuity => {
                        let Counted::Done(outcome) = self.observe(work)? else {
                            return Ok(Step::Yield);
                        };
                        if FEED && self.feed_validated_carriers {
                            let held = self
                                .held
                                .as_ref()
                                .ok_or(Error::Invalid("TS packet missing"))?;
                            let packet = &held.packet;
                            match outcome {
                                Some(ContinuityOutcome::First | ContinuityOutcome::Continuous)
                                    if matches!(self.route, Route::Pes(_))
                                        && held.stage == Stage::Clock =>
                                {
                                    // Count the hidden continuity event after the
                                    // carrier so its full-trace ordinal is retained.
                                    self.deferred_feed_events = 1;
                                    return Ok(ready(TraceEvent::Transport(packet.clone())));
                                }
                                Some(ContinuityOutcome::SignalledDiscontinuity) => {
                                    let pid = packet.pid;
                                    self.omit_feed_events(1)?;
                                    return Ok(ready(TraceEvent::Continuity {
                                        pid,
                                        outcome: ContinuityOutcome::SignalledDiscontinuity,
                                    }));
                                }
                                Some(_) => self.omit_feed_events(2)?,
                                None => self.omit_feed_events(1)?,
                            }
                            continue;
                        }
                        if let Some(outcome) = outcome {
                            if FEED && self.feed_omissions.is_some() {
                                self.omit_feed_events(1)?;
                                continue;
                            }
                            let pid = self
                                .held
                                .as_ref()
                                .ok_or(Error::Invalid("TS packet missing"))?
                                .packet
                                .pid;
                            if !matches!(
                                outcome,
                                ContinuityOutcome::First | ContinuityOutcome::Continuous
                            ) || !OBSERVING
                                || cancel.load(Ordering::Relaxed)
                                || !work.try_charge(OBSERVED_EVENT_WORK)
                            {
                                return Ok(ready(TraceEvent::Continuity { pid, outcome }));
                            }
                            observe(CarrierObservation::Continuity { pid, outcome });
                        }
                    }
                    Stage::Clock => {
                        held.stage = Stage::Payload;
                        if let Some(event) = self.clock_transition()? {
                            return Ok(ready(event));
                        }
                    }
                    Stage::Payload => self.feed::<FEED>(work)?,
                    Stage::Assemble => {
                        if let Some(event) = self.drain::<FEED>(work, cancel)? {
                            return Ok(ready(event));
                        }
                    }
                    Stage::Release => {
                        self.held = None;
                    }
                    // A refused carrier is published only by a feed poll.
                    Stage::Refused => return Err(Error::Invalid("TS packet stage")),
                }
                continue;
            }
            if self.logical_end {
                return match self.eof(work, cancel)? {
                    Step::Ready(event) => Ok(ready(event)),
                    Step::NeedInput(request) => Ok(Step::NeedInput(request)),
                    Step::Yield => Ok(Step::Yield),
                    Step::End(location) => Ok(Step::End(location)),
                };
            }
            if T::COMPACT
                && matches!(self.find_route(0x1fff), Route::Ignore)
                && let Some(event) = T::null_run(&mut self.framer, input, work, cancel)?
            {
                self.entity_end = None;
                return Ok(Step::Ready(event));
            }
            // Reading carriers advances the cursor within the same source
            // request. The feed callback never receives the cursor.
            self.framer.bind_input(input)?;
            loop {
                if RUNS && self.try_feed_payload_run(input, work, cancel, &mut consume)? {
                    if cancel.load(Ordering::Relaxed) {
                        return Err(Error::Cancelled);
                    }
                    continue;
                }
                match self
                    .framer
                    .poll_bound_carrier::<DIRECT, OBSERVING>(input, work, cancel)
                {
                    Ok(Step::Ready(mut carrier)) => {
                        self.entity_end = None;
                        self.route = self.route_for_carrier(carrier.pid());
                        if OBSERVING && let FramedCarrier::Plain(plain) = &mut carrier {
                            match self.retain_observed_continuation(
                                plain,
                                work,
                                cancel,
                                &mut observe,
                            )? {
                                ObservedContinuation::Fallback => {}
                                ObservedContinuation::Retained => continue,
                                ObservedContinuation::Completed(pes) => {
                                    return Ok(ready(TraceEvent::from_assembled_pes(pes)));
                                }
                            }
                        }
                        if FEED && self.validate_feed_continuation(&carrier, work)? {
                            match consume(
                                FeedItem::Carrier(FeedCarrier(&mut carrier)),
                                self.feed_omissions.unwrap_or(0),
                                self.source_epochs(),
                                work,
                            ) {
                                Ok(FeedContinuation::Emit) => {}
                                Ok(FeedContinuation::Buffered) => {
                                    self.omit_feed_events(1)?;
                                    if cancel.load(Ordering::Relaxed) {
                                        return Err(Error::Cancelled);
                                    }
                                    // A validated continuation never starts a table or
                                    // clock update. Resume framing at the next carrier.
                                    let deferred = std::mem::take(&mut self.deferred_feed_events);
                                    self.omit_feed_events(deferred)?;
                                    if work.remaining() == 0 {
                                        return Ok(Step::Yield);
                                    }
                                    continue;
                                }
                                Ok(FeedContinuation::Yield) => {
                                    self.omit_feed_events(1)?;
                                    return Ok(Step::Yield);
                                }
                                Err(error) => {
                                    self.held = Some(Held {
                                        packet: carrier.into_packet()?,
                                        stage: Stage::Refused,
                                    });
                                    return Err(error);
                                }
                            }
                            return Ok(ready(TraceEvent::Transport(carrier.into_packet()?)));
                        }
                        let packet = carrier.into_packet()?;
                        if FEED && self.feed_validated_carriers {
                            self.held = Some(Held {
                                packet,
                                stage: Stage::Continuity,
                            });
                            continue 'events;
                        }
                        if OBSERVING
                            && !cancel.load(Ordering::Relaxed)
                            && work.try_charge(OBSERVED_EVENT_WORK)
                        {
                            observe(CarrierObservation::Transport((&packet).into()));
                            self.held = Some(Held {
                                packet,
                                stage: Stage::Continuity,
                            });
                            continue 'events;
                        }
                        let event = TraceEvent::Transport(packet.clone());
                        self.held = Some(Held {
                            packet,
                            stage: Stage::Continuity,
                        });
                        return Ok(ready(event));
                    }
                    Ok(Step::NeedInput(request)) => return Ok(Step::NeedInput(request)),
                    Ok(Step::Yield) => return Ok(Step::Yield),
                    Ok(Step::End(location)) => {
                        self.entity_end = Some(location);
                        return Ok(Step::End(location));
                    }
                    Err(error) if matches!(error, Error::Invalid(_) | Error::Truncated) => {
                        return Ok(ready(self.framing_fault(input, error)));
                    }
                    Err(error) => return Err(error),
                }
            }
        }
    }
}

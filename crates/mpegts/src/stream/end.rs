//! Entity ends, logical EOF, discontinuities and trace identity.
use super::event::{CompactTraceEvent, FaultKind, TraceEvent};
use super::{PsiRoute, TransportStream};
use crate::AssemblyStep;
use crate::retryable;
use crate::stream::entity::{ObservedTrace, TraceOrigin};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use triergon_core::{BudgetVec, Error, InputCursor, Result, Step, WorkBudget};

static NEXT_TRACE_ISSUER: AtomicUsize = AtomicUsize::new(1);

impl TransportStream {
    /// Discard partial transport and PES state and advance both source epochs.
    /// A pending carrier, table job or deferred feed event is invalid at this boundary.
    /// Returns `Overflow` if an epoch cannot advance.
    pub fn discontinuity(&mut self) -> Result<()> {
        if self.held.is_some() || self.deferred_feed_events != 0 || self.psi.blocks_carriers() {
            return Err(Error::Invalid(
                "discontinuity while TS operation is pending",
            ));
        }
        let clock = self.clock_epoch.checked_add(1).ok_or(Error::Overflow)?;
        let config = self.config_epoch.checked_add(1).ok_or(Error::Overflow)?;
        self.clock_epoch = clock;
        self.config_epoch = config;
        self.framer.discard();
        self.pat = PsiRoute::new(0, &self.budget, self.limits)?;
        self.collector.discard();
        self.pmt = None;
        self.routes = BudgetVec::new(&self.budget);
        self.pmt_source = None;
        self.pmt_version = None;
        self.tsid = None;
        self.pcr_pid = None;
        self.clock_continuity = None;
        self.clock_pending = false;
        self.clock_signal = false;
        self.clock_samples = None;
        self.clock_quarantined = false;
        self.boundary_event = true;
        Ok(())
    }
    /// Witness the actual physical byte offset at the drained entity end.
    /// Partial PSI and PES can continue in the next entity. A mismatched or
    /// undrained cursor is rejected, and a short work grant yields `Ok(None)`.
    pub fn media_entity_end(
        &self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<crate::stream::MediaEntityEnd>> {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if !self.budget.same_pool(input.budget()) {
            return Err(Error::ForeignPool);
        }
        let location = self
            .entity_end
            .ok_or(Error::Invalid("media entity End not observed"))?;
        if self.logical_end
            || self.finished
            || self.held.is_some()
            || self.psi.blocks_carriers()
            || self.boundary_event
            || location.source != input.source()
            || location.offset != input.position()
            || !self.framer.observed_entity_end(input)
        {
            return Err(Error::Invalid("media entity End not drained"));
        }
        if !work.try_charge(1) {
            return Ok(None);
        }
        match input.read_into(&mut [0_u8; 1], work, cancel)? {
            Step::End(actual) if actual == location => Ok(Some(crate::stream::MediaEntityEnd {
                location,
                issuer: self.trace_issuer()?,
                token: input.token(),
                epochs: self.source_epochs(),
                program: self.program,
                format: self.format,
                budget: self.budget.clone(),
            })),
            _ => Err(Error::Invalid("media entity End not confirmed by core")),
        }
    }
    pub(super) fn trace_issuer(&self) -> Result<usize> {
        let issuer = self.trace_issuer.load(Ordering::Relaxed);
        if issuer != 0 {
            return Ok(issuer);
        }
        let mut next = NEXT_TRACE_ISSUER.load(Ordering::Relaxed);
        loop {
            let following = next.checked_add(1).ok_or(Error::Overflow)?;
            match NEXT_TRACE_ISSUER.compare_exchange_weak(
                next,
                following,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(current) => next = current,
            }
        }
        Ok(self
            .trace_issuer
            .compare_exchange(0, next, Ordering::Relaxed, Ordering::Relaxed)
            .map_or_else(|existing| existing, |_| next))
    }

    fn check_trace_input(&mut self, input: &InputCursor, cancel: &AtomicBool) -> Result<()> {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if !self.budget.same_pool(input.budget()) {
            return Err(Error::ForeignPool);
        }
        let generation = input.generation();
        let matches = self.trace_generation.map_or_else(
            || self.framer.accepts_observed_input(input),
            |original| original == generation,
        );
        if !matches {
            return Err(Error::Invalid(
                "trace input differs from the source operation",
            ));
        }
        self.trace_generation = Some(generation);
        Ok(())
    }

    pub(crate) fn trace_origin(&self, input: &InputCursor) -> Result<TraceOrigin> {
        if !self.budget.same_pool(input.budget()) {
            return Err(Error::ForeignPool);
        }
        Ok(TraceOrigin {
            issuer: self.trace_issuer()?,
            token: input.token(),
            source: input.source(),
            epochs: self.source_epochs(),
        })
    }

    fn bind_trace<T>(&self, step: Step<T>, input: &InputCursor) -> Result<Step<ObservedTrace<T>>> {
        Ok(match step {
            Step::Ready(event) => Step::Ready(ObservedTrace {
                event,
                origin: self.trace_origin(input)?,
            }),
            Step::NeedInput(request) => Step::NeedInput(request),
            Step::End(location) => Step::End(location),
            Step::Yield => Step::Yield,
            Step::Backpressure(never) => match never {},
        })
    }

    /// Poll one ordinary source event or progress step, attaching the issuing parser and
    /// cursor identity to each event. A foreign pool or stale cursor generation is rejected.
    /// Other failures follow [`Self::poll`].
    pub fn poll_observed(
        &mut self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<ObservedTrace<TraceEvent>>> {
        self.check_trace_input(input, cancel)?;
        let step = self.poll(input, work, cancel)?;
        self.bind_trace(step, input)
    }

    /// Poll source events with consecutive null carriers represented as runs. Each event
    /// carries the same source identity as [`Self::poll_observed`] and uses the same cursor checks.
    pub fn poll_compact_observed(
        &mut self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<ObservedTrace<CompactTraceEvent>>> {
        self.check_trace_input(input, cancel)?;
        let step = self.poll_compact(input, work, cancel)?;
        self.bind_trace(step, input)
    }

    /// Witness logical EOF for this parser and cursor after publication of `Finished`.
    /// Before that event, an undrained cursor or a foreign byte pool is rejected.
    pub fn source_finished(&self, input: &InputCursor) -> Result<ObservedTrace<()>> {
        if !self.finished || !self.framer.observed_entity_end(input) {
            return Err(Error::Invalid("source has not published Finished"));
        }
        Ok(ObservedTrace {
            event: (),
            origin: self.trace_origin(input)?,
        })
    }

    /// Start draining incomplete source assembly at the last entity’s physical end.
    /// Returns `Invalid` while the entity end is unobserved or a carrier or table job is pending.
    pub fn end_of_source(&mut self) -> Result<()> {
        // The compact end phase added 5.60% to PSI fault reads through an out-of-line constructor.
        if self.entity_end.is_none() || self.held.is_some() || self.psi.blocks_carriers() {
            return Err(Error::Invalid("logical EOF before TS entity drained"));
        }
        self.logical_end = true;
        Ok(())
    }
    /// Drains every assembler once the input has logically ended.
    #[cold]
    pub(super) fn eof(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<TraceEvent>> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if self.eof_route == 0 || self.eof_route == 1 {
                let state = if self.eof_route == 0 {
                    Some(&mut self.pat)
                } else {
                    self.pmt.as_mut()
                };
                if let Some(state) = state {
                    state.assembler.end_of_source();
                    let pid = state.pid;
                    match state.assembler.poll(work, cancel) {
                        Ok(AssemblyStep::Yield) => return Ok(Step::Yield),
                        Err(error) if retryable(error) => return Err(error),
                        Err(error) => {
                            state.assembler.discard();
                            self.eof_route += 1;
                            return Ok(Step::Ready(self.diagnostic(
                                FaultKind::Psi,
                                Some(pid),
                                error,
                            )));
                        }
                        Ok(AssemblyStep::Output(_)) => {
                            return Err(Error::Invalid("undrained PSI at logical EOF"));
                        }
                        Ok(_) => {}
                    }
                }
                self.eof_route += 1;
                continue;
            }
            let index = self.eof_route - 2;
            if index == self.routes.len() {
                self.finished = true;
                return Ok(Step::Ready(TraceEvent::Finished));
            }
            let route = &mut self.routes.as_mut_slice()[index];
            let pid = route.pid;
            let assembler = route
                .assembler
                .as_mut()
                .ok_or(Error::Invalid("PES route missing at EOF"))?;
            assembler.end_of_source();
            let step = Self::poll_pes(assembler, self.feed_omissions.is_none(), work, cancel);
            match step {
                Ok(AssemblyStep::Output(Some(pes))) => {
                    return Ok(Step::Ready(TraceEvent::from_assembled_pes(pes)));
                }
                Ok(AssemblyStep::Output(None)) => self.omit_feed_events(1)?,
                Ok(AssemblyStep::Yield) => return Ok(Step::Yield),
                Err(error) if retryable(error) => return Err(error),
                Err(error) => {
                    assembler.discard(route.epochs);
                    self.eof_route += 1;
                    return Ok(Step::Ready(self.diagnostic(
                        FaultKind::Pes,
                        Some(pid),
                        error,
                    )));
                }
                Ok(_) => self.eof_route += 1,
            }
        }
    }
}

//! The stages of the carrier in hand.
use super::event::{FaultKind, TraceEvent, TraceFault};
use super::{Route, TransportStream};
use crate::AssemblyStep;
use crate::pes::{Pes, PesAssembler};
use crate::psi::POINTER_FIELD;
use crate::psi::Section;
use crate::retryable;
use crate::transport::ContinuityOutcome;
use crate::transport::{SCRAMBLED_PAYLOAD, TRANSPORT_ERROR_INDICATOR};
use std::sync::atomic::AtomicBool;
use triergon_core::{Error, Result, SourceLocation, WorkBudget};

impl TransportStream {
    pub(super) fn observe(&mut self, work: &mut WorkBudget) -> Result<bool> {
        if !work.try_charge(1) {
            return Ok(false);
        }
        let packet = self
            .packet
            .as_ref()
            .ok_or(Error::Invalid("TS packet missing"))?;
        let outcome = match self.route {
            Route::Pat => Some(self.pat.continuity.observe(packet)?),
            Route::Pmt => Some(
                self.pmt
                    .as_mut()
                    .ok_or(Error::Invalid("PMT route missing"))?
                    .continuity
                    .observe(packet)?,
            ),
            Route::Pes(index) => Some(
                self.routes.as_mut_slice()[index]
                    .continuity
                    .as_mut()
                    .ok_or(Error::Invalid("PES continuity missing"))?
                    .observe(packet)?,
            ),
            Route::Clock => Some(
                self.clock_continuity
                    .as_mut()
                    .ok_or(Error::Invalid("PCR continuity missing"))?
                    .observe(packet)?,
            ),
            Route::Ignore => None,
        };
        let pid = packet.pid;
        let start = packet.payload_unit_start;
        let bad = if packet.transport_error {
            Some(TRANSPORT_ERROR_INDICATOR)
        } else if packet.scrambling != 0 {
            Some(SCRAMBLED_PAYLOAD)
        } else if matches!(outcome, Some(ContinuityOutcome::Lost { .. })) {
            Some(Error::Invalid("TS continuity loss"))
        } else if outcome == Some(ContinuityOutcome::ExcessDuplicate) {
            Some(Error::Invalid("TS excess duplicate"))
        } else {
            None
        };
        // The outcome describes an actual counter break, not the raw flag.
        // Continuous flagged PSI/PES can continue; PCR clock transitions are
        // evaluated separately below against the selected PCR PID and raw signal.
        let counter_break = outcome == Some(ContinuityOutcome::SignalledDiscontinuity);
        if bad.is_some() || counter_break {
            self.discard_route(self.route);
        }
        if let Some(error) = bad {
            let kind = if matches!(
                outcome,
                Some(ContinuityOutcome::Lost { .. } | ContinuityOutcome::ExcessDuplicate)
            ) {
                FaultKind::Continuity
            } else {
                FaultKind::Transport
            };
            self.fault = Some(TraceFault {
                kind,
                pid: Some(pid),
                location: self.location(),
                error,
                rejected: None,
            });
            self.phase = if kind == FaultKind::Continuity
                && start
                && outcome != Some(ContinuityOutcome::ExcessDuplicate)
            {
                2
            } else {
                7
            };
        } else if outcome == Some(ContinuityOutcome::Duplicate) {
            self.phase = 7;
        } else {
            self.phase = 2;
        }
        self.outcome = outcome;
        Ok(true)
    }

    pub(super) fn feed<const FEED: bool>(&mut self, work: &mut WorkBudget) -> Result<()> {
        let packet = self
            .packet
            .as_ref()
            .ok_or(Error::Invalid("TS packet missing"))?;
        let Some(payload) = packet.payload.as_ref() else {
            self.phase = 7;
            return Ok(());
        };
        let entity = packet.source_id.ok_or(Error::SourceMismatch)?;
        let pid = packet.pid;
        let pusi = packet.payload_unit_start;
        let offset = packet.payload_source_offset;
        if pusi
            && work.remaining() >= payload.len() as u64
            && let [0, section_and_stuffing @ ..] = payload.as_ref()
            && match self.route {
                Route::Pat => self.pat.assembler.idle(),
                Route::Pmt => self.pmt.as_ref().is_some_and(|pmt| pmt.assembler.idle()),
                _ => false,
            }
            && let Some(accepted) = self.accepted_table_source()
            && let Some(stuffing) = section_and_stuffing.strip_prefix(accepted.as_ref())
            && stuffing.iter().all(|&byte| byte == 0xff)
        {
            // Reuse immutable section bytes while recording this occurrence's
            // position. The rest of the carrier is validated stuffing.
            let length = payload.len() as u64;
            if FEED && self.feed_omissions.is_some() {
                self.omit_feed_events(1)?;
            } else {
                let start = offset.checked_add(1).ok_or(Error::Overflow)?;
                self.section = Some(Section::repeated(
                    &self.budget,
                    pid,
                    accepted,
                    entity,
                    start,
                )?);
            }
            work.take(length);
            self.phase = 7;
            return Ok(());
        }
        let payload = &mut self.packet.as_mut().expect("TS packet present").payload;
        let pushed = match self.route {
            Route::Pat => self.pat.assembler.push(
                payload.take().ok_or(Error::Invalid("TS payload missing"))?,
                pusi,
                entity,
                offset,
            ),
            Route::Pmt => self
                .pmt
                .as_mut()
                .ok_or(Error::Invalid("PMT route missing"))?
                .assembler
                .push(
                    payload.take().ok_or(Error::Invalid("TS payload missing"))?,
                    pusi,
                    entity,
                    offset,
                ),
            Route::Pes(index) => {
                let assembler = self.routes.as_mut_slice()[index]
                    .assembler
                    .as_mut()
                    .ok_or(Error::Invalid("PES route missing"))?;
                if !pusi
                    && self.feed_omissions.is_none()
                    && assembler.retain_body_carrier(
                        payload,
                        SourceLocation {
                            source: entity,
                            offset,
                        },
                        work,
                    )
                {
                    self.phase = 7;
                    return Ok(());
                }
                assembler.push(
                    payload.take().ok_or(Error::Invalid("TS payload missing"))?,
                    pusi,
                    entity,
                    offset,
                )
            }
            Route::Clock | Route::Ignore => {
                self.phase = 7;
                return Ok(());
            }
        };
        let pushed = pushed.map_err(|rejected| {
            *payload = Some(rejected.value);
            rejected.error
        });
        match pushed {
            Ok(()) => self.phase = 6,
            Err(error)
                if matches!(self.route, Route::Pat | Route::Pmt) && error == POINTER_FIELD =>
            {
                // This wire-syntax fault is rejected by push before acceptance.
                // Use the same explicit source-diagnostic recovery as poll faults;
                // invariant/ownership/resource errors still propagate unchanged.
                self.discard_route(self.route);
                self.fault = Some(TraceFault {
                    kind: FaultKind::Psi,
                    pid: Some(pid),
                    location: self.location(),
                    error,
                    rejected: None,
                });
                self.phase = 7;
            }
            Err(error) => return Err(error),
        }
        Ok(())
    }
    #[cold]
    pub(super) fn discard_faulted_route(&mut self, error: Error) -> TraceEvent {
        let pid = self.packet.as_ref().map(|packet| packet.pid);
        let kind = if matches!(self.route, Route::Pes(_)) {
            FaultKind::Pes
        } else {
            FaultKind::Psi
        };
        self.discard_route(self.route);
        self.phase = 7;
        self.diagnostic(kind, pid, error)
    }

    // Keep elementary assembly dispatch separate from PSI and empty EOF paths.
    #[inline(never)]
    pub(super) fn poll_pes(
        assembler: &mut PesAssembler,
        retain: bool,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<AssemblyStep<Option<Pes>>> {
        if retain {
            assembler.poll_source::<true>(work, cancel)
        } else {
            assembler.poll_source::<false>(work, cancel)
        }
    }

    pub(super) fn drain<const FEED: bool>(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<TraceEvent>> {
        let result = match self.route {
            Route::Pat => self
                .pat
                .assembler
                .poll(work, cancel)
                .map(|step| match step {
                    AssemblyStep::Output(section) => {
                        self.section = Some(section);
                        false
                    }
                    AssemblyStep::Yield => false,
                    _ => true,
                }),
            Route::Pmt => self
                .pmt
                .as_mut()
                .ok_or(Error::Invalid("PMT route missing"))?
                .assembler
                .poll(work, cancel)
                .map(|step| match step {
                    AssemblyStep::Output(section) => {
                        self.section = Some(section);
                        false
                    }
                    AssemblyStep::Yield => false,
                    _ => true,
                }),
            Route::Pes(index) => {
                let assembler = self.routes.as_mut_slice()[index]
                    .assembler
                    .as_mut()
                    .ok_or(Error::Invalid("PES route missing"))?;
                let step = Self::poll_pes(
                    assembler,
                    !FEED || self.feed_omissions.is_none(),
                    work,
                    cancel,
                );
                match step {
                    Ok(AssemblyStep::Output(Some(pes))) => {
                        return Ok(Some(TraceEvent::from_assembled_pes(pes)));
                    }
                    Ok(AssemblyStep::Output(None)) => {
                        self.omit_feed_events(1)?;
                        Ok(false)
                    }
                    Ok(AssemblyStep::Yield) => Ok(false),
                    Ok(_) => Ok(true),
                    Err(error) => Err(error),
                }
            }
            Route::Clock | Route::Ignore => Ok(true),
        };
        match result {
            Ok(true) => {
                self.phase = 7;
                Ok(None)
            }
            Ok(false) => Ok(None),
            Err(error) if retryable(error) => Err(error),
            Err(error) => Ok(Some(self.discard_faulted_route(error))),
        }
    }
}

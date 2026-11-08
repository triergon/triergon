//! Private, statically selected ownership around the shared receiver engine.
//! The native compatibility role owns no strict decoder or Source output.
use super::display::StoredDisplayDecoder;
use super::display_model::{PaletteSnapshot, StoredDisplaySet};
use super::display_work::DisplayWork;
use super::indexed_argb::ArgbStorage;
use super::recovery::{
    Phase, ReceiverObservations, RecoveredEvent, Refusal, StoredRecoveredPacket,
};
use super::{DisplayConfig, DisplayLimits, DisplayStatus, Envelope, PacketClock, RecoveryEof};
use std::fmt::Debug;
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result, WorkBudget};

// This module is private. Public visibility lets concrete exported aliases name
// their engine without exposing a caller-selectable feature flag or extension.
pub trait RecoveryRole: Debug {
    type Packet: Debug;
    type Eof: Copy + Debug;
    /// Whether the role also builds the displays of later ENDs in a packet,
    /// which the reference ignores.
    const BUILDS_IGNORED_DISPLAYS: bool;

    fn new(budget: &Budget, limits: DisplayLimits, config: DisplayConfig) -> Result<Self>
    where
        Self: Sized;
    fn begin_packet(&mut self, budget: &Budget);
    fn first_phase(&self) -> Phase;
    fn poll_source(
        &mut self,
        raw: &Bytes,
        clock: PacketClock,
        limits: &DisplayLimits,
        caller: &mut WorkBudget,
        spent: &mut usize,
        cancel: &AtomicBool,
    ) -> Result<bool>;
    fn poison(&mut self, error: Error);
    fn reset(&mut self, cancel: &AtomicBool) -> Result<()>;
    /// Strict source pixels for an object with this id and size, with the RLE
    /// they were validated from, when the role shares them.
    fn validated_indices(&self, id: u16, width: u16, height: u16) -> Option<(Bytes, Bytes)>;
    fn palette_candidate(&self) -> Option<&PaletteSnapshot>;
    fn reserve_event(&mut self) -> Result<()>;
    fn store_event(&mut self, event: RecoveredEvent) -> Result<()>;
    fn finish_packet(
        &mut self,
        raw: Bytes,
        clock: PacketClock,
        canvas: [u16; 2],
        observations: ReceiverObservations,
        refusal: Option<Refusal>,
    ) -> Result<Self::Packet>;
    fn finish_input(&mut self, cancel: &AtomicBool) -> Result<()>;
    fn poll_eof(
        &mut self,
        caller: &mut WorkBudget,
        spent: &mut usize,
        maximum: usize,
        cancel: &AtomicBool,
    ) -> Result<Option<Self::Eof>>;
}

#[derive(Clone, Copy, Debug)]
enum SourcePhase {
    Push,
    Poll,
    Store,
}

#[derive(Debug)]
pub struct FullRecovery<A: ArgbStorage> {
    pub(super) strict: Option<StoredDisplayDecoder<A>>,
    source_error: Option<Error>,
    pub(super) source_sets: Option<BudgetVec<StoredDisplaySet<A>>>,
    source_next: Option<StoredDisplaySet<A>>,
    events: Option<BudgetVec<RecoveredEvent>>,
    phase: SourcePhase,
    credit: usize,
}

impl<A: ArgbStorage> FullRecovery<A> {
    fn done(&mut self, phase: SourcePhase) {
        self.phase = phase;
        self.credit = 0;
    }
}

impl<A: ArgbStorage> RecoveryRole for FullRecovery<A> {
    type Packet = StoredRecoveredPacket<A>;
    type Eof = RecoveryEof;
    const BUILDS_IGNORED_DISPLAYS: bool = true;

    fn new(budget: &Budget, limits: DisplayLimits, config: DisplayConfig) -> Result<Self> {
        Ok(Self {
            strict: Some(StoredDisplayDecoder::new(budget, limits, config)?),
            source_error: None,
            source_sets: None,
            source_next: None,
            events: None,
            phase: SourcePhase::Push,
            credit: 0,
        })
    }

    fn begin_packet(&mut self, budget: &Budget) {
        self.source_sets = Some(BudgetVec::new(budget));
        self.events = Some(BudgetVec::new(budget));
        self.done(SourcePhase::Push);
    }

    fn first_phase(&self) -> Phase {
        Phase::Source
    }

    fn poll_source(
        &mut self,
        raw: &Bytes,
        clock: PacketClock,
        limits: &DisplayLimits,
        caller: &mut WorkBudget,
        spent: &mut usize,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        let mut ledger = DisplayWork {
            caller,
            spent,
            maximum: limits.max_work,
            cancel,
        };
        let work = &mut ledger;
        let maximum_sets = limits.max_segments;
        loop {
            if work.cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if work.caller.remaining() == 0 {
                return Ok(false);
            }
            match self.phase {
                SourcePhase::Push => {
                    if !work.pay(&mut self.credit, 1)? {
                        return Ok(false);
                    }
                    if self.source_error.is_some() {
                        return Ok(true);
                    }
                    self.strict
                        .as_mut()
                        .ok_or(Error::Invalid("PGS strict context absent"))?
                        .push(raw.clone(), clock, Envelope::Packet, work.cancel)
                        .map_err(|e| e.error)?;
                    self.done(SourcePhase::Poll);
                }
                SourcePhase::Poll => {
                    let cancel = work.cancel;
                    let result = work.child(|w| {
                        self.strict
                            .as_mut()
                            .ok_or(Error::Invalid("PGS strict context absent"))?
                            .poll(w, cancel)
                    });
                    match result {
                        Ok(DisplayStatus::Yield) => return Ok(false),
                        Ok(DisplayStatus::OutputReady) => {
                            self.source_next = Some(
                                self.strict
                                    .as_mut()
                                    .ok_or(Error::Invalid("PGS strict context absent"))?
                                    .take_output(cancel)?,
                            );
                            self.done(SourcePhase::Store);
                        }
                        Ok(DisplayStatus::NeedPacket) => return Ok(true),
                        Ok(_) => return Err(Error::Invalid("PGS recovery strict progress")),
                        Err(
                            error @ (Error::Invalid(_) | Error::Truncated | Error::Unsupported(_)),
                        ) => {
                            self.source_error = Some(error);
                            return Ok(true);
                        }
                        Err(error) => return Err(error),
                    }
                }
                SourcePhase::Store => {
                    let sets = self
                        .source_sets
                        .as_ref()
                        .ok_or(Error::Invalid("PGS strict output storage absent"))?;
                    let cost = if sets.len() < maximum_sets && sets.len() == sets.capacity() {
                        sets.len().checked_add(1).ok_or(Error::Overflow)?
                    } else {
                        1
                    };
                    if !work.pay(&mut self.credit, cost)? {
                        return Ok(false);
                    }
                    let sets = self
                        .source_sets
                        .as_mut()
                        .ok_or(Error::Invalid("PGS strict output storage absent"))?;
                    if sets.len() == maximum_sets {
                        return Err(Error::Limit {
                            requested: sets.len() + 1,
                            maximum: maximum_sets,
                        });
                    }
                    if sets.is_empty() {
                        sets.try_reserve_exact(1)?;
                    }
                    let source = self
                        .source_next
                        .take()
                        .ok_or(Error::Invalid("PGS strict output absent"))?;
                    if let Err(e) = sets.try_push(source) {
                        self.source_next = Some(e.value);
                        return Err(e.error);
                    }
                    self.done(SourcePhase::Poll);
                }
            }
        }
    }

    fn poison(&mut self, error: Error) {
        self.source_sets = None;
        self.source_next = None;
        self.events = None;
        // Preserve the strict identity so reset still advances its generation.
        if let Some(strict) = self.strict.as_mut() {
            strict.poison(error);
        }
    }

    fn reset(&mut self, cancel: &AtomicBool) -> Result<()> {
        self.strict
            .as_mut()
            .ok_or(Error::Invalid("PGS strict context absent"))?
            .reset(cancel)?;
        self.source_error = None;
        self.source_sets = None;
        self.source_next = None;
        self.events = None;
        self.done(SourcePhase::Push);
        Ok(())
    }

    fn validated_indices(&self, id: u16, width: u16, height: u16) -> Option<(Bytes, Bytes)> {
        if A::MATERIALIZE {
            return None;
        }
        self.strict
            .as_ref()
            .and_then(|d| d.validated_indices(id, width, height))
    }

    fn palette_candidate(&self) -> Option<&PaletteSnapshot> {
        self.source_sets
            .as_ref()
            .and_then(|sets| sets.as_slice().last())
            .and_then(|set| set.objects().first())
            .map(super::display_model::StoredDisplayObject::palette)
            .filter(|_| self.source_error.is_none())
    }

    fn reserve_event(&mut self) -> Result<()> {
        self.events
            .as_mut()
            .ok_or(Error::Invalid("PGS events absent"))?
            .try_reserve_exact(1)
    }

    fn store_event(&mut self, event: RecoveredEvent) -> Result<()> {
        self.events
            .as_mut()
            .ok_or(Error::Invalid("PGS events absent"))?
            .try_push(event)
            .map_err(|e| e.error)
    }

    fn finish_packet(
        &mut self,
        raw: Bytes,
        clock: PacketClock,
        context_canvas: [u16; 2],
        receiver_observations: ReceiverObservations,
        refusal: Option<Refusal>,
    ) -> Result<Self::Packet> {
        Ok(StoredRecoveredPacket {
            refusal,
            raw,
            clock,
            source_sets: self
                .source_sets
                .take()
                .ok_or(Error::Invalid("PGS source sets absent"))?,
            source_error: self.source_error,
            events: self
                .events
                .take()
                .ok_or(Error::Invalid("PGS events absent"))?,
            context_canvas,
            receiver_observations,
        })
    }

    fn finish_input(&mut self, cancel: &AtomicBool) -> Result<()> {
        if self.source_error.is_none() {
            match self
                .strict
                .as_mut()
                .ok_or(Error::Invalid("PGS strict context absent"))?
                .finish_input(cancel)
            {
                Ok(()) => (),
                Err(error @ (Error::Truncated | Error::Invalid(_) | Error::Unsupported(_))) => {
                    self.source_error = Some(error);
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    fn poll_eof(
        &mut self,
        caller: &mut WorkBudget,
        spent: &mut usize,
        maximum: usize,
        cancel: &AtomicBool,
    ) -> Result<Option<Self::Eof>> {
        let mut ledger = DisplayWork {
            caller,
            spent,
            maximum,
            cancel,
        };
        let work = &mut ledger;
        if self.source_error.is_some() {
            return Ok(Some(RecoveryEof {
                source_error: self.source_error,
            }));
        }
        let cancel = work.cancel;
        let result = work.child(|w| {
            self.strict
                .as_mut()
                .ok_or(Error::Invalid("PGS strict context absent"))?
                .poll(w, cancel)
        });
        match result {
            Ok(DisplayStatus::Yield) => Ok(None),
            Ok(DisplayStatus::Ended) => Ok(Some(RecoveryEof { source_error: None })),
            Err(error @ (Error::Truncated | Error::Invalid(_) | Error::Unsupported(_))) => {
                self.source_error = Some(error);
                Ok(Some(RecoveryEof {
                    source_error: Some(error),
                }))
            }
            Err(error) => Err(error),
            Ok(_) => Err(Error::Invalid("PGS source EOF progress")),
        }
    }
}

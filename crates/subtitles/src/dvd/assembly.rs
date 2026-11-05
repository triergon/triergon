//! Bounded byte assembly of one traditional DVD subpicture unit.
use super::DvdPacket;
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result, WorkBudget};

#[derive(Clone, Copy, Debug)]
pub struct SpuAssemblyLimits {
    pub max_packet: usize,
    pub max_fragments: usize,
    pub max_work: usize,
}

impl Default for SpuAssemblyLimits {
    fn default() -> Self {
        Self {
            max_packet: usize::from(u16::MAX),
            max_fragments: 1024,
            max_work: 128 * 1024,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpuAssemblyStatus {
    NeedFragment,
    Yield,
    Ready,
}

#[derive(Debug)]
pub struct AssembledSpu {
    packet: Bytes,
    trailing: Option<Bytes>,
}

impl AssembledSpu {
    pub fn packet(&self) -> &Bytes {
        &self.packet
    }

    /// Bytes after this unit retain their original owner and are not consumed
    /// as a second packet. The caller can submit them to a new/reset job.
    pub fn trailing(&self) -> Option<&Bytes> {
        self.trailing.as_ref()
    }

    pub fn into_parts(self) -> (Bytes, Option<Bytes>) {
        (self.packet, self.trailing)
    }
}

#[derive(Debug)]
pub struct SpuPushError {
    pub error: Error,
    pub payload: Bytes,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Receive,
    Reserve,
    Allocate,
    CopyFirst,
    CopyIncoming,
    Validate,
    Ready,
    Finished,
}

/// Accepts arbitrary byte fragments, including a split size word. A complete
/// input is viewed without copying; fragmented input is copied once into one
/// pre-reserved charged buffer. There is no fragment registry or uncharged Vec.
///
/// At most a first and a pending input are retained. Busy submissions return
/// the exact owner with slot Backpressure { requested: 1, available: 0 }.
/// External packet clocks and PES parsing belong to the consumer of these bytes.
#[derive(Debug)]
pub struct SpuAssembly {
    budget: Budget,
    limits: SpuAssemblyLimits,
    first: Option<Bytes>,
    incoming: Option<Bytes>,
    buffer: Option<BudgetVec<u8>>,
    trailing: Option<Bytes>,
    declared: Option<usize>,
    incoming_take: usize,
    received: usize,
    fragments: usize,
    copied: usize,
    copy_at: usize,
    work: usize,
    state: State,
    ended: bool,
    failed: Option<Error>,
}

impl SpuAssembly {
    pub fn new(budget: &Budget, limits: SpuAssemblyLimits) -> Self {
        Self {
            budget: budget.clone(),
            limits,
            first: None,
            incoming: None,
            buffer: None,
            trailing: None,
            declared: None,
            incoming_take: 0,
            received: 0,
            fragments: 0,
            copied: 0,
            copy_at: 0,
            work: 0,
            state: State::Receive,
            ended: false,
            failed: None,
        }
    }

    /// Releases private partial state. Previously returned views stay valid and
    /// charged. A caller must reset its own transport/clock generation as well.
    pub fn reset(&mut self) {
        *self = Self::new(&self.budget, self.limits);
    }

    pub fn work_spent(&self) -> usize {
        self.work
    }

    pub fn copied_bytes(&self) -> usize {
        self.copied
    }

    pub fn bytes_received(&self) -> usize {
        self.received
    }

    pub fn fragments_received(&self) -> usize {
        self.fragments
    }

    pub fn declared_size(&self) -> Option<usize> {
        self.declared
    }

    pub fn pending_payloads(&self) -> usize {
        usize::from(self.first.is_some())
            + usize::from(self.incoming.is_some())
            + usize::from(self.trailing.is_some())
    }

    fn poison(&mut self, error: Error) -> Error {
        self.first = None;
        self.incoming = None;
        self.buffer = None;
        self.trailing = None;
        self.failed = Some(error);
        error
    }

    pub fn push(
        &mut self,
        payload: Bytes,
        cancel: &AtomicBool,
    ) -> std::result::Result<(), SpuPushError> {
        let error = if let Some(error) = self.failed {
            Some(error)
        } else if cancel.load(Ordering::Relaxed) {
            Some(self.poison(Error::Cancelled))
        } else if self.ended || matches!(self.state, State::Ready | State::Finished) {
            Some(self.poison(Error::Invalid("DVD SPU input already closed")))
        } else if !payload.belongs_to(&self.budget) {
            Some(self.poison(Error::ForeignPool))
        } else if self.incoming.is_some() || self.state != State::Receive {
            Some(Error::Backpressure {
                requested: 1,
                available: 0,
            })
        } else {
            None
        };
        if let Some(error) = error {
            return Err(SpuPushError { error, payload });
        }
        self.incoming = Some(payload);
        Ok(())
    }

    fn charge(&mut self, amount: usize, work: &mut WorkBudget) -> Result<bool> {
        let next = self.work.checked_add(amount).ok_or(Error::Overflow)?;
        if next > self.limits.max_work {
            return Err(Error::Limit {
                requested: next,
                maximum: self.limits.max_work,
            });
        }
        if !work.try_charge(u64::try_from(amount).map_err(|_| Error::Overflow)?) {
            return Ok(false);
        }
        self.work = next;
        Ok(true)
    }

    fn size(&self, bytes: [u8; 2]) -> Result<usize> {
        let size = usize::from(u16::from_be_bytes(bytes));
        if size < 9 {
            return Err(Error::Invalid("DVD SPU shorter than structural header"));
        }
        if size > self.limits.max_packet {
            return Err(Error::Limit {
                requested: size,
                maximum: self.limits.max_packet,
            });
        }
        Ok(size)
    }

    fn receive(&mut self) -> Result<()> {
        let count = self.fragments.checked_add(1).ok_or(Error::Overflow)?;
        if count > self.limits.max_fragments {
            return Err(Error::Limit {
                requested: count,
                maximum: self.limits.max_fragments,
            });
        }
        self.fragments = count;
        let input = self
            .incoming
            .as_ref()
            .ok_or(Error::Invalid("DVD SPU pending fragment absent"))?;
        if input.is_empty() {
            self.incoming = None;
            return Ok(());
        }
        if self.first.is_none() && self.buffer.is_none() && self.received == 0 {
            self.declared = if input.len() >= 2 {
                Some(self.size([input[0], input[1]])?)
            } else {
                None
            };
            self.received = self.declared.map_or(input.len(), |n| n.min(input.len()));
            self.first = self.incoming.take();
            if self.declared == Some(self.received) {
                self.state = State::Validate;
            }
            return Ok(());
        }
        let declared = if let Some(size) = self.declared {
            size
        } else {
            let first = self
                .first
                .as_ref()
                .ok_or(Error::Invalid("DVD SPU first size byte absent"))?;
            self.size([first[0], input[0]])?
        };
        self.declared = Some(declared);
        self.incoming_take = input.len().min(
            declared
                .checked_sub(self.received)
                .ok_or(Error::Invalid("DVD SPU accepted byte count"))?,
        );
        self.received = self
            .received
            .checked_add(self.incoming_take)
            .ok_or(Error::Overflow)?;
        if self.buffer.is_none() {
            self.buffer = Some(BudgetVec::new(&self.budget));
            self.state = State::Reserve;
        } else {
            self.state = State::CopyIncoming;
        }
        Ok(())
    }

    pub fn poll(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<SpuAssemblyStatus> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(self.poison(Error::Cancelled));
        }
        if self.state == State::Ready {
            return Ok(SpuAssemblyStatus::Ready);
        }
        if self.state == State::Finished {
            return Err(self.poison(Error::Invalid("DVD SPU already finished")));
        }
        if work.remaining() == 0 {
            return Ok(SpuAssemblyStatus::Yield);
        }
        match self.poll_inner(work, cancel) {
            Ok(status) => Ok(status),
            Err(error @ Error::Backpressure { .. }) => Err(error),
            Err(error) => Err(self.poison(error)),
        }
    }

    fn poll_inner(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<SpuAssemblyStatus> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            match self.state {
                State::Receive => {
                    if self.incoming.is_none() {
                        return if self.ended {
                            Err(Error::Truncated)
                        } else {
                            Ok(SpuAssemblyStatus::NeedFragment)
                        };
                    }
                    if !self.charge(1, work)? {
                        return Ok(SpuAssemblyStatus::Yield);
                    }
                    self.receive()?;
                }
                State::Reserve => {
                    if !self.charge(1, work)? {
                        return Ok(SpuAssemblyStatus::Yield);
                    }
                    self.state = State::Allocate;
                }
                State::Allocate => {
                    if work.remaining() == 0 {
                        return Ok(SpuAssemblyStatus::Yield);
                    }
                    let size = self.declared.ok_or(Error::Invalid("DVD SPU size absent"))?;
                    self.buffer
                        .as_mut()
                        .ok_or(Error::Invalid("DVD SPU buffer absent"))?
                        .try_reserve_exact(size)?;
                    self.state = State::CopyFirst;
                }
                State::CopyFirst => {
                    if !self.copy_pending(true, work)? {
                        return Ok(SpuAssemblyStatus::Yield);
                    }
                    self.state = State::CopyIncoming;
                }
                State::CopyIncoming => {
                    if !self.copy_pending(false, work)? {
                        return Ok(SpuAssemblyStatus::Yield);
                    }
                    self.state = if self.declared == Some(self.received) {
                        State::Validate
                    } else {
                        State::Receive
                    };
                }
                State::Validate => {
                    if !self.charge(1, work)? {
                        return Ok(SpuAssemblyStatus::Yield);
                    }
                    let size = self.declared.ok_or(Error::Invalid("DVD SPU size absent"))?;
                    let bytes = if let Some(buffer) = &self.buffer {
                        buffer.as_slice()
                    } else {
                        self.first
                            .as_deref()
                            .ok_or(Error::Invalid("DVD SPU complete input absent"))?
                    };
                    DvdPacket::parse(bytes.get(..size).ok_or(Error::Truncated)?)?;
                    self.state = State::Ready;
                }
                State::Ready => return Ok(SpuAssemblyStatus::Ready),
                State::Finished => return Err(Error::Invalid("DVD SPU already finished")),
            }
        }
    }

    fn copy_pending(&mut self, first: bool, work: &mut WorkBudget) -> Result<bool> {
        let length = if first {
            self.first
                .as_ref()
                .ok_or(Error::Invalid("DVD SPU first source absent"))?
                .len()
        } else {
            self.incoming_take
        };
        let left = length.checked_sub(self.copy_at).ok_or(Error::Overflow)?;
        if left != 0 {
            if work.remaining() == 0 {
                return Ok(false);
            }
            let amount = left
                .min(4096)
                .min(usize::try_from(work.remaining()).unwrap_or(usize::MAX))
                .min(self.limits.max_work.saturating_sub(self.work));
            if amount == 0 {
                return Err(Error::Limit {
                    requested: self.work.checked_add(1).ok_or(Error::Overflow)?,
                    maximum: self.limits.max_work,
                });
            }
            if !self.charge(amount, work)? {
                return Ok(false);
            }
            let end = self.copy_at.checked_add(amount).ok_or(Error::Overflow)?;
            let input = if first { &self.first } else { &self.incoming };
            let bytes = input
                .as_ref()
                .and_then(|b| b.get(self.copy_at..end))
                .ok_or(Error::Invalid("DVD SPU copy source bounds"))?;
            self.buffer
                .as_mut()
                .ok_or(Error::Invalid("DVD SPU copy destination absent"))?
                .try_extend_from_slice(bytes)?;
            self.copy_at = end;
            self.copied = self.copied.checked_add(amount).ok_or(Error::Overflow)?;
        }
        if self.copy_at != length {
            return Ok(false);
        }
        self.copy_at = 0;
        if first {
            self.first = None;
        } else {
            let input = self
                .incoming
                .take()
                .ok_or(Error::Invalid("DVD SPU incoming source absent"))?;
            let end = input.len();
            if length < end {
                self.trailing = Some(input.into_slice(length..end)?);
            }
        }
        Ok(true)
    }

    /// Only actual input termination makes an incomplete unit Truncated.
    /// A queued final input can still require work and pressure retries.
    pub fn finish_input(&mut self, cancel: &AtomicBool) -> Result<()> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(self.poison(Error::Cancelled));
        }
        if self.state == State::Finished {
            return Err(self.poison(Error::Invalid("DVD SPU already finished")));
        }
        self.ended = true;
        if self.state == State::Receive && self.incoming.is_none() {
            return Err(self.poison(Error::Truncated));
        }
        Ok(())
    }

    pub fn try_finish(&mut self, cancel: &AtomicBool) -> Result<AssembledSpu> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(self.poison(Error::Cancelled));
        }
        if self.state != State::Ready {
            return Err(self.poison(Error::Invalid("DVD SPU not ready")));
        }
        let packet = if let Some(buffer) = self.buffer.take() {
            match buffer.freeze() {
                Ok(bytes) => bytes,
                Err(failure) => {
                    if matches!(failure.error, Error::Backpressure { .. }) {
                        self.buffer = Some(failure.buffer);
                        return Err(failure.error);
                    }
                    drop(failure.buffer);
                    return Err(self.poison(failure.error));
                }
            }
        } else {
            let input = self
                .first
                .take()
                .ok_or(Error::Invalid("DVD SPU single input absent"))?;
            let size = self.declared.ok_or(Error::Invalid("DVD SPU size absent"))?;
            if input.len() > size {
                self.trailing = Some(input.slice(size..input.len())?);
            }
            input.into_slice(0..size)?
        };
        self.state = State::Finished;
        Ok(AssembledSpu {
            packet,
            trailing: self.trailing.take(),
        })
    }
}

//! One bounded ODS object, before raster decoding or display/cache policy.
use super::ObjectFragment;
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result, WorkBudget};

#[derive(Clone, Copy, Debug)]
pub struct AssemblyLimits {
    pub max_axis: u16,
    pub max_pixels: usize,
    pub max_encoded: usize,
    pub max_fragments: usize,
    pub max_work: usize,
}

impl Default for AssemblyLimits {
    fn default() -> Self {
        Self {
            max_axis: 4096,
            max_pixels: 8 * 1024 * 1024,
            max_encoded: 4 * 1024 * 1024,
            max_fragments: 1024,
            max_work: 32 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssemblyStatus {
    NeedFragment,
    Yield,
    Ready,
}

#[derive(Debug)]
pub struct EncodedObject {
    pub id: u16,
    pub version: u8,
    pub width: u16,
    pub height: u16,
    /// Original wire length, including the four dimension bytes.
    pub data_length: u32,
    encoded: Bytes,
}

impl EncodedObject {
    pub fn encoded(&self) -> &Bytes {
        &self.encoded
    }

    pub fn into_encoded(self) -> Bytes {
        self.encoded
    }
}

/// A rejected submission returns the exact incoming owner to its caller.
#[derive(Debug)]
pub struct ObjectPushError {
    pub error: Error,
    pub payload: Bytes,
}

#[derive(Clone, Copy, Debug)]
struct Info {
    id: u16,
    version: u8,
    width: u16,
    height: u16,
    data_length: u32,
    encoded: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Receive,
    Reserve,
    Allocate,
    CopyFirst,
    CopyIncoming,
    Ready,
    Finished,
}

/// Accepts owned ODS payloads, not packet/SUP headers. `push` only transfers a
/// pending input; `poll` validates one header per work unit. A complete C0 input
/// becomes a consuming 11.. view, without constructing mutable output storage.
/// Fragmented inputs are copied once into one pre-reserved, charged buffer.
///
/// At most the first and one incoming `Bytes` are retained. A busy `push`
/// returns its input with Backpressure { requested: 1, available: 0 }; these
/// describe the pending-input slot, not free bytes in the allocation pool.
/// Poll allocation pressure is retryable without repeating setup/header work.
#[derive(Debug)]
pub struct ObjectAssembly {
    budget: Budget,
    limits: AssemblyLimits,
    info: Option<Info>,
    first: Option<Bytes>,
    incoming: Option<Bytes>,
    buffer: Option<BudgetVec<u8>>,
    state: State,
    fragments: usize,
    received: usize,
    copied: usize,
    copy_at: usize,
    work: usize,
    last: bool,
    ended: bool,
    failed: Option<Error>,
}

impl ObjectAssembly {
    /// Clones only the existing pool handle. No output buffer is constructed.
    pub fn new(budget: &Budget, limits: AssemblyLimits) -> Self {
        Self {
            budget: budget.clone(),
            limits,
            info: None,
            first: None,
            incoming: None,
            buffer: None,
            state: State::Receive,
            fragments: 0,
            received: 0,
            copied: 0,
            copy_at: 0,
            work: 0,
            last: false,
            ended: false,
            failed: None,
        }
    }

    fn poison(&mut self, error: Error) -> Error {
        self.first = None;
        self.incoming = None;
        self.buffer = None;
        self.failed = Some(error);
        error
    }

    pub fn push(
        &mut self,
        payload: Bytes,
        cancel: &AtomicBool,
    ) -> std::result::Result<(), ObjectPushError> {
        let error = if let Some(error) = self.failed {
            Some(error)
        } else if cancel.load(Ordering::Relaxed) {
            Some(self.poison(Error::Cancelled))
        } else if self.ended || matches!(self.state, State::Ready | State::Finished) {
            Some(self.poison(Error::Invalid("PGS object input already closed")))
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
            return Err(ObjectPushError { error, payload });
        }
        self.incoming = Some(payload);
        Ok(())
    }

    pub fn pending_payloads(&self) -> usize {
        usize::from(self.first.is_some()) + usize::from(self.incoming.is_some())
    }

    pub fn fragments_received(&self) -> usize {
        self.fragments
    }

    pub fn encoded_received(&self) -> usize {
        self.received
    }

    pub fn copied_bytes(&self) -> usize {
        self.copied
    }

    pub fn work_spent(&self) -> usize {
        self.work
    }

    fn charge(&mut self, count: usize, work: &mut WorkBudget) -> Result<bool> {
        let total = self.work.checked_add(count).ok_or(Error::Overflow)?;
        if total > self.limits.max_work {
            return Err(Error::Limit {
                requested: total,
                maximum: self.limits.max_work,
            });
        }
        if !work.try_charge(u64::try_from(count).map_err(|_| Error::Overflow)?) {
            return Ok(false);
        }
        self.work = total;
        Ok(true)
    }

    fn inspect(&self) -> Result<(Info, usize, usize, bool)> {
        let payload = self
            .incoming
            .as_ref()
            .ok_or(Error::Invalid("PGS object pending payload absent"))?;
        let fragment = ObjectFragment::parse_payload(payload)?;
        if fragment.flags & 0x3f != 0 {
            return Err(Error::Unsupported("PGS object reserved sequence flags"));
        }
        let count = self.fragments.checked_add(1).ok_or(Error::Overflow)?;
        if count > self.limits.max_fragments {
            return Err(Error::Limit {
                requested: count,
                maximum: self.limits.max_fragments,
            });
        }
        let (info, offset) = if let Some(info) = self.info {
            if fragment.first.is_some() {
                return Err(Error::Invalid("PGS repeated first object fragment"));
            }
            if (fragment.id, fragment.version) != (info.id, info.version) {
                return Err(Error::Invalid("PGS object fragment identity"));
            }
            (info, 4)
        } else {
            let first = fragment
                .first
                .ok_or(Error::Invalid("PGS object continuation before first"))?;
            if first.width == 0 || first.height == 0 {
                return Err(Error::Invalid("PGS zero object dimension"));
            }
            for axis in [first.width, first.height] {
                if axis > self.limits.max_axis {
                    return Err(Error::Limit {
                        requested: usize::from(axis),
                        maximum: usize::from(self.limits.max_axis),
                    });
                }
            }
            let pixels = usize::from(first.width)
                .checked_mul(usize::from(first.height))
                .ok_or(Error::Overflow)?;
            let encoded = usize::try_from(first.data_length - 4).map_err(|_| Error::Overflow)?;
            for (requested, maximum) in [
                (pixels, self.limits.max_pixels),
                (encoded, self.limits.max_encoded),
            ] {
                if requested > maximum {
                    return Err(Error::Limit { requested, maximum });
                }
            }
            (
                Info {
                    id: fragment.id,
                    version: fragment.version,
                    width: first.width,
                    height: first.height,
                    data_length: first.data_length,
                    encoded,
                },
                11,
            )
        };
        let received = self
            .received
            .checked_add(fragment.data.len())
            .ok_or(Error::Overflow)?;
        if received > info.encoded {
            return Err(Error::Invalid(
                "PGS object fragment exceeds declared length",
            ));
        }
        if fragment.last && received != info.encoded {
            return Err(Error::Invalid("PGS last object fragment length"));
        }
        Ok((info, offset, received, fragment.last))
    }

    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<AssemblyStatus> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(self.poison(Error::Cancelled));
        }
        if self.state == State::Ready {
            return Ok(AssemblyStatus::Ready);
        }
        if self.state == State::Finished {
            return Err(self.poison(Error::Invalid("PGS object already finished")));
        }
        if work.remaining() == 0 {
            return Ok(AssemblyStatus::Yield);
        }
        match self.poll_inner(work, cancel) {
            Ok(status) => Ok(status),
            Err(error @ Error::Backpressure { .. }) => Err(error),
            Err(error) => Err(self.poison(error)),
        }
    }

    fn poll_inner(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<AssemblyStatus> {
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
                            Ok(AssemblyStatus::NeedFragment)
                        };
                    }
                    if !self.charge(1, work)? {
                        return Ok(AssemblyStatus::Yield);
                    }
                    let (info, offset, received, last) = self.inspect()?;
                    let first = self.info.is_none();
                    let payload = self
                        .incoming
                        .take()
                        .ok_or(Error::Invalid("PGS object pending payload absent"))?;
                    let end = payload.len();
                    let data = payload.into_slice(offset..end)?;
                    self.info = Some(info);
                    self.received = received;
                    self.fragments += 1;
                    self.last = last;
                    if first {
                        self.first = Some(data);
                        if last {
                            self.state = State::Ready;
                        }
                    } else {
                        self.incoming = Some(data);
                        if self.buffer.is_none() {
                            self.buffer = Some(BudgetVec::new(&self.budget));
                            self.state = State::Reserve;
                        } else {
                            self.state = State::CopyIncoming;
                        }
                    }
                }
                State::Reserve => {
                    if !self.charge(1, work)? {
                        return Ok(AssemblyStatus::Yield);
                    }
                    // Retry resumes AFTER charging setup, including pressure.
                    self.state = State::Allocate;
                }
                State::Allocate => {
                    let length = self
                        .info
                        .ok_or(Error::Invalid("PGS object first header absent"))?
                        .encoded;
                    self.buffer
                        .as_mut()
                        .ok_or(Error::Invalid("PGS object output buffer absent"))?
                        .try_reserve_exact(length)?;
                    self.state = State::CopyFirst;
                }
                State::CopyFirst => {
                    if !self.copy_pending(true, work)? {
                        return Ok(AssemblyStatus::Yield);
                    }
                    self.state = State::CopyIncoming;
                }
                State::CopyIncoming => {
                    if !self.copy_pending(false, work)? {
                        return Ok(AssemblyStatus::Yield);
                    }
                    self.state = if self.last {
                        State::Ready
                    } else {
                        State::Receive
                    };
                }
                State::Ready => return Ok(AssemblyStatus::Ready),
                State::Finished => return Err(Error::Invalid("PGS object already finished")),
            }
        }
    }

    fn copy_pending(&mut self, first: bool, work: &mut WorkBudget) -> Result<bool> {
        let input = if first { &self.first } else { &self.incoming };
        let length = input
            .as_ref()
            .ok_or(Error::Invalid("PGS object copy source absent"))?
            .len();
        let left = length.checked_sub(self.copy_at).ok_or(Error::Overflow)?;
        if left != 0 {
            if work.remaining() == 0 {
                return Ok(false);
            }
            // Recheck cancellation between bounded copies, even with a huge
            // caller quantum. Pre-reservation prevents copying old prefixes.
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
            let source = input
                .as_ref()
                .and_then(|bytes| bytes.get(self.copy_at..end))
                .ok_or(Error::Invalid("PGS object copy source bounds"))?;
            self.buffer
                .as_mut()
                .ok_or(Error::Invalid("PGS object output buffer absent"))?
                .try_extend_from_slice(source)?;
            self.copy_at = end;
            self.copied = self.copied.checked_add(amount).ok_or(Error::Overflow)?;
        }
        if self.copy_at == length {
            if first {
                self.first = None;
            } else {
                self.incoming = None;
            }
            self.copy_at = 0;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Declares the end of submitted ODS input. A queued final fragment may
    /// still need work/pressure retries. Missing last is never inferred away.
    pub fn finish_input(&mut self, cancel: &AtomicBool) -> Result<()> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(self.poison(Error::Cancelled));
        }
        if self.state == State::Finished {
            return Err(self.poison(Error::Invalid("PGS object already finished")));
        }
        self.ended = true;
        if self.state == State::Receive && self.incoming.is_none() {
            return Err(self.poison(Error::Truncated));
        }
        Ok(())
    }

    pub fn try_finish(&mut self, cancel: &AtomicBool) -> Result<EncodedObject> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(self.poison(Error::Cancelled));
        }
        if self.state != State::Ready {
            return Err(self.poison(Error::Invalid("PGS object not ready")));
        }
        let Some(info) = self.info else {
            return Err(self.poison(Error::Invalid("PGS object first header absent")));
        };
        let encoded = if let Some(buffer) = self.buffer.take() {
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
            let Some(bytes) = self.first.take() else {
                return Err(self.poison(Error::Invalid("PGS object single payload absent")));
            };
            bytes
        };
        self.state = State::Finished;
        Ok(EncodedObject {
            id: info.id,
            version: info.version,
            width: info.width,
            height: info.height,
            data_length: info.data_length,
            encoded,
        })
    }
}

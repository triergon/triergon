//! Bounded original physical-packet HD receiver with an explicit input policy.
use super::{HdContextJob, HdContextOutput, HdLimits, HdReceiverContext, HdReceiverStatus, HdSpan};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result, TimeBase, WorkBudget};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HdReset {
    Keep,
    Flush,
    Reopen,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HdPacketClock {
    pub known: bool,
    pub raw: i64,
    pub timebase: TimeBase,
}
#[derive(Clone, Debug)]
pub struct HdPhysicalPacket {
    pub bytes: Bytes,
    pub clock: HdPacketClock,
    pub order: u64,
    pub reset: HdReset,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HdFragmentStamp {
    pub order: u64,
    pub clock: HdPacketClock,
    pub range: HdSpan,
}
impl HdFragmentStamp {
    pub fn raw(self, source: &[u8]) -> Result<&[u8]> {
        self.range.get(source)
    }
}
#[derive(Clone, Copy, Debug)]
pub struct HdFragmentHistory {
    entries: [Option<HdFragmentStamp>; 64],
    len: usize,
}
impl HdFragmentHistory {
    fn empty() -> Self {
        Self {
            entries: [None; 64],
            len: 0,
        }
    }
    fn push(&mut self, s: HdFragmentStamp, max: usize) -> Result<()> {
        if self.len >= max || self.len == 64 {
            return Err(Error::Limit {
                requested: self.len.saturating_add(1),
                maximum: max.min(64),
            });
        }
        self.entries[self.len] = Some(s);
        self.len += 1;
        Ok(())
    }
    pub fn stamps(&self) -> impl Iterator<Item = HdFragmentStamp> + '_ {
        self.entries[..self.len].iter().flatten().copied()
    }
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}
#[derive(Clone, Copy, Debug)]
pub struct HdFragmentLimits {
    pub max_calls: usize,
    pub max_fragments: usize,
    pub max_input: usize,
    pub max_pending: usize,
    pub max_work: usize,
    pub decode: HdLimits,
}
impl Default for HdFragmentLimits {
    fn default() -> Self {
        Self {
            max_calls: 64,
            max_fragments: 64,
            max_input: 16 * 1024 * 1024,
            max_pending: 16 * 1024 * 1024,
            max_work: 128 * 1024 * 1024,
            decode: HdLimits::default(),
        }
    }
}
#[derive(Debug)]
enum Carry {
    First(Bytes, HdFragmentHistory),
    Buffer(BudgetVec<u8>, HdFragmentHistory),
}
impl Carry {
    fn raw(&self) -> &[u8] {
        match self {
            Self::First(b, _) => b,
            Self::Buffer(b, _) => b.as_slice(),
        }
    }
    fn history(&self) -> HdFragmentHistory {
        match self {
            Self::First(_, h) | Self::Buffer(_, h) => *h,
        }
    }
}
#[derive(Debug)]
pub struct HdFragmentState {
    budget: Budget,
    limits: HdFragmentLimits,
    context: HdReceiverContext,
    carry: Option<Carry>,
    calls: usize,
    input: usize,
    work: usize,
    ended: bool,
}
impl HdFragmentState {
    pub fn new(b: &Budget, l: HdFragmentLimits) -> Result<Self> {
        if l.max_fragments > 64 {
            return Err(Error::Limit {
                requested: l.max_fragments,
                maximum: 64,
            });
        }
        Ok(Self {
            budget: b.clone(),
            limits: l,
            context: HdReceiverContext::new(b),
            carry: None,
            calls: 0,
            input: 0,
            work: 0,
            ended: false,
        })
    }
    pub fn pending(&self) -> Option<&[u8]> {
        self.carry.as_ref().map(Carry::raw)
    }
    pub fn work_spent(&self) -> usize {
        self.work
    }
    pub fn start(
        slot: &mut Option<Self>,
        p: HdPhysicalPacket,
    ) -> std::result::Result<HdFragmentJob, HdFragmentStartError> {
        let Some(state) = slot.as_ref() else {
            return Err(HdFragmentStartError {
                error: Error::Invalid("HD fragment state is already in use"),
                input: p,
            });
        };
        let error = if state.ended {
            Some(Error::Invalid("HD physical input after end"))
        } else if !p.bytes.belongs_to(&state.budget) {
            Some(Error::ForeignPool)
        } else if state.calls >= state.limits.max_calls {
            Some(Error::Limit {
                requested: state.calls.saturating_add(1),
                maximum: state.limits.max_calls,
            })
        } else if p.bytes.len() > state.limits.max_input.saturating_sub(state.input) {
            Some(Error::Limit {
                requested: state.input.saturating_add(p.bytes.len()),
                maximum: state.limits.max_input,
            })
        } else if p.reset == HdReset::Keep
            && !p.bytes.is_empty()
            && state
                .carry
                .as_ref()
                .is_some_and(|v| v.history().len() >= state.limits.max_fragments)
        {
            Some(Error::Limit {
                requested: state
                    .carry
                    .as_ref()
                    .map_or(0, |v| v.history().len())
                    .saturating_add(1),
                maximum: state.limits.max_fragments,
            })
        } else if p.reset == HdReset::Keep
            && state.carry.as_ref().is_some_and(|v| {
                v.raw().len().saturating_add(p.bytes.len()) > state.limits.max_pending
            })
        {
            Some(Error::Limit {
                requested: state
                    .carry
                    .as_ref()
                    .map_or(0, |v| v.raw().len())
                    .saturating_add(p.bytes.len()),
                maximum: state.limits.max_pending,
            })
        } else {
            None
        };
        if let Some(error) = error {
            return Err(HdFragmentStartError { error, input: p });
        }
        match slot.take() {
            Some(state) => Ok(HdFragmentJob::make(state, Some(p))),
            None => Err(HdFragmentStartError {
                error: Error::Invalid("HD fragment state absent"),
                input: p,
            }),
        }
    }
    pub fn end(self) -> HdFragmentJob {
        HdFragmentJob::make(self, None)
    }
}
#[derive(Debug)]
pub struct HdFragmentStartError {
    pub error: Error,
    pub input: HdPhysicalPacket,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HdFragmentIntegrity {
    Exact,
    Pending,
    PhysicalPrefixConsumed,
    Trailing { declared: usize, actual: usize },
    Truncated { declared: usize, actual: usize },
    InputEnded,
}
#[derive(Debug)]
pub struct HdPendingEvidence {
    pub bytes: Bytes,
    pub history: HdFragmentHistory,
}
#[derive(Debug)]
pub struct HdFragmentOutput {
    pub physical: Option<HdPhysicalPacket>,
    pub projected_returned: usize,
    pub integrity: HdFragmentIntegrity,
    pub frame: Option<HdContextOutput>,
    pub supplied_source: Option<Bytes>,
    pub history: HdFragmentHistory,
    pub reset_discarded: Option<HdPendingEvidence>,
    pub incomplete_at_end: Option<HdPendingEvidence>,
}
impl HdFragmentOutput {
    pub fn source_error(&self) -> Option<Error> {
        if self.incomplete_at_end.is_some()
            || matches!(self.integrity, HdFragmentIntegrity::Truncated { .. })
        {
            Some(Error::Truncated)
        } else {
            self.frame.as_ref().and_then(|v| v.source().source_error())
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HdFragmentStatus {
    Yield,
    Ready,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Setup,
    Reset,
    Prepare,
    Allocate,
    CopyFirst,
    CopyInput,
    Probe,
    Freeze,
    Decode,
    Ready,
    Finished,
}
#[derive(Debug)]
pub struct HdFragmentJob {
    state: Option<HdFragmentState>,
    physical: Option<HdPhysicalPacket>,
    phase: Phase,
    first: Option<Bytes>,
    buffer: Option<BudgetVec<u8>>,
    source: Option<Bytes>,
    history: HdFragmentHistory,
    copied: usize,
    probe_paid: usize,
    reserve_paid: usize,
    allocation_paid: bool,
    freeze_paid: bool,
    reset_paid: bool,
    reset_discarded: Option<HdPendingEvidence>,
    incomplete: Option<HdPendingEvidence>,
    inner: Option<HdContextJob>,
    frame: Option<HdContextOutput>,
    integrity: HdFragmentIntegrity,
    projected_returned: usize,
    failed: Option<Error>,
    terminal_work: usize,
}
impl HdFragmentJob {
    fn make(state: HdFragmentState, physical: Option<HdPhysicalPacket>) -> Self {
        Self {
            state: Some(state),
            physical,
            phase: Phase::Setup,
            first: None,
            buffer: None,
            source: None,
            history: HdFragmentHistory::empty(),
            copied: 0,
            probe_paid: 0,
            reserve_paid: 0,
            allocation_paid: false,
            freeze_paid: false,
            reset_paid: false,
            reset_discarded: None,
            incomplete: None,
            inner: None,
            frame: None,
            integrity: HdFragmentIntegrity::Exact,
            projected_returned: 0,
            failed: None,
            terminal_work: 0,
        }
    }
    pub fn work_spent(&self) -> usize {
        self.state.as_ref().map_or(self.terminal_work, |s| s.work)
    }
    fn poison(&mut self, e: Error) -> Error {
        self.terminal_work = self.work_spent();
        self.state = None;
        self.physical = None;
        self.first = None;
        self.buffer = None;
        self.source = None;
        self.inner = None;
        self.frame = None;
        self.reset_discarded = None;
        self.incomplete = None;
        self.failed = Some(e);
        e
    }
    fn pay(&mut self, n: usize, w: &mut WorkBudget) -> Result<usize> {
        if w.remaining() == 0 {
            return Ok(0);
        }
        let s = self
            .state
            .as_mut()
            .ok_or(Error::Invalid("HD fragment state absent"))?;
        if s.work == s.limits.max_work {
            return Err(Error::Limit {
                requested: s.work.saturating_add(1),
                maximum: s.limits.max_work,
            });
        }
        let take = n
            .min(usize::try_from(w.remaining()).unwrap_or(usize::MAX))
            .min(s.limits.max_work - s.work);
        if take != 0 {
            w.try_charge(take as u64);
            s.work += take;
        }
        Ok(take)
    }
    fn raw(&self) -> Result<&[u8]> {
        if let Some(b) = &self.buffer {
            return Ok(b.as_slice());
        }
        self.source
            .as_deref()
            .ok_or(Error::Invalid("HD fragment source absent"))
    }
    fn take_carry(&mut self, w: &mut WorkBudget) -> Result<Option<HdPendingEvidence>> {
        if self.state.as_ref().and_then(|s| s.carry.as_ref()).is_none() || w.remaining() == 0 {
            return Ok(None);
        }
        if !self.reset_paid {
            if self.pay(1, w)? == 0 {
                return Ok(None);
            }
            self.reset_paid = true;
            if w.remaining() == 0 {
                return Ok(None);
            }
        }
        let s = self
            .state
            .as_mut()
            .ok_or(Error::Invalid("HD fragment state absent"))?;
        let carry = s.carry.take().ok_or(Error::Invalid("HD carry absent"))?;
        match carry {
            Carry::First(bytes, history) => Ok(Some(HdPendingEvidence { bytes, history })),
            Carry::Buffer(buffer, history) => match buffer.freeze() {
                Ok(bytes) => Ok(Some(HdPendingEvidence { bytes, history })),
                Err(e) => {
                    let error = e.error;
                    s.carry = Some(Carry::Buffer(e.buffer, history));
                    Err(error)
                }
            },
        }
    }
    pub fn poll(&mut self, w: &mut WorkBudget, c: &AtomicBool) -> Result<HdFragmentStatus> {
        if let Some(e) = self.failed {
            return Err(e);
        }
        if c.load(Ordering::Relaxed) {
            return Err(self.poison(Error::Cancelled));
        }
        if self.phase == Phase::Finished {
            return Err(self.poison(Error::Invalid("HD fragment already finished")));
        }
        let r = self.inner_poll(w, c);
        match r {
            Err(e @ Error::Backpressure { .. }) => Err(e),
            Err(e) => Err(self.poison(e)),
            v => v,
        }
    }
    fn inner_poll(&mut self, w: &mut WorkBudget, c: &AtomicBool) -> Result<HdFragmentStatus> {
        loop {
            if c.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            match self.phase {
                Phase::Ready => return Ok(HdFragmentStatus::Ready),
                Phase::Setup => {
                    if self.pay(1, w)? == 0 {
                        return Ok(HdFragmentStatus::Yield);
                    }
                    let s = self
                        .state
                        .as_mut()
                        .ok_or(Error::Invalid("HD fragment state absent"))?;
                    if let Some(p) = &self.physical {
                        s.calls += 1;
                        s.input += p.bytes.len();
                    } else {
                        if s.ended {
                            return Err(Error::Invalid("HD physical input already ended"));
                        }
                        s.ended = true;
                    }
                    self.phase = Phase::Reset;
                }
                Phase::Reset => {
                    let reset = self.physical.as_ref().map_or(HdReset::Keep, |p| p.reset);
                    if reset != HdReset::Keep || self.physical.is_none() {
                        if self.state.as_ref().and_then(|s| s.carry.as_ref()).is_some() {
                            let Some(e) = self.take_carry(w)? else {
                                return Ok(HdFragmentStatus::Yield);
                            };
                            if self.physical.is_none() {
                                self.history = e.history;
                                self.incomplete = Some(e);
                            } else {
                                self.reset_discarded = Some(e);
                            }
                        }
                        if w.remaining() == 0 {
                            return Ok(HdFragmentStatus::Yield);
                        }
                        if reset == HdReset::Reopen {
                            self.state
                                .as_mut()
                                .ok_or(Error::Invalid("HD fragment state absent"))?
                                .context
                                .reset();
                        }
                    }
                    if self.physical.is_none() {
                        self.integrity = HdFragmentIntegrity::InputEnded;
                        self.phase = Phase::Ready;
                    } else {
                        self.phase = Phase::Prepare;
                    }
                }
                Phase::Prepare => {
                    let p = self
                        .physical
                        .as_ref()
                        .ok_or(Error::Invalid("HD physical source absent"))?;
                    if p.bytes.is_empty() {
                        self.integrity = HdFragmentIntegrity::PhysicalPrefixConsumed;
                        self.phase = Phase::Ready;
                        continue;
                    }
                    let s = self
                        .state
                        .as_mut()
                        .ok_or(Error::Invalid("HD fragment state absent"))?;
                    let start = s.carry.as_ref().map_or(0, |v| v.raw().len());
                    self.history = s
                        .carry
                        .as_ref()
                        .map_or(HdFragmentHistory::empty(), Carry::history);
                    let end = start.checked_add(p.bytes.len()).ok_or(Error::Overflow)?;
                    self.history.push(
                        HdFragmentStamp {
                            order: p.order,
                            clock: p.clock,
                            range: HdSpan { start, end },
                        },
                        s.limits.max_fragments,
                    )?;
                    if end > s.limits.max_pending {
                        return Err(Error::Limit {
                            requested: end,
                            maximum: s.limits.max_pending,
                        });
                    }
                    match s.carry.take() {
                        None => {
                            self.source = Some(p.bytes.clone());
                            self.phase = Phase::Probe;
                        }
                        Some(Carry::First(b, _)) => {
                            self.first = Some(b);
                            self.buffer = Some(BudgetVec::new(&s.budget));
                            self.phase = Phase::Allocate;
                        }
                        Some(Carry::Buffer(b, _)) => {
                            let growth = end > b.capacity();
                            self.buffer = Some(b);
                            self.phase = if growth {
                                Phase::Allocate
                            } else {
                                Phase::CopyInput
                            };
                        }
                    }
                }
                Phase::Allocate => {
                    if w.remaining() == 0 {
                        return Ok(HdFragmentStatus::Yield);
                    }
                    if !self.allocation_paid {
                        if self.pay(1, w)? == 0 {
                            return Ok(HdFragmentStatus::Yield);
                        }
                        self.allocation_paid = true;
                    }
                    let data = if let Some(first) = &self.first {
                        first.as_ref()
                    } else {
                        self.buffer
                            .as_ref()
                            .ok_or(Error::Invalid("HD carry buffer absent"))?
                            .as_slice()
                    };
                    let wide = data[..2] == [0, 0];
                    let declared = if wide {
                        u32::from_be_bytes(data[2..6].try_into().map_err(|_| Error::Truncated)?)
                            as usize
                    } else {
                        usize::from(u16::from_be_bytes([data[0], data[1]]))
                    };
                    let control = if wide {
                        u32::from_be_bytes(data[6..10].try_into().map_err(|_| Error::Truncated)?)
                            as usize
                    } else {
                        usize::from(u16::from_be_bytes([data[2], data[3]]))
                    };
                    let end = data
                        .len()
                        .checked_add(
                            self.physical
                                .as_ref()
                                .ok_or(Error::Invalid("HD physical source absent"))?
                                .bytes
                                .len(),
                        )
                        .ok_or(Error::Overflow)?;
                    let cap = declared
                        .max(control.saturating_add(if wide { 6 } else { 4 }))
                        .max(end)
                        .min(
                            self.state
                                .as_ref()
                                .ok_or(Error::Invalid("HD fragment state absent"))?
                                .limits
                                .max_pending,
                        );
                    let old = self
                        .buffer
                        .as_ref()
                        .ok_or(Error::Invalid("HD carry buffer absent"))?
                        .len();
                    self.reserve_paid += self.pay(old - self.reserve_paid, w)?;
                    if self.reserve_paid < old || w.remaining() == 0 {
                        return Ok(HdFragmentStatus::Yield);
                    }
                    self.buffer
                        .as_mut()
                        .ok_or(Error::Invalid("HD carry buffer absent"))?
                        .try_reserve_exact(cap - old)?;
                    self.phase = if self.first.is_some() {
                        Phase::CopyFirst
                    } else {
                        Phase::CopyInput
                    };
                }
                Phase::CopyFirst | Phase::CopyInput => {
                    let len = if self.phase == Phase::CopyFirst {
                        self.first
                            .as_ref()
                            .ok_or(Error::Invalid("HD first source absent"))?
                            .len()
                    } else {
                        self.physical
                            .as_ref()
                            .ok_or(Error::Invalid("HD physical source absent"))?
                            .bytes
                            .len()
                    };
                    if self.copied == len {
                        self.copied = 0;
                        if self.phase == Phase::CopyFirst {
                            self.first = None;
                            self.phase = Phase::CopyInput;
                        } else {
                            self.phase = Phase::Probe;
                        }
                        continue;
                    }
                    let n = self.pay((len - self.copied).min(256), w)?;
                    if n == 0 {
                        return Ok(HdFragmentStatus::Yield);
                    }
                    let from = self.copied;
                    let to = from + n;
                    let bytes = if self.phase == Phase::CopyFirst {
                        &self
                            .first
                            .as_ref()
                            .ok_or(Error::Invalid("HD first source absent"))?
                            [from..to]
                    } else {
                        &self
                            .physical
                            .as_ref()
                            .ok_or(Error::Invalid("HD physical source absent"))?
                            .bytes[from..to]
                    };
                    self.buffer
                        .as_mut()
                        .ok_or(Error::Invalid("HD carry buffer absent"))?
                        .try_extend_from_slice(bytes)?;
                    self.copied = to;
                }
                Phase::Probe => {
                    let n = self.raw()?.len();
                    let needed = n.min(10);
                    self.probe_paid += self.pay(needed - self.probe_paid, w)?;
                    if self.probe_paid < needed {
                        return Ok(HdFragmentStatus::Yield);
                    }
                    self.projected_returned = n;
                    if n < 10 {
                        self.integrity = HdFragmentIntegrity::PhysicalPrefixConsumed;
                        self.phase = Phase::Ready;
                        continue;
                    }
                    let raw = self.raw()?;
                    let wide = raw[..2] == [0, 0];
                    let control = if wide {
                        u32::from_be_bytes(raw[6..10].try_into().map_err(|_| Error::Truncated)?)
                            as usize
                    } else {
                        usize::from(u16::from_be_bytes([raw[2], raw[3]]))
                    };
                    let header = if wide { 6 } else { 4 };
                    let declared = if wide {
                        u32::from_be_bytes(raw[2..6].try_into().map_err(|_| Error::Truncated)?)
                            as usize
                    } else {
                        usize::from(u16::from_be_bytes([raw[0], raw[1]]))
                    };
                    if control <= declared
                        && control.checked_add(header).ok_or(Error::Overflow)? > n
                    {
                        let s = self
                            .state
                            .as_mut()
                            .ok_or(Error::Invalid("HD fragment state absent"))?;
                        if control.saturating_add(header) > s.limits.max_pending {
                            return Err(Error::Limit {
                                requested: control.saturating_add(header),
                                maximum: s.limits.max_pending,
                            });
                        }
                        s.carry = if let Some(b) = self.buffer.take() {
                            Some(Carry::Buffer(b, self.history))
                        } else {
                            Some(Carry::First(
                                self.source
                                    .take()
                                    .ok_or(Error::Invalid("HD cached source absent"))?,
                                self.history,
                            ))
                        };
                        self.integrity = HdFragmentIntegrity::Pending;
                        self.projected_returned = 0;
                        self.phase = Phase::Ready;
                        continue;
                    }
                    self.phase = Phase::Freeze;
                }
                Phase::Freeze => {
                    if w.remaining() == 0 {
                        return Ok(HdFragmentStatus::Yield);
                    }
                    if !self.freeze_paid {
                        if self.pay(1, w)? == 0 {
                            return Ok(HdFragmentStatus::Yield);
                        }
                        self.freeze_paid = true;
                    }
                    if let Some(buffer) = self.buffer.take() {
                        match buffer.freeze() {
                            Ok(bytes) => self.source = Some(bytes),
                            Err(e) => {
                                let reason = e.error;
                                self.buffer = Some(e.buffer);
                                return Err(reason);
                            }
                        }
                    }
                    let bytes = self
                        .source
                        .as_ref()
                        .ok_or(Error::Invalid("HD assembled source absent"))?;
                    let wide = bytes[..2] == [0, 0];
                    let declared = if wide {
                        u32::from_be_bytes(bytes[2..6].try_into().map_err(|_| Error::Truncated)?)
                            as usize
                    } else {
                        usize::from(u16::from_be_bytes([bytes[0], bytes[1]]))
                    };
                    let minimum = if wide { 10 } else { 4 };
                    let data = if declared >= minimum && declared < bytes.len() {
                        self.integrity = HdFragmentIntegrity::Trailing {
                            declared,
                            actual: bytes.len(),
                        };
                        bytes.slice(0..declared)?
                    } else {
                        if declared > bytes.len() {
                            self.integrity = HdFragmentIntegrity::Truncated {
                                declared,
                                actual: bytes.len(),
                            };
                        }
                        bytes.clone()
                    };
                    let s = self
                        .state
                        .as_ref()
                        .ok_or(Error::Invalid("HD fragment state absent"))?;
                    let mut limits = s.limits.decode;
                    limits.max_work = limits.max_work.min(s.limits.max_work - s.work);
                    let mut inner =
                        if matches!(self.integrity, HdFragmentIntegrity::Truncated { .. }) {
                            HdContextJob::with_prefix(data, &s.budget, limits, &s.context)?
                        } else {
                            HdContextJob::new(data, &s.budget, limits, &s.context)?
                        };
                    inner.finish_input();
                    self.inner = Some(inner);
                    self.phase = Phase::Decode;
                }
                Phase::Decode => {
                    let job = self
                        .inner
                        .as_mut()
                        .ok_or(Error::Invalid("HD physical decode absent"))?;
                    let before = job.work_spent();
                    let status = job.poll(w, c);
                    self.state
                        .as_mut()
                        .ok_or(Error::Invalid("HD fragment state absent"))?
                        .work += job.work_spent() - before;
                    match status? {
                        HdReceiverStatus::Yield => return Ok(HdFragmentStatus::Yield),
                        HdReceiverStatus::NeedInputEnd => {
                            return Err(Error::Invalid("HD physical end not applied"));
                        }
                        HdReceiverStatus::Ready => {
                            let frame = job.try_finish(c)?;
                            self.state
                                .as_mut()
                                .ok_or(Error::Invalid("HD fragment state absent"))?
                                .context = frame.next_state().clone();
                            self.frame = Some(frame);
                            self.inner = None;
                            self.phase = Phase::Ready;
                        }
                    }
                }
                Phase::Finished => return Err(Error::Invalid("HD physical job finished")),
            }
        }
    }
    pub fn try_finish(&mut self, c: &AtomicBool) -> Result<(HdFragmentState, HdFragmentOutput)> {
        if let Some(e) = self.failed {
            return Err(e);
        }
        if c.load(Ordering::Relaxed) {
            return Err(self.poison(Error::Cancelled));
        }
        if self.phase != Phase::Ready {
            return Err(Error::Invalid("HD physical result not ready"));
        }
        let state = self
            .state
            .take()
            .ok_or(Error::Invalid("HD physical state absent"))?;
        self.terminal_work = state.work;
        let out = HdFragmentOutput {
            physical: self.physical.take(),
            projected_returned: self.projected_returned,
            integrity: self.integrity,
            frame: self.frame.take(),
            supplied_source: self.source.take(),
            history: self.history,
            reset_discarded: self.reset_discarded.take(),
            incomplete_at_end: self.incomplete.take(),
        };
        self.phase = Phase::Finished;
        Ok((state, out))
    }
}

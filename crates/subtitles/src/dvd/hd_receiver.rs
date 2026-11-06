//! Explicit single-input fresh-context receiver lifecycle. Source rejection is
//! retained separately from a decoded image; it never becomes a valid frame.
//! Fragment reassembly and the zero-progress adapter are separate.
use super::{HdFacts, HdLimits, HdPolicy, HdSpuJob, HdStatus, HdSubpicture};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, Result, WorkBudget};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HdReceiverStatus {
    Yield,
    NeedInputEnd,
    Ready,
}
#[derive(Debug)]
pub struct HdReceiverOutput {
    packet: Bytes,
    picture: Option<HdSubpicture>,
    source_error: Option<Error>,
    facts: HdFacts,
    context_facts: HdFacts,
}
impl HdReceiverOutput {
    pub fn packet(&self) -> &Bytes {
        &self.packet
    }
    pub fn picture(&self) -> Option<&HdSubpicture> {
        self.picture.as_ref()
    }
    pub fn source_error(&self) -> Option<Error> {
        self.source_error
    }
    pub fn source_facts(&self) -> HdFacts {
        self.facts
    }
    /// All parsed control facts at the native stop boundary, including later palette-only controls.
    pub fn parsed_context_facts(&self) -> HdFacts {
        self.context_facts
    }
    pub fn into_parts(self) -> (Bytes, Option<HdSubpicture>, Option<Error>, HdFacts) {
        (self.packet, self.picture, self.source_error, self.facts)
    }
}
/// The supplied owned bytes are one externally framed SPU, or an explicitly
/// ended incomplete input. This job does not pretend packet fragments are a
/// complete source frame or reproduce the reference adapter's progress loop.
#[derive(Debug)]
pub struct HdReceiverJob {
    packet: Option<Bytes>,
    budget: Budget,
    limits: HdLimits,
    allow_prefix: bool,
    inner: Option<HdSpuJob>,
    picture: Option<HdSubpicture>,
    source_error: Option<Error>,
    facts: HdFacts,
    context_facts: HdFacts,
    setup: bool,
    input_ended: bool,
    ready: bool,
    finished: bool,
    failed: Option<Error>,
    work: usize,
}
fn source_rejection(e: Error) -> bool {
    match e {
        Error::Truncated => true,
        Error::Invalid(reason) => matches!(
            reason,
            "HD SPU control extent"
                | "HD control before control area"
                | "HD field outside pixel data"
                | "HD run exceeds source row"
                | "DVD reversed or overflowing display area"
        ),
        Error::Unsupported("HD no locally eligible raster") => true,
        _ => false,
    }
}
impl HdReceiverJob {
    pub fn new(packet: Bytes, budget: &Budget, limits: HdLimits) -> Result<Self> {
        if !packet.belongs_to(budget) {
            return Err(Error::ForeignPool);
        }
        if packet.len() > limits.max_packet {
            return Err(Error::Limit {
                requested: packet.len(),
                maximum: limits.max_packet,
            });
        }
        Ok(Self {
            packet: Some(packet),
            budget: budget.clone(),
            limits,
            allow_prefix: false,
            inner: None,
            picture: None,
            source_error: None,
            facts: HdFacts::default(),
            context_facts: HdFacts::default(),
            setup: false,
            input_ended: false,
            ready: false,
            finished: false,
            failed: None,
            work: 0,
        })
    }
    pub(crate) fn with_prefix(packet: Bytes, budget: &Budget, limits: HdLimits) -> Result<Self> {
        let mut j = Self::new(packet, budget, limits)?;
        j.allow_prefix = true;
        Ok(j)
    }
    pub fn finish_input(&mut self) {
        self.input_ended = true;
    }
    pub fn work_spent(&self) -> usize {
        self.work
    }
    fn poison(&mut self, e: Error) -> Error {
        self.inner = None;
        self.picture = None;
        self.packet = None;
        self.failed = Some(e);
        e
    }
    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<HdReceiverStatus> {
        if let Some(e) = self.failed {
            return Err(e);
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(self.poison(Error::Cancelled));
        }
        if self.finished {
            return Err(self.poison(Error::Invalid("HD receiver already finished")));
        }
        if self.ready {
            return Ok(HdReceiverStatus::Ready);
        }
        let result = self.inner_poll(work, cancel);
        match result {
            Err(e @ Error::Backpressure { .. }) => Err(e),
            Err(e) => Err(self.poison(e)),
            value => value,
        }
    }
    fn inner_poll(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<HdReceiverStatus> {
        let allow_freeze = work.remaining() != 0;
        if !self.setup {
            if work.remaining() == 0 {
                return Ok(HdReceiverStatus::Yield);
            }
            if self.limits.max_work == 0 {
                return Err(Error::Limit {
                    requested: 1,
                    maximum: 0,
                });
            }
            if !work.try_charge(1) {
                return Ok(HdReceiverStatus::Yield);
            }
            self.work = 1;
            self.setup = true;
            let raw = self
                .packet
                .as_ref()
                .ok_or(Error::Invalid("HD receiver source absent"))?
                .clone();
            let mut inner_limits = self.limits;
            inner_limits.max_work -= 1;
            let create = if self.allow_prefix {
                HdSpuJob::with_native_prefix(raw, &self.budget, inner_limits)
            } else {
                HdSpuJob::with_policy(raw, &self.budget, inner_limits, HdPolicy::Receiver)
            };
            match create {
                Ok(job) => self.inner = Some(job),
                Err(e) if source_rejection(e) => self.source_error = Some(e),
                Err(e) => return Err(e),
            }
        }
        if let Some(job) = self.inner.as_mut() {
            let status = job.poll(work, cancel);
            self.work = 1 + job.work_spent();
            match status {
                Ok(HdStatus::Yield) => return Ok(HdReceiverStatus::Yield),
                Ok(HdStatus::Ready) => {
                    if !allow_freeze {
                        return Ok(HdReceiverStatus::Yield);
                    }
                    let context_facts = job.parsed_context_facts();
                    let picture = job.try_finish(cancel)?;
                    self.context_facts = context_facts;
                    self.facts = picture.facts();
                    self.picture = Some(picture);
                    self.inner = None;
                    self.ready = true;
                    return Ok(HdReceiverStatus::Ready);
                }
                Err(e) if source_rejection(e) => {
                    self.facts = job.source_facts();
                    self.context_facts = job.parsed_context_facts();
                    self.source_error = Some(e);
                    self.inner = None;
                }
                Err(e) => return Err(e),
            }
        }
        if !self.input_ended {
            return Ok(HdReceiverStatus::NeedInputEnd);
        }
        self.ready = true;
        Ok(HdReceiverStatus::Ready)
    }
    pub fn try_finish(&mut self, cancel: &AtomicBool) -> Result<HdReceiverOutput> {
        if let Some(e) = self.failed {
            return Err(e);
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(self.poison(Error::Cancelled));
        }
        if !self.ready || self.finished {
            return Err(Error::Invalid("HD receiver not ready"));
        }
        let packet = self
            .packet
            .take()
            .ok_or(Error::Invalid("HD receiver source absent"))?;
        self.finished = true;
        Ok(HdReceiverOutput {
            packet,
            picture: self.picture.take(),
            source_error: self.source_error,
            facts: self.facts,
            context_facts: self.context_facts,
        })
    }
}

//! Explicit native physical-byte reads. Source declarations are never repaired.
use crate::{Mov9RecoveryV3Event, Mov9RecoveryV3Read};
use std::{ops::Range, sync::atomic::AtomicBool};
use triergon_core::{
    BudgetVec, Bytes, Error, InputCursor, InputToken, Result, SourceId, Step, WorkBudget,
};

#[derive(Clone, Copy, Debug)]
pub struct PinnedMovD32PhysicalReadV4;

/// Physical availability is a source fact. The short-packet/terminal projection
/// is the separately selected empirical D32 operation, never an I/O receipt.
#[derive(Clone, Debug)]
pub struct Mov9PhysicalReadV4 {
    origin_pool: triergon_core::Budget,
    origin_source: SourceId,
    origin_token: InputToken,
    requested: Range<u64>,
    available: Range<u64>,
    physical_length: u64,
    declared_media: Range<u64>,
    source_range_error: Option<Error>,
    short_at_physical_end: bool,
}
impl Mov9PhysicalReadV4 {
    pub fn from_event(
        event: &Mov9RecoveryV3Event,
        input: &InputCursor,
        declared_media: Range<u64>,
        maximum_packet: usize,
        _profile: PinnedMovD32PhysicalReadV4,
    ) -> Result<Self> {
        let physical_length = input.capabilities().length.ok_or(Error::Unsupported(
            "v4 physical read needs known source length",
        ))?;
        if !input.capabilities().random_access {
            return Err(Error::Unsupported(
                "v4 physical read needs the pinned random-access source",
            ));
        }
        let packet = event
            .packet
            .as_ref()
            .ok_or(Error::Invalid("v4 suppressed native read"))?;
        let requested = packet.source_range.clone();
        let size = requested
            .end
            .checked_sub(requested.start)
            .ok_or(Error::Overflow)?;
        if size == 0 {
            return Err(Error::Unsupported(
                "v4 physical prefix requires a positive native request",
            ));
        }
        let size = usize::try_from(size).map_err(|_| Error::Overflow)?;
        if size > maximum_packet {
            return Err(Error::Limit {
                requested: size,
                maximum: maximum_packet,
            });
        }
        if declared_media.start > declared_media.end || declared_media.end > physical_length {
            return Err(Error::Unsupported("v4 physical source/media binding"));
        }
        if event.read != Mov9RecoveryV3Read::Available || requested.start >= physical_length {
            return Err(Error::Unsupported(
                "v4 no positive physical prefix; native terminal transition remains separate",
            ));
        }
        let available = requested.start..requested.end.min(physical_length);
        Ok(Self {
            origin_pool: input.budget().clone(),
            origin_source: input.source(),
            origin_token: input.token(),
            short_at_physical_end: available.end < requested.end,
            requested,
            available,
            physical_length,
            declared_media,
            source_range_error: event.range_error,
        })
    }
    pub fn requested(&self) -> Range<u64> {
        self.requested.clone()
    }
    pub fn available(&self) -> Range<u64> {
        self.available.clone()
    }
    pub fn source_range_error(&self) -> Option<Error> {
        self.source_range_error
    }
    pub fn declared_media(&self) -> Range<u64> {
        self.declared_media.clone()
    }
    pub fn is_short_at_physical_end(&self) -> bool {
        self.short_at_physical_end
    }
    pub fn physical_length(&self) -> u64 {
        self.physical_length
    }
    /// The admitted contiguous-packet operation ends after a positive short
    /// prefix followed by its immediately adjacent, physically absent request.
    /// This is an empirical projection, not a native cursor/EOF observation.
    /// A noncontiguous chunk is not silently discarded or modeled as a seek.
    pub fn modeled_following_eof(
        &self,
        next: &Mov9RecoveryV3Event,
        _profile: PinnedMovD32PhysicalReadV4,
    ) -> bool {
        if !self.short_at_physical_end || next.read != Mov9RecoveryV3Read::UnavailablePhysicalStart
        {
            return false;
        }
        let Some(packet) = &next.packet else {
            return false;
        };
        let Some(size) = packet
            .source_range
            .end
            .checked_sub(packet.source_range.start)
        else {
            return false;
        };
        size != 0
            && size <= 16 * 1024 * 1024
            && packet.source_range.start == self.requested.end
            && packet.source_range.start >= self.physical_length
    }
}

#[derive(Clone, Debug)]
pub struct Mov9PhysicalPayloadV4 {
    pub event: Mov9RecoveryV3Event,
    pub plan: Mov9PhysicalReadV4,
    pub payload: Bytes,
    pub source: SourceId,
    pub token: InputToken,
}

/// A core-bound native byte transaction, never a replacement source Sample.
/// The original event accompanies the retained output. Range, source, pool,
/// generation and request identities are checked before every read/publication.
#[derive(Debug)]
pub struct Mov9PhysicalReaderV4 {
    event: Mov9RecoveryV3Event,
    plan: Mov9PhysicalReadV4,
    token: InputToken,
    position: u64,
    length: usize,
    buffer: Option<BudgetVec<u8>>,
    freeze_paid: bool,
    failed: Option<Error>,
}
impl Mov9PhysicalReaderV4 {
    pub fn new(
        input: &mut InputCursor,
        event: Mov9RecoveryV3Event,
        plan: Mov9PhysicalReadV4,
        maximum_packet: usize,
        _profile: PinnedMovD32PhysicalReadV4,
    ) -> Result<Self> {
        if !input.budget().same_pool(&plan.origin_pool) {
            return Err(Error::ForeignPool);
        }
        if input.source() != plan.origin_source {
            return Err(Error::SourceMismatch);
        }
        require_token(input.token(), &plan.origin_token)?;
        if input.capabilities().length != Some(plan.physical_length)
            || !input.capabilities().random_access
            || event.packet.as_ref().map(|p| &p.source_range) != Some(&plan.requested)
            || event.range_error != plan.source_range_error
            || event.read != Mov9RecoveryV3Read::Available
        {
            return Err(Error::Unsupported("v4 physical read event/source binding"));
        }
        let size = usize::try_from(
            plan.requested
                .end
                .checked_sub(plan.requested.start)
                .ok_or(Error::Overflow)?,
        )
        .map_err(|_| Error::Overflow)?;
        if size > maximum_packet {
            return Err(Error::Limit {
                requested: size,
                maximum: maximum_packet,
            });
        }
        let length = usize::try_from(
            plan.available
                .end
                .checked_sub(plan.available.start)
                .ok_or(Error::Overflow)?,
        )
        .map_err(|_| Error::Overflow)?;
        let mut buffer = BudgetVec::new(input.budget());
        buffer.try_reserve_exact(length)?;
        input.request_range(plan.available.start)?;
        Ok(Self {
            event,
            token: input.token(),
            position: plan.available.start,
            plan,
            length,
            buffer: Some(buffer),
            freeze_paid: false,
            failed: None,
        })
    }
    pub fn poll(
        &mut self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<Mov9PhysicalPayloadV4>> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = self.poll_inner(input, work, cancel);
        if let Err(error) = result {
            if !matches!(error, Error::Backpressure { .. }) {
                self.buffer = None;
                self.failed = Some(error);
            }
        }
        result
    }
    fn poll_inner(
        &mut self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<Mov9PhysicalPayloadV4>> {
        if !input.budget().same_pool(&self.plan.origin_pool) {
            return Err(Error::ForeignPool);
        }
        if input.source() != self.plan.origin_source {
            return Err(Error::SourceMismatch);
        }
        require_token(input.token(), &self.token)?;
        if input.position() != self.position {
            return Err(Error::Invalid("v4 input changed outside physical reader"));
        }
        input.read_into(&mut [], work, cancel)?;
        if work.remaining() == 0 {
            return Ok(Step::Yield);
        }
        loop {
            let buffer = self
                .buffer
                .as_mut()
                .ok_or(Error::Invalid("v4 physical reader completed"))?;
            if buffer.len() == self.length {
                if work.remaining() == 0 {
                    return Ok(Step::Yield);
                }
                if !self.freeze_paid {
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    self.freeze_paid = true;
                }
                let buffer = self
                    .buffer
                    .take()
                    .ok_or(Error::Invalid("v4 physical buffer absent"))?;
                let payload = match buffer.freeze() {
                    Ok(payload) => payload,
                    Err(failure) => {
                        self.buffer = Some(failure.buffer);
                        return Err(failure.error);
                    }
                };
                return Ok(Step::Ready(Mov9PhysicalPayloadV4 {
                    event: self.event.clone(),
                    plan: self.plan.clone(),
                    payload,
                    source: input.source(),
                    token: self.token.clone(),
                }));
            }
            let remaining = self.length - buffer.len();
            match input.read_append(buffer, remaining, work, cancel)? {
                Step::Ready(_) => {
                    self.position = input.position();
                }
                Step::NeedInput(request) => return Ok(Step::NeedInput(request)),
                Step::Yield => return Ok(Step::Yield),
                Step::End(_) => return Err(Error::Truncated),
            }
        }
    }
}
fn require_token(token: InputToken, expected: &InputToken) -> Result<()> {
    if token.generation() != expected.generation() {
        return Err(Error::StaleGeneration {
            expected: expected.generation(),
            received: token.generation(),
        });
    }
    if token.request() != expected.request() {
        return Err(Error::StaleRequest {
            expected: expected.request(),
            received: token.request(),
        });
    }
    if token != *expected {
        return Err(Error::ForeignPool);
    }
    Ok(())
}

//! Format-specific <=192-byte carry, not an I/O cursor/token framework.
use crate::{PacketFormat, TransportPacket, parse_transport};
use safe_media_core::{
    Budget, BudgetVec, Error, InputCursor, InputToken, Result, SourceId, Step, WorkBudget,
};
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug)]
#[allow(
    clippy::large_enum_variant,
    reason = "one bounded inline packet per poll; boxing would introduce another allocation and budget owner"
)]
pub enum FrameState {
    Packet(TransportPacket),
    NeedInput,
    Yield,
    Error(Error),
}
#[derive(Debug)]
pub struct FrameStep {
    pub consumed: usize,
    pub state: FrameState,
}

#[derive(Debug)]
pub struct TransportFramer {
    budget: Budget,
    format: PacketFormat,
    carry: Option<BudgetVec<u8>>,
    start: Option<u64>,
    next: Option<u64>,
    token: Option<InputToken>,
    entity: Option<SourceId>,
    entity_end_seen: bool,
    last_packet_offset: Option<u64>,
}

impl TransportFramer {
    pub fn new(budget: Budget, format: PacketFormat) -> Self {
        Self {
            carry: Some(BudgetVec::new(&budget)),
            budget,
            format,
            start: None,
            next: None,
            token: None,
            entity: None,
            entity_end_seen: false,
            last_packet_offset: None,
        }
    }
    pub fn discard(&mut self) {
        self.carry = Some(BudgetVec::new(&self.budget));
        self.start = None;
        self.next = None;
        self.token = None;
        self.entity = None;
        self.entity_end_seen = false;
        self.last_packet_offset = None;
    }
    pub fn pending_or_last_packet_offset(&self) -> Option<u64> {
        self.start.or(self.last_packet_offset)
    }
    fn complete(&mut self) -> Result<TransportPacket> {
        let carry = self
            .carry
            .take()
            .ok_or(Error::Invalid("missing packet carry"))?;
        let source = match carry.freeze() {
            Ok(source) => source,
            Err(error) => {
                self.carry = Some(error.buffer);
                return Err(error.error);
            }
        };
        let start = self
            .start
            .take()
            .ok_or(Error::Invalid("missing packet start"))?;
        self.carry = Some(BudgetVec::new(&self.budget));
        self.last_packet_offset = Some(start);
        let mut packet = parse_transport(source, self.format, start)?;
        packet.source_id = self.entity;
        Ok(packet)
    }
    /// Uses the shared cursor's source/pool/generation/request/failure contract.
    /// A completed entity is surfaced as Step::End; the format coordinator decides
    /// logical EOF versus continue_source. PES state is unaffected by this reader.
    pub fn poll(
        &mut self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<TransportPacket>> {
        if !input.budget().same_pool(&self.budget) {
            return Err(Error::ForeignPool);
        }
        let token = input.token();
        if let Some(old) = self.token.as_ref() {
            if old.generation() != token.generation() {
                return Err(Error::StaleGeneration {
                    expected: old.generation(),
                    received: token.generation(),
                });
            }
            if old.request() != token.request() {
                let empty = self.carry.as_ref().is_some_and(|carry| carry.is_empty());
                if !self.entity_end_seen || !empty {
                    return Err(Error::StaleRequest {
                        expected: old.request(),
                        received: token.request(),
                    });
                }
                self.start = None;
                self.next = None;
                self.entity_end_seen = false;
            }
        }
        self.token = Some(token);
        self.entity = Some(input.source());
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if self.next.is_some_and(|offset| offset != input.position()) {
                return Err(Error::Invalid("TS cursor moved without format reset"));
            }
            let carry = self
                .carry
                .as_mut()
                .ok_or(Error::Invalid("missing packet carry"))?;
            let missing = self.format.bytes() - carry.len();
            if missing == 0 {
                return self.complete().map(Step::Ready);
            }
            // Reserve before advancing the shared cursor; a failed allocation has
            // consumed no source bytes. A complete carry survives failed freeze.
            carry.try_reserve_exact(missing)?;
            let mut scratch = [0_u8; 192];
            let offset = input.position();
            match input.read_into(&mut scratch[..missing], work, cancel)? {
                Step::Ready(count) => {
                    carry.try_extend_from_slice(&scratch[..count])?;
                    if self.start.is_none() {
                        self.start = Some(offset);
                    }
                    self.next = Some(input.position());
                }
                Step::NeedInput(request) => return Ok(Step::NeedInput(request)),
                Step::Yield => return Ok(Step::Yield),
                Step::End(location) => {
                    if !carry.is_empty() {
                        return Err(Error::Truncated);
                    }
                    self.entity_end_seen = true;
                    return Ok(Step::End(location));
                }
            }
        }
    }
    /// `offset` identifies input[0], not the start of a previous partial packet.
    /// Always honour consumed, including on a later freeze/parse error. Backpressure
    /// retains a complete carry for retry and never consumes the next source packet.
    pub fn push(
        &mut self,
        input: &[u8],
        offset: u64,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> FrameStep {
        let mut consumed = 0;
        let result = (|| {
            if cancel.load(Ordering::Relaxed) {
                return FrameState::Error(Error::Cancelled);
            }
            if self.token.is_some() {
                return FrameState::Error(Error::Invalid("mixed TS feed modes"));
            }
            if self.next.is_some_and(|expected| expected != offset) {
                return FrameState::Error(Error::Invalid("non-contiguous transport input"));
            }
            let carry = match self.carry.as_mut() {
                Some(carry) => carry,
                None => return FrameState::Error(Error::Invalid("missing packet carry")),
            };
            let missing = self.format.bytes() - carry.len();
            if missing != 0 {
                if input.is_empty() {
                    return FrameState::NeedInput;
                }
                if let Err(error) = carry.try_reserve_exact(missing) {
                    return FrameState::Error(error);
                }
                let maximum = missing.min(input.len());
                let count = usize::try_from(work.take(maximum as u64)).unwrap_or(maximum);
                if count == 0 {
                    return FrameState::Yield;
                }
                let next = match offset.checked_add(count as u64) {
                    Some(next) => next,
                    None => return FrameState::Error(Error::Overflow),
                };
                if let Err(error) = carry.try_extend_from_slice(&input[..count]) {
                    return FrameState::Error(error);
                }
                if self.start.is_none() {
                    self.start = Some(offset);
                }
                consumed = count;
                self.next = Some(next);
                if carry.len() != self.format.bytes() {
                    return if consumed == input.len() {
                        FrameState::NeedInput
                    } else {
                        FrameState::Yield
                    };
                }
            }
            match self.complete() {
                Ok(packet) => FrameState::Packet(packet),
                Err(error) => FrameState::Error(error),
            }
        })();
        FrameStep {
            consumed,
            state: result,
        }
    }
    /// Only call for definitive source EOF, never for starvation/EndSegment/failure.
    pub fn finish(&self) -> safe_media_core::Result<()> {
        if self.carry.as_ref().is_some_and(|carry| !carry.is_empty()) {
            Err(Error::Truncated)
        } else {
            Ok(())
        }
    }
}

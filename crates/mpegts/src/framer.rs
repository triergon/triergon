//! Format-specific <=192-byte carry, not an I/O cursor/token framework.
use crate::{
    AssemblyStep, PacketFormat, RejectedTransport, TransportInspection, TransportInspector,
    TransportPacket,
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, BudgetVec, Bytes, Error, InputCursor, InputToken, Result, SourceId, SourceLocation,
    Step, WorkBudget,
};

/// A validated run of null carriers on the source grid. The shared backing
/// span includes every opaque M2TS prefix and every byte of null payload.
#[derive(Clone, Debug)]
pub struct NullCarrierRun {
    bytes: Bytes,
    format: PacketFormat,
    start: SourceLocation,
}

impl NullCarrierRun {
    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }

    pub fn format(&self) -> PacketFormat {
        self.format
    }

    pub fn start(&self) -> SourceLocation {
        self.start
    }

    pub fn len(&self) -> usize {
        self.bytes.len() / self.format.bytes()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Expand one retained carrier through the normal strict inspector.
    pub fn packet(&self, index: usize) -> Result<TransportPacket> {
        if index >= self.len() {
            return Err(Error::Invalid("null carrier index"));
        }
        let stride = self.format.bytes();
        let at = index * stride;
        let bytes = self.bytes.slice(at..at + stride)?;
        let mut packet = crate::parse_transport(bytes, self.format, self.start.offset + at as u64)?;
        packet.source_id = Some(self.start.source);
        Ok(packet)
    }
}

// H.222.0 2.4.3.3–4. Limit the fast path to clear, unflagged null
// packets with payload and no adaptation syntax beyond a zero flags byte.
fn plain_null(ts: &[u8]) -> bool {
    if ts[0] != 0x47 || ts[1] & 0xdf != 0x1f || ts[2] != 0xff {
        return false;
    }
    match ts[3] & 0xf0 {
        0x10 => true,
        0x30 => ts[4] == 0 || (ts[4] == 1 && ts[5] == 0),
        _ => false,
    }
}

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
    share_input: bool,
    carry: Option<BudgetVec<u8>>,
    start: Option<u64>,
    next: Option<u64>,
    token: Option<InputToken>,
    entity: Option<SourceId>,
    entity_end_seen: bool,
    last_packet_offset: Option<u64>,
    inspection: Option<TransportInspector>,
    rejection: Option<RejectedTransport>,
    push_ack: Option<(u64, usize)>,
}

impl TransportFramer {
    pub fn new(budget: Budget, format: PacketFormat) -> Self {
        Self {
            carry: Some(BudgetVec::new(&budget)),
            budget,
            format,
            share_input: false,
            start: None,
            next: None,
            token: None,
            entity: None,
            entity_end_seen: false,
            last_packet_offset: None,
            inspection: None,
            rejection: None,
            push_ack: None,
        }
    }
    pub(crate) fn share_input(&mut self) {
        self.share_input = true;
    }

    pub fn discard(&mut self) {
        self.carry = Some(BudgetVec::new(&self.budget));
        self.start = None;
        self.next = None;
        self.token = None;
        self.entity = None;
        self.entity_end_seen = false;
        self.last_packet_offset = None;
        self.inspection = None;
        self.rejection = None;
        self.push_ack = None;
    }
    pub fn pending_or_last_packet_offset(&self) -> Option<u64> {
        self.start.or(self.last_packet_offset)
    }
    pub(crate) fn observed_entity_end(&self, input: &InputCursor) -> bool {
        self.entity_end_seen
            && self.entity == Some(input.source())
            && self.token.as_ref() == Some(&input.token())
            && self.inspection.is_none()
            && self.push_ack.is_none()
            && self.carry.as_ref().is_some_and(|carry| carry.is_empty())
    }
    /// Take the real owned syntax rejection before advancing/discarding this framer.
    pub fn take_rejection(&mut self) -> Option<RejectedTransport> {
        self.rejection.take()
    }
    fn complete(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<TransportPacket>> {
        if self.inspection.is_none() {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if !work.try_charge(1) {
                return Ok(None);
            }
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
            self.inspect(source, start)?;
        }
        let result = self
            .inspection
            .as_mut()
            .ok_or(Error::Invalid("missing carrier inspection"))?
            .poll(work, cancel)?;
        match result {
            AssemblyStep::Yield => Ok(None),
            AssemblyStep::Output(TransportInspection::Accepted(packet)) => {
                self.inspection = None;
                Ok(Some(packet))
            }
            AssemblyStep::Output(TransportInspection::Rejected(rejected)) => {
                let error = rejected.error();
                self.rejection = Some(rejected);
                self.inspection = None;
                Err(error)
            }
            _ => Err(Error::Invalid("carrier inspection state")),
        }
    }
    fn inspect(&mut self, source: Bytes, start: u64) -> Result<()> {
        self.last_packet_offset = Some(start);
        self.inspection = Some(
            TransportInspector::detached(source, self.format, start, self.entity)
                .map_err(|failure| failure.error)?,
        );
        Ok(())
    }

    pub(crate) fn try_null_run(
        &mut self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<NullCarrierRun>> {
        // A changed binding, partial carrier or outstanding entity boundary
        // must go through the regular framer state machine.
        if !input.budget().same_pool(&self.budget)
            || self.entity != Some(input.source())
            || self.token.as_ref() != Some(&input.token())
            || self.next != Some(input.position())
            || self.entity_end_seen
            || self.start.is_some()
            || self.inspection.is_some()
            || self.rejection.is_some()
            || self.push_ack.is_some()
            || self.carry.as_ref().is_none_or(|carry| !carry.is_empty())
        {
            return Ok(None);
        }
        let stride = self.format.bytes();
        if input.position() % stride as u64 != 0 {
            return Ok(None);
        }
        let prefix = self.format.prefix_bytes();
        let granted = (work.remaining() / stride as u64).min(128) as usize;
        let mut count = 0;
        for carrier in input.buffered().chunks_exact(stride).take(granted) {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if !plain_null(&carrier[prefix..]) {
                break;
            }
            count += 1;
        }
        if count == 0 {
            return Ok(None);
        }
        let start = SourceLocation {
            source: input.source(),
            offset: input.position(),
        };
        let Step::Ready(bytes) = input.read_span(count * stride, work, cancel)? else {
            return Err(Error::Invalid("validated null run lost buffered span"));
        };
        self.next = Some(input.position());
        self.last_packet_offset = Some(input.position() - stride as u64);
        Ok(Some(NullCarrierRun {
            bytes,
            format: self.format,
            start,
        }))
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
            if self.inspection.is_some() {
                return self
                    .complete(work, cancel)
                    .map(|value| value.map_or(Step::Yield, Step::Ready));
            }
            if work.remaining() == 0 {
                return Ok(Step::Yield);
            }
            let carry = self
                .carry
                .as_mut()
                .ok_or(Error::Invalid("missing packet carry"))?;
            let missing = self.format.bytes() - carry.len();
            if missing == 0 {
                return self
                    .complete(work, cancel)
                    .map(|value| value.map_or(Step::Yield, Step::Ready));
            }
            // Segment feeds already retain source windows. Share a complete
            // carrier and use the same inspector as split or budgeted reads.
            if self.share_input
                && carry.is_empty()
                && input.buffered().len() >= missing
                && work.remaining() >= missing as u64
            {
                let start = input.position();
                let Step::Ready(source) = input.read_span(missing, work, cancel)? else {
                    return Err(Error::Invalid("buffered carrier span missing"));
                };
                self.next = Some(input.position());
                self.inspect(source, start)?;
                continue;
            }
            // Reserve before advancing the shared cursor; a failed allocation has
            // consumed no source bytes. A complete carry survives failed freeze.
            if carry.capacity() < carry.len() + missing {
                if !work.try_charge(1) {
                    return Ok(Step::Yield);
                }
                carry.try_reserve_exact(missing)?;
            }
            let offset = input.position();
            match input.read_append(carry, missing, work, cancel)? {
                Step::Ready(_) => {
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
    /// `offset` identifies `input[0]`, not the start of a previous partial packet.
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
        let state = (|| {
            if cancel.load(Ordering::Relaxed) {
                return FrameState::Error(Error::Cancelled);
            }
            if self.token.is_some() {
                return FrameState::Error(Error::Invalid("mixed TS feed modes"));
            }
            if let Some((held_offset, count)) = self.push_ack {
                if offset != held_offset {
                    return FrameState::Error(Error::Invalid("pending TS input offset"));
                }
                return match self.complete(work, cancel) {
                    Ok(Some(packet)) => {
                        consumed = count;
                        self.push_ack = None;
                        FrameState::Packet(packet)
                    }
                    Ok(None) => FrameState::Yield,
                    Err(error) => {
                        if !matches!(
                            error,
                            Error::Backpressure { .. } | Error::Allocation | Error::Cancelled
                        ) {
                            consumed = count;
                            self.push_ack = None;
                        }
                        FrameState::Error(error)
                    }
                };
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
                if work.remaining() == 0 {
                    return FrameState::Yield;
                }
                if carry.capacity() < carry.len() + missing {
                    if !work.try_charge(1) {
                        return FrameState::Yield;
                    }
                    if let Err(error) = carry.try_reserve_exact(missing) {
                        return FrameState::Error(error);
                    }
                }
                let count =
                    usize::try_from(work.take(missing.min(input.len()) as u64)).unwrap_or(0);
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
                self.next = Some(next);
                if carry.len() != self.format.bytes() {
                    consumed = count;
                    return FrameState::NeedInput;
                }
                // Hold acknowledgement of this final chunk until its actual field
                // inspection finishes. Retried input is never copied/read again.
                self.push_ack = Some((offset, count));
            }
            match self.complete(work, cancel) {
                Ok(Some(packet)) => {
                    consumed = self.push_ack.take().map_or(0, |(_, count)| count);
                    FrameState::Packet(packet)
                }
                Ok(None) => FrameState::Yield,
                Err(error) => {
                    if !matches!(
                        error,
                        Error::Backpressure { .. } | Error::Allocation | Error::Cancelled
                    ) {
                        consumed = self.push_ack.take().map_or(0, |(_, count)| count);
                    }
                    FrameState::Error(error)
                }
            }
        })();
        FrameStep { consumed, state }
    }
    /// Only call for definitive source EOF, never for starvation/EndSegment/failure.
    pub fn finish(&self) -> triergon_core::Result<()> {
        if self.inspection.is_some() || self.push_ack.is_some() {
            return Err(Error::Invalid("TS inspection pending"));
        }
        if self.carry.as_ref().is_some_and(|carry| !carry.is_empty()) {
            Err(Error::Truncated)
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use triergon_core::{InputWindow, SourceCapabilities};

    #[test]
    fn shared_segment_carriers_keep_their_owner_and_rejection_offsets() {
        let budget = Budget::new(8192).unwrap();
        let mut input = InputCursor::new(
            &budget,
            SourceId(1),
            SourceCapabilities {
                length: Some(376),
                random_access: false,
            },
            376,
        )
        .unwrap();
        let mut wire = [0xff; 376];
        wire[..4].copy_from_slice(&[0x47, 0x1f, 0xff, 0x10]);
        let window = Bytes::copy_from_slice(&budget, &wire).unwrap();
        let request = input.request();
        input
            .provide_owned(
                InputWindow::new(request.source, request.token, 0, window.clone()).unwrap(),
            )
            .unwrap();
        let mut framer = TransportFramer::new(budget, PacketFormat::Ts188);
        framer.share_input();
        let cancel = AtomicBool::new(false);
        // Spend only the carrier read this call. Inspection must resume
        // without consuming the next carrier.
        assert!(matches!(
            framer
                .poll(&mut input, &mut WorkBudget::new(188), &cancel)
                .unwrap(),
            Step::Yield
        ));
        let mut work = WorkBudget::new(4096);
        let Step::Ready(packet) = framer.poll(&mut input, &mut work, &cancel).unwrap() else {
            panic!("expected the buffered carrier");
        };
        assert!(packet.source.shares_storage_with(&window));
        assert_eq!(input.position(), 188);
        assert_eq!(
            framer.poll(&mut input, &mut work, &cancel).unwrap_err(),
            Error::Invalid("TS sync byte")
        );
        let rejected = framer.take_rejection().unwrap();
        assert_eq!(rejected.source_offset(), 188);
        assert_eq!(rejected.bytes().as_ref(), &wire[188..]);
        assert!(rejected.backing_bytes().shares_storage_with(&window));
    }

    #[test]
    fn null_runs_require_an_aligned_source_grid() {
        let budget = Budget::new(8192).unwrap();
        let mut input = InputCursor::new(
            &budget,
            SourceId(1),
            SourceCapabilities {
                length: None,
                random_access: true,
            },
            4096,
        )
        .unwrap();
        let request = input.reset(1).unwrap();
        let mut carrier = [0xff; 188];
        carrier[..4].copy_from_slice(&[0x47, 0x1f, 0xff, 0x10]);
        input
            .provide_owned(
                InputWindow::new(
                    request.source,
                    request.token,
                    request.offset,
                    Bytes::copy_from_slice(&budget, &[carrier, carrier].concat()).unwrap(),
                )
                .unwrap(),
            )
            .unwrap();
        let mut framer = TransportFramer::new(budget, PacketFormat::Ts188);
        let mut work = WorkBudget::new(4096);
        let cancel = AtomicBool::new(false);
        assert!(
            matches!(framer.poll(&mut input, &mut work, &cancel).unwrap(),
            Step::Ready(packet) if packet.source_offset == 1)
        );
        assert!(
            framer
                .try_null_run(&mut input, &mut work, &cancel)
                .unwrap()
                .is_none()
        );
        assert_eq!(input.position(), 189);
        assert!(
            matches!(framer.poll(&mut input, &mut work, &cancel).unwrap(),
            Step::Ready(packet) if packet.source_offset == 189)
        );
    }
}

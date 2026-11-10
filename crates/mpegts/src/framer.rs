//! Format-specific <=192-byte carry, not an I/O cursor/token framework.
use crate::carrier::{PlainCarrier, PlainContinuation, TS188_BYTES};
use crate::{
    AssemblyStep, PacketFormat, RejectedTransport, TransportInspection, TransportInspector,
    TransportPacket,
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, BudgetVec, ByteStorageAddress, Bytes, Error, InputCursor, InputToken, Result, SourceId,
    SourceLocation, Step, WorkBudget,
};

const MAX_SHARED_INPUT_CAPACITY: usize = 128 * 1024;
const MAX_PROGRESSIVE_SHARED_CAPACITY: usize = 4 * 1024;
const MAX_PROGRESSIVE_SHARED_WINDOWS: usize = 4;

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
#[allow(
    clippy::large_enum_variant,
    reason = "keep the literal carrier inline for allocation-free fallback"
)]
pub(crate) enum FramedCarrier {
    Packet(TransportPacket),
    Plain(PlainContinuation),
}

impl FramedCarrier {
    pub(crate) fn pid(&self) -> u16 {
        match self {
            Self::Packet(packet) => packet.pid,
            Self::Plain(packet) => packet.pid(),
        }
    }

    pub(crate) fn into_packet(self) -> Result<TransportPacket> {
        match self {
            Self::Packet(packet) => Ok(packet),
            Self::Plain(packet) => packet.into_packet(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BufferedCarrier {
    Carry,
    Shared,
    Progressive,
}

#[derive(Debug)]
pub struct TransportFramer {
    budget: Budget,
    format: PacketFormat,
    buffered_carrier: BufferedCarrier,
    shared_owner: Option<ByteStorageAddress>,
    progressive_windows: [Option<Bytes>; MAX_PROGRESSIVE_SHARED_WINDOWS],
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
            buffered_carrier: BufferedCarrier::Carry,
            shared_owner: None,
            progressive_windows: [const { None }; MAX_PROGRESSIVE_SHARED_WINDOWS],
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
        self.buffered_carrier = BufferedCarrier::Shared;
    }

    pub(crate) fn compact_progressive_input(&mut self) {
        self.buffered_carrier = BufferedCarrier::Progressive;
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
    pub(crate) fn accepts_observed_input(&self, input: &InputCursor) -> bool {
        self.token.is_none() || self.observed_input(input)
    }
    pub(crate) fn observed_input(&self, input: &InputCursor) -> bool {
        self.entity == Some(input.source()) && self.token.as_ref() == Some(&input.token())
    }
    pub(crate) fn observed_entity_end(&self, input: &InputCursor) -> bool {
        self.entity_end_seen
            && self.observed_input(input)
            && self.inspection.is_none()
            && self.push_ack.is_none()
            && self.carry.as_ref().is_some_and(BudgetVec::is_empty)
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
            // A complete carry can use the same accepted layout as a shared window.
            if self.format == PacketFormat::Ts188
                && !cancel.load(Ordering::Relaxed)
                && let Some(carrier) = PlainCarrier::inspect_ts188(&source, start, work)
            {
                self.last_packet_offset = Some(start);
                return carrier.into_packet(source, start, self.entity).map(Some);
            }
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

    fn share_buffered_owner(
        shared_owner: &mut Option<ByteStorageAddress>,
        input: &InputCursor,
    ) -> bool {
        let Some(window) = input.buffered_owner() else {
            return false;
        };
        // One bounded owner per segment. A short read backed by an oversized
        // allocation must not keep that capacity alive through a PES span.
        let capacity = window.retained_capacity();
        if capacity > MAX_SHARED_INPUT_CAPACITY || capacity > window.len().saturating_mul(8) {
            return false;
        }
        let address = window.storage_address();
        // Address reuse requires the former allocation and all its views to
        // have dropped. At most one shared backing allocation can be live.
        *shared_owner.get_or_insert(address) == address
    }

    fn release_unused_windows(retained: &mut [Option<Bytes>]) {
        for slot in retained {
            if slot
                .as_ref()
                .is_some_and(|window| !window.has_shared_storage())
            {
                *slot = None;
            }
        }
    }

    fn share_progressive_window(retained: &mut [Option<Bytes>], input: &InputCursor) -> bool {
        let Some(window) = input.buffered_owner() else {
            return false;
        };
        // The most recent window is first, so the next carrier usually
        // needs only one storage comparison.
        if let Some(index) = retained.iter().position(|slot| {
            slot.as_ref()
                .is_some_and(|previous| previous.shares_storage_with(window))
        }) {
            retained.swap(0, index);
            return true;
        }
        Self::release_unused_windows(retained);
        let Some(index) = retained.iter().position(Option::is_none) else {
            return false;
        };
        // Small, tight windows can stay charged while an unfinished PES
        // retains their carriers. Keep room for the next source reply.
        let capacity = window.retained_capacity();
        let charge = window.retained_storage_bytes();
        if capacity > MAX_PROGRESSIVE_SHARED_CAPACITY
            || capacity > window.len()
            || window.len() < 2 * PacketFormat::Ts188.bytes()
            || input.budget().available() < input.request().maximum.saturating_add(2 * charge)
        {
            return false;
        }
        retained[index] = Some(window.clone());
        retained.swap(0, index);
        true
    }

    /// The cursor is already bound by the feed loop. Reuse the window admitted
    /// by its last carrier so a probe never changes window retention.
    pub(crate) fn retained_payload_window(&self, input: &InputCursor) -> bool {
        self.buffered_carrier == BufferedCarrier::Progressive
            && self.format == PacketFormat::Ts188
            && self.next == Some(input.position())
            && !self.entity_end_seen
            && self.start.is_none()
            && self.inspection.is_none()
            && self.rejection.is_none()
            && self.push_ack.is_none()
            && self.carry.as_ref().is_some_and(BudgetVec::is_empty)
            && self.progressive_windows[0]
                .as_ref()
                .zip(input.buffered_owner())
                .is_some_and(|(retained, current)| retained.shares_storage_with(current))
    }

    /// Consume a preflighted, complete prefix of the already shared window.
    pub(crate) fn read_payload_run(
        &mut self,
        input: &mut InputCursor,
        carriers: usize,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Bytes> {
        debug_assert!(self.retained_payload_window(input));
        let width = carriers * TS188_BYTES;
        let Step::Ready(bytes) = input.read_span(width, work, cancel)? else {
            return Err(Error::Invalid("validated payload run lost buffered span"));
        };
        debug_assert_eq!(bytes.len(), width);
        self.next = Some(input.position());
        self.last_packet_offset = Some(input.position() - TS188_BYTES as u64);
        Ok(bytes)
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
        if !input.position().is_multiple_of(stride as u64) {
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
        match self.poll_carrier::<false>(input, work, cancel)? {
            Step::Ready(carrier) => Ok(Step::Ready(carrier.into_packet()?)),
            Step::NeedInput(request) => Ok(Step::NeedInput(request)),
            Step::Yield => Ok(Step::Yield),
            Step::End(location) => Ok(Step::End(location)),
        }
    }

    pub(crate) fn poll_carrier<const DIRECT: bool>(
        &mut self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<FramedCarrier>> {
        self.bind_input(input)?;
        self.poll_bound_carrier::<DIRECT>(input, work, cancel)
    }

    pub(crate) fn bind_input(&mut self, input: &InputCursor) -> Result<()> {
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
                let empty = self.carry.as_ref().is_some_and(BudgetVec::is_empty);
                if !self.entity_end_seen || !empty {
                    return Err(Error::StaleRequest {
                        expected: old.request(),
                        received: token.request(),
                    });
                }
                self.start = None;
                self.next = None;
                self.entity_end_seen = false;
                self.shared_owner = None;
            }
        }
        self.token = Some(token);
        self.entity = Some(input.source());
        Ok(())
    }

    /// The caller has bound this cursor and kept exclusive access since then.
    pub(crate) fn poll_bound_carrier<const DIRECT: bool>(
        &mut self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<FramedCarrier>> {
        fn ready(packet: TransportPacket) -> Step<FramedCarrier> {
            Step::Ready(FramedCarrier::Packet(packet))
        }
        debug_assert_eq!(self.token.as_ref(), Some(&input.token()));
        debug_assert_eq!(self.entity, Some(input.source()));
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
                    .map(|value| value.map_or(Step::Yield, ready));
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
                    .map(|value| value.map_or(Step::Yield, ready));
            }
            // Inspect complete feed carriers directly for common flags/PCR
            // layouts. Other fields use the incremental inspector.
            if self.buffered_carrier != BufferedCarrier::Carry
                && carry.is_empty()
                && input.buffered().len() >= missing
                && work.remaining() >= missing as u64
            {
                let share = match self.buffered_carrier {
                    BufferedCarrier::Carry => false,
                    BufferedCarrier::Shared => {
                        Self::share_buffered_owner(&mut self.shared_owner, input)
                    }
                    BufferedCarrier::Progressive => {
                        Self::share_progressive_window(&mut self.progressive_windows, input)
                    }
                };
                if share || self.buffered_carrier == BufferedCarrier::Progressive {
                    let start = input.position();
                    let source = if share {
                        let Step::Ready(source) = input.read_span(missing, work, cancel)? else {
                            return Err(Error::Invalid("buffered carrier span missing"));
                        };
                        source
                    } else {
                        // Preserve only the tight carrier if a shared window
                        // cannot be replaced. Allocate before advancing input.
                        let source =
                            Bytes::copy_from_slice(&self.budget, &input.buffered()[..missing])?;
                        if !matches!(input.read_discard(missing, work, cancel)?, Step::Ready(read) if read == missing)
                        {
                            return Err(Error::Invalid("buffered carrier extent changed"));
                        }
                        source
                    };
                    self.next = Some(input.position());
                    if self.format == PacketFormat::Ts188
                        && !cancel.load(Ordering::Relaxed)
                        && let Some(carrier) = PlainCarrier::inspect_ts188(&source, start, work)
                    {
                        self.last_packet_offset = Some(start);
                        if DIRECT
                            && self.buffered_carrier == BufferedCarrier::Progressive
                            && carrier.is_plain_continuation()
                        {
                            return Ok(Step::Ready(FramedCarrier::Plain(PlainContinuation {
                                layout: carrier,
                                source: Some(source),
                                offset: start,
                                entity: self.entity,
                            })));
                        }
                        return Ok(ready(carrier.into_packet(source, start, self.entity)?));
                    }
                    self.inspect(source, start)?;
                    continue;
                }
            }
            // Reserve before advancing the shared cursor; a failed allocation has
            // consumed no source bytes. A complete carry survives failed freeze.
            if carry.capacity() < carry.len() + missing
                && (self.buffered_carrier != BufferedCarrier::Progressive
                    || !input.buffered().is_empty())
            {
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
                Step::NeedInput(request) => {
                    Self::release_unused_windows(&mut self.progressive_windows);
                    return Ok(Step::NeedInput(request));
                }
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
            let Some(carry) = self.carry.as_mut() else {
                return FrameState::Error(Error::Invalid("missing packet carry"));
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
                let Some(next) = offset.checked_add(count as u64) else {
                    return FrameState::Error(Error::Overflow);
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
    pub fn finish(&self) -> Result<()> {
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
    fn pcr_carriers_keep_the_packet_route_after_budget_and_cancel_retries() {
        for (high, flags) in [(1, 0x10), (0x41, 0x50)] {
            for initial_work in [188, 197, 198] {
                let budget = Budget::new(8192).unwrap();
                let mut input = InputCursor::new(
                    &budget,
                    SourceId(7),
                    SourceCapabilities {
                        length: Some(188),
                        random_access: false,
                    },
                    188,
                )
                .unwrap();
                let mut wire = [0x42; 188];
                wire[..12].copy_from_slice(&[0x47, high, 7, 0x37, 7, flags, 0, 0, 0, 0, 0x7e, 0]);
                let backing = Bytes::copy_from_slice(&budget, &wire).unwrap();
                let request = input.request();
                input
                    .provide_owned(
                        InputWindow::new(request.source, request.token, 0, backing).unwrap(),
                    )
                    .unwrap();
                let mut framer = TransportFramer::new(budget, PacketFormat::Ts188);
                framer.compact_progressive_input();
                let cancel = AtomicBool::new(true);
                let mut work = WorkBudget::new(initial_work);
                assert!(matches!(
                    framer.poll_carrier::<true>(&mut input, &mut work, &cancel),
                    Err(Error::Cancelled)
                ));
                assert_eq!(work.spent(), 0);
                assert_eq!(input.position(), 0);
                cancel.store(false, Ordering::Relaxed);
                let mut step = framer
                    .poll_carrier::<true>(&mut input, &mut work, &cancel)
                    .unwrap();
                let mut spent = work.spent();
                let packet = loop {
                    match step {
                        Step::Ready(FramedCarrier::Packet(packet)) => break packet,
                        Step::Yield => {
                            assert_eq!(input.position(), 188);
                            cancel.store(true, Ordering::Relaxed);
                            let mut retry = WorkBudget::new(1);
                            assert!(matches!(
                                framer.poll_carrier::<true>(&mut input, &mut retry, &cancel),
                                Err(Error::Cancelled)
                            ));
                            assert_eq!(retry.spent(), 0);
                            cancel.store(false, Ordering::Relaxed);
                            step = framer
                                .poll_carrier::<true>(&mut input, &mut retry, &cancel)
                                .unwrap();
                            spent += retry.spent();
                        }
                        _ => panic!("PCR carrier bypassed the ordinary packet route"),
                    }
                };
                assert_eq!(spent, 198); // 188 input bytes plus ten inspector actions.
                assert_eq!(input.position(), 188);
                assert_eq!(packet.pcr_range, Some(6..12));
                assert_eq!(
                    packet.adaptation.as_ref().unwrap().pcr,
                    Some(crate::Pcr {
                        base: 0,
                        extension: 0
                    })
                );
                assert_eq!(
                    packet.adaptation.as_ref().unwrap().source.as_ref(),
                    &wire[4..12]
                );
                assert_eq!(packet.payload.as_ref().unwrap().as_ref(), &wire[12..]);
                assert_eq!(packet.payload_source_offset, 12);
                assert_eq!(packet.source_id, Some(SourceId(7)));
            }
        }
    }

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
    fn segment_carriers_copy_sparse_and_independent_windows() {
        let budget = Budget::new(256 * 1024).unwrap();
        let mut input = InputCursor::new(
            &budget,
            SourceId(1),
            SourceCapabilities {
                length: Some(3 * 188),
                random_access: false,
            },
            188,
        )
        .unwrap();
        let mut framer = TransportFramer::new(budget.clone(), PacketFormat::Ts188);
        framer.share_input();
        let mut wire = [0xff; 188];
        wire[..4].copy_from_slice(&[0x47, 0x1f, 0xff, 0x10]);
        let mut owners = Vec::new();
        for (capacity, share) in [(64 * 1024, false), (188, true), (188, false)] {
            let mut data = Vec::with_capacity(capacity);
            data.extend_from_slice(&wire);
            let owner = Bytes::try_from_vec(&budget, data).unwrap();
            let request = input.request();
            input
                .provide_owned(
                    InputWindow::new(request.source, request.token, request.offset, owner.clone())
                        .unwrap(),
                )
                .unwrap();
            let Step::Ready(packet) = framer
                .poll(
                    &mut input,
                    &mut WorkBudget::new(4096),
                    &AtomicBool::new(false),
                )
                .unwrap()
            else {
                panic!("expected a complete carrier");
            };
            assert_eq!(packet.source.as_ref(), &wire);
            assert_eq!(packet.source.shares_storage_with(&owner), share);
            owners.push(owner);
        }
    }

    #[test]
    fn progressive_carriers_share_only_within_the_window_bound() {
        const WIDTH: usize = 188;
        const WINDOW: usize = 2 * WIDTH;
        let budget = Budget::new(8192).unwrap();
        let mut input = InputCursor::new(
            &budget,
            SourceId(1),
            SourceCapabilities {
                length: None,
                random_access: false,
            },
            WINDOW,
        )
        .unwrap();
        let mut framer = TransportFramer::new(budget.clone(), PacketFormat::Ts188);
        framer.compact_progressive_input();
        let mut wire = [0xff; WINDOW];
        for carrier in wire.chunks_exact_mut(WIDTH) {
            carrier[..4].copy_from_slice(&[0x47, 0x1f, 0xff, 0x10]);
        }
        let cancel = AtomicBool::new(false);
        let mut retained = Vec::new();
        for index in 0..MAX_PROGRESSIVE_SHARED_WINDOWS + 2 {
            let share = index != MAX_PROGRESSIVE_SHARED_WINDOWS;
            let window = Bytes::copy_from_slice(&budget, &wire).unwrap();
            let request = input.request();
            input
                .provide_owned(
                    InputWindow::new(
                        request.source,
                        request.token,
                        request.offset,
                        window.clone(),
                    )
                    .unwrap(),
                )
                .unwrap();
            for carrier in 0..2 {
                let Step::Ready(packet) = framer
                    .poll(&mut input, &mut WorkBudget::new(4096), &cancel)
                    .unwrap()
                else {
                    panic!("expected a complete carrier");
                };
                assert_eq!(
                    packet.source_offset,
                    (index * WINDOW + carrier * WIDTH) as u64
                );
                assert_eq!(packet.source.shares_storage_with(&window), share);
                if index < MAX_PROGRESSIVE_SHARED_WINDOWS && carrier == 1 {
                    retained.push(packet);
                }
            }
            if index == MAX_PROGRESSIVE_SHARED_WINDOWS {
                drop(retained.swap_remove(0));
            }
        }
        drop(retained);
        assert!(matches!(
            framer
                .poll(&mut input, &mut WorkBudget::new(4096), &cancel)
                .unwrap(),
            Step::NeedInput(_)
        ));
        assert_eq!(budget.used(), Budget::control_storage_bytes());
    }

    #[test]
    fn progressive_continuation_moves_charged_payload_storage() {
        use crate::compat::pes::{
            DemuxObserver, DemuxProfile, DemuxScope, ReadyContinuation, SourceContext,
        };
        use crate::{ParseLimits, SourceEpochs, SourceRange};

        let pool = Budget::new(8192).unwrap();
        let entity = SourceId(1);
        let mut input = InputCursor::new(
            &pool,
            entity,
            SourceCapabilities {
                length: Some(376),
                random_access: false,
            },
            376,
        )
        .unwrap();
        // One finite PES with 170 bytes in its start and 180 in its
        // continuation. The second carrier has a plain adaptation field.
        let mut wire = [0xff; 376];
        wire[..18].copy_from_slice(&[
            0x47, 0x41, 0, 0x10, 0, 0, 1, 0xe0, 1, 0x66, 0x80, 0x80, 5, 0x21, 0, 1, 0, 1,
        ]);
        wire[18..188].fill(0x11);
        wire[188..194].copy_from_slice(&[0x47, 1, 0, 0x31, 3, 0x40]);
        wire[196..].fill(0x22);
        let backing = Bytes::copy_from_slice(&pool, &wire).unwrap();
        let request = input.request();
        input
            .provide_owned(InputWindow::new(entity, request.token, 0, backing.clone()).unwrap())
            .unwrap();
        let mut framer = TransportFramer::new(pool.clone(), PacketFormat::Ts188);
        framer.compact_progressive_input();
        let cancel = AtomicBool::new(false);
        let mut work = WorkBudget::new(4096);
        let Step::Ready(FramedCarrier::Packet(start)) = framer
            .poll_carrier::<true>(&mut input, &mut work, &cancel)
            .unwrap()
        else {
            panic!("a PES start needs the ordinary carrier");
        };
        let epochs = SourceEpochs {
            clock: 0,
            configuration: 2,
        };
        let context = |source_event_ordinal| SourceContext {
            source_event_ordinal,
            epochs,
            byte_run_epoch: None,
        };
        let mut observer = DemuxObserver::new(
            DemuxProfile::PesPackets,
            DemuxScope {
                program: 1,
                pid: 256,
                stream_type: 0x1b,
                stream_id: 0xe0,
                entity,
                epochs,
            },
            pool.clone(),
            ParseLimits::default(),
        )
        .unwrap();
        observer.push(start, context(0)).unwrap();
        assert!(matches!(
            observer.poll(&mut work, &cancel).unwrap(),
            AssemblyStep::NeedPayload
        ));
        let Step::Ready(FramedCarrier::Plain(mut continuation)) = framer
            .poll_carrier::<true>(&mut input, &mut work, &cancel)
            .unwrap()
        else {
            panic!("expected a compact continuation");
        };
        assert!(
            continuation
                .source
                .as_ref()
                .unwrap()
                .shares_storage_with(&backing)
        );
        let before = pool.used();
        for (units, cancelled) in [(1, false), (2, true)] {
            let mut blocked = WorkBudget::new(units);
            assert_eq!(
                observer
                    .append_plain_continuation(
                        &mut continuation,
                        context(2),
                        &mut blocked,
                        &AtomicBool::new(cancelled),
                    )
                    .unwrap(),
                ReadyContinuation::Unhandled
            );
            assert_eq!(blocked.remaining(), units);
            assert!(continuation.source.is_some());
        }
        let mut append = WorkBudget::new(2);
        assert_eq!(
            observer
                .append_plain_continuation(&mut continuation, context(2), &mut append, &cancel)
                .unwrap(),
            ReadyContinuation::Complete
        );
        assert_eq!(append.remaining(), 0);
        assert_eq!(pool.used(), before);
        assert!(continuation.source.is_none());
        let AssemblyStep::Output(packet) = observer.poll(&mut work, &cancel).unwrap() else {
            panic!("expected the finite PES to complete");
        };
        let [first, last] = packet.spans.as_slice() else {
            panic!("expected the two original transport ranges");
        };
        assert_eq!(first.as_ref(), &wire[18..188]);
        assert_eq!(last.as_ref(), &wire[196..]);
        assert!(last.backing_bytes().shares_storage_with(&backing));
        assert_eq!(last.context, context(2));
        assert_eq!(
            last.source,
            SourceRange {
                entity,
                offset: 196,
                bytes: 180
            }
        );
        assert_eq!(
            last.transport,
            SourceRange {
                entity,
                offset: 188,
                bytes: 188
            }
        );
    }

    #[test]
    fn an_unretained_shared_carrier_returns_its_window_budget() {
        let limit = 188 + Bytes::owner_overhead_bytes() + Budget::control_storage_bytes();
        let budget = Budget::new(limit).unwrap();
        let mut input = InputCursor::new(
            &budget,
            SourceId(1),
            SourceCapabilities {
                length: Some(188),
                random_access: false,
            },
            188,
        )
        .unwrap();
        let request = input.request();
        let mut wire = [0xff; 188];
        wire[..4].copy_from_slice(&[0x47, 0x1f, 0xff, 0x10]);
        input
            .provide_owned(
                InputWindow::new(
                    request.source,
                    request.token,
                    0,
                    Bytes::copy_from_slice(&budget, &wire).unwrap(),
                )
                .unwrap(),
            )
            .unwrap();
        let mut framer = TransportFramer::new(budget.clone(), PacketFormat::Ts188);
        framer.share_input();
        assert!(matches!(
            framer
                .poll(
                    &mut input,
                    &mut WorkBudget::new(4096),
                    &AtomicBool::new(false)
                )
                .unwrap(),
            Step::Ready(_)
        ));
        assert_eq!(budget.used(), Budget::control_storage_bytes());
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

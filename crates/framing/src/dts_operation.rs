//! Complete source acquisition alongside the separately named carrier/parser view.
//!
//! The two real input cursors have distinct tokens and generations. Callers answer
//! both from the same declared entity; no Source frame boundaries are supplied to
//! the parser view. These are two operations, with both costs exposed, rather than
//! a replacement of the original Strict/native comparison or automatic DTS probing.
//! The reference revision is recorded once, on [`crate::reference_trace::dts`].
use crate::{
    Dts14CarrierLimits, Dts14CarrierPacket, Dts14CarrierReader, Dts14CarrierSyntax, Dts14SyncFrame,
    Dts14SyncLimits, Dts14SyncReader, DtsCoreHeader, DtsSourceIdentity, DtsWordOrder,
};
use std::{
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};
use triergon_core::{
    Budget, Bytes, Error, InputRequest, InputToken, InputWindow, Result, SourceCapabilities,
    SourceFailure, SourceId, SourceLocation, Step, WorkBudget,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParserOperationLane {
    Source,
    ParserProjection,
}

#[derive(Clone, Copy, Debug)]
pub struct ParserOperationLimits {
    pub source: Dts14SyncLimits,
    pub parser: Dts14CarrierLimits,
}

/// Actual cursor identities. A generation is never rewritten to make views agree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParserOperationContext {
    source: SourceId,
    source_generation: u64,
    parser_generation: u64,
    fresh_zero_origin: bool,
}
impl ParserOperationContext {
    pub fn source_generation(&self) -> u64 {
        self.source_generation
    }
    pub fn parser_generation(&self) -> u64 {
        self.parser_generation
    }
}

/// Public-parser clock projection, separately retained from Source time.
/// The reference parser inspects an 18-byte physical prefix, latches the first
/// accepted core rate, and rounds subsequent coding windows in that clock.
/// It does not publish the source channel layout or nominal header bitrate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParserClock {
    pub duration: i64,
    pub codec_sample_rate: u32,
    pub profile: i32,
    pub disposition: ParserClockDisposition,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParserClockDisposition {
    OpaqueOrShort,
    SelectedCorePrefix,
}
#[derive(Clone, Copy)]
struct ClockState {
    rate: u32,
    profile: i32,
    unknown: bool,
    order: DtsWordOrder,
}
impl ClockState {
    fn new(order: DtsWordOrder) -> Self {
        Self {
            rate: 0,
            profile: -99,
            unknown: false,
            order,
        }
    }
    fn project(&mut self, packet: &Dts14CarrierPacket) -> Result<ParserClock> {
        if self.unknown {
            return Err(Error::Unsupported("DTS clock after unmodeled prefix"));
        }
        let mut output = ParserClock {
            duration: 0,
            codec_sample_rate: self.rate,
            profile: self.profile,
            disposition: ParserClockDisposition::OpaqueOrShort,
        };
        // This is the literal reference core-prefix bound, not a completeness/EOF rule.
        // An opaque carrier fails conversion; a short one never updates its clock.
        if packet.source_bytes().len() < 18
            || matches!(packet.syntax, Dts14CarrierSyntax::OpaqueCarrier)
        {
            return Ok(output);
        }
        // A whole-packet conversion may refuse its tail while this complete
        // physical prefix still supplies the public parser's clock. Retain both.
        let prefix: [u8; 18] = packet.source_bytes().as_ref()[..18]
            .try_into()
            .map_err(|_| Error::Truncated)?;
        let bits = crate::dts14_sync::header_payload(&prefix, self.order)?;
        let header = crate::probe_dts_core_header(&bits);
        let header = match header {
            Ok(h) if h.extension == 0 => h,
            _ => {
                self.unknown = true;
                return Err(Error::Unsupported(
                    "DTS clock outside selected normal core prefix",
                ));
            }
        };
        // The pinned parser's optional profile probe uses an aligned coded
        // FSIZE offset in its actual packet, not a Source physical boundary.
        // If that location advertises EXSS, refuse rather than invent its profile.
        let at = header.frame_bytes.checked_add(3).ok_or(Error::Overflow)? / 4 * 4;
        if packet
            .source_bytes()
            .as_ref()
            .get(at..at.checked_add(4).ok_or(Error::Overflow)?)
            == Some(&[0x64, 0x58, 0x20, 0x25])
        {
            self.unknown = true;
            return Err(Error::Unsupported(
                "DTS clock extension-substream profile not modeled",
            ));
        }
        let rate = if self.rate == 0 {
            header.core_rate_hz
        } else {
            self.rate
        };
        let scaled = u64::from(header.coding_window_samples)
            .checked_mul(u64::from(rate))
            .ok_or(Error::Overflow)?;
        let rounded = scaled
            .checked_add(u64::from(header.core_rate_hz) / 2)
            .ok_or(Error::Overflow)?
            / u64::from(header.core_rate_hz);
        output.duration = i64::try_from(rounded).map_err(|_| Error::Overflow)?;
        output.codec_sample_rate = rate;
        output.profile = 20;
        output.disposition = ParserClockDisposition::SelectedCorePrefix;
        self.rate = rate;
        self.profile = 20;
        Ok(output)
    }
}

#[derive(Clone, Debug)]
pub struct ParserPacket {
    packet: Dts14CarrierPacket,
    context: ParserOperationContext,
    range: Range<u64>,
    clock: Result<ParserClock>,
}
impl ParserPacket {
    fn new(
        packet: Dts14CarrierPacket,
        context: ParserOperationContext,
        clock: Result<ParserClock>,
    ) -> Result<Self> {
        if packet.identity.source != context.source
            || packet.identity.generation != context.parser_generation
        {
            return Err(Error::Invalid("DTS parser projection context"));
        }
        let end = packet
            .identity
            .offset
            .checked_add(u64::try_from(packet.source_bytes().len()).map_err(|_| Error::Overflow)?)
            .ok_or(Error::Overflow)?;
        let range = packet.identity.offset..end;
        Ok(Self {
            packet,
            context,
            range,
            clock,
        })
    }
    pub fn source_bytes(&self) -> &Bytes {
        self.packet.source_bytes()
    }
    pub fn physical_range(&self) -> Range<u64> {
        self.range.clone()
    }
    pub fn identity(&self) -> DtsSourceIdentity {
        self.packet.identity
    }
    pub fn context(&self) -> ParserOperationContext {
        self.context
    }
    pub fn packet(&self) -> &Dts14CarrierPacket {
        &self.packet
    }
    /// Complete-packet Source header only when exact geometry and the existing
    /// strict probe both succeed. Prefix facts alone do not certify a packet;
    /// no associated Source frame fills an opaque or refused packet header.
    pub fn source_header(&self) -> Option<DtsCoreHeader> {
        if let Dts14CarrierSyntax::ExactNormal(header) = self.packet.syntax {
            Some(header)
        } else {
            None
        }
    }
    /// This modeled public state is never a source coding-window duration.
    pub fn clock(&self) -> Result<ParserClock> {
        self.clock
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParserOperationTerminal {
    End(SourceLocation),
    Refused { error: Error, at: SourceLocation },
}

#[derive(Clone, Debug)]
pub enum ParserOperationEvent {
    SourceFrame(Dts14SyncFrame),
    ParserPacket(ParserPacket),
    /// A genuine terminal source/parser error remains a required outcome.
    ViewRefusal {
        lane: ParserOperationLane,
        error: Error,
        request: InputRequest,
    },
}

#[derive(Debug)]
pub enum ParserOperationStep {
    /// A complete inline payload is available through `take_event`.
    Event,
    NeedInput {
        lane: ParserOperationLane,
        request: InputRequest,
    },
    Yield,
    /// Completeness of both views, not native/module acceptance.
    End {
        source: ParserOperationTerminal,
        parser: ParserOperationTerminal,
    },
}

/// Bounded orchestration of the existing full Source and carrier readers.
/// No queue, hidden frame reassembly, cached native result or new dependency.
/// A fatal cancellation/transport failure drops both private owners immediately;
/// caller-held events continue owning their complete source/canonical storage.
pub struct ParserOperation {
    source: Option<Dts14SyncReader>,
    parser: Option<Dts14CarrierReader>,
    context: ParserOperationContext,
    turn: ParserOperationLane,
    dispatch_paid: bool,
    pending_packet: Option<Dts14CarrierPacket>,
    ready_event: Option<ParserOperationEvent>,
    clock_charge: u16,
    clock_state: ClockState,
    source_end: Option<ParserOperationTerminal>,
    parser_end: Option<ParserOperationTerminal>,
    failure: Option<Error>,
}
const _: () = assert!(size_of::<ParserOperation>() <= 4096);
const _: () = assert!(size_of::<ParserOperationStep>() <= 256);

impl ParserOperation {
    pub fn new(
        pool: &Budget,
        source: SourceId,
        capabilities: SourceCapabilities,
        order: DtsWordOrder,
        limits: ParserOperationLimits,
    ) -> Result<Self> {
        let facts = Dts14SyncReader::per_frame(pool, source, capabilities, order, limits.source)?;
        let parser = Dts14CarrierReader::new(pool, source, capabilities, order, limits.parser)?;
        let context = ParserOperationContext {
            source,
            source_generation: facts.generation(),
            parser_generation: parser.generation(),
            fresh_zero_origin: true,
        };
        Ok(Self {
            source: Some(facts),
            parser: Some(parser),
            context,
            turn: ParserOperationLane::Source,
            dispatch_paid: false,
            pending_packet: None,
            ready_event: None,
            clock_charge: 0,
            clock_state: ClockState::new(order),
            source_end: None,
            parser_end: None,
            failure: None,
        })
    }
    pub fn context(&self) -> ParserOperationContext {
        self.context
    }
    /// Moves the complete owned event out once. Repeated polls leave this slot
    /// unchanged until taken; no new read, owner or work is hidden by a poll.
    pub fn take_event(&mut self) -> Result<ParserOperationEvent> {
        self.healthy()?;
        self.ready_event
            .take()
            .ok_or(Error::Invalid("DTS event slot empty"))
    }
    fn publish(&mut self, event: ParserOperationEvent) -> Result<ParserOperationStep> {
        if self.ready_event.is_some() {
            return Err(Error::Invalid("DTS event slot occupied"));
        }
        self.ready_event = Some(event);
        Ok(ParserOperationStep::Event)
    }
    fn healthy(&self) -> Result<()> {
        self.failure.map_or(Ok(()), Err)
    }
    fn abort(&mut self, error: Error) {
        self.source = None;
        self.parser = None;
        self.pending_packet = None;
        self.ready_event = None;
        self.failure = Some(error);
    }
    pub fn request(&self, lane: ParserOperationLane) -> Result<InputRequest> {
        self.healthy()?;
        match lane {
            ParserOperationLane::Source if self.source_end.is_none() => {
                self.source.as_ref().map(Dts14SyncReader::request)
            }
            ParserOperationLane::ParserProjection if self.parser_end.is_none() => {
                self.parser.as_ref().map(Dts14CarrierReader::request)
            }
            _ => None,
        }
        .ok_or(Error::Invalid("DTS operation view is terminal"))
    }
    pub fn provide(&mut self, lane: ParserOperationLane, window: &InputWindow) -> Result<()> {
        self.request(lane)?;
        match lane {
            ParserOperationLane::Source => self
                .source
                .as_mut()
                .ok_or(Error::Invalid("DTS source cursor absent"))?
                .provide(window),
            ParserOperationLane::ParserProjection => self
                .parser
                .as_mut()
                .ok_or(Error::Invalid("DTS parser cursor absent"))?
                .provide(window),
        }
    }
    pub fn finish(
        &mut self,
        lane: ParserOperationLane,
        source: SourceId,
        token: InputToken,
    ) -> Result<()> {
        self.request(lane)?;
        match lane {
            ParserOperationLane::Source => self
                .source
                .as_mut()
                .ok_or(Error::Invalid("DTS source cursor absent"))?
                .finish(source, token),
            ParserOperationLane::ParserProjection => self
                .parser
                .as_mut()
                .ok_or(Error::Invalid("DTS parser cursor absent"))?
                .finish(source, token),
        }
    }
    pub fn fail(
        &mut self,
        lane: ParserOperationLane,
        source: SourceId,
        token: InputToken,
        failure: SourceFailure,
    ) -> Result<()> {
        self.request(lane)?;
        match lane {
            ParserOperationLane::Source => self
                .source
                .as_mut()
                .ok_or(Error::Invalid("DTS source cursor absent"))?
                .fail(source, token, failure)?,
            ParserOperationLane::ParserProjection => self
                .parser
                .as_mut()
                .ok_or(Error::Invalid("DTS parser cursor absent"))?
                .fail(source, token, failure)?,
        }
        self.abort(match failure {
            SourceFailure::Transport => Error::Transport,
            SourceFailure::Cancelled => Error::Cancelled,
        });
        Ok(())
    }
    fn next_turn(&mut self) {
        self.dispatch_paid = false;
        self.turn = match self.turn {
            ParserOperationLane::Source => ParserOperationLane::ParserProjection,
            ParserOperationLane::ParserProjection => ParserOperationLane::Source,
        };
    }
    fn refused(&mut self, lane: ParserOperationLane, error: Error) -> Result<ParserOperationStep> {
        let request = self.request(lane)?;
        let terminal = ParserOperationTerminal::Refused {
            error,
            at: SourceLocation {
                source: request.source,
                offset: request.offset,
            },
        };
        match lane {
            ParserOperationLane::Source => {
                self.source = None;
                self.source_end = Some(terminal);
            }
            ParserOperationLane::ParserProjection => {
                self.parser = None;
                self.parser_end = Some(terminal);
            }
        }
        self.next_turn();
        self.publish(ParserOperationEvent::ViewRefusal {
            lane,
            error,
            request,
        })
    }
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParserOperationStep> {
        self.healthy()?;
        if cancel.load(Ordering::Relaxed) {
            self.abort(Error::Cancelled);
            return Err(Error::Cancelled);
        }
        if self.ready_event.is_some() {
            return Ok(ParserOperationStep::Event);
        }
        loop {
            if self.pending_packet.is_some() {
                // Conservative bound for the fixed120-bit probe and arithmetic.
                // Accumulate across calls so WorkBudget(1) makes real progress.
                while self.clock_charge < 128 {
                    if !work.try_charge(1) {
                        return Ok(ParserOperationStep::Yield);
                    }
                    self.clock_charge += 1;
                }
                let raw = self
                    .pending_packet
                    .take()
                    .ok_or(Error::Invalid("DTS pending clock packet"))?;
                let clock = self.clock_state.project(&raw);
                let packet = match ParserPacket::new(raw, self.context, clock) {
                    Ok(packet) => packet,
                    Err(error) => {
                        self.abort(error);
                        return Err(error);
                    }
                };
                self.clock_charge = 0;
                self.next_turn();
                return self.publish(ParserOperationEvent::ParserPacket(packet));
            }
            if let (Some(source), Some(parser)) = (self.source_end, self.parser_end) {
                if let (ParserOperationTerminal::End(a), ParserOperationTerminal::End(b)) =
                    (source, parser)
                    && a != b
                {
                    self.abort(Error::Invalid("DTS operation view source ends differ"));
                    return self.healthy().map(|()| ParserOperationStep::Yield);
                }
                return Ok(ParserOperationStep::End { source, parser });
            }
            if work.remaining() == 0 {
                return Ok(ParserOperationStep::Yield);
            }
            if (self.turn == ParserOperationLane::Source && self.source_end.is_some())
                || (self.turn == ParserOperationLane::ParserProjection && self.parser_end.is_some())
            {
                self.next_turn();
                continue;
            }
            if !self.dispatch_paid {
                if !work.try_charge(1) {
                    return Ok(ParserOperationStep::Yield);
                }
                self.dispatch_paid = true;
            }
            let lane = self.turn;
            match lane {
                ParserOperationLane::Source => {
                    match self
                        .source
                        .as_mut()
                        .ok_or(Error::Invalid("DTS source cursor absent"))?
                        .step(work, cancel)
                    {
                        Ok(Step::Ready(frame)) => {
                            self.next_turn();
                            return self.publish(ParserOperationEvent::SourceFrame(frame));
                        }
                        Ok(Step::NeedInput(request)) => {
                            self.next_turn();
                            return Ok(ParserOperationStep::NeedInput { lane, request });
                        }
                        Ok(Step::Yield) => return Ok(ParserOperationStep::Yield),
                        Ok(Step::End(end)) => {
                            self.source_end = Some(ParserOperationTerminal::End(end));
                            self.source = None;
                            self.next_turn();
                        }
                        Err(e @ (Error::Backpressure { .. } | Error::Allocation)) => return Err(e),
                        Err(Error::Cancelled) => {
                            self.abort(Error::Cancelled);
                            return Err(Error::Cancelled);
                        }
                        Err(error) => return self.refused(lane, error),
                    }
                }
                ParserOperationLane::ParserProjection => {
                    match self
                        .parser
                        .as_mut()
                        .ok_or(Error::Invalid("DTS parser cursor absent"))?
                        .step(work, cancel)
                    {
                        Ok(Step::Ready(packet)) => {
                            self.pending_packet = Some(packet);
                            self.clock_charge = 0;
                        }
                        Ok(Step::NeedInput(request)) => {
                            self.next_turn();
                            return Ok(ParserOperationStep::NeedInput { lane, request });
                        }
                        Ok(Step::Yield) => return Ok(ParserOperationStep::Yield),
                        Ok(Step::End(end)) => {
                            self.parser_end = Some(ParserOperationTerminal::End(end));
                            self.parser = None;
                            self.next_turn();
                        }
                        Err(e @ (Error::Backpressure { .. } | Error::Allocation)) => return Err(e),
                        Err(Error::Cancelled) => {
                            self.abort(Error::Cancelled);
                            return Err(Error::Cancelled);
                        }
                        Err(error) => return self.refused(lane, error),
                    }
                }
            }
        }
    }
}

/// Byte-validated physical containment. It never copies Source facts into a
/// native header or mistakes an opaque packet for one Source frame.
#[derive(Clone, Debug)]
pub struct FactAssociation {
    source_frame: Dts14SyncFrame,
    parser_packet: ParserPacket,
    source_range: Range<u64>,
    packet_range: Range<u64>,
}
impl FactAssociation {
    pub fn source_frame(&self) -> &Dts14SyncFrame {
        &self.source_frame
    }
    pub fn parser_packet(&self) -> &ParserPacket {
        &self.parser_packet
    }
    pub fn source_range(&self) -> Range<u64> {
        self.source_range.clone()
    }
    pub fn packet_range(&self) -> Range<u64> {
        self.packet_range.clone()
    }
}

/// One source frame may be contained in a coarser parser packet. Work charges
/// each original byte comparison; output retains both complete backing owners.
pub struct FactAssociationJob {
    source: Option<Dts14SyncFrame>,
    packet: Option<ParserPacket>,
    source_range: Range<u64>,
    packet_range: Range<u64>,
    start: usize,
    checked: usize,
    failed: Option<Error>,
    taken: bool,
}
impl FactAssociationJob {
    pub fn new(
        pool: &Budget,
        context: ParserOperationContext,
        source: &Dts14SyncFrame,
        packet: &ParserPacket,
    ) -> Result<Self> {
        if !source.physical_bytes().belongs_to(pool) || !packet.source_bytes().belongs_to(pool) {
            return Err(Error::ForeignPool);
        }
        let fact = source.facts();
        if fact.identity.source != context.source || packet.identity().source != context.source {
            return Err(Error::SourceMismatch);
        }
        if fact.identity.generation != context.source_generation || packet.context() != context {
            return Err(Error::Invalid("DTS association generation context"));
        }
        let end = fact
            .identity
            .offset
            .checked_add(u64::try_from(source.physical_bytes().len()).map_err(|_| Error::Overflow)?)
            .ok_or(Error::Overflow)?;
        let source_range = fact.identity.offset..end;
        let packet_range = packet.physical_range();
        if source_range.start < packet_range.start || source_range.end > packet_range.end {
            return Err(Error::Invalid(
                "DTS source frame not contained in parser packet",
            ));
        }
        let start = usize::try_from(source_range.start - packet_range.start)
            .map_err(|_| Error::Overflow)?;
        Ok(Self {
            source: Some(source.clone()),
            packet: Some(packet.clone()),
            source_range,
            packet_range,
            start,
            checked: 0,
            failed: None,
            taken: false,
        })
    }
    fn abort(&mut self, error: Error) {
        self.source = None;
        self.packet = None;
        self.failed = Some(error);
    }
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<FactAssociation>> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if cancel.load(Ordering::Relaxed) {
            self.abort(Error::Cancelled);
            return Err(Error::Cancelled);
        }
        if self.taken {
            return Err(Error::Invalid("DTS association already taken"));
        }
        let source = self
            .source
            .as_ref()
            .ok_or(Error::Invalid("DTS association source absent"))?
            .physical_bytes();
        let packet = self
            .packet
            .as_ref()
            .ok_or(Error::Invalid("DTS association packet absent"))?
            .source_bytes();
        while self.checked < source.len() {
            if cancel.load(Ordering::Relaxed) {
                self.abort(Error::Cancelled);
                return Err(Error::Cancelled);
            }
            if !work.try_charge(1) {
                return Ok(Step::Yield);
            }
            let at = self
                .start
                .checked_add(self.checked)
                .ok_or(Error::Overflow)?;
            if packet.as_ref().get(at) != source.as_ref().get(self.checked) {
                self.abort(Error::Invalid("DTS associated source bytes differ"));
                return Err(Error::Invalid("DTS associated source bytes differ"));
            }
            self.checked += 1;
        }
        if !work.try_charge(1) {
            return Ok(Step::Yield);
        }
        self.taken = true;
        Ok(Step::Ready(FactAssociation {
            source_frame: self
                .source
                .take()
                .ok_or(Error::Invalid("DTS association source absent"))?,
            parser_packet: self
                .packet
                .take()
                .ok_or(Error::Invalid("DTS association packet absent"))?,
            source_range: self.source_range.clone(),
            packet_range: self.packet_range.clone(),
        }))
    }
}

//! The complete source frames of a raw 14-bit DTS entity side by side with the
//! packets of the reference parser.
//!
//! The two readers have input cursors of their own, with distinct tokens and
//! generations. The caller answers both from the same entity, and the packet reader
//! is told nothing about the boundaries of the source frames. Both readers run in
//! full and both are charged for. The operation does not compare their results and
//! does not detect DTS or its word order. The reference revision is named once, on
//! [`crate::reference_trace::dts`].
use crate::dts::sync14::header_payload;
use crate::dts::{
    Dts14CarrierLimits, Dts14CarrierPacket, Dts14CarrierReader, Dts14CarrierSyntax, Dts14SyncFrame,
    Dts14SyncLimits, Dts14SyncReader, DtsCoreHeader, DtsFrameExtentLimits, DtsSourceIdentity,
    DtsWordOrder, probe_core_header, probe_extension_substream_extent,
};
use std::{
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};
use triergon_core::{
    Budget, Bytes, Error, InputRequest, InputToken, InputWindow, Result, SourceCapabilities,
    SourceFailure, SourceId, SourceLocation, Step, WorkBudget,
};

/// One of the two readers of a [`ParserOperation`]. Each has an input cursor of its
/// own, so a request and its answer name the reader they belong to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParserOperationLane {
    /// The source-frame reader, a [`Dts14SyncReader::per_frame`].
    Source,
    /// The parser-packet reader, a [`Dts14CarrierReader`].
    ParserProjection,
}

/// Limits of a [`ParserOperation`], one set for each reader.
#[derive(Clone, Copy, Debug)]
pub struct ParserOperationLimits {
    /// Limits of the source-frame reader.
    pub source: Dts14SyncLimits,
    /// Limits of the parser-packet reader.
    pub parser: Dts14CarrierLimits,
}

/// The cursors of a [`ParserOperation`]: its source and the generation of each
/// reader's cursor. The two generations differ, and each is reported as its cursor
/// has it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParserOperationContext {
    source: SourceId,
    source_generation: u64,
    parser_generation: u64,
}
impl ParserOperationContext {
    /// Cursor generation of the source-frame reader, which every source frame
    /// carries in its identity.
    pub fn source_generation(&self) -> u64 {
        self.source_generation
    }
    /// Cursor generation of the parser-packet reader, which every parser packet
    /// carries in its identity.
    pub fn parser_generation(&self) -> u64 {
        self.parser_generation
    }
}

/// The duration, sample rate and profile the reference parser reports with one
/// packet. This is the parser's clock, kept apart from the duration of a source
/// frame ([`DtsCoreHeader::window`]). The parser reads the core header in a packet's
/// first 18 bytes, keeps the sample rate of the first header it accepts, and rounds
/// every later coding window to that rate. It reports neither the channel layout nor
/// the nominal bit rate of the header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParserClock {
    /// The packet's duration in samples at `codec_sample_rate`: the header's coding
    /// window rescaled from the header's own sample rate and rounded to the nearest
    /// sample. 0 for [`ParserClockDisposition::OpaqueOrShort`].
    pub duration: i64,
    /// Core sample rate of the first accepted header, in Hz; 0 before one is
    /// accepted.
    pub codec_sample_rate: u32,
    /// The parser's profile number: -99 before a header is accepted, 20 for core,
    /// 30 for a core channel extension, or 40 for a core frequency extension.
    pub profile: i32,
    /// Whether the packet's own header gave these values.
    pub disposition: ParserClockDisposition,
}
/// Whether a packet's header contributed to its [`ParserClock`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParserClockDisposition {
    /// The packet is opaque or shorter than 18 bytes. No header is read: the
    /// duration is 0 and the rate and profile are those of the packets before.
    OpaqueOrShort,
    /// The packet's first 18 bytes hold a core header, and the duration comes
    /// from its core clock even when the profile names an extension.
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
        // The reference parser reads the clock from a packet's first 18 bytes; a
        // shorter packet, or one without the sync pattern, leaves the clock as it is.
        if packet.source_bytes().len() < 18
            || matches!(packet.syntax, Dts14CarrierSyntax::OpaqueCarrier)
        {
            return Ok(output);
        }
        // A packet whose whole unpacking is refused still has a complete header in
        // its first 18 bytes, which sets the clock.
        let prefix: [u8; 18] = packet.source_bytes().as_ref()[..18]
            .try_into()
            .map_err(|_| Error::Truncated)?;
        let bits = header_payload(&prefix, self.order)?;
        let (header, profile) = match clock_header(bits) {
            Ok(facts) => facts,
            Err(_) => {
                self.unknown = true;
                return Err(Error::Unsupported(
                    "DTS clock outside selected normal core prefix",
                ));
            }
        };
        // This is a parser-packet offset, independent of the physical core
        // boundary. An isolated sync word has no extension extent (TS 102 114
        // section 7.5.2). A complete substream would need asset inspection.
        let at = header.frame_bytes.checked_add(3).ok_or(Error::Overflow)? / 4 * 4;
        if packet
            .source_bytes()
            .as_ref()
            .get(at..)
            .filter(|bytes| bytes.starts_with(&[0x64, 0x58, 0x20, 0x25]))
            .is_some_and(|bytes| {
                probe_extension_substream_extent(
                    bytes,
                    DtsFrameExtentLimits {
                        maximum_header_bytes: bytes.len(),
                        maximum_frame_bytes: bytes.len(),
                    },
                )
                .is_ok()
            })
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
        output.profile = profile;
        output.disposition = ParserClockDisposition::SelectedCorePrefix;
        self.rate = rate;
        self.profile = profile;
        Ok(output)
    }
}

fn clock_header(mut bits: [u8; 15]) -> Result<(DtsCoreHeader, i32)> {
    // TS 102 114 Table 5-11 assigns the type independently of SFREQ and NBLKS.
    // The parser keeps the core clock for reserved extension IDs as well.
    let profile = match (bits[10] & 0x10 != 0, bits[10] >> 5) {
        (true, 0 | 6) => 30,
        (true, 2) => 40,
        (true, _) => {
            bits[10] &= !0x10;
            20
        }
        (false, _) => 20,
    };
    Ok((probe_core_header(&bits)?, profile))
}

/// One packet of the parser-packet reader together with the parser's clock for it.
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
    /// The packet as the source stores it.
    pub fn source_bytes(&self) -> &Bytes {
        self.packet.source_bytes()
    }
    /// The source offsets the packet covers, in bytes; the end is exclusive.
    pub fn physical_range(&self) -> Range<u64> {
        self.range.clone()
    }
    /// Source, parser-cursor generation and byte offset of the packet's first byte.
    pub fn identity(&self) -> DtsSourceIdentity {
        self.packet.identity
    }
    /// The cursors of the operation that read the packet.
    pub fn context(&self) -> ParserOperationContext {
        self.context
    }
    /// The packet as the packet reader returned it.
    pub fn packet(&self) -> &Dts14CarrierPacket {
        &self.packet
    }
    /// The packet's core header, when the whole packet is one exact normal frame
    /// ([`Dts14CarrierSyntax::ExactNormal`]). `None` for an opaque or refused packet:
    /// a header readable in the first bytes of such a packet is not reported here,
    /// and the header of a source frame is never put in its place.
    pub fn source_header(&self) -> Option<DtsCoreHeader> {
        if let Dts14CarrierSyntax::ExactNormal(header) = self.packet.syntax {
            Some(header)
        } else {
            None
        }
    }
    /// The parser's clock for the packet. It is not the duration of a source
    /// frame's coding window.
    ///
    /// Fails with [`Error::Unsupported`] where the parser's clock is not modelled: the
    /// core header in the packet's first 18 bytes is refused, the packet holds a
    /// complete extension substream extent at `FSIZE + 1` rounded up to a multiple
    /// of four, or an earlier packet of the operation had one of these.
    pub fn clock(&self) -> Result<ParserClock> {
        self.clock
    }
}

/// How one reader of a [`ParserOperation`] stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParserOperationTerminal {
    /// The source ended here, after the reader's last frame or packet.
    End(SourceLocation),
    /// The reader stopped with an error.
    Refused {
        /// The reader's error.
        error: Error,
        /// Where in the source the reader had read to.
        at: SourceLocation,
    },
}

/// One result of a [`ParserOperation`], taken with [`ParserOperation::take_event`].
#[derive(Clone, Debug)]
pub enum ParserOperationEvent {
    /// The next frame of the source-frame reader.
    SourceFrame(Dts14SyncFrame),
    /// The next packet of the parser-packet reader, with the parser's clock.
    ParserPacket(ParserPacket),
    /// A reader stopped with an error, which is a result like any other: the other
    /// reader continues, and [`ParserOperationStep::End`] reports the error again.
    ViewRefusal {
        /// The reader that stopped.
        lane: ParserOperationLane,
        /// Its error.
        error: Error,
        /// The input request it had open.
        request: InputRequest,
    },
}

/// The outcome of [`ParserOperation::step`].
#[derive(Debug)]
pub enum ParserOperationStep {
    /// An event is ready and complete: take it with
    /// [`ParserOperation::take_event`].
    Event,
    /// A reader needs input: answer `request` through the operation, naming the
    /// same `lane`.
    NeedInput {
        /// The reader that asks.
        lane: ParserOperationLane,
        /// The bytes it asks for.
        request: InputRequest,
    },
    /// The work budget is spent: call again with a new one.
    Yield,
    /// Both readers have stopped. This says how each one ended; it does not say
    /// that the reference or a decoder accepts the stream.
    End {
        /// How the source-frame reader stopped.
        source: ParserOperationTerminal,
        /// How the parser-packet reader stopped.
        parser: ParserOperationTerminal,
    },
}

/// Runs a [`Dts14SyncReader::per_frame`] and a [`Dts14CarrierReader`] over the same
/// raw 14-bit DTS entity, in turns, and returns the source frames of the first and
/// the parser packets of the second as events.
///
/// The two readers are run as they are and stay unconnected: nothing is queued
/// between them, no packet is assembled from source frames, and no recorded
/// reference output is used. At most one event is held at a time. A cancellation
/// or a source failure drops both readers at once; events the caller has taken
/// keep their source and unpacked bytes.
///
/// [`step`](Self::step) names the reader a request belongs to. Answer it with
/// [`provide`](Self::provide), [`finish`](Self::finish) or [`fail`](Self::fail) for
/// that lane, and give both readers the same bytes.
pub struct ParserOperation {
    source: Lane<Dts14SyncReader>,
    parser: Lane<Dts14CarrierReader>,
    context: ParserOperationContext,
    turn: ParserOperationLane,
    dispatch_paid: bool,
    /// A parser packet whose clock is projected next, `clock_charge` work units of
    /// it paid.
    pending_packet: Option<Dts14CarrierPacket>,
    ready_event: Option<ParserOperationEvent>,
    clock_charge: u16,
    clock_state: ClockState,
    failure: Option<Error>,
}
const _: () = assert!(size_of::<ParserOperation>() <= 4096);
const _: () = assert!(size_of::<ParserOperationStep>() <= 256);

/// One reader of a [`ParserOperation`].
enum Lane<R> {
    /// The reader, still reading.
    Reading(R),
    /// How the reader stopped.
    Stopped(ParserOperationTerminal),
    /// The operation failed and dropped the reader.
    Dropped,
}

impl<R> Lane<R> {
    const fn terminal(&self) -> Option<ParserOperationTerminal> {
        match self {
            Self::Stopped(terminal) => Some(*terminal),
            Self::Reading(_) | Self::Dropped => None,
        }
    }
}

/// The reader of one lane, borrowed.
enum LaneReader<'a> {
    Source(&'a mut Dts14SyncReader),
    Parser(&'a mut Dts14CarrierReader),
}

/// What the reader of one lane returned.
enum LaneOutput {
    Frame(Dts14SyncFrame),
    Packet(Dts14CarrierPacket),
}

/// `step` with its value made a lane output.
fn lane_step<T>(step: Step<T>, output: fn(T) -> LaneOutput) -> Step<LaneOutput> {
    match step {
        Step::Ready(value) => Step::Ready(output(value)),
        Step::NeedInput(request) => Step::NeedInput(request),
        Step::Yield => Step::Yield,
        Step::End(end) => Step::End(end),
    }
}

impl std::fmt::Debug for ParserOperation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParserOperation")
            .field("context", &self.context)
            .field("turn", &self.turn)
            .field("source_end", &self.source.terminal())
            .field("parser_end", &self.parser.terminal())
            .field("failure", &self.failure)
            .finish_non_exhaustive()
    }
}

impl ParserOperation {
    /// An operation at offset zero of `source`, whose 16-bit words are in `order`.
    ///
    /// Fails as [`Dts14SyncReader::per_frame`] and [`Dts14CarrierReader::new`] do for
    /// their limits.
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
        };
        Ok(Self {
            source: Lane::Reading(facts),
            parser: Lane::Reading(parser),
            context,
            turn: ParserOperationLane::Source,
            dispatch_paid: false,
            pending_packet: None,
            ready_event: None,
            clock_charge: 0,
            clock_state: ClockState::new(order),
            failure: None,
        })
    }
    /// The source and the cursor generations of the two readers.
    pub fn context(&self) -> ParserOperationContext {
        self.context
    }
    /// Takes the event that [`ParserOperationStep::Event`] announced, once. Until then
    /// every [`step`](Self::step) returns `Event` again and reads nothing, allocates
    /// nothing and charges no work. Fails with [`Error::Invalid`] when no event is
    /// ready. A failed operation returns its earlier error.
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
        self.source = Lane::Dropped;
        self.parser = Lane::Dropped;
        self.pending_packet = None;
        self.ready_event = None;
        self.failure = Some(error);
    }
    /// The reader of `lane` while it reads.
    fn reader(&mut self, lane: ParserOperationLane) -> Result<LaneReader<'_>> {
        self.healthy()?;
        match (lane, &mut self.source, &mut self.parser) {
            (ParserOperationLane::Source, Lane::Reading(reader), _) => {
                Ok(LaneReader::Source(reader))
            }
            (ParserOperationLane::ParserProjection, _, Lane::Reading(reader)) => {
                Ok(LaneReader::Parser(reader))
            }
            _ => Err(Error::Invalid("DTS operation view is terminal")),
        }
    }
    /// The input request of one reader. Fails with [`Error::Invalid`] when that reader
    /// has stopped. A failed operation returns its earlier error.
    pub fn request(&self, lane: ParserOperationLane) -> Result<InputRequest> {
        self.healthy()?;
        match (lane, &self.source, &self.parser) {
            (ParserOperationLane::Source, Lane::Reading(reader), _) => Ok(reader.request()),
            (ParserOperationLane::ParserProjection, _, Lane::Reading(reader)) => {
                Ok(reader.request())
            }
            _ => Err(Error::Invalid("DTS operation view is terminal")),
        }
    }
    /// Answers the current request of one reader with `window`. Fails as
    /// [`request`](Self::request) does, or with the reader's refusal of a window that
    /// does not answer its request, which leaves the operation as it was.
    pub fn provide(&mut self, lane: ParserOperationLane, window: &InputWindow) -> Result<()> {
        match self.reader(lane)? {
            LaneReader::Source(reader) => reader.provide(window),
            LaneReader::Parser(reader) => reader.provide(window),
        }
    }
    /// Answers the current request of one reader with the end of the source. Each
    /// reader is told separately.
    ///
    /// Fails as [`request`](Self::request) does, or with the reader's refusal of a token
    /// or source that does not match its request, which leaves the operation as it was,
    /// or with [`Error::Truncated`] when the source ends before its known length: that
    /// reader then stops with a [`ParserOperationEvent::ViewRefusal`].
    pub fn finish(
        &mut self,
        lane: ParserOperationLane,
        source: SourceId,
        token: InputToken,
    ) -> Result<()> {
        match self.reader(lane)? {
            LaneReader::Source(reader) => reader.finish(source, token),
            LaneReader::Parser(reader) => reader.finish(source, token),
        }
    }
    /// Answers the current request of one reader with a source failure, which fails
    /// the whole operation: both readers and an event not yet taken are dropped, and
    /// every later call returns [`Error::Transport`] or [`Error::Cancelled`]. The
    /// other reader's bytes never become a last frame or packet.
    ///
    /// Fails as [`request`](Self::request) does, or with the reader's refusal of a token
    /// or source that does not match its request, which leaves the operation as it was.
    pub fn fail(
        &mut self,
        lane: ParserOperationLane,
        source: SourceId,
        token: InputToken,
        failure: SourceFailure,
    ) -> Result<()> {
        match self.reader(lane)? {
            LaneReader::Source(reader) => reader.fail(source, token, failure)?,
            LaneReader::Parser(reader) => reader.fail(source, token, failure)?,
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
    /// Stops the reader of `lane` with `terminal`.
    fn stop(&mut self, lane: ParserOperationLane, terminal: ParserOperationTerminal) {
        match lane {
            ParserOperationLane::Source => self.source = Lane::Stopped(terminal),
            ParserOperationLane::ParserProjection => self.parser = Lane::Stopped(terminal),
        }
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
        self.stop(lane, terminal);
        self.next_turn();
        self.publish(ParserOperationEvent::ViewRefusal {
            lane,
            error,
            request,
        })
    }
    /// Advances the two readers in turns, as far as `work` allows. Besides the
    /// readers' own work, one unit is charged for each turn and 128 for the clock of
    /// each parser packet.
    ///
    /// [`Error::Backpressure`] and [`Error::Allocation`] leave the operation as it was:
    /// free pool storage and call again. [`Error::Cancelled`] when `cancel` is set, and
    /// [`Error::Invalid`] when the two readers end at different source locations, fail
    /// the operation for good. Any other error of a reader is not returned: it stops
    /// that reader and arrives as a [`ParserOperationEvent::ViewRefusal`].
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
                // Bound the core probe, optional extension extent and clock
                // arithmetic even when the caller grants one unit at a time.
                while self.clock_charge < 168 {
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
            if let (Some(source), Some(parser)) = (self.source.terminal(), self.parser.terminal()) {
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
            let lane = self.turn;
            let reader = match (lane, &mut self.source, &mut self.parser) {
                (ParserOperationLane::Source, Lane::Reading(reader), _) => {
                    LaneReader::Source(reader)
                }
                (ParserOperationLane::ParserProjection, _, Lane::Reading(reader)) => {
                    LaneReader::Parser(reader)
                }
                _ => {
                    self.next_turn();
                    continue;
                }
            };
            if !self.dispatch_paid {
                if !work.try_charge(1) {
                    return Ok(ParserOperationStep::Yield);
                }
                self.dispatch_paid = true;
            }
            let result = match reader {
                LaneReader::Source(reader) => reader
                    .step(work, cancel)
                    .map(|step| lane_step(step, LaneOutput::Frame)),
                LaneReader::Parser(reader) => reader
                    .step(work, cancel)
                    .map(|step| lane_step(step, LaneOutput::Packet)),
            };
            match result {
                Ok(Step::Ready(LaneOutput::Frame(frame))) => {
                    self.next_turn();
                    return self.publish(ParserOperationEvent::SourceFrame(frame));
                }
                Ok(Step::Ready(LaneOutput::Packet(packet))) => {
                    self.pending_packet = Some(packet);
                    self.clock_charge = 0;
                }
                Ok(Step::NeedInput(request)) => {
                    self.next_turn();
                    return Ok(ParserOperationStep::NeedInput { lane, request });
                }
                Ok(Step::Yield) => return Ok(ParserOperationStep::Yield),
                Ok(Step::End(end)) => {
                    self.stop(lane, ParserOperationTerminal::End(end));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_extension_profiles_keep_the_core_clock() {
        let mut prefix = [
            0x7f, 0xfe, 0x80, 0x01, 0xfc, 0x3c, 0x05, 0xf0, 0xb5, 0xe0, 0x01, 0x38, 0, 0, 0,
        ];
        for (id, expected) in [30, 20, 40, 20, 20, 20, 30, 20].into_iter().enumerate() {
            prefix[10] = (id as u8) << 5 | 0x11;
            let (header, profile) = clock_header(prefix).unwrap();
            assert_eq!(
                (header.core_rate_hz, header.coding_window_samples),
                (48000, 512)
            );
            assert_eq!(profile, expected);
            assert_eq!(probe_core_header(&prefix).is_ok(), expected != 20);
        }
    }
}

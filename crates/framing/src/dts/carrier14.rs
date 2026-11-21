//! Packets of a 14-bit DTS stream, cut at the byte pattern of its sync words. This
//! is a reader of its own beside the frame reader for 14-bit streams. The 14-bit
//! unpacking follows ETSI TS 102 114 5.3. A packet starts only at the 16-bit words
//! `1FFF E800 07Fx`, the two unused high bits of each word included; that is
//! narrower than the frame reader's sync, which accepts any value in those bits. A
//! packet that does not start with the pattern stays opaque: it gets no header, no
//! unpacked payload, no claim about its audio, and no end but the end of the source.
use crate::dts::{
    Dts14BitPayload, Dts14BitUnpacker, DtsCoreHeader, DtsPackingLimits, DtsSourceIdentity,
    DtsWordOrder, probe_core_header,
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, BudgetVec, Bytes, Error, InputCursor, InputRequest, InputToken, InputWindow, Result,
    SourceCapabilities, SourceFailure, SourceId, SourceLocation, Step, WorkBudget,
};

/// Limits of a [`Dts14CarrierReader`]. None may be zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dts14CarrierLimits {
    /// The most source bytes one packet may hold.
    pub maximum_packet_bytes: usize,
    /// The most packets read since construction or the last reset.
    pub maximum_packets: usize,
    /// The most bytes one input window may hold.
    pub maximum_input_window: usize,
}
/// What ended a packet of a [`Dts14CarrierReader`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dts14CarrierBoundary {
    /// The next sync pattern starts where this packet ends.
    FollowingCarrierSync {
        /// Source offset of the following sync pattern, in bytes.
        offset: u64,
    },
    /// The source ended, as [`Dts14CarrierReader::finish`] declared.
    ExplicitSourceEnd,
}
/// What a packet of a [`Dts14CarrierReader`] holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dts14CarrierSyntax {
    /// The packet does not start with the sync pattern. It keeps all of its source
    /// bytes and is not unpacked.
    OpaqueCarrier,
    /// The packet unpacks to exactly one normal core frame: its 14-bit payload ends
    /// on a byte boundary, [`probe_core_header`] accepts its header, and it holds
    /// `FSIZE` + 1 bytes. Carries that header.
    ExactNormal(DtsCoreHeader),
    /// The packet starts with the sync pattern but is not one exact normal frame.
    /// Carries the reason: [`Error::Truncated`] for an odd number of source bytes,
    /// the header's refusal by [`probe_core_header`], or [`Error::Unsupported`] for
    /// a payload that does not end on a byte boundary or does not hold `FSIZE` + 1
    /// bytes.
    Refused(Error),
}
/// Where the reader's input stood when it returned a packet: the last window it
/// accepted and how far it had read. These describe the input, not the packet's
/// boundaries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dts14CarrierProvision {
    /// Source offset of the first byte of the last window accepted; the reader's
    /// starting offset before any window.
    pub last_window_start: u64,
    /// Source offset after the last byte of that window.
    pub last_window_end: u64,
    /// Source offset after the last byte the reader had read: six bytes past a
    /// packet that ended at a following sync pattern, the packet's end at the end
    /// of the source.
    pub consumed_through: u64,
}
/// One packet of a [`Dts14CarrierReader`]. It owns its bytes and outlives the
/// reader.
#[derive(Clone, Debug)]
pub struct Dts14CarrierPacket {
    raw: Bytes,
    payload: Option<Dts14BitPayload>,
    /// Source, cursor generation and byte offset of the packet's first byte.
    pub identity: DtsSourceIdentity,
    /// What ended the packet.
    pub boundary: Dts14CarrierBoundary,
    /// What the packet holds.
    pub syntax: Dts14CarrierSyntax,
    /// Where the reader's input stood when it returned the packet.
    pub provision: Dts14CarrierProvision,
}
impl Dts14CarrierPacket {
    /// The packet as the source stores it.
    pub fn source_bytes(&self) -> &Bytes {
        &self.raw
    }
    /// The packet's unpacked 14-bit payload. `None` for an opaque packet and for
    /// one with an odd number of source bytes.
    pub fn payload(&self) -> Option<&Dts14BitPayload> {
        self.payload.as_ref()
    }
    /// The whole bytes of the unpacked payload; `None` when
    /// [`payload`](Self::payload) is.
    pub fn canonical_bytes(&self) -> Option<&Bytes> {
        self.payload.as_ref().map(Dts14BitPayload::canonical_bytes)
    }
}
#[derive(Clone, Copy)]
enum Phase {
    Reserve,
    Scan,
    Check,
    Append,
    GrowReserve,
    GrowCopy,
    GrowSwap,
    Freeze,
    Unpack,
    Publish,
}
/// Cuts a 14-bit DTS stream into packets at its sync pattern: the 16-bit words
/// `1FFF E800 07Fx` in a given byte order.
///
/// A packet that starts with the pattern ends at the next pattern found where its
/// frame ends, or at the end of the source. The frame's length is `FSIZE` + 1 bytes
/// from the header in the packet's first 18 source bytes, eight source bytes for
/// every seven; a pattern before that point is packet content. A stream that does
/// not start with the pattern is one opaque packet that runs to the end of the
/// source. [`Dts14CarrierPacket::syntax`] says whether a packet unpacks to one exact
/// normal core frame.
///
/// Bytes read so far become a packet only when [`finish`](Self::finish) declares the
/// end of the source, never after [`fail`](Self::fail). Every packet owns its bytes,
/// whatever becomes of the reader or of the input windows. Complete frames of the
/// same stream come from [`Dts14SyncReader`](crate::dts::Dts14SyncReader); the two
/// readers are independent. With the `reference-trace` feature,
/// `reference_trace::dts` pairs these packets with the clock and byte positions of
/// FFmpeg's `dca` parser.
///
/// Call [`step`](Self::step) and answer each [`Step::NeedInput`] with
/// [`provide`](Self::provide), [`finish`](Self::finish) or [`fail`](Self::fail).
pub struct Dts14CarrierReader {
    pool: Budget,
    cursor: Option<InputCursor>,
    last_request: InputRequest,
    order: DtsWordOrder,
    limits: Dts14CarrierLimits,
    phase: Phase,
    buffer: Option<BudgetVec<u8>>,
    growth: Option<BudgetVec<u8>>,
    growth_at: usize,
    extent: Option<usize>,
    header_probed: bool,
    last_window_start: u64,
    last_window_end: u64,
    raw: Option<Bytes>,
    unpacker: Option<Dts14BitUnpacker>,
    payload: Option<Dts14BitPayload>,
    syntax: Dts14CarrierSyntax,
    look: [u8; 6],
    look_len: usize,
    append_at: usize,
    append_count: usize,
    offset: u64,
    end: Option<SourceLocation>,
    complete: bool,
    packets: usize,
    failure: Option<Error>,
}
impl std::fmt::Debug for Dts14CarrierReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dts14CarrierReader")
            .field("order", &self.order)
            .field("limits", &self.limits)
            .field("position", &self.position())
            .field("generation", &self.generation())
            .field("packets", &self.packets)
            .field("failure", &self.failure)
            .finish_non_exhaustive()
    }
}
impl Dts14CarrierReader {
    /// A reader at offset zero of `source`, whose 16-bit words are in `order`.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] when a limit is zero, and [`Error::Limit`] when the first
    /// 128 bytes of a packet, or `maximum_packet_bytes` if that is less, cannot fit
    /// `pool`.
    pub fn new(
        pool: &Budget,
        source: SourceId,
        caps: SourceCapabilities,
        order: DtsWordOrder,
        limits: Dts14CarrierLimits,
    ) -> Result<Self> {
        if limits.maximum_packet_bytes == 0
            || limits.maximum_packets == 0
            || limits.maximum_input_window == 0
        {
            return Err(Error::Invalid("zero DTS carrier limit"));
        }
        let minimum = limits
            .maximum_packet_bytes
            .min(128)
            .checked_add(Bytes::owner_overhead_bytes())
            .and_then(|n| n.checked_add(Budget::control_storage_bytes()))
            .ok_or(Error::Overflow)?;
        if minimum > pool.limit() {
            return Err(Error::Limit {
                requested: minimum,
                maximum: pool.limit(),
            });
        }
        let cursor = InputCursor::new(pool, source, caps, limits.maximum_input_window)?;
        let last_request = cursor.request();
        Ok(Self {
            pool: pool.clone(),
            cursor: Some(cursor),
            last_request,
            order,
            limits,
            phase: Phase::Reserve,
            buffer: None,
            growth: None,
            growth_at: 0,
            extent: None,
            header_probed: false,
            last_window_start: 0,
            last_window_end: 0,
            raw: None,
            unpacker: None,
            payload: None,
            syntax: Dts14CarrierSyntax::OpaqueCarrier,
            look: [0; 6],
            look_len: 0,
            append_at: 0,
            append_count: 0,
            offset: 0,
            end: None,
            complete: false,
            packets: 0,
            failure: None,
        })
    }
    /// The request for the bytes at [`position`](Self::position). A failed reader
    /// returns the request it had open when it failed.
    pub fn request(&self) -> InputRequest {
        self.cursor
            .as_ref()
            .map_or_else(|| self.last_request.clone(), InputCursor::request)
    }
    /// Source offset of the next byte the reader takes from its input. After a
    /// packet that ended at a following sync pattern it is six bytes past the
    /// packet's end: the reader has read that pattern.
    pub fn position(&self) -> u64 {
        self.cursor
            .as_ref()
            .map_or(self.last_request.offset, InputCursor::position)
    }
    /// Generation of the reader's input cursor, which every packet carries in its
    /// identity. [`reset`](Self::reset) starts a new one.
    pub fn generation(&self) -> u64 {
        self.request().token.generation()
    }
    fn healthy(&self) -> Result<()> {
        self.failure.map_or(Ok(()), Err)
    }
    /// Answers the current request with `window`.
    ///
    /// # Errors
    ///
    /// The cursor's refusal of a window that does not answer the request (see
    /// [`InputCursor::provide`]), which leaves the reader as it was. A failed reader
    /// returns its earlier error.
    pub fn provide(&mut self, window: &InputWindow) -> Result<()> {
        self.healthy()?;
        self.cursor_mut()?.provide(window)?;
        self.last_window_start = window.offset;
        self.last_window_end = window
            .offset
            .checked_add(u64::try_from(window.bytes().len()).map_err(|_| Error::Overflow)?)
            .ok_or(Error::Overflow)?;
        Ok(())
    }
    /// Answers the current request with the end of the source, which also ends the
    /// packet being read.
    ///
    /// # Errors
    ///
    /// [`Error::Truncated`], from then on, when the source ends before its known
    /// length. A token or source that does not match the request is refused and
    /// leaves the reader as it was. A failed reader returns its earlier error.
    pub fn finish(&mut self, source: SourceId, token: InputToken) -> Result<()> {
        self.healthy()?;
        let r = self.cursor_mut()?.finish(source, token);
        if r == Err(Error::Truncated) {
            self.abort(Error::Truncated);
        }
        r
    }
    /// Answers the current request with a source failure. The reader drops the
    /// packet it was reading and from then on returns [`Error::Transport`] or
    /// [`Error::Cancelled`]; the bytes read so far never become a packet.
    ///
    /// # Errors
    ///
    /// A token or source that does not match the request is refused and leaves the
    /// reader as it was. A failed reader returns its earlier error.
    pub fn fail(
        &mut self,
        source: SourceId,
        token: InputToken,
        failure: SourceFailure,
    ) -> Result<()> {
        self.healthy()?;
        self.cursor_mut()?.fail(source, token, failure)?;
        self.abort(match failure {
            SourceFailure::Transport => Error::Transport,
            SourceFailure::Cancelled => Error::Cancelled,
        });
        Ok(())
    }
    /// Restarts at source offset `offset`, where a packet then starts, and returns
    /// the request for the bytes there. The packet being read is dropped, the packet
    /// count starts again and the cursor takes a new generation, so answers to
    /// earlier requests are refused. Packets already returned stay as they are.
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`] when `offset` is odd, or the cursor's refusal of an
    /// offset it cannot serve (see [`InputCursor::reset`]). A failed reader returns
    /// its earlier error and cannot be reset.
    pub fn reset(&mut self, offset: u64) -> Result<InputRequest> {
        self.healthy()?;
        if !offset.is_multiple_of(2) {
            return Err(Error::Unsupported("DTS carrier reset outside word grid"));
        }
        let r = self.cursor_mut()?.reset(offset)?;
        self.clear();
        self.phase = Phase::Reserve;
        self.look_len = 0;
        self.offset = offset;
        self.last_window_start = offset;
        self.last_window_end = offset;
        self.end = None;
        self.complete = false;
        self.packets = 0;
        Ok(r)
    }
    fn clear(&mut self) {
        self.buffer = None;
        self.growth = None;
        self.raw = None;
        self.unpacker = None;
        self.payload = None;
    }
    fn cursor_mut(&mut self) -> Result<&mut InputCursor> {
        self.cursor
            .as_mut()
            .ok_or(Error::Invalid("missing healthy carrier cursor"))
    }
    fn abort(&mut self, e: Error) {
        self.last_request = self.request();
        self.cursor = None;
        self.failure = Some(e);
        self.clear();
    }
    /// Reads on, as far as `work` allows. [`Step::Ready`] carries the next packet,
    /// [`Step::NeedInput`] the request to answer before the next call, and
    /// [`Step::End`] the end of the source once the last packet is returned;
    /// [`Step::Yield`] means the work is spent.
    ///
    /// # Errors
    ///
    /// [`Error::Backpressure`] and [`Error::Allocation`] leave the reader as it was:
    /// free pool storage and call again. Every other error is kept and returned by
    /// all later calls: [`Error::Cancelled`] when `cancel` is set,
    /// [`Error::Unsupported`] for a sync pattern that follows opaque bytes, that
    /// follows a packet whose header gives no length of whole words, or that first
    /// comes after the frame's end, and [`Error::Limit`] when a packet, the number
    /// of packets or the pool's size is exceeded.
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<Dts14CarrierPacket>> {
        self.healthy()?;
        let r = self.advance(work, cancel);
        if let Err(e) = r
            && !matches!(e, Error::Backpressure { .. } | Error::Allocation)
        {
            self.abort(e);
        }
        r
    }
    fn advance(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<Dts14CarrierPacket>> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if self.complete {
                return Ok(Step::End(
                    self.end.ok_or(Error::Invalid("carrier end missing"))?,
                ));
            }
            if work.remaining() == 0 {
                return Ok(Step::Yield);
            }
            match self.phase {
                Phase::Reserve => {
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    let b = self
                        .buffer
                        .get_or_insert_with(|| BudgetVec::new(&self.pool));
                    b.try_reserve_exact(self.limits.maximum_packet_bytes.min(128))?;
                    self.extent = None;
                    self.header_probed = false;
                    self.syntax = Dts14CarrierSyntax::OpaqueCarrier;
                    self.phase = if self.look_len == 0 {
                        Phase::Scan
                    } else {
                        self.append_at = 0;
                        self.append_count = self.look_len;
                        Phase::Append
                    };
                }
                Phase::Scan => {
                    match self
                        .cursor
                        .as_mut()
                        .ok_or(Error::Invalid("missing carrier scan cursor"))?
                        .read_into(&mut self.look[self.look_len..], work, cancel)?
                    {
                        Step::Ready(n) => {
                            if n == 0 {
                                return Err(Error::Invalid("empty carrier read"));
                            }
                            self.look_len += n;
                            if self.look_len == 6 {
                                self.phase = Phase::Check;
                            }
                        }
                        Step::NeedInput(r) => return Ok(Step::NeedInput(r)),
                        Step::Yield => return Ok(Step::Yield),
                        Step::End(e) => {
                            self.end = Some(e);
                            if self.look_len == 0 {
                                self.phase = Phase::Freeze;
                            } else {
                                self.append_at = 0;
                                self.append_count = self.look_len;
                                self.phase = Phase::Append;
                            }
                        }
                    }
                }
                Phase::Check => {
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    let b = self
                        .buffer
                        .as_ref()
                        .ok_or(Error::Invalid("carrier builder missing"))?;
                    // Reuse the existing fixed120-bit primary header probe.
                    // The physical extent is admitted only for exact-word
                    // geometry; neither a96-byte cutoff nor a signature alone
                    // proves a boundary. This never validates coded audio.
                    if !self.header_probed && b.len() >= 18 {
                        self.header_probed = true;
                        if conventional_sync(b.as_slice(), self.order) {
                            let prefix: [u8; 18] = b.as_slice()[..18]
                                .try_into()
                                .map_err(|_| Error::Truncated)?;
                            let bits = crate::dts::sync14::header_payload(&prefix, self.order)?;
                            if let Ok(header) = probe_core_header(&bits)
                                && header.frame_bytes % 7 == 0
                            {
                                self.extent = Some(
                                    (header.frame_bytes / 7)
                                        .checked_mul(8)
                                        .ok_or(Error::Overflow)?,
                                );
                            }
                        }
                    }
                    if b.len() >= 6 && conventional_sync(&self.look, self.order) {
                        if !conventional_sync(b.as_slice(), self.order) {
                            return Err(Error::Unsupported(
                                "mixed opaque/conventional DTS carrier",
                            ));
                        }
                        match self.extent {
                            Some(end) if b.len() == end => {
                                self.phase = Phase::Freeze;
                                continue;
                            }
                            Some(end) if b.len() < end => {}
                            Some(_) => {
                                return Err(Error::Unsupported("late DTS carrier synchronization"));
                            }
                            None if self.header_probed => {
                                return Err(Error::Unsupported("unresolved DTS carrier extent"));
                            }
                            None => {}
                        }
                    }
                    self.append_at = 0;
                    self.append_count = 2;
                    self.phase = Phase::Append;
                }
                Phase::Append => {
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    if self.append_at < self.append_count {
                        let b = self
                            .buffer
                            .as_mut()
                            .ok_or(Error::Invalid("carrier append missing"))?;
                        if b.len() >= self.limits.maximum_packet_bytes {
                            return Err(Error::Limit {
                                requested: b.len().checked_add(1).ok_or(Error::Overflow)?,
                                maximum: self.limits.maximum_packet_bytes,
                            });
                        }
                        if b.len() == b.capacity() {
                            self.phase = Phase::GrowReserve;
                            continue;
                        }
                        b.try_push(self.look[self.append_at]).map_err(|e| e.error)?;
                        self.append_at += 1;
                    } else {
                        self.look.copy_within(self.append_count..self.look_len, 0);
                        self.look_len -= self.append_count;
                        self.phase = if self.end.is_some() {
                            Phase::Freeze
                        } else {
                            Phase::Scan
                        };
                    }
                }
                Phase::GrowReserve => {
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    let old = self
                        .buffer
                        .as_ref()
                        .ok_or(Error::Invalid("missing carrier growth input"))?;
                    let target = old
                        .capacity()
                        .checked_mul(2)
                        .ok_or(Error::Overflow)?
                        .min(self.limits.maximum_packet_bytes);
                    let minimum = old
                        .capacity()
                        .checked_add(target)
                        .and_then(|n| n.checked_add(Budget::control_storage_bytes()))
                        .ok_or(Error::Overflow)?;
                    if minimum > self.pool.limit() {
                        return Err(Error::Limit {
                            requested: minimum,
                            maximum: self.pool.limit(),
                        });
                    }
                    let next = self
                        .growth
                        .get_or_insert_with(|| BudgetVec::new(&self.pool));
                    // Only an empty allocation grows. No dynamic prefix moves
                    // inside an allocation call or one uncharged operation.
                    next.try_reserve_exact(target)?;
                    self.growth_at = 0;
                    self.phase = Phase::GrowCopy;
                }
                Phase::GrowCopy => {
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    let old = self
                        .buffer
                        .as_ref()
                        .ok_or(Error::Invalid("missing carrier copied input"))?;
                    if self.growth_at == old.len() {
                        self.phase = Phase::GrowSwap;
                    } else {
                        let next = self
                            .growth
                            .as_mut()
                            .ok_or(Error::Invalid("missing carrier growth output"))?;
                        if next.len() == next.capacity() {
                            return Err(Error::Invalid("carrier growth reservation exhausted"));
                        }
                        next.try_push(old.as_slice()[self.growth_at])
                            .map_err(|e| e.error)?;
                        self.growth_at += 1;
                    }
                }
                Phase::GrowSwap => {
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    self.buffer = self.growth.take();
                    self.phase = Phase::Append;
                }
                Phase::Freeze => {
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    let b = self
                        .buffer
                        .take()
                        .ok_or(Error::Invalid("carrier freeze missing"))?;
                    if b.is_empty() && self.end.is_some() {
                        self.complete = true;
                        continue;
                    }
                    if self.packets >= self.limits.maximum_packets {
                        self.buffer = Some(b);
                        return Err(Error::Limit {
                            requested: self.packets.checked_add(1).ok_or(Error::Overflow)?,
                            maximum: self.limits.maximum_packets,
                        });
                    }
                    match b.freeze() {
                        Ok(raw) => self.raw = Some(raw),
                        Err(e) => {
                            self.buffer = Some(e.value);
                            return Err(e.error);
                        }
                    }
                    let raw = self
                        .raw
                        .as_ref()
                        .ok_or(Error::Invalid("carrier raw missing"))?;
                    if raw.len() >= 6 && conventional_sync(raw.as_ref(), self.order) {
                        if raw.len() % 2 != 0 {
                            self.syntax = Dts14CarrierSyntax::Refused(Error::Truncated);
                            self.phase = Phase::Publish;
                        } else {
                            self.phase = Phase::Unpack;
                        }
                    } else {
                        self.phase = Phase::Publish;
                    }
                }
                Phase::Unpack => {
                    if self.unpacker.is_none() {
                        if !work.try_charge(1) {
                            return Ok(Step::Yield);
                        }
                        self.unpacker = Some(Dts14BitUnpacker::new(
                            &self.pool,
                            self.identity(),
                            self.raw
                                .as_ref()
                                .ok_or(Error::Invalid("carrier unpack missing"))?
                                .clone(),
                            self.order,
                            DtsPackingLimits {
                                maximum_source_bytes: self.limits.maximum_packet_bytes,
                                maximum_output_bytes: self.limits.maximum_packet_bytes,
                            },
                        )?);
                    }
                    match self
                        .unpacker
                        .as_mut()
                        .ok_or(Error::Invalid("carrier unpacker missing"))?
                        .step(work, cancel)?
                    {
                        Step::Ready(p) => {
                            // No discarded tail or silently selected FSIZE domain.
                            let checked =
                                probe_core_header(p.canonical_bytes().as_ref()).and_then(|h| {
                                    if p.residual_bits() != 0
                                        || h.frame_bytes != p.canonical_bytes().len()
                                    {
                                        Err(Error::Unsupported(
                                            "DTS carrier unresolved extent/tail",
                                        ))
                                    } else {
                                        Ok(h)
                                    }
                                });
                            self.syntax = match checked {
                                Ok(h) => Dts14CarrierSyntax::ExactNormal(h),
                                Err(e) => Dts14CarrierSyntax::Refused(e),
                            };
                            self.payload = Some(p);
                            self.unpacker = None;
                            self.phase = Phase::Publish;
                        }
                        Step::Yield => return Ok(Step::Yield),
                        _ => return Err(Error::Invalid("carrier owned unpack progress")),
                    }
                }
                Phase::Publish => {
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    let raw = self
                        .raw
                        .take()
                        .ok_or(Error::Invalid("carrier publication missing"))?;
                    let next = self
                        .offset
                        .checked_add(u64::try_from(raw.len()).map_err(|_| Error::Overflow)?)
                        .ok_or(Error::Overflow)?;
                    let boundary = match self.end {
                        Some(e) => {
                            if e.offset != next {
                                return Err(Error::Invalid("carrier EOF range"));
                            }
                            self.complete = true;
                            Dts14CarrierBoundary::ExplicitSourceEnd
                        }
                        None => Dts14CarrierBoundary::FollowingCarrierSync { offset: next },
                    };
                    let packet = Dts14CarrierPacket {
                        identity: self.identity(),
                        raw,
                        payload: self.payload.take(),
                        boundary,
                        syntax: self.syntax,
                        provision: Dts14CarrierProvision {
                            last_window_start: self.last_window_start,
                            last_window_end: self.last_window_end,
                            consumed_through: self.position(),
                        },
                    };
                    self.offset = next;
                    self.packets = self.packets.checked_add(1).ok_or(Error::Overflow)?;
                    self.phase = Phase::Reserve;
                    return Ok(Step::Ready(packet));
                }
            }
        }
    }
    fn identity(&self) -> DtsSourceIdentity {
        DtsSourceIdentity {
            source: self.request().source,
            generation: self.generation(),
            offset: self.offset,
        }
    }
}
/// Conventional physical representation of the primary extended normal sync.
/// Only the carrier signature is tested here; this is not syntax validation.
fn conventional_sync(b: &[u8], order: DtsWordOrder) -> bool {
    if b.len() < 6 {
        return false;
    }
    let word = |i| match order {
        DtsWordOrder::BigEndian => u16::from_be_bytes([b[i], b[i + 1]]),
        DtsWordOrder::LittleEndian => u16::from_le_bytes([b[i], b[i + 1]]),
    };
    word(0) == 0x1fff && word(2) == 0xe800 && word(4) & 0xfff0 == 0x07f0
}

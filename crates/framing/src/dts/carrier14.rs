//! Packets of a 14-bit DTS stream, cut at the byte pattern of its sync words. This
//! is a reader of its own beside the frame reader for 14-bit streams. The 14-bit
//! unpacking follows ETSI TS 102 114 5.3. A packet starts only at the 16-bit words
//! `1FFF E800 07Fx`, the two unused high bits of each word included; that is
//! narrower than the frame reader's sync, which accepts any value in those bits. A
//! packet that does not start with the pattern stays opaque: it gets no header, no
//! unpacked payload, no claim about its audio, and no end but the end of the source.
use crate::dts::reader::{ReaderInput, limit, source_error};
use crate::dts::sync14::{header_payload, word};
use crate::dts::{
    Dts14BitPayload, Dts14BitUnpacker, DtsCoreHeader, DtsPackingLimits, DtsSourceIdentity,
    DtsWordOrder, probe_core_header,
};
use crate::retryable;
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
/// The packet a [`Dts14CarrierReader`] is reading.
enum Packet {
    /// Reserving the first storage of the next packet.
    Reserve,
    /// Reading the packet's bytes into `buffer`.
    Body {
        buffer: BudgetVec<u8>,
        extent: Extent,
        step: BodyStep,
    },
    /// The packet's bytes, which start with the sync pattern and are unpacked next.
    Frozen { raw: Bytes },
    /// Unpacking the packet's 14-bit words.
    Unpack {
        raw: Bytes,
        unpacker: Dts14BitUnpacker,
    },
    /// The packet, to be returned.
    Ready {
        raw: Bytes,
        payload: Option<Dts14BitPayload>,
        syntax: Dts14CarrierSyntax,
    },
    /// The source ended after the last packet.
    Ended(SourceLocation),
}

/// Where the frame of a packet that starts with the sync pattern ends.
#[derive(Clone, Copy)]
enum Extent {
    /// The packet's first 18 bytes are not read yet.
    Unprobed,
    /// Its header gives no length of whole words, or the packet does not start with
    /// the pattern.
    Unknown,
    /// The frame spans this many source bytes.
    Known(usize),
}

/// Moving the first `count` lookahead bytes into the packet, `at` of them so far.
#[derive(Clone, Copy)]
struct Append {
    at: usize,
    count: usize,
}

/// The steps of reading a packet's bytes.
enum BodyStep {
    /// Reading the six bytes after the packet so far into the lookahead.
    Scan,
    /// Deciding whether the lookahead starts the next packet.
    Check,
    /// Moving lookahead bytes into the packet.
    Append(Append),
    /// Moving the packet into the larger storage `next`, then appending on.
    Grow {
        append: Append,
        next: BudgetVec<u8>,
        step: GrowStep,
    },
    /// The packet's bytes are complete.
    Freeze,
}

/// The steps of moving a packet into larger storage.
#[derive(Clone, Copy)]
enum GrowStep {
    /// Reserving the larger storage.
    Reserve,
    /// Copying the packet, `copied` bytes of it so far.
    Copy { copied: usize },
    /// Replacing the packet's storage with the larger one.
    Swap,
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
    input: ReaderInput,
    order: DtsWordOrder,
    limits: Dts14CarrierLimits,
    packet: Packet,
    last_window_start: u64,
    last_window_end: u64,
    /// The bytes after the packet so far, read to find the pattern that ends it.
    look: [u8; 6],
    look_len: usize,
    offset: u64,
    /// The end of the source, once it ended inside the packet being read.
    source_end: Option<SourceLocation>,
    packets: usize,
}
impl std::fmt::Debug for Dts14CarrierReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dts14CarrierReader")
            .field("order", &self.order)
            .field("limits", &self.limits)
            .field("position", &self.position())
            .field("generation", &self.generation())
            .field("packets", &self.packets)
            .field("failure", &self.input.failure())
            .finish_non_exhaustive()
    }
}
impl Dts14CarrierReader {
    /// A reader at offset zero of `source`, whose 16-bit words are in `order`. Fails
    /// with [`Error::Invalid`] when a limit is zero, and [`Error::Limit`] when the
    /// first 128 bytes of a packet, or `maximum_packet_bytes` if that is less, cannot
    /// fit `pool`.
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
        limit(minimum, pool.limit())?;
        let cursor = InputCursor::new(pool, source, caps, limits.maximum_input_window)?;
        Ok(Self {
            pool: pool.clone(),
            input: ReaderInput::Open(cursor),
            order,
            limits,
            packet: Packet::Reserve,
            last_window_start: 0,
            last_window_end: 0,
            look: [0; 6],
            look_len: 0,
            offset: 0,
            source_end: None,
            packets: 0,
        })
    }
    /// The request for the bytes at [`position`](Self::position). A failed reader
    /// returns the request it had open when it failed.
    pub fn request(&self) -> InputRequest {
        self.input.request()
    }
    /// Source offset of the next byte the reader takes from its input. After a
    /// packet that ended at a following sync pattern it is six bytes past the
    /// packet's end: the reader has read that pattern.
    pub fn position(&self) -> u64 {
        self.input.position()
    }
    /// Generation of the reader's input cursor, which every packet carries in its
    /// identity. [`reset`](Self::reset) starts a new one.
    pub fn generation(&self) -> u64 {
        self.input.generation()
    }
    /// Answers the current request with `window`, as
    /// [`InputCursor::provide`](triergon_core::InputCursor::provide) does: a refused
    /// window changes nothing. A failed reader returns its error.
    pub fn provide(&mut self, window: &InputWindow) -> Result<()> {
        self.input.cursor()?.provide(window)?;
        self.last_window_start = window.offset;
        self.last_window_end = window
            .offset
            .checked_add(u64::try_from(window.bytes().len()).map_err(|_| Error::Overflow)?)
            .ok_or(Error::Overflow)?;
        Ok(())
    }
    /// Answers the current request with the end of the source, as
    /// [`InputCursor::finish`](triergon_core::InputCursor::finish) does: a source that
    /// ends before its known length fails the reader with [`Error::Truncated`]. The end
    /// of the source also ends the packet being read.
    pub fn finish(&mut self, source: SourceId, token: InputToken) -> Result<()> {
        let result = self.input.cursor()?.finish(source, token);
        if result == Err(Error::Truncated) {
            self.stop(Error::Truncated);
        }
        result
    }
    /// Answers the current request with a failure, as
    /// [`InputCursor::fail`](triergon_core::InputCursor::fail) does: every later call
    /// returns [`Error::Transport`] or [`Error::Cancelled`], as `failure` says. The
    /// bytes read so far never become a packet.
    pub fn fail(
        &mut self,
        source: SourceId,
        token: InputToken,
        failure: SourceFailure,
    ) -> Result<()> {
        self.input.cursor()?.fail(source, token, failure)?;
        self.stop(source_error(failure));
        Ok(())
    }
    /// Restarts at source offset `offset`, where a packet then starts, and returns
    /// the request for the bytes there. The packet being read is dropped, the packet
    /// count starts again and the cursor takes a new generation, so answers to
    /// earlier requests are refused. Packets already returned stay as they are.
    ///
    /// Fails with [`Error::Unsupported`] when `offset` is odd, or the cursor's refusal
    /// of an offset it cannot serve (see [`InputCursor::reset`]). A failed reader
    /// returns its earlier error and cannot be reset.
    pub fn reset(&mut self, offset: u64) -> Result<InputRequest> {
        let cursor = self.input.cursor()?;
        if !offset.is_multiple_of(2) {
            return Err(Error::Unsupported("DTS carrier reset outside word grid"));
        }
        let request = cursor.reset(offset)?;
        self.packet = Packet::Reserve;
        self.look_len = 0;
        self.offset = offset;
        self.last_window_start = offset;
        self.last_window_end = offset;
        self.source_end = None;
        self.packets = 0;
        Ok(request)
    }
    /// Ends the reader with `error`, dropping the packet it was reading.
    fn stop(&mut self, error: Error) {
        self.input.fail_with(error);
        self.packet = Packet::Reserve;
    }
    /// Reads on, as far as `work` allows. [`Step::Ready`] carries the next packet,
    /// [`Step::NeedInput`] the request to answer before the next call, and
    /// [`Step::End`] the end of the source once the last packet is returned;
    /// [`Step::Yield`] means the work is spent.
    ///
    /// [`Error::Backpressure`] and [`Error::Allocation`] leave the reader as it was:
    /// free pool storage and call again. Every other error is kept and returned by all
    /// later calls: [`Error::Cancelled`] when `cancel` is set, [`Error::Unsupported`]
    /// for a sync pattern that follows opaque bytes, that follows a packet whose header
    /// gives no length of whole words, or that first comes after the frame's end, and
    /// [`Error::Limit`] when a packet, the number of packets or the pool's size is
    /// exceeded.
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<Dts14CarrierPacket>> {
        if let Some(error) = self.input.failure() {
            return Err(error);
        }
        let result = self.advance(work, cancel);
        if let Err(error) = result
            && !retryable(error)
        {
            self.stop(error);
        }
        result
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
            if let Packet::Ended(end) = self.packet {
                return Ok(Step::End(end));
            }
            if work.remaining() == 0 {
                return Ok(Step::Yield);
            }
            match &mut self.packet {
                Packet::Reserve => {
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    let mut buffer = BudgetVec::new(&self.pool);
                    buffer.try_reserve_exact(self.limits.maximum_packet_bytes.min(128))?;
                    self.packet = Packet::Body {
                        buffer,
                        extent: Extent::Unprobed,
                        step: self.after_lookahead(),
                    };
                }
                Packet::Body {
                    buffer,
                    extent,
                    step,
                } => match step {
                    BodyStep::Scan => {
                        match self.input.cursor()?.read_into(
                            &mut self.look[self.look_len..],
                            work,
                            cancel,
                        )? {
                            Step::Ready(n) => {
                                if n == 0 {
                                    return Err(Error::Invalid("empty carrier read"));
                                }
                                self.look_len += n;
                                if self.look_len == 6 {
                                    *step = BodyStep::Check;
                                }
                            }
                            Step::NeedInput(r) => return Ok(Step::NeedInput(r)),
                            Step::Yield => return Ok(Step::Yield),
                            Step::End(e) => {
                                self.source_end = Some(e);
                                *step = if self.look_len == 0 {
                                    BodyStep::Freeze
                                } else {
                                    BodyStep::Append(Append {
                                        at: 0,
                                        count: self.look_len,
                                    })
                                };
                            }
                        }
                    }
                    BodyStep::Check => {
                        if !work.try_charge(1) {
                            return Ok(Step::Yield);
                        }
                        let packet = buffer.as_slice();
                        // The extent comes from the same 120-bit header probe as the
                        // frame reader's, and only for a frame of whole words; neither
                        // a 96-byte cutoff nor the pattern alone proves a boundary,
                        // and the coded audio is never checked.
                        if matches!(extent, Extent::Unprobed) && packet.len() >= 18 {
                            *extent = probe_extent(packet, self.order)?;
                        }
                        if packet.len() >= 6 && conventional_sync(&self.look, self.order) {
                            if !conventional_sync(packet, self.order) {
                                return Err(Error::Unsupported(
                                    "mixed opaque/conventional DTS carrier",
                                ));
                            }
                            match *extent {
                                Extent::Known(end) if packet.len() == end => {
                                    *step = BodyStep::Freeze;
                                    continue;
                                }
                                Extent::Known(end) if packet.len() < end => {}
                                Extent::Known(_) => {
                                    return Err(Error::Unsupported(
                                        "late DTS carrier synchronization",
                                    ));
                                }
                                Extent::Unknown => {
                                    return Err(Error::Unsupported(
                                        "unresolved DTS carrier extent",
                                    ));
                                }
                                Extent::Unprobed => {}
                            }
                        }
                        *step = BodyStep::Append(Append { at: 0, count: 2 });
                    }
                    BodyStep::Append(append) => {
                        if !work.try_charge(1) {
                            return Ok(Step::Yield);
                        }
                        let Append { at, count } = *append;
                        if at < count {
                            if buffer.len() >= self.limits.maximum_packet_bytes {
                                return Err(Error::Limit {
                                    requested: buffer
                                        .len()
                                        .checked_add(1)
                                        .ok_or(Error::Overflow)?,
                                    maximum: self.limits.maximum_packet_bytes,
                                });
                            }
                            if buffer.len() == buffer.capacity() {
                                *step = BodyStep::Grow {
                                    append: *append,
                                    next: BudgetVec::new(&self.pool),
                                    step: GrowStep::Reserve,
                                };
                                continue;
                            }
                            buffer.try_push(self.look[at]).map_err(|e| e.error)?;
                            append.at += 1;
                        } else {
                            self.look.copy_within(count..self.look_len, 0);
                            self.look_len -= count;
                            *step = if self.source_end.is_some() {
                                BodyStep::Freeze
                            } else {
                                BodyStep::Scan
                            };
                        }
                    }
                    BodyStep::Grow {
                        append,
                        next,
                        step: grow,
                    } => {
                        if !work.try_charge(1) {
                            return Ok(Step::Yield);
                        }
                        match *grow {
                            GrowStep::Reserve => {
                                let target = buffer
                                    .capacity()
                                    .checked_mul(2)
                                    .ok_or(Error::Overflow)?
                                    .min(self.limits.maximum_packet_bytes);
                                let minimum = buffer
                                    .capacity()
                                    .checked_add(target)
                                    .and_then(|n| n.checked_add(Budget::control_storage_bytes()))
                                    .ok_or(Error::Overflow)?;
                                limit(minimum, self.pool.limit())?;
                                // Only empty storage grows: no copied bytes move inside
                                // an allocation or any one uncharged operation.
                                next.try_reserve_exact(target)?;
                                *grow = GrowStep::Copy { copied: 0 };
                            }
                            GrowStep::Copy { copied } => {
                                if copied == buffer.len() {
                                    *grow = GrowStep::Swap;
                                } else {
                                    if next.len() == next.capacity() {
                                        return Err(Error::Invalid(
                                            "carrier growth reservation exhausted",
                                        ));
                                    }
                                    next.try_push(buffer.as_slice()[copied])
                                        .map_err(|e| e.error)?;
                                    *grow = GrowStep::Copy { copied: copied + 1 };
                                }
                            }
                            GrowStep::Swap => {
                                let append = *append;
                                *buffer = std::mem::replace(next, BudgetVec::new(&self.pool));
                                *step = BodyStep::Append(append);
                            }
                        }
                    }
                    BodyStep::Freeze => {
                        if !work.try_charge(1) {
                            return Ok(Step::Yield);
                        }
                        if buffer.is_empty()
                            && let Some(end) = self.source_end
                        {
                            self.packet = Packet::Ended(end);
                            continue;
                        }
                        if self.packets >= self.limits.maximum_packets {
                            return Err(Error::Limit {
                                requested: self.packets.checked_add(1).ok_or(Error::Overflow)?,
                                maximum: self.limits.maximum_packets,
                            });
                        }
                        let raw =
                            match std::mem::replace(buffer, BudgetVec::new(&self.pool)).freeze() {
                                Ok(raw) => raw,
                                Err(refused) => {
                                    *buffer = refused.value;
                                    return Err(refused.error);
                                }
                            };
                        self.packet =
                            if raw.len() >= 6 && conventional_sync(raw.as_ref(), self.order) {
                                if raw.len() % 2 != 0 {
                                    Packet::Ready {
                                        raw,
                                        payload: None,
                                        syntax: Dts14CarrierSyntax::Refused(Error::Truncated),
                                    }
                                } else {
                                    Packet::Frozen { raw }
                                }
                            } else {
                                Packet::Ready {
                                    raw,
                                    payload: None,
                                    syntax: Dts14CarrierSyntax::OpaqueCarrier,
                                }
                            };
                    }
                },
                Packet::Frozen { raw } => {
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    let raw = raw.clone();
                    let unpacker = Dts14BitUnpacker::new(
                        &self.pool,
                        self.identity(),
                        raw.clone(),
                        self.order,
                        DtsPackingLimits {
                            maximum_source_bytes: self.limits.maximum_packet_bytes,
                            maximum_output_bytes: self.limits.maximum_packet_bytes,
                        },
                    )?;
                    self.packet = Packet::Unpack { raw, unpacker };
                }
                Packet::Unpack { raw, unpacker } => match unpacker.step(work, cancel)? {
                    Step::Ready(p) => {
                        // No tail is discarded and no reading of FSIZE is chosen.
                        let checked =
                            probe_core_header(p.canonical_bytes().as_ref()).and_then(|h| {
                                if p.residual_bits() != 0
                                    || h.frame_bytes != p.canonical_bytes().len()
                                {
                                    Err(Error::Unsupported("DTS carrier unresolved extent/tail"))
                                } else {
                                    Ok(h)
                                }
                            });
                        let syntax = match checked {
                            Ok(h) => Dts14CarrierSyntax::ExactNormal(h),
                            Err(e) => Dts14CarrierSyntax::Refused(e),
                        };
                        let raw = raw.clone();
                        self.packet = Packet::Ready {
                            raw,
                            payload: Some(p),
                            syntax,
                        };
                    }
                    Step::Yield => return Ok(Step::Yield),
                    _ => return Err(Error::Invalid("carrier owned unpack progress")),
                },
                Packet::Ready { .. } => return self.publish(work),
                Packet::Ended(end) => return Ok(Step::End(*end)),
            }
        }
    }
    /// The first step of reading a packet: scanning on, or first moving in the
    /// lookahead bytes of the pattern that ended the packet before.
    fn after_lookahead(&self) -> BodyStep {
        if self.look_len == 0 {
            BodyStep::Scan
        } else {
            BodyStep::Append(Append {
                at: 0,
                count: self.look_len,
            })
        }
    }
    /// Returns the ready packet and moves on to the next one.
    fn publish(&mut self, work: &mut WorkBudget) -> Result<Step<Dts14CarrierPacket>> {
        if !work.try_charge(1) {
            return Ok(Step::Yield);
        }
        let Packet::Ready {
            raw,
            payload,
            syntax,
        } = std::mem::replace(&mut self.packet, Packet::Reserve)
        else {
            return Err(Error::Invalid("carrier publication missing"));
        };
        let next = self
            .offset
            .checked_add(u64::try_from(raw.len()).map_err(|_| Error::Overflow)?)
            .ok_or(Error::Overflow)?;
        let boundary = match self.source_end {
            Some(e) => {
                if e.offset != next {
                    return Err(Error::Invalid("carrier EOF range"));
                }
                self.packet = Packet::Ended(e);
                Dts14CarrierBoundary::ExplicitSourceEnd
            }
            None => Dts14CarrierBoundary::FollowingCarrierSync { offset: next },
        };
        let packet = Dts14CarrierPacket {
            identity: self.identity(),
            raw,
            payload,
            boundary,
            syntax,
            provision: Dts14CarrierProvision {
                last_window_start: self.last_window_start,
                last_window_end: self.last_window_end,
                consumed_through: self.position(),
            },
        };
        self.offset = next;
        self.packets = self.packets.checked_add(1).ok_or(Error::Overflow)?;
        Ok(Step::Ready(packet))
    }
    fn identity(&self) -> DtsSourceIdentity {
        DtsSourceIdentity {
            source: self.input.source(),
            generation: self.generation(),
            offset: self.offset,
        }
    }
}

/// Where the frame of a packet ends, from the header in its first 18 bytes when
/// the packet starts with the sync pattern.
fn probe_extent(packet: &[u8], order: DtsWordOrder) -> Result<Extent> {
    if !conventional_sync(packet, order) {
        return Ok(Extent::Unknown);
    }
    let prefix: [u8; 18] = packet[..18].try_into().map_err(|_| Error::Truncated)?;
    let bits = header_payload(&prefix, order)?;
    Ok(match probe_core_header(&bits) {
        Ok(header) if header.frame_bytes % 7 == 0 => Extent::Known(
            (header.frame_bytes / 7)
                .checked_mul(8)
                .ok_or(Error::Overflow)?,
        ),
        _ => Extent::Unknown,
    })
}
/// Conventional physical representation of the primary extended normal sync.
/// Only the carrier signature is tested here; this is not syntax validation.
fn conventional_sync(b: &[u8], order: DtsWordOrder) -> bool {
    if b.len() < 6 {
        return false;
    }
    word(b, 0, order) == 0x1fff
        && word(b, 2, order) == 0xe800
        && word(b, 4, order) & 0xfff0 == 0x07f0
}

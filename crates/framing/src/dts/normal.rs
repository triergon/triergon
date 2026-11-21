//! Normal core frames of a DTS stream in any word packing, ETSI TS 102 114 §5.3 and
//! §5.4.2. A frame is the span from one normal sync to the next; how its length
//! relates to `FSIZE` is reported, and neither shows that the coded audio is valid.
use crate::dts::core_frame::CORE_SYNC;
use crate::dts::reader::{limit, source_error};
use crate::dts::{
    DtsCoreField, DtsPackedHeader, DtsPackedLimits, DtsPackedPayload, DtsPackedPayloadJob,
    DtsPacking, DtsSourceIdentity, inspect_header_bits,
};
use crate::{ReaderInput, freeze_whole, retryable};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, BudgetVec, Bytes, Error, InputCursor, InputRequest, InputToken, InputWindow, Result,
    SourceCapabilities, SourceFailure, SourceId, SourceLocation, Step, WorkBudget,
};

/// Limits of a [`DtsNormalSourceReader`]. None may be zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DtsNormalLimits {
    /// The most source bytes one frame may span, from its sync to the next. The
    /// reader reserves this many for every frame, and a returned frame keeps them
    /// charged to the pool while it is held.
    pub maximum_source_frame_bytes: usize,
    /// The most bytes one frame's payload may take once its bits are packed into
    /// bytes ([`DtsNormalSourceFrame::storage_bytes`]).
    pub maximum_storage_bytes: usize,
    /// The most frames read since construction or the last reset.
    pub maximum_frames: usize,
    /// The most bytes one input window may hold.
    pub maximum_input_window: usize,
}

/// What ended a frame of a [`DtsNormalSourceReader`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DtsNormalBoundary {
    /// The next normal sync (`SYNC`, `FTYPE` 1, `SHORT` 31) on the same source unit
    /// grid starts where this frame ends. Coded audio can hold the same 38 bits, so
    /// the sync alone does not prove that a frame starts there.
    FollowingNormalSync {
        /// Source offset of the following sync, in bytes.
        offset: u64,
    },
    /// The source ended, as [`DtsNormalSourceReader::finish`] declared. A caller may
    /// declare the end of a part of a stream as well as of a whole one.
    ExplicitSourceEnd,
}

/// How the measured length of a frame relates to its `FSIZE`. `FSIZE` + 1 counts the
/// bytes of a frame; in a packed stream that can be read as a count of source bytes
/// or of payload bytes. Each reading is tested on its own and neither is preferred.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DtsExtentRelation {
    /// Both readings hold.
    Both,
    /// The frame's payload holds `FSIZE` + 1 bytes and less than one further source
    /// unit of payload bits.
    PayloadWordCount,
    /// The frame spans exactly `FSIZE` + 1 source bytes.
    PhysicalByteCount,
    /// Neither reading holds.
    Neither,
}

/// The payload bits a frame carries after its `FSIZE` + 1 payload bytes. There are
/// fewer of them than one source unit carries. Whether they are valid, and what they
/// mean, is not established.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DtsCarrierTail {
    /// Index of the first tail bit in the frame's payload: `8 * (FSIZE + 1)`.
    pub start_bit: usize,
    /// Number of tail bits, less than [`DtsPacking::word_payload_bits`]; 0 when the
    /// payload ends with its byte `FSIZE`.
    pub bit_count: u8,
    /// The tail bits in stream order, the last of them in bit 0.
    pub value: u16,
}

/// What a [`DtsNormalSourceReader`] measured for one frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DtsNormalFacts {
    /// Source, cursor generation and byte offset of the frame's first byte.
    pub identity: DtsSourceIdentity,
    /// The packing the frame is stored in.
    pub packing: DtsPacking,
    /// Length of the frame in source bytes.
    pub source_bytes: usize,
    /// Number of payload bits the frame carries: 8, 16 or 14 for each source unit.
    pub source_payload_bits: usize,
    /// Length in bytes of the payload packed into bytes: `source_payload_bits` / 8,
    /// rounded up.
    pub storage_bytes: usize,
    /// What ended the frame.
    pub boundary: DtsNormalBoundary,
    /// Number of normal syncs inside the frame that did not end it, because fewer
    /// than `FSIZE` + 1 source bytes preceded them.
    pub skipped_short_syncs: usize,
    /// How the frame's length relates to `FSIZE`.
    pub extent_relation: DtsExtentRelation,
    /// The payload bits after `FSIZE` + 1 bytes. `None` unless the payload reading of
    /// `FSIZE` holds ([`DtsExtentRelation::Both`] or
    /// [`DtsExtentRelation::PayloadWordCount`]).
    pub carrier_tail: Option<DtsCarrierTail>,
    /// The header fields, read from at most the first 120 payload bits of the
    /// frame's first 18 source bytes.
    pub header: DtsPackedHeader,
}

/// One frame of a [`DtsNormalSourceReader`]: its source bytes, its payload and what
/// the reader measured. It owns its bytes and outlives the reader.
#[derive(Clone, Debug)]
pub struct DtsNormalSourceFrame {
    payload: DtsPackedPayload,
    facts: DtsNormalFacts,
}
impl DtsNormalSourceFrame {
    /// The frame's payload, which also maps each payload bit to its source byte.
    pub fn payload(&self) -> &DtsPackedPayload {
        &self.payload
    }
    /// The frame as the source stores it.
    pub fn source_bytes(&self) -> &Bytes {
        self.payload.source_bytes()
    }
    /// The frame's payload bits packed into bytes, the first bit in the high bit of
    /// byte 0 and the last byte padded with zero bits. For
    /// [`DtsPacking::CanonicalBe`] these are the source bytes.
    pub fn storage_bytes(&self) -> &Bytes {
        self.payload.storage_bytes()
    }
    /// What the reader measured for the frame.
    pub const fn facts(&self) -> DtsNormalFacts {
        self.facts
    }
}

/// The frame a [`DtsNormalSourceReader`] is reading.
enum Frame {
    /// Reading the frame's first 18 source bytes into the prefix, fewer when the
    /// source ends first.
    Header,
    /// The prefix is read; the header in it is checked next.
    Probe,
    /// Reading the frame's source bytes into `buffer`.
    Body {
        header: DtsPackedHeader,
        buffer: BudgetVec<u8>,
        step: BodyStep,
    },
    /// The frame's source bytes are complete.
    Frozen { header: DtsPackedHeader, raw: Bytes },
    /// Unpacking the frame's payload.
    Unpack {
        header: DtsPackedHeader,
        job: DtsPackedPayloadJob,
    },
    /// The frame, to be returned.
    Ready(DtsNormalSourceFrame),
    /// The source ended after the last frame.
    Ended(SourceLocation),
}

/// The steps of reading a frame's source bytes.
#[derive(Clone, Copy)]
enum BodyStep {
    /// Reserving storage for the longest frame.
    Reserve,
    /// Copying the prefix into the frame, `copied` bytes of it so far.
    CopyHeader { copied: usize },
    /// Reading the six bytes after the frame so far into the lookahead.
    Scan,
    /// Deciding whether the lookahead starts the next frame.
    CheckSync,
    /// Moving the first `count` lookahead bytes into the frame, `copied` of them so
    /// far.
    Append { count: usize, copied: usize },
    /// The frame's source bytes are complete.
    Freeze,
}

/// Reads the normal core frames of a DTS stream stored in any of the four
/// [`DtsPacking`]s, each frame with its own measured length.
///
/// The stream must start with a normal sync: `SYNC`, `FTYPE` 1 and `SHORT` 31. A
/// frame ends at the next normal sync on the source unit grid, or at the end of the
/// source. Source units are bytes for [`DtsPacking::CanonicalBe`] and whole two-byte
/// words for the other packings. A normal sync that follows fewer than `FSIZE` + 1
/// source bytes, the smaller of the two extents `FSIZE` can describe, is kept as
/// frame content and counted in [`DtsNormalFacts::skipped_short_syncs`]. A later one
/// ends the frame even when neither extent matches the frame's length;
/// [`DtsNormalFacts::extent_relation`] reports that relation as it is.
///
/// The packing states how the source stores the stream; it does not select a reading
/// of `FSIZE`. Termination frames and extension substreams are not supported, no sync
/// is searched for after input that is not a frame, and the coded audio is not
/// checked: a returned frame is a measured span of the source, not a judgement on
/// its content.
///
/// Call [`step`](Self::step) and answer each [`Step::NeedInput`] with
/// [`provide`](Self::provide), [`finish`](Self::finish) or [`fail`](Self::fail).
pub struct DtsNormalSourceReader {
    pool: Budget,
    input: ReaderInput,
    packing: DtsPacking,
    limits: DtsNormalLimits,
    frame: Frame,
    /// The frame's first source bytes. After a frame that ended at a sync, they
    /// begin with that sync's six bytes.
    prefix: [u8; 18],
    prefix_len: usize,
    /// The bytes after the frame so far, read to find the sync that ends it.
    lookahead: [u8; 6],
    lookahead_len: usize,
    frame_offset: u64,
    /// The end of the source, once it ended inside the frame being read.
    source_end: Option<SourceLocation>,
    skipped_short_syncs: usize,
    completed: usize,
}

impl std::fmt::Debug for DtsNormalSourceReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DtsNormalSourceReader")
            .field("packing", &self.packing)
            .field("limits", &self.limits)
            .field("position", &self.position())
            .field("generation", &self.generation())
            .field("completed", &self.completed)
            .field("failure", &self.input.failure())
            .finish_non_exhaustive()
    }
}

impl DtsNormalSourceReader {
    /// A reader at offset zero of `source`, whose stream is stored in `packing`. Fails
    /// with [`Error::Invalid`] when a limit is zero, and [`Error::Limit`] when a frame
    /// of `maximum_source_frame_bytes` cannot fit `pool`.
    pub fn new(
        pool: &Budget,
        source: SourceId,
        capabilities: SourceCapabilities,
        packing: DtsPacking,
        limits: DtsNormalLimits,
    ) -> Result<Self> {
        if limits.maximum_source_frame_bytes == 0
            || limits.maximum_storage_bytes == 0
            || limits.maximum_frames == 0
            || limits.maximum_input_window == 0
        {
            return Err(Error::Invalid("zero DTS normal source policy"));
        }
        // The frame's storage is reserved whole while it is empty, so no copied
        // bytes ever move.
        let minimum = limits
            .maximum_source_frame_bytes
            .checked_add(Bytes::owner_overhead_bytes())
            .and_then(|v| v.checked_add(Budget::control_storage_bytes()))
            .ok_or(Error::Overflow)?;
        limit(minimum, pool.limit())?;
        let cursor = InputCursor::new(pool, source, capabilities, limits.maximum_input_window)?;
        Ok(Self {
            pool: pool.clone(),
            input: ReaderInput::Open(cursor),
            packing,
            limits,
            frame: Frame::Header,
            prefix: [0; 18],
            prefix_len: 0,
            lookahead: [0; 6],
            lookahead_len: 0,
            frame_offset: 0,
            source_end: None,
            skipped_short_syncs: 0,
            completed: 0,
        })
    }
    /// The request for the bytes at [`position`](Self::position). A failed reader
    /// returns the request it had open when it failed.
    pub fn request(&self) -> InputRequest {
        self.input.request()
    }
    /// Source offset of the next byte the reader takes from its input. After a frame
    /// that ended at a following sync it is six bytes past the frame's end: the
    /// reader has read that sync.
    pub fn position(&self) -> u64 {
        self.input.position()
    }
    /// Generation of the reader's input cursor, which every frame carries in its
    /// identity. [`reset`](Self::reset) starts a new one.
    pub fn generation(&self) -> u64 {
        self.input.generation()
    }
    /// Answers the current request with `window`, as
    /// [`InputCursor::provide`](triergon_core::InputCursor::provide) does: a refused
    /// window changes nothing. A failed reader returns its error.
    pub fn provide(&mut self, window: &InputWindow) -> Result<()> {
        self.input.cursor()?.provide(window)
    }
    /// Answers the current request with the end of the source, as
    /// [`InputCursor::finish`](triergon_core::InputCursor::finish) does: a source that
    /// ends before its known length fails the reader with [`Error::Truncated`]. The end
    /// of the source also ends the frame being read.
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
    /// bytes read so far never become a frame.
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
    /// Restarts at source offset `offset`, where a frame must start, and returns the
    /// request for the bytes there. The frame being read is dropped, the frame count
    /// starts again and the cursor takes a new generation, so answers to earlier
    /// requests are refused. Frames already returned stay as they are.
    ///
    /// Fails with [`Error::Unsupported`] when `offset` is not a multiple of the
    /// packing's source unit, or the cursor's refusal of an offset it cannot serve (see
    /// [`InputCursor::reset`]). A failed reader returns its earlier error and cannot be
    /// reset.
    pub fn reset(&mut self, offset: u64) -> Result<InputRequest> {
        let cursor = self.input.cursor()?;
        if !offset.is_multiple_of(self.packing.source_unit_bytes() as u64) {
            return Err(Error::Unsupported("DTS reset outside source unit grid"));
        }
        let request = cursor.reset(offset)?;
        self.frame = Frame::Header;
        self.prefix_len = 0;
        self.lookahead_len = 0;
        self.source_end = None;
        self.frame_offset = offset;
        self.skipped_short_syncs = 0;
        self.completed = 0;
        Ok(request)
    }
    /// Ends the reader with `error`, dropping the frame it was reading.
    fn stop(&mut self, error: Error) {
        self.input.fail_with(error);
        self.frame = Frame::Header;
    }
    /// Reads on, as far as `work` allows. [`Step::Ready`] carries the next frame,
    /// [`Step::NeedInput`] the request to answer before the next call, and
    /// [`Step::End`] the end of the source once the last frame is returned;
    /// [`Step::Yield`] means the work is spent.
    ///
    /// [`Error::Backpressure`] and [`Error::Allocation`] leave the reader as it was:
    /// free pool storage and call again. Every other error is kept and returned by all
    /// later calls: [`Error::Cancelled`] when `cancel` is set, [`Error::Invalid`] when
    /// the stream does not start with `SYNC`, [`Error::Unsupported`] when it starts
    /// with a termination frame (`FTYPE` 0) or a `SHORT` other than 31,
    /// [`Error::Truncated`] when the source ends inside a header or a source word, and
    /// [`Error::Limit`] when a frame or the number of frames exceeds its limit.
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<DtsNormalSourceFrame>> {
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
    ) -> Result<Step<DtsNormalSourceFrame>> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if let Frame::Ended(end) = self.frame {
                return Ok(Step::End(end));
            }
            if work.remaining() == 0 {
                return Ok(Step::Yield);
            }
            match &mut self.frame {
                Frame::Header => {
                    if let Some(step) = self.read_header(work, cancel)? {
                        return Ok(step);
                    }
                }
                Frame::Probe => {
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    let header = self.probe()?;
                    self.frame = Frame::Body {
                        header,
                        buffer: BudgetVec::new(&self.pool),
                        step: BodyStep::Reserve,
                    };
                }
                Frame::Body {
                    header,
                    buffer,
                    step,
                } => match *step {
                    BodyStep::Reserve => {
                        if !work.try_charge(1) {
                            return Ok(Step::Yield);
                        }
                        buffer.try_reserve_exact(self.limits.maximum_source_frame_bytes)?;
                        *step = BodyStep::CopyHeader { copied: 0 };
                    }
                    BodyStep::CopyHeader { copied } => {
                        let count = usize::try_from(work.take(
                            u64::try_from(self.prefix_len - copied).map_err(|_| Error::Overflow)?,
                        ))
                        .map_err(|_| Error::Overflow)?;
                        if count == 0 {
                            return Ok(Step::Yield);
                        }
                        let end = copied + count;
                        buffer.try_extend_from_slice(&self.prefix[copied..end])?;
                        *step = if end < self.prefix_len {
                            BodyStep::CopyHeader { copied: end }
                        } else if self.source_end.is_some() {
                            BodyStep::Freeze
                        } else {
                            BodyStep::Scan
                        };
                    }
                    BodyStep::Scan => {
                        match self.input.cursor()?.read_into(
                            &mut self.lookahead[self.lookahead_len..],
                            work,
                            cancel,
                        )? {
                            Step::Ready(count) => {
                                if count == 0 {
                                    return Err(Error::Invalid("empty DTS scan read"));
                                }
                                self.lookahead_len += count;
                                if self.lookahead_len == self.lookahead.len() {
                                    *step = BodyStep::CheckSync;
                                }
                            }
                            Step::NeedInput(request) => return Ok(Step::NeedInput(request)),
                            Step::Yield => return Ok(Step::Yield),
                            Step::End(end) => {
                                self.packing.payload_bits(self.lookahead_len)?;
                                self.source_end = Some(end);
                                *step = if self.lookahead_len == 0 {
                                    BodyStep::Freeze
                                } else {
                                    BodyStep::Append {
                                        count: self.lookahead_len,
                                        copied: 0,
                                    }
                                };
                            }
                        }
                    }
                    BodyStep::CheckSync => {
                        if !work.try_charge(1) {
                            return Ok(Step::Yield);
                        }
                        let normal = self.packing.normal_sync(&self.lookahead)?;
                        let shortest = shortest_extent(header, self.packing)?;
                        if normal && buffer.len() >= shortest {
                            *step = BodyStep::Freeze;
                        } else {
                            if normal {
                                self.skipped_short_syncs = self
                                    .skipped_short_syncs
                                    .checked_add(1)
                                    .ok_or(Error::Overflow)?;
                            }
                            *step = BodyStep::Append {
                                count: self.packing.source_unit_bytes(),
                                copied: 0,
                            };
                        }
                    }
                    BodyStep::Append { count, copied } => {
                        if !work.try_charge(1) {
                            return Ok(Step::Yield);
                        }
                        if copied < count {
                            let next = buffer.len().checked_add(1).ok_or(Error::Overflow)?;
                            limit(next, self.limits.maximum_source_frame_bytes)?;
                            if next > buffer.capacity() {
                                return Err(Error::Invalid("DTS source capacity invariant"));
                            }
                            buffer
                                .try_push(self.lookahead[copied])
                                .map_err(|failure| failure.error)?;
                            *step = BodyStep::Append {
                                count,
                                copied: copied + 1,
                            };
                        } else {
                            self.lookahead.copy_within(count..self.lookahead_len, 0);
                            self.lookahead_len -= count;
                            *step = if self.source_end.is_some() {
                                BodyStep::Freeze
                            } else {
                                BodyStep::Scan
                            };
                        }
                    }
                    BodyStep::Freeze => {
                        if !work.try_charge(1) {
                            return Ok(Step::Yield);
                        }
                        let header = *header;
                        let raw = freeze_whole(buffer)?;
                        self.frame = Frame::Frozen { header, raw };
                    }
                },
                Frame::Frozen { header, raw } => {
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    let identity = DtsSourceIdentity {
                        source: self.input.source(),
                        generation: self.input.generation(),
                        offset: self.frame_offset,
                    };
                    let job = DtsPackedPayloadJob::new(
                        &self.pool,
                        identity,
                        raw.clone(),
                        self.packing,
                        DtsPackedLimits {
                            maximum_source_bytes: self.limits.maximum_source_frame_bytes,
                            maximum_storage_bytes: self.limits.maximum_storage_bytes,
                        },
                    )?;
                    let header = *header;
                    self.frame = Frame::Unpack { header, job };
                }
                Frame::Unpack { header, job } => {
                    let header = *header;
                    match job.step(work, cancel)? {
                        Step::Ready(payload) => {
                            let facts = self.measure(header, &payload)?;
                            self.frame = Frame::Ready(DtsNormalSourceFrame { payload, facts });
                        }
                        Step::Yield => return Ok(Step::Yield),
                        Step::End(_) | Step::NeedInput(_) => {
                            return Err(Error::Invalid("DTS owned payload progress"));
                        }
                    }
                }
                Frame::Ready(_) => return self.publish(work),
                Frame::Ended(end) => return Ok(Step::End(*end)),
            }
        }
    }
    /// Returns the ready frame and moves on to the next one.
    fn publish(&mut self, work: &mut WorkBudget) -> Result<Step<DtsNormalSourceFrame>> {
        if !work.try_charge(1) {
            return Ok(Step::Yield);
        }
        let Frame::Ready(ready) = std::mem::replace(&mut self.frame, Frame::Header) else {
            return Err(Error::Invalid("DTS ready source record"));
        };
        self.completed = self.completed.checked_add(1).ok_or(Error::Overflow)?;
        if let Some(end) = self.source_end {
            self.frame = Frame::Ended(end);
        } else {
            self.prefix[..6].copy_from_slice(&self.lookahead);
            self.prefix_len = 6;
            self.frame_offset = ready
                .facts
                .identity
                .offset
                .checked_add(u64::try_from(ready.facts.source_bytes).map_err(|_| Error::Overflow)?)
                .ok_or(Error::Overflow)?;
            self.lookahead_len = 0;
        }
        self.skipped_short_syncs = 0;
        Ok(Step::Ready(ready))
    }
    /// Reads the prefix on. `None` once it is complete, or the source ended inside
    /// it; otherwise the step to return.
    fn read_header(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<Step<DtsNormalSourceFrame>>> {
        if self.prefix_len == 0 {
            self.frame_offset = self.position();
        }
        match self
            .input
            .cursor()?
            .read_into(&mut self.prefix[self.prefix_len..], work, cancel)?
        {
            Step::Ready(count) => {
                if count == 0 {
                    return Err(Error::Invalid("empty DTS header read"));
                }
                self.prefix_len += count;
                if self.completed >= self.limits.maximum_frames {
                    return Err(Error::Limit {
                        requested: self.completed.checked_add(1).ok_or(Error::Overflow)?,
                        maximum: self.limits.maximum_frames,
                    });
                }
                if self.prefix_len == self.prefix.len() {
                    self.frame = Frame::Probe;
                }
                Ok(None)
            }
            Step::NeedInput(request) => Ok(Some(Step::NeedInput(request))),
            Step::Yield => Ok(Some(Step::Yield)),
            Step::End(end) => {
                if self.prefix_len == 0 {
                    self.frame = Frame::Ended(end);
                    return Ok(Some(Step::End(end)));
                }
                self.source_end = Some(end);
                self.frame = Frame::Probe;
                Ok(None)
            }
        }
    }
    /// The header in the prefix, which must be a complete normal frame header.
    fn probe(&self) -> Result<DtsPackedHeader> {
        limit(self.prefix_len, self.limits.maximum_source_frame_bytes)?;
        let bits = self.packing.payload_bits(self.prefix_len)?;
        let mut stack = [0_u8; 15];
        for bit in 0..bits.min(120) {
            stack[bit / 8] |=
                self.packing.bit(&self.prefix[..self.prefix_len], bit)? << (7 - bit % 8);
        }
        let header = inspect_header_bits(&stack, bits.min(120))?;
        if header.value(DtsCoreField::Sync) != Some(CORE_SYNC) {
            return Err(if bits < 32 {
                Error::Truncated
            } else {
                Error::Invalid("DTS normal source sync")
            });
        }
        if header.value(DtsCoreField::Normal) != Some(1)
            || header.value(DtsCoreField::Short) != Some(31)
        {
            return Err(Error::Unsupported(
                "DTS termination or abnormal normal sync",
            ));
        }
        if !header.syntax_complete() {
            return Err(Error::Truncated);
        }
        Ok(header)
    }
    /// What the reader measured for a frame whose payload the job returned.
    fn measure(
        &self,
        header: DtsPackedHeader,
        payload: &DtsPackedPayload,
    ) -> Result<DtsNormalFacts> {
        let declared = declared_frame_bytes(&header, "DTS output FSIZE")?;
        let declared_bits = declared.checked_mul(8).ok_or(Error::Overflow)?;
        let surplus = payload.valid_bits().checked_sub(declared_bits);
        let word_count = surplus.is_some_and(|v| v < self.packing.word_payload_bits());
        let physical = payload.source_bytes().len() == declared;
        let extent_relation = match (word_count, physical) {
            (true, true) => DtsExtentRelation::Both,
            (true, false) => DtsExtentRelation::PayloadWordCount,
            (false, true) => DtsExtentRelation::PhysicalByteCount,
            (false, false) => DtsExtentRelation::Neither,
        };
        let carrier_tail = if word_count {
            let count = surplus.ok_or(Error::Invalid("DTS tail extent"))?;
            let mut value = 0_u16;
            for bit in declared_bits..payload.valid_bits() {
                value = (value << 1) | u16::from(payload.bit(bit)?);
            }
            Some(DtsCarrierTail {
                start_bit: declared_bits,
                bit_count: u8::try_from(count).map_err(|_| Error::Overflow)?,
                value,
            })
        } else {
            None
        };
        let end = self
            .frame_offset
            .checked_add(u64::try_from(payload.source_bytes().len()).map_err(|_| Error::Overflow)?)
            .ok_or(Error::Overflow)?;
        let boundary = if let Some(source_end) = self.source_end {
            if end != source_end.offset {
                return Err(Error::Invalid("DTS EOF source mapping"));
            }
            DtsNormalBoundary::ExplicitSourceEnd
        } else {
            if self.lookahead_len != 6
                || self.position() != end.checked_add(6).ok_or(Error::Overflow)?
            {
                return Err(Error::Invalid("DTS following sync source mapping"));
            }
            DtsNormalBoundary::FollowingNormalSync { offset: end }
        };
        Ok(DtsNormalFacts {
            identity: payload.identity(),
            packing: self.packing,
            source_bytes: payload.source_bytes().len(),
            source_payload_bits: payload.valid_bits(),
            storage_bytes: payload.storage_bytes().len(),
            boundary,
            skipped_short_syncs: self.skipped_short_syncs,
            extent_relation,
            carrier_tail,
            header,
        })
    }
}

/// `FSIZE` + 1, the frame length the header declares; `missing` when it has none.
fn declared_frame_bytes(header: &DtsPackedHeader, missing: &'static str) -> Result<usize> {
    Ok(header
        .views()
        .declared_frame_bytes
        .ok_or(Error::Invalid(missing))? as usize)
}

/// The fewest source bytes a frame must span before a normal sync can end it: the
/// smaller of the two extents `FSIZE` + 1 can describe, a count of source bytes or of
/// payload bytes rounded up to whole source units.
fn shortest_extent(header: &DtsPackedHeader, packing: DtsPacking) -> Result<usize> {
    let declared = declared_frame_bytes(header, "DTS missing FSIZE")?;
    let bits = declared.checked_mul(8).ok_or(Error::Overflow)?;
    let words = bits
        .checked_add(packing.word_payload_bits() - 1)
        .ok_or(Error::Overflow)?
        / packing.word_payload_bits();
    let payload_span = words
        .checked_mul(packing.source_unit_bytes())
        .ok_or(Error::Overflow)?;
    Ok(declared.min(payload_span))
}

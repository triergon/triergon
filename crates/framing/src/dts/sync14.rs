//! Normal core frames of a 14-bit DTS stream whose frames fill whole 14-bit words,
//! found from one sync to the next. Experimental, and not a reader for 14-bit DTS in
//! general. ETSI TS 102 114 V1.2.1 §5.3 and V1.6.1 §5.3–5.4.2.
use crate::dts::core_frame::CORE_SYNC;
use crate::dts::reader::{ReaderInput, limit, retryable, source_error};
use crate::dts::{
    Dts14BitPayload, Dts14BitUnpacker, DtsCoreHeader, DtsPackingLimits, DtsSourceIdentity,
    DtsWordOrder, probe_core_header,
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, BudgetVec, Bytes, Error, InputCursor, InputRequest, InputToken, InputWindow, Result,
    SourceCapabilities, SourceFailure, SourceId, SourceLocation, Step, WorkBudget,
};

/// Limits of a [`Dts14SyncReader`]. None may be zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dts14SyncLimits {
    /// The most source bytes one frame may span. The reader reserves this many for
    /// every frame, and a returned frame keeps them charged to the pool while it is
    /// held.
    pub maximum_physical_frame_bytes: usize,
    /// The most bytes one unpacked frame may hold: its `FSIZE` + 1.
    pub maximum_canonical_frame_bytes: usize,
    /// The most frames read since construction or the last reset.
    pub maximum_frames: usize,
    /// The most bytes one input window may hold.
    pub maximum_input_window: usize,
}

/// What ended a frame of a [`Dts14SyncReader`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dts14BoundaryEvidence {
    /// The next normal sync (`SYNC`, `FTYPE` 1, `SHORT` 31) starts where this frame
    /// ends. The reader has read it; its bytes are not part of this frame.
    FollowingNormalSync {
        /// Source offset of the following sync, in bytes.
        physical_offset: u64,
    },
    /// The source ended, as [`Dts14SyncReader::finish`] declared. A caller may
    /// declare the end of a part of a stream as well as of a whole one.
    ExplicitSourceEnd,
}

/// What a [`Dts14SyncReader`] measured for one frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dts14SyncFacts {
    /// Source, cursor generation and byte offset of the frame's first byte.
    pub identity: DtsSourceIdentity,
    /// Byte order of the source's 16-bit words.
    pub word_order: DtsWordOrder,
    /// Length of the frame in source bytes: two for each 14-bit word.
    pub physical_bytes: usize,
    /// Length of the unpacked frame in bytes, equal to `FSIZE` + 1.
    pub payload_bytes: usize,
    /// What ended the frame.
    pub boundary: Dts14BoundaryEvidence,
    /// Number of normal syncs inside the frame. They did not end it because the
    /// words before them did not hold `FSIZE` + 1 bytes.
    pub skipped_interior_syncs: usize,
    /// The frame's header, read from its first 18 source bytes.
    pub header: DtsCoreHeader,
}

/// One frame of a [`Dts14SyncReader`]: its source bytes, the unpacked frame and what
/// the reader measured. It owns its bytes and outlives the reader. The frame is a
/// complete span of the source with a checked header and length; its coded audio
/// and checksums are not checked, and nothing says a decoder accepts it.
#[derive(Clone, Debug)]
pub struct Dts14SyncFrame {
    payload: Dts14BitPayload,
    facts: Dts14SyncFacts,
}

impl Dts14SyncFrame {
    /// The frame as the source stores it: 16-bit words carrying 14 bits each.
    pub fn physical_bytes(&self) -> &Bytes {
        self.payload.source_bytes()
    }
    /// The unpacked frame: `FSIZE` + 1 bytes that start with the 32-bit `SYNC`.
    pub fn canonical_bytes(&self) -> &Bytes {
        self.payload.canonical_bytes()
    }
    /// The unpacked payload together with its source words: the two bits each word
    /// leaves unused and the source byte of every payload bit.
    pub fn payload(&self) -> &Dts14BitPayload {
        &self.payload
    }
    /// What the reader measured for the frame.
    pub const fn facts(&self) -> Dts14SyncFacts {
        self.facts
    }
}

/// The frame a [`Dts14SyncReader`] is reading.
enum Frame {
    /// Reading the frame's first 18 source bytes into the prefix.
    Header,
    /// The prefix is read; the header in it is checked next.
    Probe,
    /// Reading the frame's source bytes into `buffer`.
    Body {
        header: DtsCoreHeader,
        buffer: BudgetVec<u8>,
        step: BodyStep,
    },
    /// The frame's source bytes are complete.
    Frozen { facts: Dts14SyncFacts, raw: Bytes },
    /// Unpacking the frame's 14-bit words.
    Unpack {
        facts: Dts14SyncFacts,
        unpacker: Dts14BitUnpacker,
    },
    /// The frame, to be returned.
    Ready(Dts14SyncFrame),
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
    /// Checking the frame's length against its header.
    Geometry,
    /// The frame's source bytes are complete and measured.
    Freeze { facts: Dts14SyncFacts },
}

/// Reads the normal core frames of a 14-bit DTS stream: 16-bit words in a given byte
/// order, each carrying 14 bits of the frame in its low bits.
///
/// The stream must start with a frame. A frame ends at the next normal sync (`SYNC`,
/// `FTYPE` 1, `SHORT` 31) found where the words read so far hold exactly `FSIZE` + 1
/// bytes, or at the end of the source. `FSIZE` is only compared with the length
/// found; the reader never reads a number of bytes derived from it. A normal sync
/// anywhere else is frame content, counted in
/// [`Dts14SyncFacts::skipped_interior_syncs`].
///
/// Only frames of a multiple of seven bytes are read: they fill whole words, eight
/// source bytes for every seven. Any other frame is [`Error::Unsupported`], which
/// makes this narrower than 14-bit DTS in general. A reader from [`new`](Self::new)
/// also requires every frame to span as many source bytes as the first; one from
/// [`per_frame`](Self::per_frame) lets each frame have its own length.
///
/// Call [`step`](Self::step) and answer each [`Step::NeedInput`] with
/// [`provide`](Self::provide), [`finish`](Self::finish) or [`fail`](Self::fail).
/// [`reset`](Self::reset) on a reader that has not failed forgets the stream read so
/// far and starts a new cursor generation.
pub struct Dts14SyncReader {
    pool: Budget,
    input: ReaderInput,
    order: DtsWordOrder,
    limits: Dts14SyncLimits,
    frame: Frame,
    /// The frame's first 18 source bytes. After a frame that ended at a sync, they
    /// begin with that sync's six bytes.
    prefix: [u8; 18],
    prefix_len: usize,
    /// The bytes after the frame so far, read to find the sync that ends it.
    lookahead: [u8; 6],
    lookahead_len: usize,
    /// The end of the source, once it ended inside the frame being read.
    source_end: Option<SourceLocation>,
    frame_offset: u64,
    /// Source bytes of the last frame returned, which a reader from
    /// [`new`](Self::new) requires of every frame.
    interval: Option<usize>,
    require_fixed_interval: bool,
    skipped_interior_syncs: usize,
    completed: usize,
}

impl std::fmt::Debug for Dts14SyncReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dts14SyncReader")
            .field("order", &self.order)
            .field("limits", &self.limits)
            .field("require_fixed_interval", &self.require_fixed_interval)
            .field("position", &self.position())
            .field("generation", &self.generation())
            .field("completed", &self.completed)
            .field("failure", &self.input.failure())
            .finish_non_exhaustive()
    }
}

impl Dts14SyncReader {
    /// A reader at offset zero of `source`, whose words are in `order` and whose
    /// frames all span the same number of source bytes. A frame of another length
    /// than the first since construction or the last reset is
    /// [`Error::Unsupported`].
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] when a limit is zero, and [`Error::Limit`] when a frame of
    /// `maximum_physical_frame_bytes` cannot fit `pool`.
    pub fn new(
        pool: &Budget,
        source: SourceId,
        capabilities: SourceCapabilities,
        order: DtsWordOrder,
        limits: Dts14SyncLimits,
    ) -> Result<Self> {
        if limits.maximum_physical_frame_bytes == 0
            || limits.maximum_canonical_frame_bytes == 0
            || limits.maximum_frames == 0
            || limits.maximum_input_window == 0
        {
            return Err(Error::Invalid("zero DTS14 source-sync policy"));
        }
        let raw_owner = limits
            .maximum_physical_frame_bytes
            .checked_add(Bytes::owner_overhead_bytes())
            .and_then(|v| v.checked_add(Budget::control_storage_bytes()))
            .ok_or(Error::Overflow)?;
        limit(raw_owner, pool.limit())?;
        let cursor = InputCursor::new(pool, source, capabilities, limits.maximum_input_window)?;
        Ok(Self {
            pool: pool.clone(),
            input: ReaderInput::Open(cursor),
            order,
            limits,
            frame: Frame::Header,
            prefix: [0; 18],
            prefix_len: 0,
            lookahead: [0; 6],
            lookahead_len: 0,
            source_end: None,
            frame_offset: 0,
            interval: None,
            require_fixed_interval: true,
            skipped_interior_syncs: 0,
            completed: 0,
        })
    }

    /// Generation of the reader's input cursor, which every frame carries in its
    /// identity. [`reset`](Self::reset) starts a new one.
    pub fn generation(&self) -> u64 {
        self.input.generation()
    }
    /// Source offset of the next byte the reader takes from its input. After a frame
    /// that ended at a following sync it is six bytes past the frame's end: the
    /// reader has read that sync.
    pub fn position(&self) -> u64 {
        self.input.position()
    }
    /// A reader like one from [`new`](Self::new), except that each frame has its own
    /// length (ETSI TS 102 114 V1.6.1 5.3). Each frame ends at a following normal
    /// sync or at the declared end of the source, and its 14-bit payload holds
    /// exactly `FSIZE` + 1 bytes. The length and every source bit of a frame are
    /// checked for that frame alone; a payload that does not end on a byte boundary
    /// or does not hold `FSIZE` + 1 bytes is [`Error::Unsupported`].
    ///
    /// # Errors
    ///
    /// As [`new`](Self::new).
    pub fn per_frame(
        pool: &Budget,
        source: SourceId,
        capabilities: SourceCapabilities,
        order: DtsWordOrder,
        limits: Dts14SyncLimits,
    ) -> Result<Self> {
        let mut reader = Self::new(pool, source, capabilities, order, limits)?;
        reader.require_fixed_interval = false;
        Ok(reader)
    }

    /// The request for the bytes at [`position`](Self::position). A failed reader
    /// returns the request it had open when it failed.
    pub fn request(&self) -> InputRequest {
        self.input.request()
    }
    /// Answers the current request with `window`.
    ///
    /// # Errors
    ///
    /// The cursor's refusal of a window that does not answer the request (see
    /// [`InputCursor::provide`]), which leaves the reader as it was. A failed reader
    /// returns its earlier error.
    pub fn provide(&mut self, window: &InputWindow) -> Result<()> {
        self.input.cursor()?.provide(window)
    }
    /// Answers the current request with the end of the source, which also ends the
    /// frame being read.
    ///
    /// # Errors
    ///
    /// [`Error::Truncated`], from then on, when the source ends before its known
    /// length. A token or source that does not match the request is refused and
    /// leaves the reader as it was. A failed reader returns its earlier error.
    pub fn finish(&mut self, source: SourceId, token: InputToken) -> Result<()> {
        let result = self.input.cursor()?.finish(source, token);
        if result == Err(Error::Truncated) {
            self.stop(Error::Truncated);
        }
        result
    }
    /// Answers the current request with a source failure. The reader drops the frame
    /// it was reading and from then on returns [`Error::Transport`] or
    /// [`Error::Cancelled`]; the bytes read so far never become a frame.
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
        self.input.cursor()?.fail(source, token, failure)?;
        self.stop(source_error(failure));
        Ok(())
    }
    /// Restarts at source offset `offset`, where a frame must start, and returns the
    /// request for the bytes there. The frame being read is dropped, the frame count
    /// starts again, a reader from [`new`](Self::new) takes its frame length from the
    /// next frame, and the cursor takes a new generation, so answers to earlier
    /// requests are refused. Frames already returned stay as they are.
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`] when `offset` is odd, or the cursor's refusal of an
    /// offset it cannot serve (see [`InputCursor::reset`]). A failed reader returns
    /// its earlier error and cannot be reset.
    pub fn reset(&mut self, offset: u64) -> Result<InputRequest> {
        let cursor = self.input.cursor()?;
        if !offset.is_multiple_of(2) {
            return Err(Error::Unsupported(
                "DTS14 reset outside the source word grid",
            ));
        }
        let request = cursor.reset(offset)?;
        self.frame = Frame::Header;
        self.prefix_len = 0;
        self.lookahead_len = 0;
        self.source_end = None;
        self.frame_offset = offset;
        self.interval = None;
        self.skipped_interior_syncs = 0;
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
    /// # Errors
    ///
    /// [`Error::Backpressure`] and [`Error::Allocation`] leave the reader as it was:
    /// free pool storage and call again. Every other error is kept and returned by
    /// all later calls: [`Error::Cancelled`] when `cancel` is set, the refusal of a
    /// frame's header by [`probe_core_header`], [`Error::Unsupported`] for a frame
    /// that is not a multiple of seven bytes, that does not hold `FSIZE` + 1 bytes
    /// when the source ends, or whose length a reader from [`new`](Self::new) does
    /// not accept, [`Error::Truncated`] when the source ends inside a header or a
    /// word, and [`Error::Limit`] when a frame or the number of frames exceeds its
    /// limit.
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<Dts14SyncFrame>> {
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
    ) -> Result<Step<Dts14SyncFrame>> {
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
                    let header = probe_core_header(&header_payload(&self.prefix, self.order)?)?;
                    if header.frame_bytes % 7 != 0 {
                        return Err(Error::Unsupported("DTS14 unresolved nonzero-tail geometry"));
                    }
                    limit(
                        header.frame_bytes,
                        self.limits.maximum_canonical_frame_bytes,
                    )?;
                    limit(self.prefix.len(), self.limits.maximum_physical_frame_bytes)?;
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
                        buffer.try_reserve_exact(self.limits.maximum_physical_frame_bytes)?;
                        *step = BodyStep::CopyHeader { copied: 0 };
                    }
                    BodyStep::CopyHeader { copied } => {
                        let count = usize::try_from(
                            work.take(
                                u64::try_from(self.prefix.len() - copied)
                                    .map_err(|_| Error::Overflow)?,
                            ),
                        )
                        .map_err(|_| Error::Overflow)?;
                        if count == 0 {
                            return Ok(Step::Yield);
                        }
                        let end = copied.checked_add(count).ok_or(Error::Overflow)?;
                        if buffer.len().checked_add(count).ok_or(Error::Overflow)?
                            > buffer.capacity()
                        {
                            return Err(Error::Invalid("DTS14 header exceeds reserved capacity"));
                        }
                        buffer.try_extend_from_slice(&self.prefix[copied..end])?;
                        *step = if end == self.prefix.len() {
                            BodyStep::Scan
                        } else {
                            BodyStep::CopyHeader { copied: end }
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
                                    return Err(Error::Invalid("empty DTS14 scan read"));
                                }
                                self.lookahead_len = self
                                    .lookahead_len
                                    .checked_add(count)
                                    .ok_or(Error::Overflow)?;
                                if self.lookahead_len == self.lookahead.len() {
                                    *step = BodyStep::CheckSync;
                                }
                            }
                            Step::NeedInput(request) => return Ok(Step::NeedInput(request)),
                            Step::Yield => return Ok(Step::Yield),
                            Step::End(end) => {
                                if !self.lookahead_len.is_multiple_of(2) {
                                    return Err(Error::Truncated);
                                }
                                self.source_end = Some(end);
                                *step = if self.lookahead_len == 0 {
                                    BodyStep::Geometry
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
                        let normal_sync = normal_sync(&self.lookahead, self.order);
                        let bits = (buffer.len() / 2).checked_mul(14).ok_or(Error::Overflow)?;
                        let declared = header.frame_bytes.checked_mul(8).ok_or(Error::Overflow)?;
                        if normal_sync && bits == declared {
                            *step = BodyStep::Geometry;
                        } else {
                            if normal_sync {
                                self.skipped_interior_syncs = self
                                    .skipped_interior_syncs
                                    .checked_add(1)
                                    .ok_or(Error::Overflow)?;
                            }
                            *step = BodyStep::Append {
                                count: 2,
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
                            limit(next, self.limits.maximum_physical_frame_bytes)?;
                            if next > buffer.capacity() {
                                return Err(Error::Invalid("DTS14 body exceeds reserved capacity"));
                            }
                            buffer
                                .try_push(self.lookahead[copied])
                                .map_err(|e| e.error)?;
                            *step = BodyStep::Append {
                                count,
                                copied: copied + 1,
                            };
                        } else {
                            // A shift of at most six bytes, charged on its own; never a
                            // copy of the frame so far.
                            self.lookahead.copy_within(count..self.lookahead_len, 0);
                            self.lookahead_len -= count;
                            *step = if self.source_end.is_some() {
                                BodyStep::Geometry
                            } else {
                                BodyStep::Scan
                            };
                        }
                    }
                    BodyStep::Geometry => {
                        if !work.try_charge(1) {
                            return Ok(Step::Yield);
                        }
                        let length = buffer.len();
                        let bits = (length / 2).checked_mul(14).ok_or(Error::Overflow)?;
                        if length % 2 != 0 || bits % 8 != 0 || bits / 8 != header.frame_bytes {
                            return Err(Error::Unsupported(
                                "DTS14 unresolved FSIZE domain or tail geometry",
                            ));
                        }
                        if self.require_fixed_interval
                            && self.interval.is_some_and(|distance| distance != length)
                        {
                            return Err(Error::Unsupported("DTS14 changing source-sync intervals"));
                        }
                        let end = self
                            .frame_offset
                            .checked_add(u64::try_from(length).map_err(|_| Error::Overflow)?)
                            .ok_or(Error::Overflow)?;
                        let boundary = if let Some(source_end) = self.source_end {
                            if source_end.offset != end {
                                return Err(Error::Invalid("DTS14 source end mapping"));
                            }
                            Dts14BoundaryEvidence::ExplicitSourceEnd
                        } else {
                            if self.lookahead_len != 6
                                || self.input.position()
                                    != end.checked_add(6).ok_or(Error::Overflow)?
                            {
                                return Err(Error::Invalid("DTS14 following-sync mapping"));
                            }
                            Dts14BoundaryEvidence::FollowingNormalSync {
                                physical_offset: end,
                            }
                        };
                        *step = BodyStep::Freeze {
                            facts: Dts14SyncFacts {
                                identity: DtsSourceIdentity {
                                    source: self.input.source(),
                                    generation: self.input.generation(),
                                    offset: self.frame_offset,
                                },
                                word_order: self.order,
                                physical_bytes: length,
                                payload_bytes: bits / 8,
                                boundary,
                                skipped_interior_syncs: self.skipped_interior_syncs,
                                header: *header,
                            },
                        };
                    }
                    BodyStep::Freeze { facts } => {
                        if !work.try_charge(1) {
                            return Ok(Step::Yield);
                        }
                        match std::mem::replace(buffer, BudgetVec::new(&self.pool)).freeze() {
                            Ok(raw) => self.frame = Frame::Frozen { facts, raw },
                            Err(refused) => {
                                *buffer = refused.value;
                                return Err(refused.error);
                            }
                        }
                    }
                },
                Frame::Frozen { facts, raw } => {
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    let facts = *facts;
                    let unpacker = Dts14BitUnpacker::new(
                        &self.pool,
                        facts.identity,
                        raw.clone(),
                        self.order,
                        DtsPackingLimits {
                            maximum_source_bytes: self.limits.maximum_physical_frame_bytes,
                            maximum_output_bytes: self.limits.maximum_canonical_frame_bytes,
                        },
                    )?;
                    self.frame = Frame::Unpack { facts, unpacker };
                }
                Frame::Unpack { facts, unpacker } => match unpacker.step(work, cancel)? {
                    Step::Ready(payload) => {
                        if payload.residual_bits() != 0
                            || payload.canonical_bytes().len() != facts.payload_bytes
                        {
                            return Err(Error::Invalid("DTS14 exact-word unpack invariant"));
                        }
                        let facts = *facts;
                        self.frame = Frame::Ready(Dts14SyncFrame { payload, facts });
                    }
                    Step::Yield => return Ok(Step::Yield),
                    Step::NeedInput(_) | Step::End(_) => {
                        return Err(Error::Invalid("DTS14 owned unpack progress"));
                    }
                },
                Frame::Ready(_) => return self.publish(work),
                Frame::Ended(end) => return Ok(Step::End(*end)),
            }
        }
    }
    /// Reads the prefix on. `None` once it is complete; otherwise the step to
    /// return.
    fn read_header(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<Step<Dts14SyncFrame>>> {
        if self.prefix_len == 0 {
            self.frame_offset = self.position();
        }
        if self.completed >= self.limits.maximum_frames && self.prefix_len != 0 {
            return Err(self.frame_limit());
        }
        match self
            .input
            .cursor()?
            .read_into(&mut self.prefix[self.prefix_len..], work, cancel)?
        {
            Step::Ready(count) => {
                if count == 0 {
                    return Err(Error::Invalid("empty DTS14 header read"));
                }
                self.prefix_len = self.prefix_len.checked_add(count).ok_or(Error::Overflow)?;
                if self.completed >= self.limits.maximum_frames {
                    return Err(self.frame_limit());
                }
                if self.prefix_len == self.prefix.len() {
                    self.frame = Frame::Probe;
                }
                Ok(None)
            }
            Step::NeedInput(request) => Ok(Some(Step::NeedInput(request))),
            Step::Yield => Ok(Some(Step::Yield)),
            Step::End(end) => {
                if self.prefix_len != 0 {
                    return Err(Error::Truncated);
                }
                self.frame = Frame::Ended(end);
                Ok(Some(Step::End(end)))
            }
        }
    }
    /// The error for a frame past `maximum_frames`.
    fn frame_limit(&self) -> Error {
        match self.completed.checked_add(1) {
            Some(requested) => Error::Limit {
                requested,
                maximum: self.limits.maximum_frames,
            },
            None => Error::Overflow,
        }
    }
    /// Returns the ready frame and moves on to the next one.
    fn publish(&mut self, work: &mut WorkBudget) -> Result<Step<Dts14SyncFrame>> {
        if !work.try_charge(1) {
            return Ok(Step::Yield);
        }
        let Frame::Ready(ready) = std::mem::replace(&mut self.frame, Frame::Header) else {
            return Err(Error::Invalid("missing DTS14 ready envelope"));
        };
        self.completed = self.completed.checked_add(1).ok_or(Error::Overflow)?;
        self.interval = Some(ready.facts.physical_bytes);
        if let Some(end) = self.source_end {
            self.frame = Frame::Ended(end);
        } else {
            self.prefix[..6].copy_from_slice(&self.lookahead);
            self.prefix_len = 6;
            self.frame_offset = ready
                .facts
                .identity
                .offset
                .checked_add(
                    u64::try_from(ready.facts.physical_bytes).map_err(|_| Error::Overflow)?,
                )
                .ok_or(Error::Overflow)?;
            self.lookahead_len = 0;
        }
        self.skipped_interior_syncs = 0;
        Ok(Step::Ready(ready))
    }
}

/// The 16-bit word at byte `at` of `source`.
pub(super) fn word(source: &[u8], at: usize, order: DtsWordOrder) -> u16 {
    let pair = [source[at], source[at + 1]];
    match order {
        DtsWordOrder::BigEndian => u16::from_be_bytes(pair),
        DtsWordOrder::LittleEndian => u16::from_le_bytes(pair),
    }
}

fn normal_sync(source: &[u8; 6], order: DtsWordOrder) -> bool {
    let prefix = (u64::from(word(source, 0, order) & 0x3fff) << 28)
        | (u64::from(word(source, 2, order) & 0x3fff) << 14)
        | u64::from(word(source, 4, order) & 0x3fff);
    prefix >> 4 == ((u64::from(CORE_SYNC) << 6) | 0x3f)
}

pub(crate) fn header_payload(source: &[u8; 18], order: DtsWordOrder) -> Result<[u8; 15]> {
    // Fixed120-bit stack probe. Complete source words stay in the raw owner.
    let mut output = [0_u8; 15];
    for bit in 0..120_usize {
        let value = (word(source, (bit / 14) * 2, order) >> (13 - bit % 14)) & 1;
        output[bit / 8] |= u8::try_from(value).map_err(|_| Error::Overflow)? << (7 - bit % 8);
    }
    Ok(output)
}

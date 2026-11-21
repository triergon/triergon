//! Normal core frames of a DTS stream in any word packing, ETSI TS 102 114 §5.3 and
//! §5.4.2. A frame is the span from one normal sync to the next; how its length
//! relates to `FSIZE` is reported, and neither shows that the coded audio is valid.
use crate::dts::packed::limit;
use crate::dts::{
    DtsCoreField, DtsPackedHeader, DtsPackedLimits, DtsPackedPayload, DtsPackedPayloadJob,
    DtsPacking, DtsSourceIdentity, inspect_header_bits,
};
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

#[derive(Clone, Copy)]
enum Phase {
    Header,
    Probe,
    Reserve,
    CopyHeader,
    Scan,
    CheckSync,
    Append,
    Freeze,
    Unpack,
    Publish,
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
    cursor: Option<InputCursor>,
    last_request: InputRequest,
    packing: DtsPacking,
    limits: DtsNormalLimits,
    phase: Phase,
    prefix: [u8; 18],
    prefix_len: usize,
    prefix_copied: usize,
    lookahead: [u8; 6],
    lookahead_len: usize,
    append_count: usize,
    append_copied: usize,
    frame_offset: u64,
    source_end: Option<SourceLocation>,
    ended: Option<SourceLocation>,
    header: Option<DtsPackedHeader>,
    buffer: Option<BudgetVec<u8>>,
    raw: Option<Bytes>,
    job: Option<DtsPackedPayloadJob>,
    ready: Option<DtsNormalSourceFrame>,
    skipped_short_syncs: usize,
    completed: usize,
    failure: Option<Error>,
}

impl std::fmt::Debug for DtsNormalSourceReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DtsNormalSourceReader")
            .field("packing", &self.packing)
            .field("limits", &self.limits)
            .field("position", &self.position())
            .field("generation", &self.generation())
            .field("completed", &self.completed)
            .field("failure", &self.failure)
            .finish_non_exhaustive()
    }
}

impl DtsNormalSourceReader {
    /// A reader at offset zero of `source`, whose stream is stored in `packing`.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] when a limit is zero, and [`Error::Limit`] when a frame of
    /// `maximum_source_frame_bytes` cannot fit `pool`.
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
        // Reserve while empty; no growing dynamic prefix is moved under work=1.
        let minimum = limits
            .maximum_source_frame_bytes
            .checked_add(Bytes::owner_overhead_bytes())
            .and_then(|v| v.checked_add(Budget::control_storage_bytes()))
            .ok_or(Error::Overflow)?;
        limit(minimum, pool.limit())?;
        let cursor = InputCursor::new(pool, source, capabilities, limits.maximum_input_window)?;
        let last_request = cursor.request();
        Ok(Self {
            pool: pool.clone(),
            cursor: Some(cursor),
            last_request,
            packing,
            limits,
            phase: Phase::Header,
            prefix: [0; 18],
            prefix_len: 0,
            prefix_copied: 0,
            lookahead: [0; 6],
            lookahead_len: 0,
            append_count: 0,
            append_copied: 0,
            frame_offset: 0,
            source_end: None,
            ended: None,
            header: None,
            buffer: None,
            raw: None,
            job: None,
            ready: None,
            skipped_short_syncs: 0,
            completed: 0,
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
    /// Source offset of the next byte the reader takes from its input. After a frame
    /// that ended at a following sync it is six bytes past the frame's end: the
    /// reader has read that sync.
    pub fn position(&self) -> u64 {
        self.cursor
            .as_ref()
            .map_or(self.last_request.offset, InputCursor::position)
    }
    /// Generation of the reader's input cursor, which every frame carries in its
    /// identity. [`reset`](Self::reset) starts a new one.
    pub fn generation(&self) -> u64 {
        self.cursor.as_ref().map_or(
            self.last_request.token.generation(),
            InputCursor::generation,
        )
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
        self.cursor_mut()?.provide(window)
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
        self.healthy()?;
        let result = self.cursor_mut()?.finish(source, token);
        if result == Err(Error::Truncated) {
            self.record_failure(Error::Truncated);
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
        self.healthy()?;
        self.cursor_mut()?.fail(source, token, failure)?;
        self.record_failure(match failure {
            SourceFailure::Transport => Error::Transport,
            SourceFailure::Cancelled => Error::Cancelled,
        });
        Ok(())
    }
    /// Restarts at source offset `offset`, where a frame must start, and returns the
    /// request for the bytes there. The frame being read is dropped, the frame count
    /// starts again and the cursor takes a new generation, so answers to earlier
    /// requests are refused. Frames already returned stay as they are.
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`] when `offset` is not a multiple of the packing's source
    /// unit, or the cursor's refusal of an offset it cannot serve (see
    /// [`InputCursor::reset`]). A failed reader returns its earlier error and cannot
    /// be reset.
    pub fn reset(&mut self, offset: u64) -> Result<InputRequest> {
        self.healthy()?;
        if !offset.is_multiple_of(self.packing.source_unit_bytes() as u64) {
            return Err(Error::Unsupported("DTS reset outside source unit grid"));
        }
        let request = self.cursor_mut()?.reset(offset)?;
        self.clear_builders();
        self.phase = Phase::Header;
        self.prefix_len = 0;
        self.lookahead_len = 0;
        self.source_end = None;
        self.ended = None;
        self.frame_offset = offset;
        self.skipped_short_syncs = 0;
        self.completed = 0;
        self.last_request = request.clone();
        Ok(request)
    }
    fn healthy(&self) -> Result<()> {
        self.failure.map_or(Ok(()), Err)
    }
    fn cursor_mut(&mut self) -> Result<&mut InputCursor> {
        self.cursor
            .as_mut()
            .ok_or(Error::Invalid("missing DTS normal cursor"))
    }
    fn clear_builders(&mut self) {
        self.buffer = None;
        self.raw = None;
        self.job = None;
        self.ready = None;
        self.header = None;
    }
    fn record_failure(&mut self, error: Error) {
        if let Some(cursor) = self.cursor.take() {
            self.last_request = cursor.request();
        }
        self.clear_builders();
        self.failure = Some(error);
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
    /// all later calls: [`Error::Cancelled`] when `cancel` is set, [`Error::Invalid`]
    /// when the stream does not start with `SYNC`, [`Error::Unsupported`] when it
    /// starts with a termination frame (`FTYPE` 0) or a `SHORT` other than 31,
    /// [`Error::Truncated`] when the source ends inside a header or a source word,
    /// and [`Error::Limit`] when a frame or the number of frames exceeds its limit.
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<DtsNormalSourceFrame>> {
        self.healthy()?;
        let result = self.advance(work, cancel);
        if let Err(error) = result
            && !matches!(error, Error::Backpressure { .. } | Error::Allocation)
        {
            self.record_failure(error);
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
            if let Some(end) = self.ended {
                return Ok(Step::End(end));
            }
            if work.remaining() == 0 {
                return Ok(Step::Yield);
            }
            match self.phase {
                Phase::Header => {
                    if self.prefix_len == 0 {
                        self.frame_offset = self.position();
                    }
                    match self
                        .cursor
                        .as_mut()
                        .ok_or(Error::Invalid("DTS header cursor"))?
                        .read_into(&mut self.prefix[self.prefix_len..], work, cancel)?
                    {
                        Step::Ready(count) => {
                            if count == 0 {
                                return Err(Error::Invalid("empty DTS header read"));
                            }
                            self.prefix_len += count;
                            if self.completed >= self.limits.maximum_frames {
                                return Err(Error::Limit {
                                    requested: self
                                        .completed
                                        .checked_add(1)
                                        .ok_or(Error::Overflow)?,
                                    maximum: self.limits.maximum_frames,
                                });
                            }
                            if self.prefix_len == self.prefix.len() {
                                self.phase = Phase::Probe;
                            }
                        }
                        Step::NeedInput(request) => return Ok(Step::NeedInput(request)),
                        Step::Yield => return Ok(Step::Yield),
                        Step::End(end) => {
                            if self.prefix_len == 0 {
                                self.ended = Some(end);
                                return Ok(Step::End(end));
                            }
                            self.source_end = Some(end);
                            self.phase = Phase::Probe;
                        }
                    }
                }
                Phase::Probe => {
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    limit(self.prefix_len, self.limits.maximum_source_frame_bytes)?;
                    let bits = self.packing.payload_bits(self.prefix_len)?;
                    let mut stack = [0_u8; 15];
                    for bit in 0..bits.min(120) {
                        stack[bit / 8] |= self.packing.bit(&self.prefix[..self.prefix_len], bit)?
                            << (7 - bit % 8);
                    }
                    let header = inspect_header_bits(&stack, bits.min(120))?;
                    if header.value(DtsCoreField::Sync) != Some(0x7ffe8001) {
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
                    self.header = Some(header);
                    self.phase = Phase::Reserve;
                }
                Phase::Reserve => {
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    let buffer = self
                        .buffer
                        .get_or_insert_with(|| BudgetVec::new(&self.pool));
                    if !buffer.is_empty() {
                        return Err(Error::Invalid("DTS reserve would copy live prefix"));
                    }
                    buffer.try_reserve_exact(self.limits.maximum_source_frame_bytes)?;
                    self.prefix_copied = 0;
                    self.phase = Phase::CopyHeader;
                }
                Phase::CopyHeader => {
                    let count = usize::try_from(
                        work.take(
                            u64::try_from(self.prefix_len - self.prefix_copied)
                                .map_err(|_| Error::Overflow)?,
                        ),
                    )
                    .map_err(|_| Error::Overflow)?;
                    if count == 0 {
                        return Ok(Step::Yield);
                    }
                    let end = self.prefix_copied + count;
                    self.buffer
                        .as_mut()
                        .ok_or(Error::Invalid("DTS header builder"))?
                        .try_extend_from_slice(&self.prefix[self.prefix_copied..end])?;
                    self.prefix_copied = end;
                    if end == self.prefix_len {
                        self.phase = if self.source_end.is_some() {
                            Phase::Freeze
                        } else {
                            Phase::Scan
                        };
                    }
                }
                Phase::Scan => {
                    match self
                        .cursor
                        .as_mut()
                        .ok_or(Error::Invalid("DTS scan cursor"))?
                        .read_into(&mut self.lookahead[self.lookahead_len..], work, cancel)?
                    {
                        Step::Ready(count) => {
                            if count == 0 {
                                return Err(Error::Invalid("empty DTS scan read"));
                            }
                            self.lookahead_len += count;
                            if self.lookahead_len == self.lookahead.len() {
                                self.phase = Phase::CheckSync;
                            }
                        }
                        Step::NeedInput(request) => return Ok(Step::NeedInput(request)),
                        Step::Yield => return Ok(Step::Yield),
                        Step::End(end) => {
                            self.packing.payload_bits(self.lookahead_len)?;
                            self.source_end = Some(end);
                            self.append_count = self.lookahead_len;
                            self.append_copied = 0;
                            self.phase = if self.lookahead_len == 0 {
                                Phase::Freeze
                            } else {
                                Phase::Append
                            };
                        }
                    }
                }
                Phase::CheckSync => {
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    let normal = self.packing.normal_sync(&self.lookahead)?;
                    let length = self
                        .buffer
                        .as_ref()
                        .ok_or(Error::Invalid("DTS sync builder"))?
                        .len();
                    let declared = self
                        .header
                        .ok_or(Error::Invalid("DTS sync header"))?
                        .views()
                        .declared_frame_bytes
                        .ok_or(Error::Invalid("DTS missing FSIZE"))?
                        as usize;
                    // This is a short-candidate filter, not a physical read target.
                    // Neither relation is chosen as the authoritative FSIZE domain.
                    let bits = declared.checked_mul(8).ok_or(Error::Overflow)?;
                    let words = bits
                        .checked_add(self.packing.word_payload_bits() - 1)
                        .ok_or(Error::Overflow)?
                        / self.packing.word_payload_bits();
                    let payload_span = words
                        .checked_mul(self.packing.source_unit_bytes())
                        .ok_or(Error::Overflow)?;
                    if normal && length >= declared.min(payload_span) {
                        self.phase = Phase::Freeze;
                    } else {
                        if normal {
                            self.skipped_short_syncs = self
                                .skipped_short_syncs
                                .checked_add(1)
                                .ok_or(Error::Overflow)?;
                        }
                        self.append_count = self.packing.source_unit_bytes();
                        self.append_copied = 0;
                        self.phase = Phase::Append;
                    }
                }
                Phase::Append => {
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    if self.append_copied < self.append_count {
                        let buffer = self
                            .buffer
                            .as_mut()
                            .ok_or(Error::Invalid("DTS body builder"))?;
                        let next = buffer.len().checked_add(1).ok_or(Error::Overflow)?;
                        limit(next, self.limits.maximum_source_frame_bytes)?;
                        if next > buffer.capacity() {
                            return Err(Error::Invalid("DTS source capacity invariant"));
                        }
                        buffer
                            .try_push(self.lookahead[self.append_copied])
                            .map_err(|failure| failure.error)?;
                        self.append_copied += 1;
                    } else {
                        self.lookahead
                            .copy_within(self.append_count..self.lookahead_len, 0);
                        self.lookahead_len -= self.append_count;
                        self.phase = if self.source_end.is_some() {
                            Phase::Freeze
                        } else {
                            Phase::Scan
                        };
                    }
                }
                Phase::Freeze => {
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    let buffer = self
                        .buffer
                        .take()
                        .ok_or(Error::Invalid("DTS source freeze"))?;
                    match buffer.freeze() {
                        Ok(raw) => {
                            self.raw = Some(raw);
                            self.phase = Phase::Unpack;
                        }
                        Err(failure) => {
                            self.buffer = Some(failure.value);
                            return Err(failure.error);
                        }
                    }
                }
                Phase::Unpack => {
                    if self.job.is_none() {
                        if !work.try_charge(1) {
                            return Ok(Step::Yield);
                        }
                        let raw = self
                            .raw
                            .as_ref()
                            .ok_or(Error::Invalid("DTS source job owner"))?;
                        let identity = DtsSourceIdentity {
                            source: self.last_request.source,
                            generation: self.generation(),
                            offset: self.frame_offset,
                        };
                        self.job = Some(DtsPackedPayloadJob::new(
                            &self.pool,
                            identity,
                            raw.clone(),
                            self.packing,
                            DtsPackedLimits {
                                maximum_source_bytes: self.limits.maximum_source_frame_bytes,
                                maximum_storage_bytes: self.limits.maximum_storage_bytes,
                            },
                        )?);
                        self.raw = None;
                    }
                    match self
                        .job
                        .as_mut()
                        .ok_or(Error::Invalid("DTS payload job"))?
                        .step(work, cancel)?
                    {
                        Step::Ready(payload) => {
                            let header = self.header.ok_or(Error::Invalid("DTS output header"))?;
                            let declared = header
                                .views()
                                .declared_frame_bytes
                                .ok_or(Error::Invalid("DTS output FSIZE"))?
                                as usize;
                            let declared_bits = declared.checked_mul(8).ok_or(Error::Overflow)?;
                            let surplus = payload.valid_bits().checked_sub(declared_bits);
                            let word_count =
                                surplus.is_some_and(|v| v < self.packing.word_payload_bits());
                            let physical = payload.source_bytes().len() == declared;
                            let relation = match (word_count, physical) {
                                (true, true) => DtsExtentRelation::Both,
                                (true, false) => DtsExtentRelation::PayloadWordCount,
                                (false, true) => DtsExtentRelation::PhysicalByteCount,
                                (false, false) => DtsExtentRelation::Neither,
                            };
                            let tail = if word_count {
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
                                .checked_add(
                                    u64::try_from(payload.source_bytes().len())
                                        .map_err(|_| Error::Overflow)?,
                                )
                                .ok_or(Error::Overflow)?;
                            let boundary = if let Some(source_end) = self.source_end {
                                if end != source_end.offset {
                                    return Err(Error::Invalid("DTS EOF source mapping"));
                                }
                                DtsNormalBoundary::ExplicitSourceEnd
                            } else {
                                if self.lookahead_len != 6
                                    || self.position()
                                        != end.checked_add(6).ok_or(Error::Overflow)?
                                {
                                    return Err(Error::Invalid(
                                        "DTS following sync source mapping",
                                    ));
                                }
                                DtsNormalBoundary::FollowingNormalSync { offset: end }
                            };
                            let facts = DtsNormalFacts {
                                identity: payload.identity(),
                                packing: self.packing,
                                source_bytes: payload.source_bytes().len(),
                                source_payload_bits: payload.valid_bits(),
                                storage_bytes: payload.storage_bytes().len(),
                                boundary,
                                skipped_short_syncs: self.skipped_short_syncs,
                                extent_relation: relation,
                                carrier_tail: tail,
                                header,
                            };
                            self.ready = Some(DtsNormalSourceFrame { payload, facts });
                            self.job = None;
                            self.phase = Phase::Publish;
                        }
                        Step::Yield => return Ok(Step::Yield),
                        Step::End(_) | Step::NeedInput(_) => {
                            return Err(Error::Invalid("DTS owned payload progress"));
                        }
                    }
                }
                Phase::Publish => {
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    let ready = self
                        .ready
                        .take()
                        .ok_or(Error::Invalid("DTS ready source record"))?;
                    self.completed = self.completed.checked_add(1).ok_or(Error::Overflow)?;
                    if let Some(end) = self.source_end {
                        self.ended = Some(end);
                    } else {
                        self.prefix[..6].copy_from_slice(&self.lookahead);
                        self.prefix_len = 6;
                        self.frame_offset = ready
                            .facts
                            .identity
                            .offset
                            .checked_add(
                                u64::try_from(ready.facts.source_bytes)
                                    .map_err(|_| Error::Overflow)?,
                            )
                            .ok_or(Error::Overflow)?;
                        self.lookahead_len = 0;
                        self.phase = Phase::Header;
                    }
                    self.header = None;
                    self.skipped_short_syncs = 0;
                    return Ok(Step::Ready(ready));
                }
            }
        }
    }
}

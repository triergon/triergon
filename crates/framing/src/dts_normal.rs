//! Original normal-frame source acquisition, ETSI TS102114 §5.3/5.4.2.
//! A measured sync span and FSIZE arithmetic are evidence, not audio validity.
use crate::dts_packed::limit;
use crate::{
    DtsCoreField, DtsPackedHeader, DtsPackedLimits, DtsPackedPayload, DtsPackedPayloadJob,
    DtsPacking, DtsSourceIdentity, inspect_dts_header_bits,
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, BudgetVec, Bytes, Error, InputCursor, InputRequest, InputToken, InputWindow, Result,
    SourceCapabilities, SourceFailure, SourceId, SourceLocation, Step, WorkBudget,
};

pub const DTS_NORMAL_PENDING: &str = "DTS normal source acquisition pending parent RED";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DtsNormalLimits {
    pub maximum_source_frame_bytes: usize,
    pub maximum_storage_bytes: usize,
    pub maximum_frames: usize,
    pub maximum_input_window: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DtsNormalBoundary {
    /// A following normal38 sync on the same source unit grid was observed.
    /// This alone does not prove that a coded-audio payload cannot contain it.
    FollowingNormalSync { offset: u64 },
    /// Completion is explicit, including completion of an artificial view.
    ExplicitSourceEnd,
}

/// Two independently named arithmetic equalities, never a chosen FSIZE domain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DtsExtentRelation {
    Both,
    PayloadWordCount,
    PhysicalByteCount,
    Neither,
}

/// Actual source bits after FSIZE+1 payload bytes, only when their number is
/// less than one source unit's payload width. Their validity/meaning is open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DtsCarrierTail {
    pub start_bit: usize,
    pub bit_count: u8,
    pub value: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DtsNormalFacts {
    pub identity: DtsSourceIdentity,
    pub packing: DtsPacking,
    pub source_bytes: usize,
    pub source_payload_bits: usize,
    pub storage_bytes: usize,
    pub boundary: DtsNormalBoundary,
    pub skipped_short_syncs: usize,
    pub extent_relation: DtsExtentRelation,
    pub carrier_tail: Option<DtsCarrierTail>,
    pub header: DtsPackedHeader,
}

#[derive(Clone, Debug)]
pub struct DtsNormalSourceFrame {
    payload: DtsPackedPayload,
    facts: DtsNormalFacts,
}
impl DtsNormalSourceFrame {
    pub fn payload(&self) -> &DtsPackedPayload {
        &self.payload
    }
    pub fn source_bytes(&self) -> &Bytes {
        self.payload.source_bytes()
    }
    pub fn storage_bytes(&self) -> &Bytes {
        self.payload.storage_bytes()
    }
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

/// Four normal source packings with independently measured, variable intervals.
/// Packing is a source storage fact, not a caller-selected FSIZE interpretation.
/// Source units are bytes for CanonicalBe and complete words for other packings.
/// A normal sync before the minimum of the two named size equalities is retained
/// as an interior candidate. Later candidates delimit source records even if
/// neither equality holds; that unresolved relation stays in the output.
/// Termination/HD, arbitrary resynchronization and coded-body validation remain
/// unsupported. In particular a published source record is not a parity verdict.
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

impl DtsNormalSourceReader {
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
    pub fn request(&self) -> InputRequest {
        self.cursor
            .as_ref()
            .map_or_else(|| self.last_request.clone(), InputCursor::request)
    }
    pub fn position(&self) -> u64 {
        self.cursor
            .as_ref()
            .map_or(self.last_request.offset, InputCursor::position)
    }
    pub fn generation(&self) -> u64 {
        self.cursor.as_ref().map_or(
            self.last_request.token.generation(),
            InputCursor::generation,
        )
    }
    pub fn provide(&mut self, window: &InputWindow) -> Result<()> {
        self.healthy()?;
        self.cursor_mut()?.provide(window)
    }
    pub fn finish(&mut self, source: SourceId, token: InputToken) -> Result<()> {
        self.healthy()?;
        let result = self.cursor_mut()?.finish(source, token);
        if result == Err(Error::Truncated) {
            self.record_failure(Error::Truncated);
        }
        result
    }
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
                    let header = inspect_dts_header_bits(&stack, bits.min(120))?;
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
                            self.buffer = Some(failure.buffer);
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

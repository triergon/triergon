//! Experimental exact-word normal DTS14 envelopes, discovered from source syncs.
//! ETSI TS102114 V1.2.1 §5.3 / V1.6.1 §5.3–5.4.2; not universal DTS framing.
use crate::{
    Dts14BitPayload, Dts14BitUnpacker, DtsCoreHeader, DtsPackingLimits, DtsSourceIdentity,
    DtsWordOrder, probe_dts_core_header,
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, BudgetVec, Bytes, Error, InputCursor, InputRequest, InputToken, InputWindow, Result,
    SourceCapabilities, SourceFailure, SourceId, SourceLocation, Step, WorkBudget,
};

/// Guard-only RED overlay uses this exact diagnostic; candidate never emits it.
pub const DTS14_SYNC_PENDING: &str = "DTS14 source-sync envelope pending parent runtime RED";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dts14SyncLimits {
    pub maximum_physical_frame_bytes: usize,
    pub maximum_canonical_frame_bytes: usize,
    pub maximum_frames: usize,
    pub maximum_input_window: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dts14BoundaryEvidence {
    /// Following normal38-bit sync was read, but is not in this frame's bytes.
    FollowingNormalSync { physical_offset: u64 },
    /// Explicit completion of the supplied entity, possibly an artificial view.
    ExplicitSourceEnd,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dts14SyncFacts {
    pub identity: DtsSourceIdentity,
    pub word_order: DtsWordOrder,
    pub physical_bytes: usize,
    pub payload_bytes: usize,
    pub boundary: Dts14BoundaryEvidence,
    pub skipped_interior_syncs: usize,
    pub header: DtsCoreHeader,
}

/// Complete source span, not a coded-audio/CRC or complete-module certificate.
#[derive(Clone, Debug)]
pub struct Dts14SyncFrame {
    payload: Dts14BitPayload,
    facts: Dts14SyncFacts,
}

impl Dts14SyncFrame {
    pub fn physical_bytes(&self) -> &Bytes {
        self.payload.source_bytes()
    }
    pub fn canonical_bytes(&self) -> &Bytes {
        self.payload.canonical_bytes()
    }
    pub fn payload(&self) -> &Dts14BitPayload {
        &self.payload
    }
    pub const fn facts(&self) -> Dts14SyncFacts {
        self.facts
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Header,
    Probe,
    Reserve,
    CopyHeader,
    Scan,
    CheckSync,
    Append,
    Geometry,
    Freeze,
    Unpack,
    Publish,
}

/// Bounded source-sync discovery with no FSIZE-derived physical read target.
/// Exact-word geometry is deliberately narrower than a general DTS14 stream.
/// A healthy reset clears synchronization and advances the core generation.
pub struct Dts14SyncReader {
    pool: Budget,
    cursor: Option<InputCursor>,
    last_request: InputRequest,
    order: DtsWordOrder,
    limits: Dts14SyncLimits,
    phase: Phase,
    prefix: [u8; 18],
    prefix_len: usize,
    prefix_copied: usize,
    lookahead: [u8; 6],
    lookahead_len: usize,
    append_count: usize,
    append_copied: usize,
    source_end: Option<SourceLocation>,
    frame_offset: u64,
    header: Option<DtsCoreHeader>,
    buffer: Option<BudgetVec<u8>>,
    raw: Option<Bytes>,
    unpacker: Option<Dts14BitUnpacker>,
    ready: Option<Dts14SyncFrame>,
    facts: Option<Dts14SyncFacts>,
    interval: Option<usize>,
    require_fixed_interval: bool,
    skipped_interior_syncs: usize,
    completed: usize,
    ended: Option<SourceLocation>,
    failure: Option<Error>,
}

impl Dts14SyncReader {
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
        let last_request = cursor.request();
        Ok(Self {
            pool: pool.clone(),
            cursor: Some(cursor),
            last_request,
            order,
            limits,
            phase: Phase::Header,
            prefix: [0; 18],
            prefix_len: 0,
            prefix_copied: 0,
            lookahead: [0; 6],
            lookahead_len: 0,
            append_count: 0,
            append_copied: 0,
            source_end: None,
            frame_offset: 0,
            header: None,
            buffer: None,
            raw: None,
            unpacker: None,
            ready: None,
            facts: None,
            interval: None,
            require_fixed_interval: true,
            skipped_interior_syncs: 0,
            completed: 0,
            ended: None,
            failure: None,
        })
    }

    pub fn generation(&self) -> u64 {
        self.cursor.as_ref().map_or(
            self.last_request.token.generation(),
            InputCursor::generation,
        )
    }
    pub fn position(&self) -> u64 {
        self.cursor
            .as_ref()
            .map_or(self.last_request.offset, InputCursor::position)
    }
    /// Edition-bound per-frame cadence. Geometry and every source bit are still
    /// validated independently for each record; no universal FSIZE/tail rule.
    pub(crate) fn per_frame(
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

    pub fn request(&self) -> InputRequest {
        self.cursor
            .as_ref()
            .map_or_else(|| self.last_request.clone(), InputCursor::request)
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
        if offset % 2 != 0 {
            return Err(Error::Unsupported(
                "DTS14 reset outside the source word grid",
            ));
        }
        let request = self.cursor_mut()?.reset(offset)?;
        self.clear_builders();
        self.phase = Phase::Header;
        self.prefix_len = 0;
        self.lookahead_len = 0;
        self.source_end = None;
        self.frame_offset = offset;
        self.interval = None;
        self.skipped_interior_syncs = 0;
        self.completed = 0;
        self.ended = None;
        self.last_request = request.clone();
        Ok(request)
    }

    fn cursor_mut(&mut self) -> Result<&mut InputCursor> {
        self.cursor
            .as_mut()
            .ok_or(Error::Invalid("missing healthy DTS14 cursor"))
    }
    fn healthy(&self) -> Result<()> {
        self.failure.map_or(Ok(()), Err)
    }
    fn clear_builders(&mut self) {
        self.buffer = None;
        self.raw = None;
        self.unpacker = None;
        self.ready = None;
        self.facts = None;
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
    ) -> Result<Step<Dts14SyncFrame>> {
        self.healthy()?;
        let result = self.advance(work, cancel);
        if let Err(error) = result {
            if !matches!(error, Error::Backpressure { .. } | Error::Allocation) {
                self.record_failure(error);
            }
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
                    if self.completed >= self.limits.maximum_frames && self.prefix_len != 0 {
                        return Err(Error::Limit {
                            requested: self.completed.checked_add(1).ok_or(Error::Overflow)?,
                            maximum: self.limits.maximum_frames,
                        });
                    }
                    let cursor = self
                        .cursor
                        .as_mut()
                        .ok_or(Error::Invalid("missing DTS14 header cursor"))?;
                    match cursor.read_into(&mut self.prefix[self.prefix_len..], work, cancel)? {
                        Step::Ready(count) => {
                            if count == 0 {
                                return Err(Error::Invalid("empty DTS14 header read"));
                            }
                            self.prefix_len =
                                self.prefix_len.checked_add(count).ok_or(Error::Overflow)?;
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
                            if self.prefix_len != 0 {
                                return Err(Error::Truncated);
                            }
                            self.ended = Some(end);
                            return Ok(Step::End(end));
                        }
                    }
                }
                Phase::Probe => {
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    let canonical = header_payload(&self.prefix, self.order)?;
                    let header = probe_dts_core_header(&canonical)?;
                    if header.frame_bytes % 7 != 0 {
                        return Err(Error::Unsupported("DTS14 unresolved nonzero-tail geometry"));
                    }
                    limit(
                        header.frame_bytes,
                        self.limits.maximum_canonical_frame_bytes,
                    )?;
                    limit(self.prefix.len(), self.limits.maximum_physical_frame_bytes)?;
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
                        return Err(Error::Invalid("DTS14 reserve would move copied bytes"));
                    }
                    buffer.try_reserve_exact(self.limits.maximum_physical_frame_bytes)?;
                    self.prefix_copied = 0;
                    self.phase = Phase::CopyHeader;
                }
                Phase::CopyHeader => {
                    let count = usize::try_from(
                        work.take(
                            u64::try_from(self.prefix.len() - self.prefix_copied)
                                .map_err(|_| Error::Overflow)?,
                        ),
                    )
                    .map_err(|_| Error::Overflow)?;
                    if count == 0 {
                        return Ok(Step::Yield);
                    }
                    let end = self
                        .prefix_copied
                        .checked_add(count)
                        .ok_or(Error::Overflow)?;
                    let buffer = self
                        .buffer
                        .as_mut()
                        .ok_or(Error::Invalid("missing DTS14 header builder"))?;
                    if buffer.len().checked_add(count).ok_or(Error::Overflow)? > buffer.capacity() {
                        return Err(Error::Invalid("DTS14 header exceeds reserved capacity"));
                    }
                    buffer.try_extend_from_slice(&self.prefix[self.prefix_copied..end])?;
                    self.prefix_copied = end;
                    if end == self.prefix.len() {
                        self.phase = Phase::Scan;
                    }
                }
                Phase::Scan => {
                    let cursor = self
                        .cursor
                        .as_mut()
                        .ok_or(Error::Invalid("missing DTS14 scan cursor"))?;
                    match cursor.read_into(
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
                                self.phase = Phase::CheckSync;
                            }
                        }
                        Step::NeedInput(request) => return Ok(Step::NeedInput(request)),
                        Step::Yield => return Ok(Step::Yield),
                        Step::End(end) => {
                            if self.lookahead_len % 2 != 0 {
                                return Err(Error::Truncated);
                            }
                            self.source_end = Some(end);
                            if self.lookahead_len == 0 {
                                self.phase = Phase::Geometry;
                            } else {
                                self.append_count = self.lookahead_len;
                                self.append_copied = 0;
                                self.phase = Phase::Append;
                            }
                        }
                    }
                }
                Phase::CheckSync => {
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    let length = self
                        .buffer
                        .as_ref()
                        .ok_or(Error::Invalid("missing DTS14 scan builder"))?
                        .len();
                    let normal_sync = normal_sync(&self.lookahead, self.order);
                    let bits = (length / 2).checked_mul(14).ok_or(Error::Overflow)?;
                    let declared = self
                        .header
                        .ok_or(Error::Invalid("missing DTS14 scan header"))?
                        .frame_bytes
                        .checked_mul(8)
                        .ok_or(Error::Overflow)?;
                    if normal_sync && bits == declared {
                        self.phase = Phase::Geometry;
                    } else {
                        if normal_sync {
                            self.skipped_interior_syncs = self
                                .skipped_interior_syncs
                                .checked_add(1)
                                .ok_or(Error::Overflow)?;
                        }
                        self.append_count = 2;
                        self.append_copied = 0;
                        self.phase = Phase::Append;
                    }
                }
                Phase::Append => {
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    let buffer = self
                        .buffer
                        .as_mut()
                        .ok_or(Error::Invalid("missing DTS14 append builder"))?;
                    if self.append_copied < self.append_count {
                        let next = buffer.len().checked_add(1).ok_or(Error::Overflow)?;
                        limit(next, self.limits.maximum_physical_frame_bytes)?;
                        if next > buffer.capacity() {
                            return Err(Error::Invalid("DTS14 body exceeds reserved capacity"));
                        }
                        buffer
                            .try_push(self.lookahead[self.append_copied])
                            .map_err(|e| e.error)?;
                        self.append_copied += 1;
                    } else {
                        // Fixed<=6-byte shift, separately charged; never a dynamic prefix copy.
                        self.lookahead
                            .copy_within(self.append_count..self.lookahead_len, 0);
                        self.lookahead_len -= self.append_count;
                        self.phase = if self.source_end.is_some() {
                            Phase::Geometry
                        } else {
                            Phase::Scan
                        };
                    }
                }
                Phase::Geometry => {
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    let length = self
                        .buffer
                        .as_ref()
                        .ok_or(Error::Invalid("missing DTS14 geometry builder"))?
                        .len();
                    let header = self
                        .header
                        .ok_or(Error::Invalid("missing DTS14 geometry header"))?;
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
                            || self.position() != end.checked_add(6).ok_or(Error::Overflow)?
                        {
                            return Err(Error::Invalid("DTS14 following-sync mapping"));
                        }
                        Dts14BoundaryEvidence::FollowingNormalSync {
                            physical_offset: end,
                        }
                    };
                    self.facts = Some(Dts14SyncFacts {
                        identity: DtsSourceIdentity {
                            source: self.last_request.source,
                            generation: self.generation(),
                            offset: self.frame_offset,
                        },
                        word_order: self.order,
                        physical_bytes: length,
                        payload_bytes: bits / 8,
                        boundary,
                        skipped_interior_syncs: self.skipped_interior_syncs,
                        header,
                    });
                    self.phase = Phase::Freeze;
                }
                Phase::Freeze => {
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    let buffer = self
                        .buffer
                        .take()
                        .ok_or(Error::Invalid("missing DTS14 freeze builder"))?;
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
                    if self.unpacker.is_none() {
                        if !work.try_charge(1) {
                            return Ok(Step::Yield);
                        }
                        let raw = self
                            .raw
                            .as_ref()
                            .ok_or(Error::Invalid("missing DTS14 frozen source"))?;
                        let facts = self
                            .facts
                            .ok_or(Error::Invalid("missing DTS14 unpack facts"))?;
                        let job = Dts14BitUnpacker::new(
                            &self.pool,
                            facts.identity,
                            raw.clone(),
                            self.order,
                            DtsPackingLimits {
                                maximum_source_bytes: self.limits.maximum_physical_frame_bytes,
                                maximum_output_bytes: self.limits.maximum_canonical_frame_bytes,
                            },
                        )?;
                        self.unpacker = Some(job);
                        self.raw = None;
                    }
                    match self
                        .unpacker
                        .as_mut()
                        .ok_or(Error::Invalid("missing DTS14 unpack job"))?
                        .step(work, cancel)?
                    {
                        Step::Ready(payload) => {
                            let facts = self
                                .facts
                                .ok_or(Error::Invalid("missing DTS14 output facts"))?;
                            if payload.residual_bits() != 0
                                || payload.canonical_bytes().len() != facts.payload_bytes
                            {
                                return Err(Error::Invalid("DTS14 exact-word unpack invariant"));
                            }
                            self.ready = Some(Dts14SyncFrame { payload, facts });
                            self.unpacker = None;
                            self.phase = Phase::Publish;
                        }
                        Step::Yield => return Ok(Step::Yield),
                        Step::NeedInput(_) | Step::End(_) => {
                            return Err(Error::Invalid("DTS14 owned unpack progress"));
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
                        .ok_or(Error::Invalid("missing DTS14 ready envelope"))?;
                    self.completed = self.completed.checked_add(1).ok_or(Error::Overflow)?;
                    self.interval = Some(ready.facts.physical_bytes);
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
                                u64::try_from(ready.facts.physical_bytes)
                                    .map_err(|_| Error::Overflow)?,
                            )
                            .ok_or(Error::Overflow)?;
                        self.lookahead_len = 0;
                        self.phase = Phase::Header;
                    }
                    self.header = None;
                    self.facts = None;
                    self.skipped_interior_syncs = 0;
                    return Ok(Step::Ready(ready));
                }
            }
        }
    }
}

fn limit(requested: usize, maximum: usize) -> Result<()> {
    if requested > maximum {
        Err(Error::Limit { requested, maximum })
    } else {
        Ok(())
    }
}

fn word(source: &[u8], at: usize, order: DtsWordOrder) -> u16 {
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
    prefix >> 4 == ((0x7ffe8001_u64 << 6) | 0x3f)
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

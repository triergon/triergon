//! Resumable Matroska envelope reader, authored from RFC 8794 §§4–6 and
//! RFC 9559 §§5, 10. It collects at most one policy-bounded leaf, never a file.
//!
//! Segment/Cluster structure and leaf storage are incremental. Typed metadata,
//! track configuration, lacing, packet clocks and Cues seek policy are separate
//! format operations; an envelope event does not assert those have been decoded.
use crate::{Error, Limits, Result, ebml, storage::SharedBytes};
use safe_media_core::{
    Budget, BudgetVec, Bytes, InputCursor, InputRequest, InputToken, InputWindow,
    SourceCapabilities, SourceFailure, SourceId, Step, WorkBudget,
};
use std::sync::atomic::AtomicBool;

const SEGMENT: u64 = 0x18538067;
const CLUSTER: u64 = 0x1f43b675;
const EBML: u64 = 0x1a45dfa3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ElementHeader {
    pub id: u64,
    pub size: Option<u64>,
    pub start: u64,
    pub data: u64,
    pub generation: u64,
}
impl ElementHeader {
    fn end(self) -> Result<Option<u64>> {
        self.size
            .map(|size| self.data.checked_add(size).ok_or(Error::Overflow))
            .transpose()
    }
}

#[derive(Clone, Debug)]
pub enum ElementEvent {
    Start(ElementHeader),
    Leaf {
        header: ElementHeader,
        data: SharedBytes,
    },
    Skipped(ElementHeader),
    End {
        id: u64,
        offset: u64,
        generation: u64,
    },
}
#[derive(Debug)]
pub enum ParseStep<T = ElementEvent> {
    Event(T),
    NeedInput(InputRequest),
    Yield,
    Backpressure { requested: usize, available: usize },
    Finished,
}

#[derive(Debug)]
struct HeaderState {
    bytes: [u8; 12],
    have: usize,
    target: usize,
    id_width: usize,
    size_width: usize,
    start: u64,
}
impl HeaderState {
    fn new(start: u64) -> Self {
        Self {
            bytes: [0; 12],
            have: 0,
            target: 1,
            id_width: 0,
            size_width: 0,
            start,
        }
    }
}
#[derive(Debug)]
struct BodyState {
    header: ElementHeader,
    remaining: u64,
    collect: bool,
    allocated: bool,
    buffer: Option<BudgetVec<u8>>,
    span: Option<Bytes>,
    copied: usize,
}
#[derive(Clone, Copy, Debug)]
struct Master {
    id: u64,
    start: u64,
    end: Option<u64>,
}

/// A source location captured by this parser at a validated Cluster header.
/// It supplies envelope context only; a packet consumer still owns track/Cues
/// validation, previous-random-access selection and all-track preroll policy.
#[derive(Clone, Debug)]
pub struct ClusterCheckpoint {
    pool: Budget,
    parser: u64,
    source: SourceId,
    offset: u64,
    segment: Master,
}
impl ClusterCheckpoint {
    pub fn offset(&self) -> u64 {
        self.offset
    }
}
#[derive(Debug)]
enum Phase {
    Header(HeaderState),
    Body(BodyState),
    Between,
    Eof,
    Done,
}

/// Each call resumes exactly where the previous call stopped. Storage rejection
/// retains the header/body builder, including a complete leaf awaiting freeze.
#[derive(Debug)]
pub struct ElementReader {
    input: InputCursor,
    parser: u64,
    limits: Limits,
    phase: Phase,
    segment: Option<Master>,
    cluster: Option<Master>,
    pending_header: Option<ElementHeader>,
    seen_ebml: bool,
    seen_segment: bool,
    elements_left: u64,
    scan_left: u64,
    failed: bool,
    expected_cluster: bool,
}
impl ElementReader {
    pub fn new(
        pool: &Budget,
        source: SourceId,
        capabilities: SourceCapabilities,
        limits: Limits,
        maximum_window: usize,
    ) -> Result<Self> {
        let input = InputCursor::new(pool, source, capabilities, maximum_window)?;
        let parser = input.generation();
        Ok(Self {
            input,
            parser,
            limits,
            phase: Phase::Between,
            segment: None,
            cluster: None,
            pending_header: None,
            seen_ebml: false,
            seen_segment: false,
            elements_left: limits.elements,
            scan_left: limits.scan_bytes,
            failed: false,
            expected_cluster: false,
        })
    }
    pub fn budget(&self) -> &Budget {
        self.input.budget()
    }
    pub fn position(&self) -> u64 {
        self.input.position()
    }
    pub fn generation(&self) -> u64 {
        self.input.generation()
    }
    pub fn request(&self) -> InputRequest {
        self.input.request()
    }
    pub fn provide(&mut self, window: &InputWindow) -> Result<()> {
        Ok(self.input.provide(window)?)
    }
    pub fn finish(&mut self, source: SourceId, token: InputToken) -> Result<()> {
        Ok(self.input.finish(source, token)?)
    }
    pub fn fail(
        &mut self,
        source: SourceId,
        token: InputToken,
        failure: SourceFailure,
    ) -> Result<()> {
        Ok(self.input.fail(source, token, failure)?)
    }
    pub fn cluster_checkpoint(&self) -> Option<ClusterCheckpoint> {
        Some(ClusterCheckpoint {
            pool: self.input.budget().clone(),
            parser: self.parser,
            source: self.input.source(),
            offset: self.cluster?.start,
            segment: self.segment?,
        })
    }
    /// Explicit logical range seek. Generation changes invalidate every earlier
    /// supplied range; retained earlier-generation leaf payloads stay charged.
    pub fn reset_to_cluster(&mut self, checkpoint: &ClusterCheckpoint) -> Result<InputRequest> {
        if !checkpoint.pool.same_pool(self.input.budget()) {
            return Err(safe_media_core::Error::ForeignPool.into());
        }
        if checkpoint.parser != self.parser || checkpoint.source != self.input.source() {
            return Err(safe_media_core::Error::SourceMismatch.into());
        }
        if !self.input.capabilities().random_access {
            return Err(Error::Unsupported("random-access input"));
        }
        let request = self.input.reset(checkpoint.offset)?;
        self.phase = Phase::Between;
        self.segment = Some(checkpoint.segment);
        self.cluster = None;
        self.pending_header = None;
        self.seen_ebml = true;
        self.seen_segment = true;
        self.elements_left = self.limits.elements;
        self.scan_left = self.limits.scan_bytes;
        self.failed = false;
        self.expected_cluster = true;
        Ok(request)
    }
    /// Reset the logical document. Caller-retained earlier events remain valid
    /// and charged; their old range tokens are rejected by the core cursor.
    pub fn reset(&mut self) -> Result<InputRequest> {
        let request = self.input.reset(0)?;
        self.phase = Phase::Between;
        self.segment = None;
        self.cluster = None;
        self.pending_header = None;
        self.seen_ebml = false;
        self.seen_segment = false;
        self.elements_left = self.limits.elements;
        self.scan_left = self.limits.scan_bytes;
        self.failed = false;
        self.expected_cluster = false;
        Ok(request)
    }
    fn ended(&self, master: Master) -> ElementEvent {
        ElementEvent::End {
            id: master.id,
            offset: self.input.position(),
            generation: self.generation(),
        }
    }
    fn check_parent(&self, header: ElementHeader, end: Option<u64>) -> Result<()> {
        if end.is_some_and(|end| header.data > end) {
            return Err(Error::Invalid("element exceeds parent"));
        }
        if let (Some(parent), Some(end)) = (end, header.end()?) {
            if end > parent {
                return Err(Error::Invalid("element exceeds parent"));
            }
        }
        Ok(())
    }
    fn begin(&mut self, header: ElementHeader) -> Result<Option<ElementEvent>> {
        if self.expected_cluster {
            if header.id != CLUSTER {
                return Err(Error::Invalid("range no longer starts at Cluster"));
            }
            self.expected_cluster = false;
        }
        if self.seen_segment && self.segment.is_none() {
            return Err(Error::Unsupported("element after finite Segment"));
        }
        self.check_parent(header, self.segment.and_then(|m| m.end))?;
        if header.id == CLUSTER || is_level_one(header.id) {
            if let Some(cluster) = self.cluster {
                if cluster.end.is_some() {
                    return Err(Error::Invalid("level-one element in finite Cluster"));
                }
                self.cluster = None;
                self.pending_header = Some(header);
                return Ok(Some(ElementEvent::End {
                    id: cluster.id,
                    offset: header.start,
                    generation: self.generation(),
                }));
            }
        }
        self.check_parent(header, self.cluster.and_then(|m| m.end))?;
        if !self.seen_ebml {
            if header.id != EBML {
                return Err(Error::Invalid("missing EBML header"));
            }
            self.seen_ebml = true;
        } else if !self.seen_segment && header.id == SEGMENT {
            self.seen_segment = true;
            self.segment = Some(Master {
                id: SEGMENT,
                start: header.start,
                end: header.end()?,
            });
            return Ok(Some(ElementEvent::Start(header)));
        } else if !self.seen_segment && !matches!(header.id, 0xec | 0xbf) {
            return Err(Error::Invalid("element before Segment"));
        } else if header.id == SEGMENT || header.id == EBML {
            return Err(Error::Unsupported("multiple EBML documents/Segments"));
        }
        if header.id == CLUSTER {
            if self.segment.is_none() {
                return Err(Error::Invalid("Cluster outside Segment"));
            }
            self.cluster = Some(Master {
                id: CLUSTER,
                start: header.start,
                end: header.end()?,
            });
            return Ok(Some(ElementEvent::Start(header)));
        }
        let size = header
            .size
            .ok_or(Error::Invalid("unknown non-master size"))?;
        let (collect, maximum) = match header.id {
            EBML | 0x1549a966 | 0x1654ae6b | 0x1c53bb6b | 0x114d9b74 | 0x1043a770 | 0x1254c367 => {
                (true, self.limits.metadata_bytes)
            }
            0x1941a469 => (
                true,
                self.limits
                    .metadata_bytes
                    .checked_add(self.limits.attachment_bytes)
                    .ok_or(Error::Overflow)?,
            ),
            0xa3 | 0xa0 if self.cluster.is_some() => (true, self.limits.block_bytes),
            0xe7 if self.cluster.is_some() => (true, 8),
            _ => (false, usize::MAX),
        };
        if collect && size > maximum as u64 {
            return Err(Error::Limit("element bytes"));
        }
        if size > self.scan_left {
            return Err(Error::Limit("operation read bytes"));
        }
        self.phase = Phase::Body(BodyState {
            header,
            remaining: size,
            collect,
            allocated: false,
            buffer: Some(BudgetVec::new(self.input.budget())),
            span: None,
            copied: 0,
        });
        Ok(None)
    }
    pub fn step(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<ParseStep> {
        // Empty read checks sticky transport/cancellation without consuming bytes
        // or depending on an OS read. Even a completed/pending event observes it.
        self.input.read_into(&mut [], work, cancel)?;
        if self.failed {
            return Err(Error::Invalid("reader requires explicit reset after error"));
        }
        let result = self.advance(work, cancel);
        match result {
            Err(Error::Backpressure {
                requested,
                available,
            }) => Ok(ParseStep::Backpressure {
                requested,
                available,
            }),
            Err(error) => {
                self.failed = true;
                Err(error)
            }
            other => other,
        }
    }
    fn advance(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<ParseStep> {
        loop {
            // Cancellation also runs between every bounded inner operation.
            self.input.read_into(&mut [], work, cancel)?;
            match &mut self.phase {
                Phase::Done => return Ok(ParseStep::Finished),
                Phase::Eof => {
                    if let Some(master) = self.cluster.take() {
                        return Ok(ParseStep::Event(self.ended(master)));
                    }
                    if let Some(master) = self.segment.take() {
                        return Ok(ParseStep::Event(self.ended(master)));
                    }
                    self.phase = Phase::Done;
                }
                Phase::Between => {
                    if let Some(header) = self.pending_header.take() {
                        if let Some(event) = self.begin(header)? {
                            return Ok(ParseStep::Event(event));
                        }
                        continue;
                    }
                    if let Some(master) = self.cluster {
                        if master.end == Some(self.position()) {
                            self.cluster = None;
                            return Ok(ParseStep::Event(self.ended(master)));
                        }
                    }
                    if let Some(master) = self.segment {
                        if master.end == Some(self.position()) {
                            self.segment = None;
                            return Ok(ParseStep::Event(self.ended(master)));
                        }
                    }
                    self.phase = Phase::Header(HeaderState::new(self.position()));
                }
                Phase::Header(state) => {
                    if state.have < state.target {
                        let old = self.input.position();
                        match self.input.read_into(
                            &mut state.bytes[state.have..state.target],
                            work,
                            cancel,
                        )? {
                            Step::Ready(count) => {
                                state.have += count;
                            }
                            Step::NeedInput(request) => return Ok(ParseStep::NeedInput(request)),
                            Step::Yield => return Ok(ParseStep::Yield),
                            Step::End(_) => {
                                if state.have != 0
                                    || !self.seen_segment
                                    || self.segment.is_some_and(|m| {
                                        m.end.is_some_and(|end| self.input.position() < end)
                                    })
                                    || self.cluster.is_some_and(|m| {
                                        m.end.is_some_and(|end| self.input.position() < end)
                                    })
                                {
                                    return Err(Error::Truncated);
                                }
                                self.phase = Phase::Eof;
                                continue;
                            }
                        }
                        self.scan_left = self
                            .scan_left
                            .checked_sub(self.input.position() - old)
                            .ok_or(Error::Limit("operation read bytes"))?;
                        continue;
                    }
                    if state.id_width == 0 {
                        if state.bytes[0] == 0 {
                            return Err(Error::Invalid("zero VINT marker"));
                        }
                        state.id_width = state.bytes[0].leading_zeros() as usize + 1;
                        if state.id_width > 4 {
                            return Err(Error::Invalid("element ID width"));
                        }
                        state.target = state.id_width + 1;
                        continue;
                    }
                    if state.size_width == 0 {
                        let first = state.bytes[state.id_width];
                        if first == 0 {
                            return Err(Error::Invalid("zero VINT marker"));
                        }
                        state.size_width = first.leading_zeros() as usize + 1;
                        state.target = state.id_width + state.size_width;
                        continue;
                    }
                    if !work.try_charge(1) {
                        return Ok(ParseStep::Yield);
                    }
                    let (id, _, _) = ebml::vint(&state.bytes[..state.id_width], true)?;
                    let (size, _, unknown) =
                        ebml::vint(&state.bytes[state.id_width..state.target], false)?;
                    let header = ElementHeader {
                        id,
                        size: (!unknown).then_some(size),
                        start: state.start,
                        data: self.input.position(),
                        generation: self.input.generation(),
                    };
                    self.elements_left = self
                        .elements_left
                        .checked_sub(1)
                        .ok_or(Error::Limit("element count"))?;
                    self.phase = Phase::Between;
                    if let Some(event) = self.begin(header)? {
                        return Ok(ParseStep::Event(event));
                    }
                }
                Phase::Body(body) => {
                    if body.collect && !body.allocated {
                        if !work.try_charge(1) {
                            return Ok(ParseStep::Yield);
                        }
                        let size = usize::try_from(body.remaining)
                            .map_err(|_| Error::Limit("address space"))?;
                        body.buffer
                            .as_mut()
                            .ok_or(Error::Invalid("missing body builder"))?
                            .try_reserve_exact(size)?;
                        body.allocated = true;
                    }
                    if let Some(span) = &body.span {
                        let count = work.take((span.len() - body.copied) as u64) as usize;
                        if count == 0 {
                            return Ok(ParseStep::Yield);
                        }
                        body.buffer
                            .as_mut()
                            .ok_or(Error::Invalid("missing body builder"))?
                            .try_extend_from_slice(&span[body.copied..body.copied + count])?;
                        body.copied += count;
                        if body.copied == span.len() {
                            body.span = None;
                            body.copied = 0;
                        }
                        continue;
                    }
                    if body.remaining == 0 {
                        if !work.try_charge(1) {
                            return Ok(ParseStep::Yield);
                        }
                        let header = body.header;
                        let event = if body.collect {
                            let buffer = body
                                .buffer
                                .take()
                                .ok_or(Error::Invalid("missing body builder"))?;
                            let bytes = match buffer.freeze() {
                                Ok(bytes) => bytes,
                                Err(error) => {
                                    body.buffer = Some(error.buffer);
                                    return Err(error.error.into());
                                }
                            };
                            ElementEvent::Leaf {
                                header,
                                data: SharedBytes::from_core(bytes),
                            }
                        } else {
                            ElementEvent::Skipped(header)
                        };
                        // Only a collected Cluster block ends this envelope
                        // scan phase. A skipped Block ID cannot refill it.
                        if body.collect && matches!(header.id, 0xa0 | 0xa3) {
                            self.scan_left = self.limits.scan_bytes;
                        }
                        self.phase = Phase::Between;
                        return Ok(ParseStep::Event(event));
                    }
                    let maximum =
                        usize::try_from(body.remaining.min(8192)).map_err(|_| Error::Overflow)?;
                    let old = self.input.position();
                    match self.input.read_span(maximum, work, cancel)? {
                        Step::Ready(bytes) => {
                            body.remaining -= bytes.len() as u64;
                            if body.collect {
                                body.span = Some(bytes);
                            }
                        }
                        Step::NeedInput(request) => return Ok(ParseStep::NeedInput(request)),
                        Step::Yield => return Ok(ParseStep::Yield),
                        Step::End(_) => return Err(Error::Truncated),
                    }
                    self.scan_left = self
                        .scan_left
                        .checked_sub(self.input.position() - old)
                        .ok_or(Error::Limit("operation read bytes"))?;
                }
            }
        }
    }
}
fn is_level_one(id: u64) -> bool {
    matches!(
        id,
        0x1549a966 | 0x1654ae6b | 0x1c53bb6b | 0x114d9b74 | 0x1941a469 | 0x1043a770 | 0x1254c367
    )
}

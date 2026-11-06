//! Resumable Matroska envelope reader, authored from RFC 8794 §§4–6 and
//! RFC 9559 §§5, 10. It collects at most one policy-bounded leaf, never a file.
//!
//! Segment/Cluster structure and leaf storage are incremental. Typed metadata,
//! track configuration, lacing, packet clocks and Cues seek policy are separate
//! format operations; an envelope event does not assert those have been decoded.
use crate::{
    Error, Limits, Result, ebml,
    storage::{PayloadOwnership, SharedBytes},
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, BudgetVec, Bytes, InputCursor, InputRequest, InputToken, InputWindow,
    SourceCapabilities, SourceFailure, SourceId, Step, WorkBudget,
};

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
    direct: Option<Bytes>,
    target: usize,
    copied: usize,
    typed_admitted: bool,
    transfer: BodyTransfer,
    scan: ScanBody,
}
/// Exclusive ownership transitions in the existing span/direct slots.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BodyTransfer {
    Copy,
    Borrow,
    Ready,
}

// One parser and one set of work/error/byte rules. Only the leaf delivery
// representation is selected at compile time; the public event API is intact.
trait EnvelopeDelivery {
    type Event;
    const RETAIN_LEAF: bool;
    fn start(header: ElementHeader) -> Self::Event;
    fn end(id: u64, offset: u64, generation: u64) -> Self::Event;
    fn skipped(header: ElementHeader) -> Self::Event;
    fn leaf(body: &mut BodyState, bytes: Bytes) -> Self::Event;
    fn retained_leaf(body: &mut BodyState) -> Result<Self::Event> {
        let bytes = body
            .direct
            .take()
            .ok_or(Error::Invalid("missing compact leaf owner"))?;
        Ok(Self::leaf(body, bytes))
    }
}
struct PublicDelivery;
impl EnvelopeDelivery for PublicDelivery {
    type Event = ElementEvent;
    const RETAIN_LEAF: bool = false;
    fn start(header: ElementHeader) -> Self::Event {
        ElementEvent::Start(header)
    }
    fn end(id: u64, offset: u64, generation: u64) -> Self::Event {
        ElementEvent::End {
            id,
            offset,
            generation,
        }
    }
    fn skipped(header: ElementHeader) -> Self::Event {
        ElementEvent::Skipped(header)
    }
    fn leaf(body: &mut BodyState, bytes: Bytes) -> Self::Event {
        ElementEvent::Leaf {
            header: body.header,
            data: SharedBytes::from_core(bytes),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScanBody {
    Off,
    FirstSpan,
    Prefix,
    Drain,
    Complete,
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
    pub(crate) fn segment_end(&self) -> Option<u64> {
        self.segment.end
    }
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
    payload_ownership: PayloadOwnership,
}
/// The state-history job's leaf admission: every leaf is admitted for one work
/// unit; a SimpleBlock is skipped unless it is the replay boundary or the
/// referenced Block.
#[derive(Clone, Copy, Debug)]
pub(crate) struct EpochAdmission {
    pub keep: [Option<u64>; 2],
}
/// Select whether the typed caller admits leaves after a pause or the
/// envelope applies a preselected admission rule in place.
#[derive(Clone, Copy, Debug)]
enum Admit {
    Never,
    Pause,
    OrdinaryBlocks,
    Epoch(EpochAdmission),
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
            payload_ownership: PayloadOwnership::OwnedExact,
        })
    }
    pub(crate) fn set_payload_ownership(&mut self, policy: PayloadOwnership) {
        self.payload_ownership = policy;
    }
    pub(crate) fn payload_ownership(&self) -> PayloadOwnership {
        self.payload_ownership
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
            return Err(triergon_core::Error::ForeignPool.into());
        }
        if checkpoint.parser != self.parser || checkpoint.source != self.input.source() {
            return Err(triergon_core::Error::SourceMismatch.into());
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
    fn ended<D: EnvelopeDelivery>(&self, master: Master) -> D::Event {
        D::end(master.id, self.position(), self.generation())
    }
    fn check_parent(&self, header: ElementHeader, end: Option<u64>) -> Result<()> {
        if end.is_some_and(|end| header.data > end) {
            return Err(Error::Invalid("element exceeds parent"));
        }
        if let (Some(parent), Some(end)) = (end, header.end()?)
            && end > parent
        {
            return Err(Error::Invalid("element exceeds parent"));
        }
        Ok(())
    }
    fn begin<D: EnvelopeDelivery>(&mut self, header: ElementHeader) -> Result<Option<D::Event>> {
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
        if (header.id == CLUSTER || is_level_one(header.id))
            && let Some(cluster) = self.cluster
        {
            if cluster.end.is_some() {
                return Err(Error::Invalid("level-one element in finite Cluster"));
            }
            self.cluster = None;
            self.pending_header = Some(header);
            return Ok(Some(D::end(cluster.id, header.start, self.generation())));
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
            return Ok(Some(D::start(header)));
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
            return Ok(Some(D::start(header)));
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
            buffer: None,
            span: None,
            direct: None,
            target: 0,
            copied: 0,
            typed_admitted: false,
            transfer: BodyTransfer::Copy,
            scan: ScanBody::Off,
        });
        Ok(None)
    }
    pub fn step(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<ParseStep> {
        self.step_mode::<PublicDelivery>(work, cancel, Admit::Never)
    }
    fn step_mode<D: EnvelopeDelivery>(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        admit: Admit,
    ) -> Result<ParseStep<D::Event>> {
        // Empty read checks sticky transport/cancellation without consuming bytes
        // or depending on an OS read. Even a completed/pending event observes it.
        self.input.read_into(&mut [], work, cancel)?;
        self.step_after_input_health::<D>(work, cancel, admit)
    }

    // The typed controller checks source health once on entry and propagates
    // every subsequent input error. The envelope still checks its own latch
    // and cancellation at each bounded operation.
    fn step_after_input_health<D: EnvelopeDelivery>(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        admit: Admit,
    ) -> Result<ParseStep<D::Event>> {
        if self.failed {
            return Err(Error::Invalid("reader requires explicit reset after error"));
        }
        let result = self.advance::<D>(work, cancel, admit);
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
    /// Checks cancellation between bounded operations after this uninterrupted
    /// call has checked sticky input health. No caller may suspend, supply input,
    /// or recover from an input error between that check and this guard.
    #[inline]
    pub(crate) fn check_inner_cancel(
        &mut self,
        _work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<()> {
        Self::check_input_cancel(&mut self.input, cancel)
    }
    #[inline]
    fn check_input_cancel(input: &mut InputCursor, cancel: &AtomicBool) -> Result<()> {
        if cancel.load(Ordering::Relaxed) {
            // Preserve the core's sticky cancellation and window release. Using
            // the current token preserves any earlier source failure's priority;
            // do not re-load a flag which another thread could already clear.
            input.fail(input.source(), input.token(), SourceFailure::Cancelled)?;
            return Err(Error::Cancelled);
        }
        Ok(())
    }
    #[inline]
    fn accept_header<D: EnvelopeDelivery>(
        &mut self,
        header: ElementHeader,
    ) -> Result<Option<D::Event>> {
        self.elements_left = self
            .elements_left
            .checked_sub(1)
            .ok_or(Error::Limit("element count"))?;
        self.phase = Phase::Between;
        self.begin::<D>(header)
    }

    #[inline]
    fn take_buffered_header(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<ElementHeader>> {
        let bytes = self.input.buffered();
        let Some((id_width, size_width)) = buffered_header_widths(bytes) else {
            return Ok(None);
        };
        let width = id_width + size_width;
        if self.scan_left < width as u64 || work.remaining() <= width as u64 {
            return Ok(None);
        }

        // The complete header and its final work unit fit. Parse in place and
        // retain the same byte debit and cancellation boundary as the scratch path.
        let (id, _, _) = ebml::vint(&bytes[..id_width], true)?;
        let (size, _, unknown) = ebml::vint(&bytes[id_width..width], false)?;
        let start = self.input.position();
        if !matches!(self.input.read_discard(width, work, cancel)?, Step::Ready(n) if n == width) {
            return Err(Error::Invalid("buffered header length changed"));
        }
        self.scan_left -= width as u64;
        Self::check_input_cancel(&mut self.input, cancel)?;
        if !work.try_charge(1) {
            return Err(Error::Invalid("buffered header work changed"));
        }
        Ok(Some(ElementHeader {
            id,
            size: (!unknown).then_some(size),
            start,
            data: self.input.position(),
            generation: self.input.generation(),
        }))
    }

    fn advance<D: EnvelopeDelivery>(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        admit: Admit,
    ) -> Result<ParseStep<D::Event>> {
        let pause_for_admission = !matches!(admit, Admit::Never);
        'advance: loop {
            // Cancellation also runs between every bounded inner operation.
            self.check_inner_cancel(work, cancel)?;
            match &mut self.phase {
                Phase::Done => return Ok(ParseStep::Finished),
                Phase::Eof => {
                    if let Some(master) = self.cluster.take() {
                        return Ok(ParseStep::Event(self.ended::<D>(master)));
                    }
                    if let Some(master) = self.segment.take() {
                        return Ok(ParseStep::Event(self.ended::<D>(master)));
                    }
                    self.phase = Phase::Done;
                }
                Phase::Between => {
                    if let Some(header) = self.pending_header.take() {
                        if let Some(event) = self.begin::<D>(header)? {
                            return Ok(ParseStep::Event(event));
                        }
                        continue;
                    }
                    if let Some(master) = self.cluster
                        && master.end == Some(self.position())
                    {
                        self.cluster = None;
                        return Ok(ParseStep::Event(self.ended::<D>(master)));
                    }
                    if let Some(master) = self.segment
                        && master.end == Some(self.position())
                    {
                        self.segment = None;
                        return Ok(ParseStep::Event(self.ended::<D>(master)));
                    }
                    if let Some(header) = self.take_buffered_header(work, cancel)? {
                        if let Some(event) = self.accept_header::<D>(header)? {
                            return Ok(ParseStep::Event(event));
                        }
                    } else {
                        self.phase = Phase::Header(HeaderState::new(self.position()));
                    }
                }
                Phase::Header(state) => {
                    if state.have == 0 && state.id_width == 0 {
                        // A complete, well-formed header already buffered needs
                        // no separate width discovery reads. The same bytes, work
                        // and scan bounds are consumed by the one read below;
                        // malformed or partial headers keep the byte-wise path.
                        if let Some((id_width, size_width)) =
                            buffered_header_widths(self.input.buffered())
                                .filter(|(i, s)| self.scan_left >= (i + s) as u64)
                        {
                            state.id_width = id_width;
                            state.size_width = size_width;
                            state.target = id_width + size_width;
                        }
                    }
                    let header = loop {
                        if state.have < state.target {
                            let old = self.input.position();
                            // The known ID/size width bounds this read to a header;
                            // InputCursor also caps it to available bytes and work.
                            // Preserve typed scanning's original failure position:
                            // at most one byte can expose an exhausted scan budget.
                            let target = if pause_for_admission {
                                let scan = usize::try_from(self.scan_left).unwrap_or(usize::MAX);
                                state
                                    .target
                                    .min(state.have.saturating_add(scan.saturating_add(1)))
                            } else {
                                state.target
                            };
                            match self.input.read_into(
                                &mut state.bytes[state.have..target],
                                work,
                                cancel,
                            )? {
                                Step::Ready(count) => {
                                    state.have += count;
                                }
                                Step::NeedInput(mut request) => {
                                    request.maximum =
                                        request.maximum.min(state.target - state.have);
                                    return Ok(ParseStep::NeedInput(request));
                                }
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
                                    continue 'advance;
                                }
                            }
                            self.scan_left = self
                                .scan_left
                                .checked_sub(self.input.position() - old)
                                .ok_or(Error::Limit("operation read bytes"))?;
                            Self::check_input_cancel(&mut self.input, cancel)?;
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
                            Self::check_input_cancel(&mut self.input, cancel)?;
                            continue;
                        }
                        if state.size_width == 0 {
                            let first = state.bytes[state.id_width];
                            if first == 0 {
                                return Err(Error::Invalid("zero VINT marker"));
                            }
                            state.size_width = first.leading_zeros() as usize + 1;
                            state.target = state.id_width + state.size_width;
                            Self::check_input_cancel(&mut self.input, cancel)?;
                            continue;
                        }
                        if !work.try_charge(1) {
                            return Ok(ParseStep::Yield);
                        }
                        let (id, _, _) = ebml::vint(&state.bytes[..state.id_width], true)?;
                        let (size, _, unknown) =
                            ebml::vint(&state.bytes[state.id_width..state.target], false)?;
                        break ElementHeader {
                            id,
                            size: (!unknown).then_some(size),
                            start: state.start,
                            data: self.input.position(),
                            generation: self.input.generation(),
                        };
                    };
                    if let Some(event) = self.accept_header::<D>(header)? {
                        return Ok(ParseStep::Event(event));
                    }
                }
                Phase::Body(body) => {
                    if body.transfer == BodyTransfer::Ready {
                        let event = D::retained_leaf(body)?;
                        if !D::RETAIN_LEAF {
                            self.phase = Phase::Between;
                        }
                        return Ok(ParseStep::Event(event));
                    }
                    if pause_for_admission && !body.typed_admitted && !body.allocated {
                        let ordinary_block = matches!(admit, Admit::OrdinaryBlocks)
                            && body.collect
                            && matches!(body.header.id, 0xa0 | 0xa3);
                        if !ordinary_block && !matches!(admit, Admit::Epoch(_)) {
                            return Ok(ParseStep::Yield);
                        }
                        if !work.try_charge(1) {
                            return Ok(ParseStep::Yield);
                        }
                        if let Admit::Epoch(admission) = admit
                            && body.header.id == 0xa3
                            && !admission.keep.contains(&Some(body.header.start))
                        {
                            body.collect = false;
                        }
                        body.typed_admitted = true;
                        continue;
                    }
                    // Private scan notifications use the existing typed bridge.
                    // The actual span stays in this body's ordinary owner slots.
                    if matches!(body.scan, ScanBody::Prefix | ScanBody::Complete) {
                        return Ok(ParseStep::Yield);
                    }
                    if body.collect && !body.allocated {
                        if !work.try_charge(1) {
                            return Ok(ParseStep::Yield);
                        }
                        body.target = usize::try_from(body.remaining)
                            .map_err(|_| Error::Limit("address space"))?;
                        // Admit the exact output capacity before requesting data.
                        // A complete input owner of that capacity may become the
                        // leaf directly; partial/hidden owners use the builder.
                        body.allocated = true;
                    }
                    if let Some(span) = &body.span {
                        let count = work.take((span.len() - body.copied) as u64) as usize;
                        if count == 0 {
                            return Ok(ParseStep::Yield);
                        }
                        if body.transfer == BodyTransfer::Borrow {
                            // Commit the same bounded byte prefix as OwnedExact.
                            // The checked immutable view stays unpublished until
                            // all byte work and the final publication unit finish.
                            span.get(body.copied..body.copied + count)
                                .ok_or(Error::Invalid("borrowed body commit range"))?;
                        } else {
                            // Direct and discarded bodies need no empty pool
                            // owner. Construct the existing exact builder only
                            // after the original bounded copy work is available.
                            let buffer = body
                                .buffer
                                .get_or_insert_with(|| BudgetVec::new(self.input.budget()));
                            // The span and old builder remain owned on rejection.
                            // No allocation/copy occurs when no byte work is left.
                            if buffer.capacity() < body.target {
                                buffer.try_reserve_exact(body.target - buffer.len())?;
                            }
                            buffer
                                .try_extend_from_slice(&span[body.copied..body.copied + count])?;
                        }
                        body.copied += count;
                        if body.copied == span.len() {
                            if body.transfer == BodyTransfer::Borrow {
                                body.direct = body.span.take();
                                body.transfer = BodyTransfer::Copy;
                            } else {
                                body.span = None;
                            }
                            body.copied = 0;
                        }
                        continue;
                    }
                    if body.remaining == 0 {
                        if !work.try_charge(1) {
                            return Ok(ParseStep::Yield);
                        }
                        let header = body.header;
                        if body.scan == ScanBody::Drain {
                            // Same boundary as the collected, validated block;
                            // the controller still owns the whole-operation
                            // position-derived scan allowance. Never emit Skipped.
                            self.scan_left = self.limits.scan_bytes;
                            body.scan = ScanBody::Complete;
                            return Ok(ParseStep::Yield);
                        }
                        let event = if body.collect {
                            let bytes = if let Some(bytes) = body.direct.take() {
                                bytes
                            } else {
                                let buffer = match body.buffer.take() {
                                    Some(buffer) => buffer,
                                    // Empty collected leaves have no copy phase.
                                    // Freeze still follows the original one-unit
                                    // publication check and recovers on pressure.
                                    None if body.target == 0 => BudgetVec::new(self.input.budget()),
                                    None => return Err(Error::Invalid("missing body builder")),
                                };
                                match buffer.freeze() {
                                    Ok(bytes) => bytes,
                                    Err(error) => {
                                        body.buffer = Some(error.buffer);
                                        return Err(error.error.into());
                                    }
                                }
                            };
                            D::leaf(body, bytes)
                        } else {
                            D::skipped(header)
                        };
                        // Only a collected Cluster block ends this envelope
                        // scan phase. A skipped Block ID cannot refill it.
                        if body.collect && matches!(header.id, 0xa0 | 0xa3) {
                            self.scan_left = self.limits.scan_bytes;
                        }
                        if body.collect && D::RETAIN_LEAF {
                            // The old public delivery dropped the exhausted body
                            // here. Keep only its already-owned frozen Bytes;
                            // discard the empty builder at that same boundary.
                            body.buffer = None;
                            body.span = None;
                        } else {
                            self.phase = Phase::Between;
                        }
                        return Ok(ParseStep::Event(event));
                    }
                    let maximum =
                        usize::try_from(body.remaining.min(8192)).map_err(|_| Error::Overflow)?;
                    let old = self.input.position();
                    if !body.collect {
                        match self.input.read_discard(maximum, work, cancel)? {
                            Step::Ready(count) => body.remaining -= count as u64,
                            Step::NeedInput(mut request) => {
                                request.maximum = request.maximum.min(maximum);
                                return Ok(ParseStep::NeedInput(request));
                            }
                            Step::Yield => return Ok(ParseStep::Yield),
                            Step::End(_) => return Err(Error::Truncated),
                        }
                        self.scan_left = self
                            .scan_left
                            .checked_sub(self.input.position() - old)
                            .ok_or(Error::Limit("operation read bytes"))?;
                        continue;
                    }
                    match self.input.read_span(maximum, work, cancel)? {
                        Step::Ready(bytes) => {
                            body.remaining -= bytes.len() as u64;
                            if body.collect {
                                if body.remaining == 0
                                    && bytes.len() == body.target
                                    && bytes.retained_capacity() == body.target
                                {
                                    // Equal visible length and full capacity
                                    // exclude a hidden prefix/suffix. This is
                                    // exactly the old leaf's capacity target.
                                    body.direct = Some(bytes);
                                } else {
                                    body.transfer = if body.remaining == 0
                                        && bytes.len() == body.target
                                        && self.payload_ownership == PayloadOwnership::SharedBounded
                                        && matches!(body.header.id, 0xa0 | 0xa3)
                                    {
                                        BodyTransfer::Borrow
                                    } else {
                                        BodyTransfer::Copy
                                    };
                                    body.span = Some(bytes);
                                }
                            }
                        }
                        Step::NeedInput(mut request) => {
                            request.maximum = request.maximum.min(maximum);
                            return Ok(ParseStep::NeedInput(request));
                        }
                        Step::Yield => return Ok(ParseStep::Yield),
                        Step::End(_) => return Err(Error::Truncated),
                    }
                    self.scan_left = self
                        .scan_left
                        .checked_sub(self.input.position() - old)
                        .ok_or(Error::Limit("operation read bytes"))?;
                    if body.scan == ScanBody::FirstSpan {
                        body.scan = ScanBody::Prefix;
                    }
                }
            }
        }
    }
}
/// ID and size VINT widths of a header wholly present in `bytes`, if valid.
#[inline]
fn buffered_header_widths(bytes: &[u8]) -> Option<(usize, usize)> {
    let first = *bytes.first()?;
    if first == 0 {
        return None;
    }
    let id_width = first.leading_zeros() as usize + 1;
    if id_width > 4 {
        return None;
    }
    let size = *bytes.get(id_width)?;
    if size == 0 {
        return None;
    }
    let size_width = size.leading_zeros() as usize + 1;
    (bytes.len() >= id_width + size_width).then_some((id_width, size_width))
}
fn is_level_one(id: u64) -> bool {
    matches!(
        id,
        0x1549a966 | 0x1654ae6b | 0x1c53bb6b | 0x114d9b74 | 0x1941a469 | 0x1043a770 | 0x1254c367
    )
}

#[path = "typed/envelope_bridge.rs"]
pub(crate) mod typed_bridge;

#[cfg(test)]
#[path = "incremental_discard_guards.rs"]
mod discard_guards;

#[cfg(test)]
#[path = "compact_envelope_regressions.rs"]
mod compact_envelope_regressions;

#[cfg(test)]
#[path = "lazy_body_regressions.rs"]
pub(crate) mod lazy_body_regressions;

#[cfg(test)]
#[path = "header_loop_regressions.rs"]
mod header_loop_regressions;

#[cfg(test)]
#[path = "incremental_header_guards.rs"]
mod header_guards;

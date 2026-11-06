//! Intended child of incremental; methods on the existing ElementReader.
//! There is still exactly one core InputCursor, no second buffered I/O layer.
//! Internal ranges keep generation; the explicit seek transition resets once.
use super::{
    Admit, CLUSTER, ClusterCheckpoint, EBML, ElementHeader, ElementReader, Master, ParseStep,
    Phase, SEGMENT, ScanBody, is_level_one,
};
use crate::matroska::typed::syntax::Required;
use crate::{Error, Result};
use std::sync::atomic::AtomicBool;
use triergon_core::{InputCursor, InputRequest, WorkBudget};

#[derive(Debug)]
pub(crate) enum EnvelopeProgress {
    Header(ElementHeader),
    Step(ParseStep),
}

/// Same source events, with no temporary owned leaf event in typed delivery.
#[derive(Debug)]
pub(crate) enum CompactElementEvent {
    Start(ElementHeader),
    Leaf(ElementHeader),
    Skipped(ElementHeader),
    End {
        id: u64,
        offset: u64,
        generation: u64,
    },
}
#[derive(Debug)]
pub(crate) enum CompactEnvelopeProgress {
    Header(ElementHeader),
    Step(ParseStep<CompactElementEvent>),
}
struct CompactDelivery;
impl super::EnvelopeDelivery for CompactDelivery {
    type Event = CompactElementEvent;
    const RETAIN_LEAF: bool = true;
    fn start(header: ElementHeader) -> Self::Event {
        CompactElementEvent::Start(header)
    }
    fn end(id: u64, offset: u64, generation: u64) -> Self::Event {
        CompactElementEvent::End {
            id,
            offset,
            generation,
        }
    }
    fn skipped(header: ElementHeader) -> Self::Event {
        CompactElementEvent::Skipped(header)
    }
    fn leaf(body: &mut super::BodyState, bytes: triergon_core::Bytes) -> Self::Event {
        body.direct = Some(bytes);
        body.transfer = super::BodyTransfer::Ready;
        CompactElementEvent::Leaf(body.header)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ReplayLocation {
    checkpoint: ClusterCheckpoint,
    cluster: Master,
    position: u64,
    generation: u64,
    pub cluster_time: Option<u64>,
}
impl ReplayLocation {
    pub fn block_offset(&self) -> u64 {
        self.position
    }
    pub fn cluster_offset(&self) -> u64 {
        self.cluster.start
    }
    pub fn checkpoint(&self) -> &ClusterCheckpoint {
        &self.checkpoint
    }
}
impl ElementReader {
    pub(crate) fn input_capabilities(&self) -> triergon_core::SourceCapabilities {
        self.input.capabilities()
    }
    pub(crate) fn set_scan_remaining(&mut self, remaining: u64) {
        self.scan_left = remaining;
    }
    fn awaiting_typed_admission(&self) -> Option<ElementHeader> {
        match &self.phase {
            Phase::Body(body) if !body.typed_admitted && !body.allocated => Some(body.header),
            _ => None,
        }
    }
    /// One shared envelope engine pauses before leaf allocation for typed
    /// admission. No zero-work probe, one-unit facade, second parser or I/O.
    #[cfg(test)]
    pub(crate) fn step_typed_envelope(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<EnvelopeProgress> {
        self.step_typed_envelope_admitting(None, work, cancel)
    }
    fn step_typed_envelope_admitting(
        &mut self,
        admission: Option<super::EpochAdmission>,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<EnvelopeProgress> {
        let admit = admission.map_or(super::Admit::Pause, super::Admit::Epoch);
        let step = self.step_mode::<super::PublicDelivery>(work, cancel, admit)?;
        if let Some(header) = self.awaiting_typed_admission() {
            Ok(EnvelopeProgress::Header(header))
        } else {
            Ok(EnvelopeProgress::Step(step))
        }
    }
    #[cfg(test)]
    pub(crate) fn step_typed_compact(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<CompactEnvelopeProgress> {
        self.healthy_for_job(work, cancel)?;
        self.step_typed_compact_after_input_health(Admit::Pause, work, cancel)
    }

    pub(crate) fn step_typed_compact_after_input_health(
        &mut self,
        admit: Admit,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<CompactEnvelopeProgress> {
        let step = self.step_after_input_health::<CompactDelivery>(work, cancel, admit)?;
        if matches!(step, ParseStep::Yield)
            && let Some(header) = self.awaiting_typed_admission()
        {
            Ok(CompactEnvelopeProgress::Header(header))
        } else {
            Ok(CompactEnvelopeProgress::Step(step))
        }
    }
    /// Called immediately by the typed controller after its ordinary read debit.
    /// No allocation, copy, new owner, charged work or cancellation boundary.
    pub(crate) fn take_compact_leaf(
        &mut self,
        header: ElementHeader,
    ) -> Result<crate::storage::SharedBytes> {
        let Phase::Body(body) = &mut self.phase else {
            return Err(Error::Invalid("missing compact leaf body"));
        };
        if body.header != header || body.transfer != super::BodyTransfer::Ready {
            return Err(Error::Invalid("compact leaf identity changed"));
        }
        let bytes = body.direct.take().required("missing compact leaf owner")?;
        self.phase = Phase::Between;
        Ok(crate::storage::SharedBytes::from_core(bytes))
    }
    /// A late controller debit error dropped the old owned event. Match its
    /// phase/drop boundary rather than retaining a leaf after that failure.
    pub(crate) fn discard_compact_leaf(&mut self) {
        if matches!(&self.phase, Phase::Body(body) if body.transfer == super::BodyTransfer::Ready) {
            self.phase = Phase::Between;
        }
    }

    pub(crate) fn needs_header_input(&self) -> bool {
        matches!(&self.phase, Phase::Header(header) if header.have < header.target)
    }
    /// The pending request only supplies bytes this envelope consumes without
    /// collecting them, so no payload owner can retain the supplied window.
    pub(crate) fn needs_discard_input(&self) -> bool {
        matches!(&self.phase, Phase::Body(body) if !body.collect && body.remaining != 0)
    }
    pub(crate) fn admit_typed_leaf(
        &mut self,
        header: ElementHeader,
        maximum: Option<usize>,
        skip: bool,
    ) -> Result<()> {
        let Phase::Body(body) = &mut self.phase else {
            return Err(Error::Invalid("no pending leaf admission"));
        };
        if body.header != header || body.allocated || body.typed_admitted {
            return Err(Error::Invalid("pending leaf admission identity changed"));
        }
        if maximum.is_some_and(|limit| body.remaining > limit as u64) {
            return Err(Error::Policy("element bytes"));
        }
        if skip {
            body.collect = false;
        }
        body.typed_admitted = true;
        Ok(())
    }
    /// Only an active scan may arm this after normal typed admission. At least
    /// four source bytes are needed for even the shortest SimpleBlock prefix.
    pub(crate) fn arm_scan_prefix(&mut self, header: ElementHeader) -> Result<()> {
        let Phase::Body(body) = &mut self.phase else {
            return Err(Error::Invalid("no scan leaf admission"));
        };
        if body.header != header
            || !body.typed_admitted
            || body.allocated
            || !body.collect
            || header.id != 0xa3
            || body.scan != ScanBody::Off
        {
            return Err(Error::Invalid("scan leaf admission identity changed"));
        }
        if body.remaining >= 4 {
            body.scan = ScanBody::FirstSpan;
        }
        Ok(())
    }
    pub(crate) fn scan_prefix_header(&self) -> Option<ElementHeader> {
        match &self.phase {
            Phase::Body(body) if body.scan == ScanBody::Prefix => Some(body.header),
            _ => None,
        }
    }
    pub(crate) fn scan_prefix_bytes(&self, header: ElementHeader) -> Result<&[u8]> {
        let Phase::Body(body) = &self.phase else {
            return Err(Error::Invalid("no retained scan prefix"));
        };
        if body.header != header || body.scan != ScanBody::Prefix {
            return Err(Error::Invalid("scan prefix identity changed"));
        }
        let bytes = body
            .span
            .as_ref()
            .or(body.direct.as_ref())
            .required("missing actual scan span")?;
        Ok(&bytes[..bytes.len().min(11)])
    }
    /// Rejection keeps the exact span/direct/builder decision and all source
    /// bytes. Acceptance can finish a fully read body with one work unit.
    pub(crate) fn resolve_scan_prefix(
        &mut self,
        header: ElementHeader,
        drain: bool,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        let Phase::Body(body) = &mut self.phase else {
            return Err(Error::Invalid("no pending scan prefix"));
        };
        if body.header != header || body.scan != ScanBody::Prefix {
            return Err(Error::Invalid("scan prefix identity changed"));
        }
        body.scan = if drain {
            ScanBody::Drain
        } else {
            ScanBody::Off
        };
        if drain {
            if body.remaining == 0 {
                Self::check_input_cancel(&mut self.input, cancel)?;
                if work.try_charge(1) {
                    self.scan_left = self.limits.scan_bytes;
                    self.phase = Phase::Between;
                    return Ok(true);
                }
            }
            body.collect = false;
            body.span = None;
            body.direct = None;
            body.transfer = super::BodyTransfer::Copy;
            body.buffer = None;
            body.copied = 0;
        }
        Ok(false)
    }
    /// Complete a scan drain after the envelope charges its final work unit.
    pub(crate) fn finish_scanned_leaf(&mut self, header: ElementHeader) -> Result<bool> {
        let Phase::Body(body) = &self.phase else {
            return Err(Error::Invalid("missing scan drain body"));
        };
        if body.header != header {
            return Err(Error::Invalid("scan drain identity changed"));
        }
        if body.scan != ScanBody::Complete {
            return Ok(false);
        }
        self.phase = Phase::Between;
        Ok(true)
    }
    #[inline(always)]
    pub(crate) fn healthy_for_job(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<()> {
        self.input.read_into(&mut [], work, cancel)?;
        if self.failed {
            return Err(Error::Invalid("reader requires explicit reset after error"));
        }
        Ok(())
    }
    /// Only the typed controller may borrow this while its envelope state is
    /// suspended. It must restore an envelope context before resuming step().
    pub(crate) fn range_input(&mut self) -> &mut InputCursor {
        &mut self.input
    }
    fn check_anchor(&self, anchor: &ClusterCheckpoint, offset: u64) -> Result<()> {
        if !anchor.pool.same_pool(self.input.budget()) {
            return Err(triergon_core::Error::ForeignPool);
        }
        if anchor.parser != self.parser || anchor.source != self.input.source() {
            return Err(triergon_core::Error::SourceMismatch);
        }
        if !self.input.capabilities().random_access {
            return Err(Error::Unsupported("random-access input"));
        }
        if offset < anchor.segment.start || anchor.segment.end.is_some_and(|end| offset >= end) {
            return Err(Error::Invalid("range outside Segment"));
        }
        Ok(())
    }
    fn install_cluster_range(&mut self, anchor: &ClusterCheckpoint) {
        self.phase = Phase::Between;
        self.segment = Some(anchor.segment);
        self.cluster = None;
        self.pending_header = None;
        self.seen_ebml = true;
        self.seen_segment = true;
        self.elements_left = self.limits.elements;
        self.scan_left = self.limits.scan_bytes;
        self.failed = false;
        self.expected_cluster = true;
    }
    pub(crate) fn begin_internal_cluster_range(
        &mut self,
        anchor: &ClusterCheckpoint,
        offset: u64,
    ) -> Result<InputRequest> {
        self.check_anchor(anchor, offset)?;
        let request = self.input.request_range(offset)?;
        self.install_cluster_range(anchor);
        Ok(request)
    }
    fn replay_block_context(&self, header: ElementHeader) -> Result<(Master, Master)> {
        if !matches!(header.id, 0xa0 | 0xa3) || header.generation != self.generation() {
            return Err(Error::Invalid("invalid replay block identity"));
        }
        let cluster = self.cluster.required("replay block outside Cluster")?;
        if header.start < cluster.start {
            return Err(Error::Invalid("replay block outside Cluster"));
        }
        self.check_parent(header, cluster.end)?;
        let segment = self.segment.required("missing replay Cluster context")?;
        Ok((cluster, segment))
    }
    fn replay_location_from_context(
        &self,
        header: ElementHeader,
        cluster_time: Option<u64>,
        cluster: Master,
        segment: Master,
    ) -> ReplayLocation {
        ReplayLocation {
            checkpoint: ClusterCheckpoint {
                pool: self.input.budget().clone(),
                parser: self.parser,
                source: self.input.source(),
                offset: cluster.start,
                segment,
            },
            cluster,
            position: header.start,
            generation: self.generation(),
            cluster_time,
        }
    }
    pub(crate) fn block_replay_location(
        &self,
        header: ElementHeader,
        cluster_time: Option<u64>,
    ) -> Result<ReplayLocation> {
        let (cluster, segment) = self.replay_block_context(header)?;
        Ok(self.replay_location_from_context(header, cluster_time, cluster, segment))
    }
    /// Validate before mutating the existing value. Earlier cloned locations
    /// remain independent; only this slot's scalar wire context changes.
    #[inline]
    pub(crate) fn refresh_block_replay_location(
        &self,
        slot: &mut Option<ReplayLocation>,
        header: ElementHeader,
        cluster_time: Option<u64>,
    ) -> Result<()> {
        let (cluster, segment) = self.replay_block_context(header)?;
        if let Some(location) = slot.as_mut().filter(|location| {
            location.checkpoint.pool.same_pool(self.input.budget())
                && location.checkpoint.parser == self.parser
                && location.checkpoint.source == self.input.source()
                && location.generation == self.generation()
        }) {
            location.checkpoint.offset = cluster.start;
            location.checkpoint.segment = segment;
            location.cluster = cluster;
            location.position = header.start;
            location.generation = self.generation();
            location.cluster_time = cluster_time;
        } else {
            *slot = Some(self.replay_location_from_context(header, cluster_time, cluster, segment));
        }
        Ok(())
    }
    pub(crate) fn restore_internal_block(
        &mut self,
        location: &ReplayLocation,
    ) -> Result<InputRequest> {
        self.check_anchor(&location.checkpoint, location.position)?;
        if location.generation != self.generation() {
            return Err(triergon_core::Error::StaleGeneration {
                expected: self.generation(),
                received: location.generation,
            });
        }
        let request = self.input.request_range(location.position)?;
        self.phase = Phase::Between;
        self.segment = Some(location.checkpoint.segment);
        self.cluster = Some(location.cluster);
        self.pending_header = None;
        self.seen_ebml = true;
        self.seen_segment = true;
        self.elements_left = self.limits.elements;
        self.scan_left = self.limits.scan_bytes;
        self.failed = false;
        self.expected_cluster = false;
        Ok(request)
    }
    /// Keep one scan limit for the whole seek operation. The existing normal
    /// packet reader's per-block refill cannot refill this remaining count.
    pub(crate) fn step_seek_operation(
        &mut self,
        remaining: &mut u64,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep> {
        self.scan_left = *remaining;
        let before = self.input.position();
        let result = self.step(work, cancel);
        let consumed = self
            .input
            .position()
            .checked_sub(before)
            .required("range changed inside envelope step")?;
        let Some(left) = remaining.checked_sub(consumed) else {
            self.failed = true;
            return Err(Error::Policy("operation read bytes"));
        };
        *remaining = left;
        self.scan_left = left;
        result
    }
}

impl super::BodyState {
    fn builder_descriptor_active(&self) -> bool {
        // begin() historically installs an empty builder workspace. Deferring
        // its pool owner must preserve the real descriptor visit and the
        // indices used by the bounded pressure walk. Drain/completion and
        // retained-leaf publication are the original workspace-drop points.
        self.buffer.is_some()
            || (self.transfer != super::BodyTransfer::Ready
                && !matches!(self.scan, ScanBody::Drain | ScanBody::Complete))
    }
}
impl crate::matroska::typed::usage::Storage for super::BodyState {
    fn storage_slots(&self) -> Result<usize> {
        let tail = crate::matroska::typed::usage::slots(&[&self.span, &self.direct])?;
        usize::from(self.builder_descriptor_active())
            .checked_add(tail)
            .ok_or(Error::Overflow)
    }
    fn storage_owner(
        &self,
        mut index: usize,
    ) -> Result<Option<crate::matroska::typed::usage::Owner>> {
        if self.builder_descriptor_active() {
            if index == 0 {
                // This is an actual field inspection, including its deferred
                // empty state. The unchanged Pressure job charges/checks each
                // visit; no work is prepaid and no owner is manufactured.
                return self.buffer.as_ref().map_or(Ok(None), |buffer| {
                    crate::matroska::typed::usage::Storage::storage_owner(buffer, 0)
                });
            }
            index -= 1;
        }
        crate::matroska::typed::usage::owner(&[&self.span, &self.direct], index)
    }
}
impl crate::matroska::typed::usage::Storage for ElementReader {
    fn storage_slots(&self) -> Result<usize> {
        let body = match &self.phase {
            Phase::Body(body) => Some(body),
            _ => None,
        };
        crate::matroska::typed::usage::slots(&[&self.input, &body])
    }
    fn storage_owner(&self, index: usize) -> Result<Option<crate::matroska::typed::usage::Owner>> {
        let body = match &self.phase {
            Phase::Body(body) => Some(body),
            _ => None,
        };
        crate::matroska::typed::usage::owner(&[&self.input, &body], index)
    }
}

/// Format-authorized ordinary-header refill. Callers cannot construct an
/// expanded request by widening an already issued canonical hint themselves.
#[derive(Debug)]
pub(crate) struct HeaderRefillRequest {
    exact: InputRequest,
    expanded: InputRequest,
}
impl HeaderRefillRequest {
    pub(crate) fn exact(&self) -> &InputRequest {
        &self.exact
    }
    pub(crate) fn expanded(&self) -> &InputRequest {
        &self.expanded
    }
    pub(crate) fn matches_exact(&self, request: &InputRequest) -> bool {
        same_header_request(&self.exact, request)
    }
    pub(crate) fn matches_expanded(&self, request: &InputRequest) -> bool {
        same_header_request(&self.expanded, request)
    }
}
fn same_header_request(a: &InputRequest, b: &InputRequest) -> bool {
    a.source == b.source && a.token == b.token && a.offset == b.offset && a.maximum == b.maximum
}
impl ElementReader {
    pub(crate) fn exact_header_request(&self) -> Option<InputRequest> {
        let Phase::Header(state) = &self.phase else {
            return None;
        };
        if state.have >= state.target || self.failed {
            return None;
        }
        let mut request = self.input.request();
        request.maximum = request.maximum.min(state.target - state.have);
        Some(request)
    }
    pub(crate) fn bounded_header_refill(
        &self,
        exact: &InputRequest,
        maximum: usize,
    ) -> Result<Option<HeaderRefillRequest>> {
        let Phase::Header(state) = &self.phase else {
            return Ok(None);
        };
        if state.have >= state.target || self.failed || maximum == 0 {
            return Ok(None);
        }
        if self.seen_segment && self.segment.is_none() {
            return Ok(None);
        }
        let core = self.input.request();
        if exact.source != core.source
            || exact.token != core.token
            || exact.offset != core.offset
            || exact.maximum != core.maximum.min(state.target - state.have)
        {
            return Err(Error::Invalid("header refill request identity changed"));
        }
        // Zero is the existing real EOF-probe request, never a manufactured
        // parent/scan EOF. Narrow/malformed-edge canonical hints stay exact.
        if exact.maximum == 0 {
            return Ok(None);
        }
        let cap = core.maximum.min(maximum).min(4096);
        self.expanded_refill(exact, core, cap)
    }
    /// A discarded body's exact request may be widened in the same bounded way
    /// as a header refill, so the following header needs no separate supply.
    /// Discarded bytes never become a payload owner. Only an immutable source,
    /// whose views need no new storage, uses this (see the blocking adapter).
    pub(crate) fn bounded_discard_refill(
        &self,
        exact: &InputRequest,
        maximum: usize,
    ) -> Result<Option<HeaderRefillRequest>> {
        let Phase::Body(body) = &self.phase else {
            return Ok(None);
        };
        if body.collect || body.remaining == 0 || self.failed || maximum == 0 {
            return Ok(None);
        }
        if self.seen_segment && self.segment.is_none() {
            return Ok(None);
        }
        let core = self.input.request();
        let body_maximum =
            usize::try_from(body.remaining.min(8192)).map_err(|_| Error::Overflow)?;
        if exact.source != core.source
            || exact.token != core.token
            || exact.offset != core.offset
            || exact.maximum != core.maximum.min(body_maximum)
        {
            return Err(Error::Invalid("discard refill request identity changed"));
        }
        if exact.maximum == 0 {
            return Ok(None);
        }
        let cap = core.maximum.min(maximum);
        self.expanded_refill(exact, core, cap)
    }
    fn expanded_refill(
        &self,
        exact: &InputRequest,
        core: InputRequest,
        cap: usize,
    ) -> Result<Option<HeaderRefillRequest>> {
        let mut cap = cap.min(usize::try_from(self.scan_left).unwrap_or(usize::MAX));
        for end in [
            self.segment.and_then(|m| m.end),
            self.cluster.and_then(|m| m.end),
        ]
        .into_iter()
        .flatten()
        {
            cap = cap.min(usize::try_from(end.saturating_sub(core.offset)).unwrap_or(usize::MAX));
        }
        if cap <= exact.maximum {
            return Ok(None);
        }
        let mut expanded = core;
        expanded.maximum = cap;
        Ok(Some(HeaderRefillRequest {
            exact: exact.clone(),
            expanded,
        }))
    }
}

/// A finite Cluster header found by [`ElementReader::owned_plain_prefix`].
#[derive(Clone, Copy, Debug)]
pub(crate) struct PrefixBoundary {
    pub consumed: u64,
    pub data: u64,
    pub end: u64,
}
impl ElementReader {
    /// Checks a complete in-memory Cluster prefix with this envelope's own
    /// structure and limit rules, from the Cluster at `start` up to and
    /// including the header of the Cluster at `boundary`. Only finite Clusters
    /// whose children are SimpleBlocks, Timestamps and other non-master leaves
    /// qualify; segment-level Void/CRC-32 are skipped. Any BlockGroup, unknown
    /// size, level-one element, malformed/truncated header, limit or parent
    /// violation returns `None`, leaving the ordinary scan to report it.
    pub(crate) fn owned_plain_prefix(
        &self,
        root: &[u8],
        start: u64,
        boundary: u64,
        segment_end: Option<u64>,
        scan_left: u64,
        cancel: &AtomicBool,
    ) -> Option<PrefixBoundary> {
        let header = |at: u64| -> Option<(u64, Option<u64>, u64)> {
            let bytes = root.get(usize::try_from(at).ok()?..)?;
            let (id, id_width, _) = crate::ebml::vint(bytes, true).ok()?;
            let (size, size_width, unknown) = crate::ebml::vint(&bytes[id_width..], false).ok()?;
            let data = at.checked_add((id_width + size_width) as u64)?;
            Some((id, (!unknown).then_some(size), data))
        };
        let mut left = scan_left;
        let mut elements = 0_u64;
        let consume = |bytes: u64, left: &mut u64| -> Option<()> {
            *left = left.checked_sub(bytes)?;
            Some(())
        };
        let mut position = start;
        loop {
            let (id, size, data) = header(position)?;
            // The ordinary range reader requires the anchor to be a Cluster.
            if position == start && id != CLUSTER {
                return None;
            }
            elements += 1;
            if elements.is_multiple_of(1024) && cancel.load(std::sync::atomic::Ordering::Relaxed) {
                return None;
            }
            if elements >= self.limits.elements {
                return None;
            }
            consume(data - position, &mut left)?;
            let size = size?;
            let end = data.checked_add(size)?;
            if segment_end.is_some_and(|limit| data > limit || end > limit) {
                return None;
            }
            match id {
                CLUSTER if position == boundary => {
                    return Some(PrefixBoundary {
                        consumed: data - start,
                        data,
                        end,
                    });
                }
                CLUSTER if position < boundary => {
                    let mut child = data;
                    while child < end {
                        let (id, size, child_data) = header(child)?;
                        elements += 1;
                        if elements.is_multiple_of(1024)
                            && cancel.load(std::sync::atomic::Ordering::Relaxed)
                        {
                            return None;
                        }
                        if elements >= self.limits.elements {
                            return None;
                        }
                        consume(child_data - child, &mut left)?;
                        let size = size?;
                        let child_end = child_data.checked_add(size)?;
                        if child_end > end || size > left {
                            return None;
                        }
                        match id {
                            0xa3 if size <= self.limits.block_bytes as u64 => (),
                            0xe7 if size <= 8 => (),
                            CLUSTER | SEGMENT | EBML | 0xa0 | 0xa3 | 0xe7 => return None,
                            id if is_level_one(id) => return None,
                            _ => (),
                        }
                        consume(size, &mut left)?;
                        child = child_end;
                    }
                    position = end;
                }
                0xec | 0xbf if size <= left => {
                    consume(size, &mut left)?;
                    position = end;
                }
                _ => return None,
            }
        }
    }
    /// The existing seek byte policy, with the ordinary typed leaf admission
    /// boundary exposed to the state-history job. One parser and one cursor.
    /// With `admission`, that job's own admission rule is applied inside the
    /// envelope instead of pausing for it after each leaf header.
    pub(crate) fn step_typed_seek_operation(
        &mut self,
        remaining: &mut u64,
        admission: Option<super::EpochAdmission>,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<EnvelopeProgress> {
        self.scan_left = *remaining;
        let before = self.input.position();
        let result = self.step_typed_envelope_admitting(admission, work, cancel);
        let consumed = self
            .input
            .position()
            .checked_sub(before)
            .required("range changed inside envelope step")?;
        let Some(left) = remaining.checked_sub(consumed) else {
            self.failed = true;
            return Err(Error::Policy("operation read bytes"));
        };
        *remaining = left;
        self.scan_left = left;
        result
    }
}

#[cfg(test)]
mod header_refill_bounds_tests {
    use super::super::Master;
    use super::*;
    use crate::Limits;
    use triergon_core::{Budget, SourceCapabilities, SourceId};

    fn pending(length: Option<u64>, window: usize) -> (ElementReader, InputRequest) {
        let pool = Budget::new(256 * 1024).unwrap();
        let mut reader = ElementReader::new(
            &pool,
            SourceId(2101),
            SourceCapabilities {
                length,
                random_access: false,
            },
            Limits::default(),
            window,
        )
        .unwrap();
        let request = match reader
            .step_typed_envelope(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            EnvelopeProgress::Step(ParseStep::NeedInput(request)) => request,
            other => panic!("initial header: {other:?}"),
        };
        (reader, request)
    }
    #[test]
    fn header_refill_is_bounded_without_mutating_the_exact_leaf_engine() {
        let (reader, exact) = pending(Some(5000), 4096);
        assert_eq!(exact.maximum, 1);
        let plan = reader
            .bounded_header_refill(&exact, usize::MAX)
            .unwrap()
            .unwrap();
        assert_eq!(plan.expanded().maximum, 4096);
        assert_eq!(plan.exact().maximum, 1);
        assert_eq!(plan.expanded().token, exact.token);
        assert_eq!(plan.expanded().offset, exact.offset);
        assert_eq!(reader.request().maximum, 4096);
        assert_eq!(reader.position(), 0);
    }
    #[test]
    fn header_refill_obeys_core_source_parent_and_scan_bounds() {
        let (mut reader, exact) = pending(Some(19), 13);
        reader.segment = Some(Master {
            id: 0x18538067,
            start: 0,
            end: Some(11),
        });
        reader.cluster = Some(Master {
            id: 0x1f43b675,
            start: 0,
            end: Some(9),
        });
        reader.scan_left = 7;
        assert_eq!(
            reader
                .bounded_header_refill(&exact, 1024)
                .unwrap()
                .unwrap()
                .expanded()
                .maximum,
            7
        );
        reader.scan_left = 50;
        assert_eq!(
            reader
                .bounded_header_refill(&exact, 1024)
                .unwrap()
                .unwrap()
                .expanded()
                .maximum,
            9
        );
        reader.cluster = None;
        reader.segment = None;
        assert_eq!(
            reader
                .bounded_header_refill(&exact, 1024)
                .unwrap()
                .unwrap()
                .expanded()
                .maximum,
            13
        );
        let (source, exact) = pending(Some(5), 8192);
        assert_eq!(
            source
                .bounded_header_refill(&exact, 1024)
                .unwrap()
                .unwrap()
                .expanded()
                .maximum,
            5
        );
    }
    #[test]
    fn header_refill_does_not_invent_parent_scan_or_entity_eof() {
        let (mut reader, exact) = pending(None, 8192);
        reader.scan_left = 0;
        assert!(
            reader
                .bounded_header_refill(&exact, 1024)
                .unwrap()
                .is_none()
        );
        assert_eq!(exact.maximum, 1);
        reader.scan_left = 100;
        reader.segment = Some(Master {
            id: 0x18538067,
            start: 0,
            end: Some(0),
        });
        assert!(
            reader
                .bounded_header_refill(&exact, 1024)
                .unwrap()
                .is_none()
        );
        assert_eq!(exact.maximum, 1); // Keep the old malformed-edge exact hint.
        let (empty, exact) = pending(Some(0), 8192);
        assert_eq!(exact.maximum, 0);
        assert!(empty.bounded_header_refill(&exact, 1024).unwrap().is_none());
    }
    #[test]
    fn header_refill_rejects_changed_identity_before_any_io() {
        let (reader, exact) = pending(None, 8192);
        let mut stale = exact.clone();
        stale.offset += 1;
        assert!(matches!(
            reader.bounded_header_refill(&stale, 1024),
            Err(Error::Invalid("header refill request identity changed"))
        ));
        let mut widened = exact.clone();
        widened.maximum += 1;
        assert!(reader.bounded_header_refill(&widened, 1024).is_err());
        assert_eq!(reader.position(), 0);
    }
}

#[cfg(test)]
mod replay_refresh_regressions;

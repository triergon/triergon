//! Intended child of incremental; methods on the existing ElementReader.
//! There is still exactly one core InputCursor, no second buffered I/O layer.
//! Internal ranges keep generation; the explicit seek transition resets once.
use super::{ClusterCheckpoint, ElementHeader, ElementReader, Master, ParseStep, Phase};
use crate::matroska::typed::syntax::Required;
use crate::{Error, Result};
use std::sync::atomic::AtomicBool;
use triergon_core::{InputCursor, InputRequest, WorkBudget};

#[derive(Debug)]
pub(crate) enum EnvelopeProgress {
    Header(ElementHeader),
    Step(ParseStep),
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
    pub(crate) fn step_typed_envelope(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<EnvelopeProgress> {
        let step = self.step_mode(work, cancel, true)?;
        if let Some(header) = self.awaiting_typed_admission() {
            Ok(EnvelopeProgress::Header(header))
        } else {
            Ok(EnvelopeProgress::Step(step))
        }
    }
    pub(crate) fn needs_header_input(&self) -> bool {
        matches!(&self.phase, Phase::Header(header) if header.have < header.target)
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
            return Err(Error::Limit("element bytes"));
        }
        if skip {
            body.collect = false;
        }
        body.typed_admitted = true;
        Ok(())
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
            return Err(triergon_core::Error::ForeignPool.into());
        }
        if anchor.parser != self.parser || anchor.source != self.input.source() {
            return Err(triergon_core::Error::SourceMismatch.into());
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
    pub(crate) fn block_replay_location(
        &self,
        header: ElementHeader,
        cluster_time: Option<u64>,
    ) -> Result<ReplayLocation> {
        if !matches!(header.id, 0xa0 | 0xa3) || header.generation != self.generation() {
            return Err(Error::Invalid("invalid replay block identity"));
        }
        let cluster = self.cluster.required("replay block outside Cluster")?;
        if header.start < cluster.start {
            return Err(Error::Invalid("replay block outside Cluster"));
        }
        self.check_parent(header, cluster.end)?;
        let checkpoint = self
            .cluster_checkpoint()
            .required("missing replay Cluster context")?;
        Ok(ReplayLocation {
            checkpoint,
            cluster,
            position: header.start,
            generation: self.generation(),
            cluster_time,
        })
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
            }
            .into());
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
            return Err(Error::Limit("operation read bytes"));
        };
        *remaining = left;
        self.scan_left = left;
        result
    }
}

crate::matroska::typed::usage::storage!(super::BodyState, |s| [s.buffer, s.span, s.direct]);
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
        let mut cap = core.maximum.min(maximum).min(4096);
        cap = cap.min(usize::try_from(self.scan_left).unwrap_or(usize::MAX));
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

impl ElementReader {
    /// The existing seek byte policy, with the ordinary typed leaf admission
    /// boundary exposed to the state-history job. One parser and one cursor.
    pub(crate) fn step_typed_seek_operation(
        &mut self,
        remaining: &mut u64,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<EnvelopeProgress> {
        self.scan_left = *remaining;
        let before = self.input.position();
        let result = self.step_typed_envelope(work, cancel);
        let consumed = self
            .input
            .position()
            .checked_sub(before)
            .required("range changed inside envelope step")?;
        let Some(left) = remaining.checked_sub(consumed) else {
            self.failed = true;
            return Err(Error::Limit("operation read bytes"));
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

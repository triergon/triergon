//! The reader-facing control of the envelope: compact typed delivery of leaf
//! events, typed leaf admission, scan prefixes, and the storage walk of the
//! envelope's state. There is one core `InputCursor` and no second buffered layer.
use super::{
    Admit, BodyStage, ElementHeader, ElementReader, ParseStep, Phase, SIMPLE_BLOCK, ScanBody,
};
use crate::step::Required;
use crate::{Error, Result};
use std::sync::atomic::AtomicBool;
use triergon_core::WorkBudget;

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
impl ElementReader {
    pub(crate) fn input_capabilities(&self) -> triergon_core::SourceCapabilities {
        self.input.capabilities()
    }
    pub(crate) fn set_scan_remaining(&mut self, remaining: u64) {
        self.scan_left = remaining;
    }
    fn awaiting_typed_admission(&self) -> Option<ElementHeader> {
        match &self.phase {
            Phase::Body(body) if body.stage == BodyStage::Pending => Some(body.header),
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
    pub(crate) fn step_typed_envelope_admitting(
        &mut self,
        admission: Option<super::EpochAdmission>,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<EnvelopeProgress> {
        let admit = admission.map_or(Admit::Pause, Admit::Epoch);
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
        if body.header != header || body.stage != BodyStage::Pending {
            return Err(Error::Invalid("pending leaf admission identity changed"));
        }
        if maximum.is_some_and(|limit| body.remaining > limit as u64) {
            return Err(Error::Policy("element bytes"));
        }
        if skip {
            body.collect = false;
        }
        body.stage = BodyStage::Admitted;
        Ok(())
    }
    /// Only an active scan may arm this after normal typed admission. At least
    /// four source bytes are needed for even the shortest SimpleBlock prefix.
    pub(crate) fn arm_scan_prefix(&mut self, header: ElementHeader) -> Result<()> {
        let Phase::Body(body) = &mut self.phase else {
            return Err(Error::Invalid("no scan leaf admission"));
        };
        if body.header != header
            || body.stage != BodyStage::Admitted
            || !body.collect
            || header.id != SIMPLE_BLOCK
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
        if drain && body.remaining == 0 && work.remaining() != 0 {
            // Dropping the completed body releases its retained input before
            // cancellation, just as the incremental drain does.
            self.phase = Phase::Between;
            Self::check_input_cancel(&mut self.input, cancel)?;
            let consumed = work.take(1);
            debug_assert_eq!(consumed, 1);
            self.scan_left = self.limits.scan_bytes;
            return Ok(true);
        }
        body.scan = if drain {
            ScanBody::Drain
        } else {
            ScanBody::Off
        };
        if drain {
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
impl crate::storage::usage::Storage for super::BodyState {
    fn storage_slots(&self) -> Result<usize> {
        let tail = crate::storage::usage::slots(&[&self.span, &self.direct])?;
        usize::from(self.builder_descriptor_active())
            .checked_add(tail)
            .ok_or(Error::Overflow)
    }
    fn storage_owner(&self, mut index: usize) -> Result<Option<crate::storage::usage::Owner>> {
        if self.builder_descriptor_active() {
            if index == 0 {
                // This is an actual field inspection, including its deferred
                // empty state. The unchanged Pressure job charges/checks each
                // visit; no work is prepaid and no owner is manufactured.
                return self.buffer.as_ref().map_or(Ok(None), |buffer| {
                    crate::storage::usage::Storage::storage_owner(buffer, 0)
                });
            }
            index -= 1;
        }
        crate::storage::usage::owner(&[&self.span, &self.direct], index)
    }
}
impl crate::storage::usage::Storage for ElementReader {
    fn storage_slots(&self) -> Result<usize> {
        let body = match &self.phase {
            Phase::Body(body) => Some(body),
            _ => None,
        };
        crate::storage::usage::slots(&[&self.input, &body])
    }
    fn storage_owner(&self, index: usize) -> Result<Option<crate::storage::usage::Owner>> {
        let body = match &self.phase {
            Phase::Body(body) => Some(body),
            _ => None,
        };
        crate::storage::usage::owner(&[&self.input, &body], index)
    }
}

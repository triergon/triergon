use super::{BlockAdditionContainer, Configuration, Event, SeekRequest};
use crate::matroska::typed::syntax::Required;
use crate::matroska::typed::{
    copy_vec::CopyVec,
    cues::CuesJob,
    epoch::{EpochReference, EpochRestoreJob, EpochTarget},
    header::{EbmlJob, SeekHeadJob},
    packet::{PacketBatchJob, SimplePacketJob, SimpleProgress},
    seek::SeekPlanJob,
    seek_validate::CueValidationJob,
    tracks::TracksJob,
};
use crate::{
    Error, Limits, Result, ebml,
    incremental::{
        ClusterCheckpoint, ElementEvent, ElementHeader, ElementReader, ParseStep,
        typed_bridge::{
            CompactElementEvent, CompactEnvelopeProgress as EnvelopeProgress, ReplayLocation,
        },
    },
    metadata::{
        Attachment, Catalogue, SharedDocument,
        typed_document::{CatalogueJob, CataloguePrepareJob},
    },
    storage::SharedBytes,
};
use std::sync::atomic::AtomicBool;
use triergon_core::{
    Budget, BudgetVec, InputRequest, InputToken, InputWindow, SourceCapabilities, SourceFailure,
    SourceId, WorkBudget,
};

mod blocking;
mod resolving;
mod seek_index;
mod seeking;
pub use blocking::BlockingReader;
use resolving::{Opening, ResolvePurpose, Resolver};
use seeking::{ReplayPrefix, ScanContext, ScanSimple, ScanState, SeekMode, SubtitleRecovery};
mod projection;
use projection::{
    LegacyEvent, LegacyProjection, PacketProjection, ScanEvent, ScanProjection, TypedProjection,
};

const SEGMENT: u64 = 0x18538067;
const CLUSTER: u64 = 0x1f43b675;
const INFO: u64 = 0x1549a966;
const TRACKS: u64 = 0x1654ae6b;
const CUES: u64 = 0x1c53bb6b;
const SEEK_HEAD: u64 = 0x114d9b74;
fn metadata(id: u64) -> bool {
    matches!(
        id,
        INFO | TRACKS
            | CUES
            | SEEK_HEAD
            | crate::metadata::ATTACHMENTS
            | crate::metadata::CHAPTERS
            | crate::metadata::TAGS
    )
}
fn document(id: u64) -> bool {
    matches!(
        id,
        INFO | crate::metadata::ATTACHMENTS | crate::metadata::CHAPTERS | crate::metadata::TAGS
    )
}

fn packet_replay_prefix(
    prefix: &mut Option<ReplayPrefix>,
    location: Option<&ReplayLocation>,
    envelope: &ElementReader,
) -> Result<Option<i64>> {
    if let Some(value) = *prefix {
        if value.source != envelope.request().source || value.generation != envelope.generation() {
            return Err(Error::Invalid("recovery replay source identity changed"));
        }
        let location = location.required("missing recovery block location")?;
        if location.block_offset() >= value.until_block {
            *prefix = None;
            Ok(None)
        } else {
            Ok(Some(value.lower_ns))
        }
    } else {
        Ok(None)
    }
}

#[derive(Debug)]
enum AdmitPhase {
    Lookup,
    Reserve,
    WaitReserve,
    Catalogue,
    Admit,
}
#[derive(Debug)]
struct Admission {
    header: ElementHeader,
    at: usize,
    phase: AdmitPhase,
    maximum: Option<usize>,
    prepare: Option<CataloguePrepareJob>,
}
// Exactly one transaction is live. A union keeps the maximum fixed workspace,
// rather than the sum, and adds no per-packet heap allocation/pressure seam.
// See .research/parallel-matroska-core/TYPED-INLINE-LAYOUT-REVIEW.md.
#[expect(
    clippy::large_enum_variant,
    reason = "Exclusive bounded inline jobs; heap indirection adds allocation and retry state; host size guarded"
)]
#[derive(Debug)]
enum State {
    Envelope,
    Admission(Admission),
    Ebml {
        header: ElementHeader,
        source: SharedBytes,
        job: EbmlJob,
    },
    Tracks {
        header: ElementHeader,
        job: TracksJob,
    },
    Document {
        header: ElementHeader,
        job: CatalogueJob,
    },
    Cues {
        header: ElementHeader,
        job: CuesJob,
    },
    Head {
        header: ElementHeader,
        source: SharedBytes,
        job: SeekHeadJob,
    },
    MergeHead {
        header: ElementHeader,
        source: SharedBytes,
        entries: BudgetVec<(u64, u64)>,
        at: usize,
        reserved: bool,
        previous_len: usize,
    },
    Simple(SimplePacketJob),
    Packet(PacketBatchJob),
    ScanSimple(ScanSimple),
    Cue(CueValidationJob),
    Epoch(EpochRestoreJob),
}

/// Retained format jobs on the existing envelope's sole InputCursor. Every lace
/// and checksum in a packet batch validates before its first packet is emitted.
#[derive(Debug)]
pub struct Reader {
    envelope: ElementReader,
    pool: Budget,
    limits: Limits,
    state: State,
    failed: bool,
    first_cluster: Option<ClusterCheckpoint>,
    configuration: Option<Configuration>,
    catalogue: Catalogue,
    cues: Option<BudgetVec<crate::cues::Cue>>,
    scale: u64,
    cluster_time: Option<u64>,
    last_location: Option<ReplayLocation>,
    segment_data: u64,
    segment_end: Option<u64>,
    revision: u64,
    metadata_used: usize,
    loaded: CopyVec<(u64, u64)>,
    seek_targets: CopyVec<(u64, u64)>,
    opening: Option<Opening>,
    resolver: Option<Resolver>,
    pending_seek: Option<SeekRequest>,
    seek_mode: SeekMode,
    plan: Option<SeekPlanJob>,
    seek_plan: Option<crate::matroska::typed::seek::SeekPlan>,
    scan: Option<ScanState>,
    subtitle_recovery: Option<SubtitleRecovery>,
    replay_prefix: Option<ReplayPrefix>,
    seek_receipt: Option<crate::matroska::SeekReceipt>,
    demux_read_bytes: u64,
    scan_remaining: Option<u64>,
    seek_metadata_bytes: u64,
    pressure: Option<crate::matroska::typed::usage::Pressure>,
    adapter_storage: usize,
    // An allocation-free negative proof: normal sequential parsing has validated
    // every group through this offset. Range jumps never extend this prefix.
    state_coverage_end: u64,
    // An immutable source's root, lent by the blocking adapter for one seek
    // poll and never retained across polls.
    pub(crate) owned_root: Option<triergon_core::Bytes>,
    seek_index: Option<Box<seek_index::ImmutableSeekIndex>>,
    state_coverage_contiguous: bool,
    state_seen: bool,
    webm_vp9_t35: bool,
    epoch_reference: Option<EpochReference>,
}
impl Reader {
    #[inline(always)]
    pub fn new(
        pool: &Budget,
        source: SourceId,
        capabilities: SourceCapabilities,
        limits: Limits,
        maximum_window: usize,
    ) -> Result<Self> {
        Ok(Self {
            envelope: ElementReader::new(pool, source, capabilities, limits, maximum_window)?,
            pool: pool.clone(),
            limits,
            state: State::Envelope,
            failed: false,
            first_cluster: None,
            configuration: None,
            catalogue: Catalogue::new(pool),
            cues: None,
            scale: 1_000_000,
            cluster_time: None,
            last_location: None,
            segment_data: 0,
            segment_end: None,
            revision: 0,
            metadata_used: 0,
            loaded: CopyVec::new(pool),
            seek_targets: CopyVec::new(pool),
            opening: None,
            resolver: None,
            pending_seek: None,
            seek_mode: SeekMode::None,
            plan: None,
            seek_plan: None,
            scan: None,
            subtitle_recovery: None,
            replay_prefix: None,
            seek_receipt: None,
            demux_read_bytes: 0,
            scan_remaining: None,
            seek_metadata_bytes: 0,
            pressure: None,
            adapter_storage: 0,
            state_coverage_end: 0,
            owned_root: None,
            seek_index: None,
            state_coverage_contiguous: true,
            state_seen: false,
            webm_vp9_t35: false,
            epoch_reference: None,
        })
    }
    /// Admit the observed WebM VP9 implicit T.35 addition when the track omits
    /// MaxBlockAdditionID. The EBML DocType still decides applicability.
    pub fn new_webm_vp9_t35_v1(
        pool: &Budget,
        source: SourceId,
        capabilities: SourceCapabilities,
        limits: Limits,
        maximum_window: usize,
    ) -> Result<Self> {
        let mut reader = Self::new(pool, source, capabilities, limits, maximum_window)?;
        reader.webm_vp9_t35 = true;
        Ok(reader)
    }
    /// Select an explicit block-payload ownership contract at construction.
    /// Default constructors and top-level metadata leaves retain OwnedExact.
    pub fn new_with_payload_ownership(
        pool: &Budget,
        source: SourceId,
        capabilities: SourceCapabilities,
        limits: Limits,
        maximum_window: usize,
        policy: crate::storage::PayloadOwnership,
    ) -> Result<Self> {
        let mut reader = Self::new(pool, source, capabilities, limits, maximum_window)?;
        reader.envelope.set_payload_ownership(policy);
        Ok(reader)
    }
    /// For a source backed by these exact immutable bytes, remember a bounded
    /// set of keyframe positions until a complete sequential pass validates it.
    /// Covered playback seeks emit their receipt without intermediate scan events.
    /// Supplied windows must be views of this input at their declared offsets.
    /// Contiguous block payloads may share the charged input allocation.
    pub fn new_with_immutable_seek_index(
        pool: &Budget,
        source: SourceId,
        bytes: triergon_core::Bytes,
        limits: Limits,
        maximum_window: usize,
    ) -> Result<Self> {
        if !bytes.belongs_to(pool) {
            return Err(triergon_core::Error::ForeignPool.into());
        }
        let capabilities = SourceCapabilities {
            length: Some(u64::try_from(bytes.len()).map_err(|_| Error::Overflow)?),
            random_access: true,
        };
        let mut reader = Self::new_with_payload_ownership(
            pool,
            source,
            capabilities,
            limits,
            maximum_window,
            crate::storage::PayloadOwnership::SharedBounded,
        )?;
        reader.seek_index = Some(seek_index::ImmutableSeekIndex::new(
            bytes,
            pool,
            limits.index_entries,
        )?);
        Ok(reader)
    }
    pub fn payload_ownership(&self) -> crate::storage::PayloadOwnership {
        self.envelope.payload_ownership()
    }
    pub fn budget(&self) -> &Budget {
        &self.pool
    }
    pub fn generation(&self) -> u64 {
        self.envelope.generation()
    }
    pub fn position(&self) -> u64 {
        self.envelope.position()
    }
    pub fn request(&self) -> InputRequest {
        self.envelope.request()
    }
    pub fn capabilities(&self) -> SourceCapabilities {
        self.envelope.input_capabilities()
    }
    pub fn configuration(&self) -> Option<&Configuration> {
        self.configuration.as_ref()
    }
    pub fn pending_seek(&self) -> Option<SeekRequest> {
        self.pending_seek
    }
    pub fn tracks(&self) -> &[crate::matroska::Track] {
        self.configuration
            .as_ref()
            .map_or(&[], Configuration::tracks)
    }
    pub fn timestamp_scale_ns(&self) -> u64 {
        self.scale
    }
    pub fn catalogue(&self) -> &[SharedDocument] {
        self.catalogue.documents()
    }
    pub fn attachments(&self) -> impl Iterator<Item = Attachment> + '_ {
        self.catalogue
            .documents()
            .iter()
            .flat_map(SharedDocument::attachments)
    }
    pub fn is_ready(&self) -> bool {
        self.first_cluster.is_some()
            && !self.tracks().is_empty()
            && self.opening.is_none()
            && self.resolver.is_none()
    }
    pub fn provide(&mut self, window: &InputWindow) -> Result<()> {
        if let Some(index) = &self.seek_index {
            index.verify(window)?;
        }
        self.envelope.provide(window)?;
        if let Some(pressure) = &self.pressure {
            self.pressure = Some(crate::matroska::typed::usage::Pressure::new(
                pressure.requested,
            ));
        }
        Ok(())
    }
    pub fn finish(&mut self, source: SourceId, token: InputToken) -> Result<()> {
        self.envelope.finish(source, token)
    }
    pub fn fail(
        &mut self,
        source: SourceId,
        token: InputToken,
        failure: SourceFailure,
    ) -> Result<()> {
        self.envelope.fail(source, token, failure)
    }
    pub fn reset(&mut self) -> Result<InputRequest> {
        let request = self.envelope.reset()?;
        self.state = State::Envelope;
        self.failed = false;
        self.pressure = None;
        self.first_cluster = None;
        self.configuration = None;
        self.catalogue = Catalogue::new(&self.pool);
        self.cues = None;
        self.scale = 1_000_000;
        self.cluster_time = None;
        self.last_location = None;
        self.segment_data = 0;
        self.segment_end = None;
        self.revision = 0;
        self.state_coverage_end = 0;
        if let Some(index) = &mut self.seek_index {
            index.reset(self.limits.index_entries);
        }
        self.state_coverage_contiguous = true;
        self.state_seen = false;
        self.epoch_reference = None;
        self.metadata_used = 0;
        self.loaded = CopyVec::new(&self.pool);
        self.seek_targets = CopyVec::new(&self.pool);
        self.opening = None;
        self.resolver = None;
        self.pending_seek = None;
        self.seek_mode = SeekMode::None;
        self.plan = None;
        self.seek_plan = None;
        self.scan = None;
        self.subtitle_recovery = None;
        self.replay_prefix = None;
        self.seek_receipt = None;
        self.scan_remaining = None;
        self.seek_metadata_bytes = 0;
        self.demux_read_bytes = 0;
        Ok(request)
    }
    #[inline]
    pub fn step(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<ParseStep<Event>> {
        let result = self.step_projected::<TypedProjection>(work, cancel);
        if self.state_coverage_contiguous
            && let Some(index) = &mut self.seek_index
            && index.is_collecting()
        {
            if self.state_seen || self.cues.as_ref().is_some_and(|cues| !cues.is_empty()) {
                index.discard();
            } else {
                match &result {
                    Ok(ParseStep::Event(Event::Packet(packet))) => {
                        if packet.configuration.tracks()[packet.packet.track].kind == 1
                            && let Some(location) = &self.last_location
                        {
                            index.observe(packet, location.cluster_offset(), work);
                        }
                    }
                    Ok(ParseStep::Finished) => index.seal(self.envelope.position()),
                    _ => {}
                }
            }
        }
        result
    }
    #[inline]
    fn step_projected<D: PacketProjection>(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<D::Event>> {
        self.step_projected_with_source::<D>(work, cancel, None)
    }
    #[inline]
    fn step_projected_with_source<D: PacketProjection>(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        source: Option<&dyn crate::matroska::typed::usage::Storage>,
    ) -> Result<ParseStep<D::Event>> {
        let result = (|| {
            self.envelope.healthy_for_job(work, cancel)?;
            if self.failed {
                return Err(Error::Invalid(
                    "reader requires explicit seek/reset after error",
                ));
            }
            if self.pressure.is_some() {
                return D::progress(self.step_pressure(work, cancel, source));
            }
            let result = if self.resolver.is_some() {
                D::progress(self.step_resolver(work, cancel))
            } else if self.opening.is_some() {
                D::progress(self.step_opening(work, cancel))
            } else if self.pending_seek.is_some() {
                D::seeking(self, work, cancel)
            } else {
                let result = self.step_demux_after_health_check::<D>(work, cancel, None, false);
                if self.state_coverage_contiguous && matches!(result, Ok(ParseStep::Event(_))) {
                    self.state_coverage_end = self.state_coverage_end.max(self.envelope.position());
                }
                result
            };
            match result {
                Err(Error::Backpressure { requested, .. })
                | Ok(ParseStep::Backpressure { requested, .. }) => {
                    self.allocation_blocked(requested);
                    D::progress(self.step_pressure(work, cancel, source))
                }
                Ok(ParseStep::NeedInput(request)) => {
                    D::progress(self.canonical_input_hint(request))
                }
                other => other,
            }
        })();
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    // The ordinary reader and the blocking adapter use this header policy.
    // Seek and metadata transactions have separate refill boundaries.
    fn ordinary_header_refill(
        &self,
    ) -> Result<Option<crate::incremental::typed_bridge::HeaderRefillRequest>> {
        if self.resolver.is_some()
            || self.opening.is_some()
            || self.pending_seek.is_some()
            || !matches!(self.seek_mode, SeekMode::None)
            || self.scan.is_some()
            || self.scan_remaining.is_some()
            || !matches!(&self.state, State::Envelope)
        {
            return Ok(None);
        }
        let Some(exact) = self.envelope.exact_header_request() else {
            return Ok(None);
        };
        self.envelope.bounded_header_refill(&exact, 4096)
    }
    fn canonical_input_hint(&self, request: InputRequest) -> Result<ParseStep<Event>> {
        let mut plan = self.ordinary_header_refill()?;
        if plan.is_none() && self.seek_index.is_some() {
            // These windows are checked against the immutable source. Read
            // ahead within the same scan/parent bounds as the blocking adapter.
            plan = if self.needs_header_input() {
                self.blocking_seek_header_refill(&request)?
            } else if self.needs_discard_input() {
                self.blocking_seek_discard_refill(&request)?
            } else {
                None
            };
        }
        let Some(plan) = plan else {
            return Ok(ParseStep::NeedInput(request));
        };
        if !plan.matches_exact(&request) {
            return Err(Error::Invalid("canonical refill request identity changed"));
        }
        Ok(ParseStep::NeedInput(plan.expanded().clone()))
    }
    // This validates an already issued canonical hint; it cannot widen it.
    // The adapter may supply a short prefix under pre-read pressure, exactly
    // as another caller may supply fewer than InputRequest::maximum bytes.
    fn header_refill_for_request(
        &self,
        request: &InputRequest,
    ) -> Result<Option<crate::incremental::typed_bridge::HeaderRefillRequest>> {
        let Some(plan) = self.ordinary_header_refill()? else {
            return Ok(None);
        };
        if !plan.matches_expanded(request) {
            return Err(Error::Invalid("canonical header supply identity changed"));
        }
        Ok(Some(plan))
    }

    // Engine-only supply certificate. Public canonical NeedInput and the old
    // ordinary refill policy remain separate. This expanded response policy is
    // an explicit new blocking seek contract, not an ordinary-hint override.
    fn blocking_seek_header_refill(
        &self,
        request: &InputRequest,
    ) -> Result<Option<crate::incremental::typed_bridge::HeaderRefillRequest>> {
        if self.pending_seek.is_none() || self.resolver.is_some() || self.opening.is_some() {
            return Ok(None);
        }
        let stop = match self.seek_mode {
            SeekMode::Scan => self
                .scan
                .as_ref()
                .required("missing active scan")?
                .read_ahead_stop(),
            SeekMode::Epoch => {
                let State::Epoch(job) = &self.state else {
                    return Err(Error::Invalid("missing active epoch"));
                };
                Some(job.read_ahead_stop())
            }
            // Cue validation and metadata ranges retain their exact path.
            _ => return Ok(None),
        };
        let mut maximum = 4096;
        if let Some(stop) = stop {
            let Some(left) = stop.checked_sub(request.offset) else {
                return Ok(None);
            };
            maximum = maximum.min(usize::try_from(left).unwrap_or(usize::MAX));
        }
        self.envelope.bounded_header_refill(request, maximum)
    }
    // The same engine-only certificate for a discarded body: bounded by the
    // active stop, parent/source ends and scan allowance. Immutable sources only.
    fn blocking_seek_discard_refill(
        &self,
        request: &InputRequest,
    ) -> Result<Option<crate::incremental::typed_bridge::HeaderRefillRequest>> {
        if self.pending_seek.is_none() || self.resolver.is_some() || self.opening.is_some() {
            return Ok(None);
        }
        let stop = match self.seek_mode {
            SeekMode::Scan => self
                .scan
                .as_ref()
                .required("missing active scan")?
                .read_ahead_stop(),
            SeekMode::Epoch => {
                let State::Epoch(job) = &self.state else {
                    return Err(Error::Invalid("missing active epoch"));
                };
                Some(job.read_ahead_stop())
            }
            _ => return Ok(None),
        };
        let mut maximum = usize::MAX;
        if let Some(stop) = stop {
            let Some(left) = stop.checked_sub(request.offset) else {
                return Ok(None);
            };
            maximum = usize::try_from(left).unwrap_or(usize::MAX);
        }
        self.envelope.bounded_discard_refill(request, maximum)
    }

    fn needs_header_input(&self) -> bool {
        self.resolver.as_ref().map_or_else(
            || self.envelope.needs_header_input(),
            Resolver::needs_header_input,
        )
    }
    fn needs_discard_input(&self) -> bool {
        self.resolver.is_none() && self.envelope.needs_discard_input()
    }
    fn allocation_blocked(&mut self, requested: usize) {
        if let Some(index) = &mut self.seek_index
            && (index.is_collecting() || index.is_complete())
        {
            index.discard();
        }
        self.pressure = Some(crate::matroska::typed::usage::Pressure::new(requested));
    }
    fn set_adapter_storage(&mut self, bytes: usize) {
        if self.adapter_storage != bytes {
            self.adapter_storage = bytes;
            if let Some(pressure) = &self.pressure {
                self.pressure = Some(crate::matroska::typed::usage::Pressure::new(
                    pressure.requested,
                ));
            }
        }
    }
    fn step_pressure(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        source: Option<&dyn crate::matroska::typed::usage::Storage>,
    ) -> Result<ParseStep<Event>> {
        let mut pressure = self
            .pressure
            .take()
            .required("missing storage assessment")?;
        let result = if let Some(source) = source {
            pressure.step(
                &crate::matroska::typed::usage::WithSource {
                    graph: self,
                    source,
                },
                &self.pool,
                work,
                cancel,
            )
        } else {
            pressure.step(self, &self.pool, work, cancel)
        };
        match result {
            Ok(ParseStep::Event(())) => Ok(ParseStep::Yield),
            Ok(step) => {
                self.pressure = Some(pressure);
                crate::matroska::typed::pending(step)
            }
            Err(error) => Err(error),
        }
    }
    fn record_demux_read(&mut self, before: u64) -> Result<()> {
        Self::record_demux_bytes(
            &mut self.scan_remaining,
            &mut self.demux_read_bytes,
            before,
            self.envelope.position(),
        )
    }
    fn record_demux_bytes(
        remaining: &mut Option<u64>,
        total: &mut u64,
        before: u64,
        after: u64,
    ) -> Result<()> {
        let bytes = after
            .checked_sub(before)
            .required("range changed during input step")?;
        if let Some(left) = remaining {
            *left = left
                .checked_sub(bytes)
                .ok_or(Error::Limit("operation read bytes"))?;
            *total = total.checked_add(bytes).ok_or(Error::Overflow)?;
        }
        Ok(())
    }
    fn commit_metadata(&mut self, header: ElementHeader, structural: usize) -> Result<()> {
        let total = self
            .metadata_used
            .checked_add(structural)
            .ok_or(Error::Overflow)?;
        if total > self.limits.metadata_bytes {
            return Err(Error::Limit("metadata"));
        }
        // Admission reserved the slot before reading the source. This final
        // transaction update cannot conceal another vector growth/allocation.
        self.loaded.push_precharged((header.start, header.id))?;
        self.metadata_used = total;
        Ok(())
    }
    fn cancel_transaction(&mut self) -> Result<()> {
        self.loaded.cancel_pending();
        self.seek_targets.cancel_pending();
        if let State::MergeHead { previous_len, .. } = &self.state {
            self.seek_targets.truncate(*previous_len)?;
        }
        Ok(())
    }
    #[inline]
    fn start_leaf(&mut self, header: ElementHeader, data: SharedBytes) -> Result<()> {
        self.start_leaf_projected::<TypedProjection>(header, data)
    }
    #[inline(never)]
    fn start_leaf_projected<D: PacketProjection>(
        &mut self,
        header: ElementHeader,
        data: SharedBytes,
    ) -> Result<()> {
        let left = self
            .limits
            .metadata_bytes
            .checked_sub(self.metadata_used)
            .ok_or(Error::Limit("metadata"))?;
        // Keep each large constructor tied to its selected destination arm,
        // avoiding a merged maximum-sized enum result in this path.
        match header.id {
            0x1a45dfa3 => {
                self.state = State::Ebml {
                    header,
                    source: data.clone(),
                    job: EbmlJob::new(data),
                };
            }
            TRACKS => {
                self.state = State::Tracks {
                    header,
                    job: TracksJob::new(
                        data,
                        self.configuration.clone(),
                        self.limits,
                        left,
                        &self.pool,
                        self.generation(),
                        self.revision.checked_add(1).ok_or(Error::Overflow)?,
                    )?
                    .with_webm_vp9_t35(self.webm_vp9_t35),
                };
            }
            INFO
            | crate::metadata::ATTACHMENTS
            | crate::metadata::CHAPTERS
            | crate::metadata::TAGS => {
                self.state = State::Document {
                    header,
                    job: CatalogueJob::new(
                        header.id,
                        data,
                        left,
                        Some(header.data),
                        self.limits,
                        &self.pool,
                    )?,
                };
            }
            CUES => {
                if self.cues.as_ref().is_some_and(|cues| !cues.is_empty()) {
                    return Err(Error::Invalid("duplicate Cues"));
                }
                self.state = State::Cues {
                    header,
                    job: CuesJob::new(
                        data,
                        self.segment_data,
                        self.segment_end,
                        self.limits.index_entries,
                        &self.pool,
                    )?,
                };
            }
            SEEK_HEAD => {
                self.state = State::Head {
                    header,
                    source: data.clone(),
                    job: SeekHeadJob::new(
                        data,
                        self.segment_data,
                        self.seek_targets.len(),
                        &self.pool,
                    ),
                };
            }
            0xe7 => {
                self.cluster_time = Some(ebml::uint(&data)?);
                self.state = State::Envelope;
            }
            0xa0 | 0xa3 => {
                let configuration = self.configuration.as_ref().required("missing Tracks")?;
                let time = self
                    .cluster_time
                    .required("Block before Cluster Timestamp")?;
                self.envelope.refresh_block_replay_location(
                    &mut self.last_location,
                    header,
                    self.cluster_time,
                )?;
                if header.id == 0xa3 {
                    self.state = State::Simple(D::simple(
                        data,
                        time,
                        self.scale,
                        configuration,
                        self.limits,
                        &self.pool,
                    )?);
                } else {
                    self.start_group_batch(header, data, configuration.clone(), time)?;
                }
            }
            _ => return Err(Error::Invalid("unexpected collected leaf")),
        }
        Ok(())
    }
    // Keep the large fallback constructor out of the ordinary small packet's
    // call frame. This is the same complete BlockGroup constructor/context order.
    #[inline(never)]
    fn start_group_batch(
        &mut self,
        header: ElementHeader,
        data: SharedBytes,
        configuration: Configuration,
        time: u64,
    ) -> Result<()> {
        self.state = State::Packet(PacketBatchJob::new(
            data,
            false,
            time,
            self.scale,
            configuration,
            self.limits,
            &self.pool,
        )?);
        let State::Packet(job) = &mut self.state else {
            return Err(Error::Invalid("missing installed packet batch"));
        };
        job.set_location(crate::matroska::typed::side_data::GroupLocation {
            source: self.envelope.request().source,
            segment_data: self.segment_data,
            header: header.start,
            data: header.data,
        })?;
        Ok(())
    }
    // Header/Track validation and all fallibility precede this infallible
    // assembly. Only laced/encoded SimpleBlocks pay for the full workspace.
    #[inline(never)]
    fn promote_simple_packet(&mut self) -> Result<()> {
        let State::Simple(job) = &mut self.state else {
            return Err(Error::Invalid("missing SimpleBlock promotion"));
        };
        if !job.needs_batch() {
            return Err(Error::Invalid("missing SimpleBlock promotion"));
        }
        job.own_configuration(self.configuration.as_ref())?;
        let batch = job.take_batch(self.limits);
        self.state = State::Packet(batch);
        Ok(())
    }
    fn step_demux(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        stop: Option<u64>,
    ) -> Result<ParseStep<Event>> {
        self.step_demux_mode(work, cancel, stop, false)
    }
    fn step_demux_mode(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        stop: Option<u64>,
        scan_prefix: bool,
    ) -> Result<ParseStep<Event>> {
        self.step_demux_projected::<TypedProjection>(work, cancel, stop, scan_prefix)
    }
    fn step_demux_projected<D: PacketProjection>(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        stop: Option<u64>,
        scan_prefix: bool,
    ) -> Result<ParseStep<D::Event>> {
        self.envelope.healthy_for_job(work, cancel)?;
        self.step_demux_after_health_check::<D>(work, cancel, stop, scan_prefix)
    }

    fn step_demux_after_health_check<D: PacketProjection>(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        stop: Option<u64>,
        scan_prefix: bool,
    ) -> Result<ParseStep<D::Event>> {
        // Every metadata commit returns its event before this call continues.
        // Limits are immutable; the remaining metadata allowance is therefore
        // constant across these pure transitions. Input errors also propagate
        // immediately, with no input-supply callback inside this exclusive call.
        let left = self
            .limits
            .metadata_bytes
            .checked_sub(self.metadata_used)
            .ok_or(Error::Limit("metadata"))?;
        let mut first = true;
        loop {
            if first {
                first = false;
            } else {
                self.envelope.check_inner_cancel(work, cancel)?;
            }
            match &mut self.state {
                State::Envelope => {
                    if stop.is_some_and(|stop| self.position() >= stop) {
                        return Ok(ParseStep::Finished);
                    }
                    if let Some(remaining) = self.scan_remaining {
                        self.envelope.set_scan_remaining(remaining);
                    }
                    let before = self.envelope.position();
                    // A collected block needs only the admission work unit.
                    // Keep the header boundary for seek stops and scan debits.
                    let ordinary_blocks =
                        stop.is_none() && !scan_prefix && self.scan_remaining.is_none();
                    let progress = self.envelope.step_typed_compact_after_input_health(
                        ordinary_blocks,
                        work,
                        cancel,
                    );
                    if let Err(error) = self.record_demux_read(before) {
                        self.envelope.discard_compact_leaf();
                        return Err(error);
                    }
                    let progress = progress?;
                    if scan_prefix
                        && matches!(progress, EnvelopeProgress::Step(ParseStep::Yield))
                        && let Some(header) = self.envelope.scan_prefix_header()
                    {
                        let context = ScanContext {
                            configuration: self.configuration.as_ref(),
                            pool: &self.pool,
                            source: self.envelope.request().source,
                            generation: self.envelope.generation(),
                            cluster_ticks: self.cluster_time,
                            scale: self.scale,
                        };
                        self.state = State::ScanSimple(ScanSimple::new(header, &context));
                        continue;
                    }
                    match progress {
                        // Non-metadata leaves have no lookup or reservation:
                        // the Admit unit and decision apply here directly, and
                        // only an unpaid unit leaves the Admission state behind.
                        EnvelopeProgress::Header(header) if !metadata(header.id) => {
                            if !work.try_charge(1) {
                                self.state = State::Admission(Admission {
                                    header,
                                    at: 0,
                                    phase: AdmitPhase::Admit,
                                    maximum: None,
                                    prepare: None,
                                });
                                return Ok(ParseStep::Yield);
                            }
                            self.envelope.admit_typed_leaf(header, None, false)?;
                            if scan_prefix
                                && header.id == 0xa3
                                && stop.is_none_or(|stop| {
                                    header
                                        .size
                                        .and_then(|size| header.data.checked_add(size))
                                        .is_some_and(|end| end < stop)
                                })
                            {
                                self.envelope.arm_scan_prefix(header)?;
                            }
                        }
                        EnvelopeProgress::Header(header) => {
                            self.state = State::Admission(Admission {
                                header,
                                at: 0,
                                phase: AdmitPhase::Lookup,
                                maximum: None,
                                prepare: None,
                            })
                        }
                        EnvelopeProgress::Step(ParseStep::Event(event)) => match event {
                            CompactElementEvent::Start(header) => {
                                if header.id == SEGMENT {
                                    self.segment_data = header.data;
                                    self.segment_end = header
                                        .size
                                        .map(|size| {
                                            header.data.checked_add(size).ok_or(Error::Overflow)
                                        })
                                        .transpose()?;
                                }
                                if header.id == CLUSTER {
                                    self.cluster_time = None;
                                    if self.first_cluster.is_none() {
                                        self.first_cluster = self.envelope.cluster_checkpoint();
                                        self.opening = Some(Opening::new());
                                    }
                                }
                                return Ok(ParseStep::Event(D::envelope(ElementEvent::Start(
                                    header,
                                ))?));
                            }
                            CompactElementEvent::End {
                                id,
                                offset,
                                generation,
                            } => {
                                if id == CLUSTER {
                                    self.cluster_time = None;
                                }
                                return Ok(ParseStep::Event(D::envelope(ElementEvent::End {
                                    id,
                                    offset,
                                    generation,
                                })?));
                            }
                            CompactElementEvent::Skipped(header) => {
                                return Ok(ParseStep::Event(D::envelope(ElementEvent::Skipped(
                                    header,
                                ))?));
                            }
                            CompactElementEvent::Leaf(header) => {
                                let data = self.envelope.take_compact_leaf(header)?;
                                self.start_leaf_projected::<D>(header, data)?;
                            }
                        },
                        EnvelopeProgress::Step(other) => {
                            return crate::matroska::typed::pending(other);
                        }
                    }
                }
                State::Admission(admission) => match admission.phase {
                    AdmitPhase::Lookup => {
                        if !metadata(admission.header.id) {
                            admission.phase = AdmitPhase::Admit;
                            continue;
                        }
                        if admission.at == self.loaded.len() {
                            let maximum = if admission.header.id == crate::metadata::ATTACHMENTS {
                                left.checked_add(
                                    self.limits
                                        .attachment_bytes
                                        .checked_sub(self.catalogue.attachment_bytes())
                                        .ok_or(Error::Limit("attachment payload bytes"))?,
                                )
                                .ok_or(Error::Overflow)?
                            } else {
                                left
                            };
                            if admission
                                .header
                                .size
                                .is_some_and(|size| size > maximum as u64)
                            {
                                return Err(Error::Limit("element bytes"));
                            }
                            admission.phase = AdmitPhase::Reserve;
                            continue;
                        }
                        if !work.try_charge(1) {
                            return Ok(ParseStep::Yield);
                        }
                        let (position, id) = self.loaded.as_slice()[admission.at];
                        admission.at += 1;
                        if position == admission.header.start {
                            if id != admission.header.id {
                                return Err(Error::Invalid("cached metadata identity changed"));
                            }
                            self.envelope
                                .admit_typed_leaf(admission.header, None, true)?;
                            self.state = State::Envelope;
                        }
                    }
                    AdmitPhase::Reserve => {
                        self.loaded.reserve(1)?;
                        admission.phase = AdmitPhase::WaitReserve;
                    }
                    AdmitPhase::WaitReserve => match self.loaded.step(work, cancel)? {
                        ParseStep::Event(()) => {
                            let max = if admission.header.id == crate::metadata::ATTACHMENTS {
                                left.checked_add(
                                    self.limits
                                        .attachment_bytes
                                        .checked_sub(self.catalogue.attachment_bytes())
                                        .ok_or(Error::Limit("attachment payload bytes"))?,
                                )
                                .ok_or(Error::Overflow)?
                            } else {
                                left
                            };
                            admission.maximum = Some(max);
                            if document(admission.header.id) {
                                admission.prepare = Some(CataloguePrepareJob::new());
                                admission.phase = AdmitPhase::Catalogue;
                            } else {
                                admission.phase = AdmitPhase::Admit;
                            }
                        }
                        other => return crate::matroska::typed::pending(other),
                    },
                    AdmitPhase::Catalogue => match admission
                        .prepare
                        .as_mut()
                        .required("missing catalogue admission")?
                        .step(&mut self.catalogue, self.limits, work, cancel)?
                    {
                        ParseStep::Event(available) => {
                            admission.maximum =
                                Some(admission.maximum.unwrap_or(available).min(available));
                            admission.prepare = None;
                            admission.phase = AdmitPhase::Admit;
                        }
                        other => return crate::matroska::typed::pending(other),
                    },
                    AdmitPhase::Admit => {
                        if !work.try_charge(1) {
                            return Ok(ParseStep::Yield);
                        }
                        self.envelope.admit_typed_leaf(
                            admission.header,
                            admission.maximum,
                            false,
                        )?;
                        // A block touching a finite scan stop keeps the old
                        // body/stop scheduling, including work-boundary cases.
                        if scan_prefix
                            && admission.header.id == 0xa3
                            && stop.is_none_or(|stop| {
                                admission
                                    .header
                                    .size
                                    .and_then(|size| admission.header.data.checked_add(size))
                                    .is_some_and(|end| end < stop)
                            })
                        {
                            self.envelope.arm_scan_prefix(admission.header)?;
                        }
                        self.state = State::Envelope;
                    }
                },
                State::Ebml {
                    header,
                    source,
                    job,
                } => match job.step(work, cancel)? {
                    ParseStep::Event(doctype) => {
                        self.webm_vp9_t35 &= doctype == BlockAdditionContainer::WebM;
                        self.metadata_used = source.len();
                        let event = ElementEvent::Leaf {
                            header: *header,
                            data: source.clone(),
                        };
                        self.state = State::Envelope;
                        return Ok(ParseStep::Event(D::envelope(event)?));
                    }
                    other => return crate::matroska::typed::pending(other),
                },
                State::Tracks { header, job } => match job.step(work, cancel)? {
                    ParseStep::Event(configuration) => {
                        let h = *header;
                        let structural = job.metadata_bytes()?;
                        self.commit_metadata(h, structural)?;
                        self.revision = configuration.revision();
                        self.configuration = Some(configuration.clone());
                        self.state = State::Envelope;
                        return Ok(ParseStep::Event(D::configuration(configuration)?));
                    }
                    other => return crate::matroska::typed::pending(other),
                },
                State::Document { header, job } => {
                    match job.step(&mut self.catalogue, work, cancel)? {
                        ParseStep::Event(commit) => {
                            let h = *header;
                            let scale = commit.timestamp_scale.unwrap_or(1_000_000);
                            if h.id == INFO && scale == 0 {
                                return Err(Error::Invalid("zero TimestampScale"));
                            }
                            self.commit_metadata(h, commit.structural_bytes)?;
                            if h.id == INFO {
                                self.scale = scale;
                            }
                            self.state = State::Envelope;
                            return Ok(ParseStep::Event(D::document(commit.document)?));
                        }
                        other => return crate::matroska::typed::pending(other),
                    }
                }
                State::Cues { header, job } => match job.step(work, cancel)? {
                    ParseStep::Event(cues) => {
                        let h = *header;
                        let source = cues.source;
                        self.commit_metadata(h, source.len())?;
                        self.cues = Some(cues.entries);
                        self.state = State::Envelope;
                        return Ok(ParseStep::Event(D::envelope(ElementEvent::Leaf {
                            header: h,
                            data: source,
                        })?));
                    }
                    other => return crate::matroska::typed::pending(other),
                },
                State::Head {
                    header,
                    source,
                    job,
                } => match job.step(work, cancel)? {
                    ParseStep::Event(entries) => {
                        self.state = State::MergeHead {
                            header: *header,
                            source: source.clone(),
                            entries,
                            at: 0,
                            reserved: false,
                            previous_len: self.seek_targets.len(),
                        };
                    }
                    other => return crate::matroska::typed::pending(other),
                },
                State::MergeHead {
                    header,
                    source,
                    entries,
                    at,
                    reserved,
                    ..
                } => {
                    if !*reserved {
                        self.seek_targets.reserve(entries.len())?;
                        *reserved = true;
                    }
                    match self.seek_targets.step(work, cancel)? {
                        ParseStep::Event(()) | ParseStep::Finished => (),
                        other => return crate::matroska::typed::pending(other),
                    }
                    if *at == entries.len() {
                        let h = *header;
                        let source = source.clone();
                        self.commit_metadata(h, source.len())?;
                        self.state = State::Envelope;
                        return Ok(ParseStep::Event(D::envelope(ElementEvent::Leaf {
                            header: h,
                            data: source,
                        })?));
                    }
                    if !work.try_charge(1) {
                        return Ok(ParseStep::Yield);
                    }
                    self.seek_targets.push_precharged(entries.as_slice()[*at])?;
                    *at += 1;
                }
                State::ScanSimple(job) => {
                    if !scan_prefix {
                        return Err(Error::Invalid("scan probe outside seek scan"));
                    }
                    let context = ScanContext {
                        configuration: self.configuration.as_ref(),
                        pool: &self.pool,
                        source: self.envelope.request().source,
                        generation: self.envelope.generation(),
                        cluster_ticks: self.cluster_time,
                        scale: self.scale,
                    };
                    if job.completed_fields().is_some() {
                        // advance_scan consumes this scalar completion in this
                        // same call; it is never a public packet/event boundary.
                        return Ok(ParseStep::Yield);
                    }
                    if job.draining() {
                        if !job.context_matches(&context) {
                            return Err(Error::Invalid("scan drain source context changed"));
                        }
                        if let Some(remaining) = self.scan_remaining {
                            self.envelope.set_scan_remaining(remaining);
                        }
                        let before = self.envelope.position();
                        let progress = self
                            .envelope
                            .step_typed_compact_after_input_health(false, work, cancel);
                        if let Err(error) = Self::record_demux_bytes(
                            &mut self.scan_remaining,
                            &mut self.demux_read_bytes,
                            before,
                            self.envelope.position(),
                        ) {
                            self.envelope.discard_compact_leaf();
                            return Err(error);
                        }
                        let progress = progress?;
                        if self.envelope.finish_scanned_leaf(job.header())? {
                            job.finish();
                            return Ok(ParseStep::Yield);
                        }
                        match progress {
                            EnvelopeProgress::Step(step) => {
                                return crate::matroska::typed::pending(step);
                            }
                            EnvelopeProgress::Header(_) => {
                                return Err(Error::Invalid(
                                    "new header before scan drain completed",
                                ));
                            }
                        }
                    }
                    let header = job.header();
                    match job.step_prefix(
                        self.envelope.scan_prefix_bytes(header)?,
                        &context,
                        work,
                        cancel,
                    )? {
                        ParseStep::Event(drain) => {
                            self.envelope.resolve_scan_prefix(header, drain)?;
                            if !drain {
                                self.state = State::Envelope;
                            }
                        }
                        step => return crate::matroska::typed::pending(step),
                    }
                }
                State::Cue(_) | State::Epoch(_) => {
                    return Err(Error::Invalid("range validation outside seek controller"));
                }
                State::Simple(job) => {
                    let prefix = packet_replay_prefix(
                        &mut self.replay_prefix,
                        self.last_location.as_ref(),
                        &self.envelope,
                    )?;
                    let configuration = self.configuration.as_ref();
                    match job.step_compact_borrowing(configuration, prefix, work, cancel)? {
                        SimpleProgress::Promote => {
                            self.promote_simple_packet()?;
                            first = true;
                            continue;
                        }
                        SimpleProgress::Ready => {
                            return Ok(ParseStep::Event(D::take_simple(job, configuration)?));
                        }
                        SimpleProgress::Finished => self.state = State::Envelope,
                        SimpleProgress::Yield => return Ok(ParseStep::Yield),
                        SimpleProgress::Backpressure {
                            requested,
                            available,
                        } => {
                            return Ok(ParseStep::Backpressure {
                                requested,
                                available,
                            });
                        }
                    }
                }
                State::Packet(job) => {
                    if let Some(configuration) = job.take_configuration_change() {
                        self.state_seen |= configuration.has_codec_states();
                        self.revision = configuration.revision();
                        self.configuration = Some(configuration.clone());
                        return Ok(ParseStep::Event(D::configuration(configuration)?));
                    }
                    let prefix = packet_replay_prefix(
                        &mut self.replay_prefix,
                        self.last_location.as_ref(),
                        &self.envelope,
                    )?;
                    let step = job.step_ready(prefix, work, cancel)?;
                    match step {
                        ParseStep::Event(()) => {
                            return Ok(ParseStep::Event(D::packet(job.take_ready()?)?));
                        }
                        ParseStep::Finished => self.state = State::Envelope,
                        other => return crate::matroska::typed::pending(other),
                    }
                }
            }
        }
    }
}

crate::matroska::typed::usage::storage!(Admission, |s| [s.prepare]);
impl crate::matroska::typed::usage::Storage for State {
    fn storage_slots(&self) -> Result<usize> {
        use crate::matroska::typed::usage::{Storage, slots};
        match self {
            Self::Envelope | Self::ScanSimple(_) => Ok(0),
            Self::Admission(job) => Storage::storage_slots(job),
            Self::Ebml { job, .. } => Storage::storage_slots(job),
            Self::Tracks { job, .. } => Storage::storage_slots(job),
            Self::Document { job, .. } => Storage::storage_slots(job),
            Self::Cues { job, .. } => Storage::storage_slots(job),
            Self::Head { job, .. } => Storage::storage_slots(job),
            Self::MergeHead {
                source, entries, ..
            } => slots(&[source, entries]),
            Self::Simple(job) => Storage::storage_slots(job),
            Self::Packet(job) => Storage::storage_slots(job),
            Self::Cue(job) => Storage::storage_slots(job),
            Self::Epoch(job) => Storage::storage_slots(job),
        }
    }
    fn storage_owner(&self, index: usize) -> Result<Option<crate::matroska::typed::usage::Owner>> {
        use crate::matroska::typed::usage::{Storage, owner};
        match self {
            Self::Envelope | Self::ScanSimple(_) => Ok(None),
            Self::Admission(job) => Storage::storage_owner(job, index),
            Self::Ebml { job, .. } => Storage::storage_owner(job, index),
            Self::Tracks { job, .. } => Storage::storage_owner(job, index),
            Self::Document { job, .. } => Storage::storage_owner(job, index),
            Self::Cues { job, .. } => Storage::storage_owner(job, index),
            Self::Head { job, .. } => Storage::storage_owner(job, index),
            Self::MergeHead {
                source, entries, ..
            } => owner(&[source, entries], index),
            Self::Simple(job) => Storage::storage_owner(job, index),
            Self::Packet(job) => Storage::storage_owner(job, index),
            Self::Cue(job) => Storage::storage_owner(job, index),
            Self::Epoch(job) => Storage::storage_owner(job, index),
        }
    }
}
crate::matroska::typed::usage::storage!(Reader, |s| [
    s.envelope,
    s.configuration,
    s.catalogue,
    s.cues,
    s.loaded,
    s.seek_targets,
    s.state,
    s.resolver,
    s.scan,
    s.seek_index.as_deref(),
    s.plan,
    crate::matroska::typed::usage::Charge(s.adapter_storage)
]);

#[cfg(test)]
mod subtitle_recovery_guards;

#[cfg(test)]
mod storage_tests;

#[cfg(test)]
mod inline_input_tests;

#[cfg(test)]
mod header_refill_route_tests {
    use super::*;
    fn reader() -> Reader {
        let pool = Budget::new(128 * 1024).unwrap();
        Reader::new(
            &pool,
            SourceId(2103),
            SourceCapabilities {
                length: None,
                random_access: true,
            },
            Limits::default(),
            8192,
        )
        .unwrap()
    }
    #[test]
    fn header_refill_is_canonical_and_disabled_for_resolver_opening_seek_and_scan() {
        let mut reader = reader();
        let request = match reader
            .step(&mut WorkBudget::new(64), &AtomicBool::new(false))
            .unwrap()
        {
            ParseStep::NeedInput(request) => request,
            other => panic!("initial header: {other:?}"),
        };
        assert_eq!(request.maximum, 4096);
        assert!(
            reader
                .header_refill_for_request(&request)
                .unwrap()
                .is_some()
        );
        let exact = reader.envelope.exact_header_request().unwrap();
        assert_eq!(exact.maximum, 1);
        let disabled = |r: &Reader| {
            assert!(r.ordinary_header_refill().unwrap().is_none());
            let ParseStep::NeedInput(hint) = r.canonical_input_hint(exact.clone()).unwrap() else {
                panic!("disabled policy must retain NeedInput");
            };
            assert_eq!(hint.maximum, exact.maximum);
            assert_eq!(hint.token, exact.token);
            assert_eq!(hint.offset, exact.offset);
        };
        reader.opening = Some(Opening::new());
        disabled(&reader);
        reader.opening = None;
        reader.resolver = Some(Resolver::new(ResolvePurpose::Opening, &reader.pool));
        disabled(&reader);
        reader.resolver = None;
        for mode in [
            SeekMode::Plan,
            SeekMode::Cue,
            SeekMode::Scan,
            SeekMode::Epoch,
            SeekMode::InitialConfiguration,
            SeekMode::Receipt,
        ] {
            reader.seek_mode = mode;
            disabled(&reader);
        }
        reader.seek_mode = SeekMode::None;
        reader.scan_remaining = Some(100);
        disabled(&reader);
        reader.scan_remaining = None;
        reader.pending_seek = Some(SeekRequest {
            track: 0,
            requested_ns: 0,
            playback_preroll: false,
        });
        disabled(&reader);
        reader.pending_seek = None;
        assert!(
            reader
                .header_refill_for_request(&request)
                .unwrap()
                .is_some()
        );
        let mut wrong = request.clone();
        wrong.maximum += 1;
        assert!(matches!(
            reader.header_refill_for_request(&wrong),
            Err(Error::Invalid("canonical header supply identity changed"))
        ));
        assert_eq!(reader.position(), 0);
    }
    #[test]
    fn header_refill_canonical_short_prefix_observes_actual_zero_one_work_and_cancel() {
        let mut reader = reader();
        let cancel = AtomicBool::new(false);
        let request = match reader.step(&mut WorkBudget::new(64), &cancel).unwrap() {
            ParseStep::NeedInput(request) => request,
            other => panic!("initial header: {other:?}"),
        };
        assert_eq!(request.maximum, 4096);
        let bytes = triergon_core::Bytes::copy_from_slice(reader.budget(), &[0x1a]).unwrap();
        reader
            .provide(
                &InputWindow::new(request.source, request.token, request.offset, bytes).unwrap(),
            )
            .unwrap();
        let mut zero = WorkBudget::new(0);
        assert!(matches!(
            reader.step(&mut zero, &cancel).unwrap(),
            ParseStep::Yield
        ));
        assert_eq!((reader.position(), zero.spent()), (0, 0));
        let mut one = WorkBudget::new(1);
        assert!(matches!(
            reader.step(&mut one, &cancel).unwrap(),
            ParseStep::NeedInput(_) | ParseStep::Yield
        ));
        assert_eq!((reader.position(), one.spent()), (1, 1));
        cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(matches!(
            reader.step(&mut WorkBudget::new(1), &cancel),
            Err(Error::Cancelled)
        ));
        assert_eq!(reader.position(), 1);
    }
}

// Baseline-compatible runtime red: the engine, not only the blocking adapter,
// must issue the bounded default header hint. No new helper API is referenced.
#[cfg(test)]
mod header_refill_canonical_runtime_red {
    use super::*;
    #[test]
    fn ordinary_reader_default_issues_the_same_bounded_header_hint() {
        let pool = Budget::new(128 * 1024).unwrap();
        let mut reader = Reader::new(
            &pool,
            SourceId(2105),
            SourceCapabilities {
                length: None,
                random_access: false,
            },
            Limits::default(),
            8192,
        )
        .unwrap();
        let mut work = WorkBudget::new(64);
        let request = match reader.step(&mut work, &AtomicBool::new(false)).unwrap() {
            ParseStep::NeedInput(request) => request,
            other => panic!("initial canonical header request: {other:?}"),
        };
        assert_eq!(reader.position(), 0);
        assert_eq!(work.spent(), 0);
        assert_eq!(
            request.maximum, 4096,
            "ordinary engine and blocking supply share one bounded hint"
        );
    }
}

#[cfg(test)]
mod shared_bounded_tests;

#[cfg(test)]
mod shared_bounded_followup_tests;

#[cfg(test)]
mod inner_guard_regressions;

#[cfg(test)]
mod simple_direct_regressions;

#[cfg(test)]
impl Reader {
    fn simple_direct_test_job_step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Option<Result<ParseStep<()>>> {
        match &mut self.state {
            State::Packet(job) => Some(job.step_ready(None, work, cancel)),
            State::Simple(job) => Some(job.step_ready(None, work, cancel)),
            _ => None,
        }
    }
}

#[cfg(test)]
mod compact_controller_regressions;

#[cfg(test)]
mod lazy_pressure_regressions;

#[cfg(test)]
mod demux_slot_regressions;

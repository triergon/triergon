use super::{Configuration, Event, SeekRequest};
use crate::matroska::typed::syntax::Required;
use crate::matroska::typed::{
    copy_vec::CopyVec,
    cues::CuesJob,
    epoch::{EpochReference, EpochRestoreJob, EpochTarget},
    header::{EbmlJob, SeekHeadJob},
    packet::PacketBatchJob,
    seek::SeekPlanJob,
    seek_validate::CueValidationJob,
    tracks::TracksJob,
};
use crate::{
    Error, Limits, Result, ebml,
    incremental::{
        ClusterCheckpoint, ElementEvent, ElementHeader, ElementReader, ParseStep,
        typed_bridge::{EnvelopeProgress, ReplayLocation},
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

#[path = "reader_blocking.rs"]
mod blocking;
#[path = "reader_resolve.rs"]
mod resolving;
#[path = "reader_seek.rs"]
mod seeking;
pub use blocking::BlockingReader;
use resolving::{Opening, ResolvePurpose, Resolver};
use seeking::{ReplayPrefix, ScanState, SeekMode, SubtitleRecovery};

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
    Packet(PacketBatchJob),
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
    state_coverage_contiguous: bool,
    state_seen: bool,
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
            state_coverage_contiguous: true,
            state_seen: false,
            epoch_reference: None,
        })
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
        let result = (|| {
            self.envelope.healthy_for_job(work, cancel)?;
            if self.failed {
                return Err(Error::Invalid(
                    "reader requires explicit seek/reset after error",
                ));
            }
            if self.pressure.is_some() {
                return self.step_pressure(work, cancel);
            }
            let result = if self.resolver.is_some() {
                self.step_resolver(work, cancel)
            } else if self.opening.is_some() {
                self.step_opening(work, cancel)
            } else if self.pending_seek.is_some() {
                self.step_seeking(work, cancel)
            } else {
                let result = self.step_demux(work, cancel, None);
                if self.state_coverage_contiguous && matches!(result, Ok(ParseStep::Event(_))) {
                    self.state_coverage_end = self.state_coverage_end.max(self.envelope.position());
                }
                result
            };
            match result {
                Err(Error::Backpressure { requested, .. })
                | Ok(ParseStep::Backpressure { requested, .. }) => {
                    self.allocation_blocked(requested);
                    self.step_pressure(work, cancel)
                }
                Ok(ParseStep::NeedInput(request)) => self.canonical_input_hint(request),
                other => other,
            }
        })();
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    // Ordinary Reader, including its blocking adapter, uses this same policy.
    // Indexed metadata/range/seek/scan transactions keep their exact hints.
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
        let Some(plan) = self.ordinary_header_refill()? else {
            return Ok(ParseStep::NeedInput(request));
        };
        if !plan.matches_exact(&request) {
            return Err(Error::Invalid("ordinary header request identity changed"));
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

    fn needs_header_input(&self) -> bool {
        self.resolver.as_ref().map_or_else(
            || self.envelope.needs_header_input(),
            Resolver::needs_header_input,
        )
    }
    fn allocation_blocked(&mut self, requested: usize) {
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
    ) -> Result<ParseStep<Event>> {
        let mut pressure = self
            .pressure
            .take()
            .required("missing storage assessment")?;
        match pressure.step(self, &self.pool, work, cancel) {
            Ok(ParseStep::Event(())) => Ok(ParseStep::Yield),
            Ok(step) => {
                self.pressure = Some(pressure);
                crate::matroska::typed::pending(step)
            }
            Err(error) => Err(error),
        }
    }
    fn record_demux_read(&mut self, before: u64) -> Result<()> {
        let bytes = self
            .position()
            .checked_sub(before)
            .required("range changed during input step")?;
        if let Some(left) = &mut self.scan_remaining {
            *left = left
                .checked_sub(bytes)
                .ok_or(Error::Limit("operation read bytes"))?;
            self.demux_read_bytes = self
                .demux_read_bytes
                .checked_add(bytes)
                .ok_or(Error::Overflow)?;
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
                    )?,
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
                let configuration = self.configuration.clone().required("missing Tracks")?;
                let time = self
                    .cluster_time
                    .required("Block before Cluster Timestamp")?;
                self.last_location = Some(
                    self.envelope
                        .block_replay_location(header, self.cluster_time)?,
                );
                self.state = State::Packet(PacketBatchJob::new(
                    data,
                    header.id == 0xa3,
                    time,
                    self.scale,
                    configuration,
                    self.limits,
                    &self.pool,
                )?);
                // SimpleBlock has no group source context. It must not pass the
                // entire large retained job through a consuming builder just
                // to discard that context. Mutate the installed BlockGroup job.
                if header.id == 0xa0 {
                    let State::Packet(job) = &mut self.state else {
                        return Err(Error::Invalid("missing installed packet batch"));
                    };
                    job.set_location(crate::matroska::typed::side_data::GroupLocation {
                        source: self.envelope.request().source,
                        segment_data: self.segment_data,
                        header: header.start,
                        data: header.data,
                    })?;
                }
            }
            _ => return Err(Error::Invalid("unexpected collected leaf")),
        }
        Ok(())
    }
    fn step_demux(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        stop: Option<u64>,
    ) -> Result<ParseStep<Event>> {
        loop {
            self.envelope.healthy_for_job(work, cancel)?;
            let left = self
                .limits
                .metadata_bytes
                .checked_sub(self.metadata_used)
                .ok_or(Error::Limit("metadata"))?;
            match &mut self.state {
                State::Envelope => {
                    if stop.is_some_and(|stop| self.position() >= stop) {
                        return Ok(ParseStep::Finished);
                    }
                    if let Some(remaining) = self.scan_remaining {
                        self.envelope.set_scan_remaining(remaining);
                    }
                    let before = self.envelope.position();
                    let progress = self.envelope.step_typed_envelope(work, cancel);
                    self.record_demux_read(before)?;
                    match progress? {
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
                            ElementEvent::Start(header) => {
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
                                return Ok(ParseStep::Event(Event::Envelope(ElementEvent::Start(
                                    header,
                                ))));
                            }
                            ElementEvent::End {
                                id,
                                offset,
                                generation,
                            } => {
                                if id == CLUSTER {
                                    self.cluster_time = None;
                                }
                                return Ok(ParseStep::Event(Event::Envelope(ElementEvent::End {
                                    id,
                                    offset,
                                    generation,
                                })));
                            }
                            ElementEvent::Skipped(header) => {
                                return Ok(ParseStep::Event(Event::Envelope(
                                    ElementEvent::Skipped(header),
                                )));
                            }
                            ElementEvent::Leaf { header, data } => self.start_leaf(header, data)?,
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
                        self.state = State::Envelope;
                    }
                },
                State::Ebml {
                    header,
                    source,
                    job,
                } => match job.step(work, cancel)? {
                    ParseStep::Event(()) => {
                        self.metadata_used = source.len();
                        let event = ElementEvent::Leaf {
                            header: *header,
                            data: source.clone(),
                        };
                        self.state = State::Envelope;
                        return Ok(ParseStep::Event(Event::Envelope(event)));
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
                        return Ok(ParseStep::Event(Event::Configuration(configuration)));
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
                            return Ok(ParseStep::Event(Event::Document(commit.document)));
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
                        return Ok(ParseStep::Event(Event::Envelope(ElementEvent::Leaf {
                            header: h,
                            data: source,
                        })));
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
                        return Ok(ParseStep::Event(Event::Envelope(ElementEvent::Leaf {
                            header: h,
                            data: source,
                        })));
                    }
                    if !work.try_charge(1) {
                        return Ok(ParseStep::Yield);
                    }
                    self.seek_targets.push_precharged(entries.as_slice()[*at])?;
                    *at += 1;
                }
                State::Cue(_) | State::Epoch(_) => {
                    return Err(Error::Invalid("range validation outside seek controller"));
                }
                State::Packet(job) => {
                    if let Some(configuration) = job.take_configuration_change() {
                        self.state_seen |= configuration.has_codec_states();
                        self.revision = configuration.revision();
                        self.configuration = Some(configuration.clone());
                        return Ok(ParseStep::Event(Event::Configuration(configuration)));
                    }
                    let prefix = if let Some(prefix) = self.replay_prefix {
                        let source = self.envelope.request().source;
                        let generation = self.envelope.generation();
                        if prefix.source != source || prefix.generation != generation {
                            return Err(Error::Invalid("recovery replay source identity changed"));
                        }
                        let location = self
                            .last_location
                            .as_ref()
                            .required("missing recovery block location")?;
                        if location.block_offset() >= prefix.until_block {
                            self.replay_prefix = None;
                            None
                        } else {
                            Some(prefix.lower_ns)
                        }
                    } else {
                        None
                    };
                    let step = match prefix {
                        Some(lower) => job.step_recovery_prefix(lower, work, cancel),
                        None => job.step(work, cancel),
                    }?;
                    match step {
                        ParseStep::Event(packet) => {
                            return Ok(ParseStep::Event(Event::Packet(packet)));
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
            Self::Envelope => Ok(0),
            Self::Admission(job) => Storage::storage_slots(job),
            Self::Ebml { job, .. } => Storage::storage_slots(job),
            Self::Tracks { job, .. } => Storage::storage_slots(job),
            Self::Document { job, .. } => Storage::storage_slots(job),
            Self::Cues { job, .. } => Storage::storage_slots(job),
            Self::Head { job, .. } => Storage::storage_slots(job),
            Self::MergeHead {
                source, entries, ..
            } => slots(&[source, entries]),
            Self::Packet(job) => Storage::storage_slots(job),
            Self::Cue(job) => Storage::storage_slots(job),
            Self::Epoch(job) => Storage::storage_slots(job),
        }
    }
    fn storage_owner(&self, index: usize) -> Result<Option<crate::matroska::typed::usage::Owner>> {
        use crate::matroska::typed::usage::{Storage, owner};
        match self {
            Self::Envelope => Ok(None),
            Self::Admission(job) => Storage::storage_owner(job, index),
            Self::Ebml { job, .. } => Storage::storage_owner(job, index),
            Self::Tracks { job, .. } => Storage::storage_owner(job, index),
            Self::Document { job, .. } => Storage::storage_owner(job, index),
            Self::Cues { job, .. } => Storage::storage_owner(job, index),
            Self::Head { job, .. } => Storage::storage_owner(job, index),
            Self::MergeHead {
                source, entries, ..
            } => owner(&[source, entries], index),
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
    s.plan,
    crate::matroska::typed::usage::Charge(s.adapter_storage)
]);

#[cfg(test)]
#[path = "subtitle_recovery_guards.rs"]
mod subtitle_recovery_guards;

#[cfg(test)]
#[path = "reader_storage_tests.rs"]
mod storage_tests;

#[cfg(test)]
#[path = "inline_input_tests.rs"]
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

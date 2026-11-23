use crate::compat::block_additions::BlockAdditionContainer;
use crate::configuration::Configuration;
use crate::packet::batch::{
    PacketBatchJob, RecoveryCutoff, SimplePacketJob, SimpleProgress, TypedPacket,
};
use crate::seek::{
    SeekReceipt, SeekRequest,
    cue_validation::CueValidationJob,
    cues::CuesJob,
    epoch::{EpochReference, EpochRestoreJob, EpochTarget},
    plan::{RecoveryAudio, SeekPlanJob},
};
use crate::step::{ParseStep, Required};
use crate::storage::copy_vec::CopyVec;
use crate::storage::usage::{Pressure, Storage};
use crate::track::parse::TracksJob;
use crate::{
    Error, Limits, Result, ebml,
    element::{
        Admit, BLOCK_GROUP, ClusterCheckpoint, EBML, ElementEvent, ElementHeader, ElementReader,
        SIMPLE_BLOCK, TIMESTAMP,
        control::{CompactElementEvent, CompactEnvelopeProgress as EnvelopeProgress},
        replay::ReplayLocation,
    },
    metadata::{
        Attachment, Catalogue, SharedDocument,
        build::{CatalogueJob, CataloguePrepareJob},
    },
    storage::SharedBytes,
};
use head::{EbmlJob, SeekHeadJob};
use std::convert::Infallible;
use std::mem::MaybeUninit;
use std::sync::atomic::AtomicBool;
use triergon_core::{
    Backpressure, Budget, BudgetVec, InputRequest, InputToken, InputWindow, SourceCapabilities,
    SourceFailure, SourceId, Step, WorkBudget,
};

/// The outcome of a reader step that produces `T`.
type ReaderStep<T> = Result<Step<T, Backpressure>>;

/// What a [`Reader`] step produced.
#[derive(Clone, Debug)]
pub enum Event {
    /// Source structure or syntax; the other variants express typed acceptance.
    Envelope(ElementEvent),
    /// A new or changed track configuration, before the first packet that uses it.
    Configuration(Configuration),
    /// A validated Info, Chapters, Tags or Attachments document.
    Document(SharedDocument),
    /// One frame with its configuration and BlockGroup side data.
    Packet(TypedPacket),
    /// The receipt that completes a seek, before the first packet after it.
    Seek(SeekReceipt),
}

pub(crate) mod blocking;
pub(crate) mod head;
pub(crate) mod input_feeder;
pub(crate) mod range;
mod resolving;
mod seek_index;
mod seeking;
pub use blocking::BlockingReader;
use resolving::{Opened, Opening, Resolver, begin_blocks};
use seeking::{ReadAhead, ReplayPrefix, ScanContext, ScanSimple, Seek};
mod projection;
use projection::{LegacyEvent, LegacyProjection, PacketProjection, TypedProjection};

use crate::metadata::{CLUSTER, CUES, INFO, SEEK_HEAD, SEGMENT, TRACKS, is_segment_metadata};
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
) -> Result<Option<RecoveryCutoff>> {
    if let Some(value) = *prefix {
        if value.source != envelope.request().source || value.generation != envelope.generation() {
            return Err(Error::Invalid("recovery replay source identity changed"));
        }
        let location = location.required("missing recovery block location")?;
        if location.block_offset() >= value.until_block {
            *prefix = None;
            Ok(None)
        } else {
            Ok(Some(value.cutoff))
        }
    } else {
        Ok(None)
    }
}

/// Where the admission of a level-one leaf stands; `maximum` bounds its payload
/// in bytes.
// An explicit tag byte: kept in a niche of `Admit`'s option, the demux step ran up
// to 0.12% more instructions.
#[derive(Clone, Copy, Debug)]
#[repr(u8)]
enum AdmitPhase {
    /// Comparing the header with loaded metadata element `at`; an element loaded
    /// before is passed over.
    Lookup { at: usize },
    /// Reserving the slot that records the element as loaded.
    Reserve,
    /// Waiting for that reservation.
    WaitReserve,
    /// Making room in the catalogue for the document.
    Catalogue { maximum: usize },
    /// Handing the leaf to the envelope; `None` keeps the envelope's own bound.
    Admit { maximum: Option<usize> },
}
#[derive(Debug)]
struct Admission {
    header: ElementHeader,
    phase: AdmitPhase,
    // The job the Catalogue phase steps. Inside that variant it cost the demux step
    // 48 bytes of stack frame and a drop at every phase change.
    catalogue: CataloguePrepareJob,
}

/// The payload bytes a level-one leaf `id` may hold: the unused metadata allowance
/// `left`, and for Attachments also the unused attachment payload allowance.
#[inline(always)]
fn leaf_allowance(limits: &Limits, catalogue: &Catalogue, id: u64, left: usize) -> Result<usize> {
    if id != crate::metadata::ATTACHMENTS {
        return Ok(left);
    }
    let attachments = limits
        .attachment_bytes
        .checked_sub(catalogue.attachment_bytes())
        .ok_or(Error::Policy("attachment payload bytes"))?;
    left.checked_add(attachments).ok_or(Error::Overflow)
}
// Exactly one transaction is live. A union keeps the maximum fixed workspace,
// rather than the sum, and adds no per-packet heap allocation/pressure seam.
// An explicit tag: kept in a niche of a job, it costs the demux step a decode on
// every dispatch.
#[expect(
    clippy::large_enum_variant,
    reason = "Exclusive bounded inline jobs; heap indirection adds allocation and retry state; host size guarded"
)]
#[derive(Debug)]
#[repr(u64)]
enum State {
    Envelope,
    Admission(Admission),
    Ebml {
        header: ElementHeader,
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
        job: SeekHeadJob,
    },
    MergeHead {
        header: ElementHeader,
        source: SharedBytes,
        entries: BudgetVec<(u64, u64)>,
        at: usize,
        previous_len: usize,
    },
    Simple(SimplePacketJob),
    Packet(PacketBatchJob),
    ScanSimple(ScanSimple),
    Cue(CueValidationJob),
    Epoch(EpochRestoreJob),
}

/// Whether a step may run.
#[derive(Debug)]
#[repr(u32)]
enum Gate {
    Open,
    /// An allocation was refused: the storage the reader itself holds is added up
    /// before the step runs again.
    Assessing(Pressure),
    /// A step failed. A seek or a reset opens the gate again.
    Failed,
}

/// Who drives the stream.
#[expect(
    clippy::large_enum_variant,
    reason = "one controller at a time, inline in the reader; boxing it would put an allocation on every seek"
)]
#[derive(Debug)]
enum Control {
    /// Elements in source order up to the first Cluster.
    Head,
    /// The first Cluster is known and `activity` reads from it.
    Clusters {
        first: ClusterCheckpoint,
        activity: Activity,
    },
}
#[derive(Debug)]
#[repr(u32)]
enum Activity {
    /// Deciding which metadata the SeekHead points at is still unread.
    Opening(Opening),
    /// Reading that metadata by range before the first packet.
    Metadata(Resolver),
    /// Blocks in source order.
    Blocks,
    /// A seek, until its receipt is returned.
    Seek(Seek),
}
impl Control {
    fn seek(&self) -> Option<&Seek> {
        match self {
            Self::Clusters {
                activity: Activity::Seek(seek),
                ..
            } => Some(seek),
            _ => None,
        }
    }
    fn resolver(&self) -> Option<&Resolver> {
        match self {
            Self::Clusters {
                activity: Activity::Metadata(resolver),
                ..
            } => Some(resolver),
            _ => self.seek().and_then(Seek::resolver),
        }
    }
    /// Whether the stream is read in source order with nothing else to do first.
    fn in_source_order(&self) -> bool {
        matches!(
            self,
            Self::Head
                | Self::Clusters {
                    activity: Activity::Blocks,
                    ..
                }
        )
    }
}

/// The element reader and everything read from the source: the one transaction in
/// progress, the track configuration, the segment metadata and where the reader is
/// in a Cluster. Its steps read elements in source order; a controller decides
/// where that order starts.
// The fields are declared in address order: the Cluster time, the bytes every step
// tests and the element reader first, the transaction workspace last, so that a job
// that changes size moves no other field. The demux and seek steps were measured
// with this layout (`reader_fields_keep_their_measured_offsets` records it); in the
// compiler's own order they ran about 0.4% more instructions.
#[derive(Debug)]
#[repr(C)]
struct Stream {
    // First: this tag is the reader's largest niche, and an enum around the reader
    // keeps its own tag there.
    cluster_time: Option<u64>,
    state_coverage_contiguous: bool,
    state_seen: bool,
    webm_vp9_t35: bool,
    envelope: ElementReader,
    segment_end: Option<u64>,
    scan_remaining: Option<u64>,
    last_location: Option<ReplayLocation>,
    loaded: CopyVec<(u64, u64)>,
    seek_targets: CopyVec<(u64, u64)>,
    replay_prefix: Option<ReplayPrefix>,
    configuration: Option<Configuration>,
    catalogue: Catalogue,
    cues: Option<BudgetVec<crate::seek::Cue>>,
    pool: Budget,
    limits: Limits,
    scale: u64,
    segment_data: u64,
    revision: u64,
    metadata_used: usize,
    demux_read_bytes: u64,
    adapter_storage: usize,
    // An allocation-free negative proof: normal sequential parsing has validated
    // every group through this offset. Range jumps never extend this prefix.
    state_coverage_end: u64,
    seek_index: Option<Box<seek_index::ImmutableSeekIndex>>,
    state: State,
    reserve: Reserve,
}

/// Bytes after the transaction workspace that keep the reader at the size its
/// callers were measured with: they hold it inline, and a smaller reader moves
/// their own fields.
#[derive(Clone, Copy)]
struct Reserve([MaybeUninit<u8>; RESERVE]);
#[cfg(target_pointer_width = "64")]
const RESERVE: usize = 6752 - size_of::<State>();
#[cfg(not(target_pointer_width = "64"))]
const RESERVE: usize = 0;
impl std::fmt::Debug for Reserve {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{RESERVE} reserved bytes")
    }
}

/// Retained format jobs on the envelope's one InputCursor. Every lace
/// and checksum in a packet batch validates before its first packet is emitted.
#[derive(Debug)]
#[repr(C)]
pub struct Reader {
    stream: Stream,
    control: Control,
    gate: Gate,
}

/// A [`Reader`] that also admits WebM VP9's implicit ITU-T T.35 addition
/// (BlockAddID 4 on a track that omits MaxBlockAdditionID). The EBML DocType still
/// decides applicability. Documented on
/// [`compat::block_additions`](crate::compat::block_additions).
pub fn new_reader(
    pool: &Budget,
    source: SourceId,
    capabilities: SourceCapabilities,
    limits: Limits,
    maximum_window: usize,
) -> Result<Reader> {
    let mut reader = Reader::new(pool, source, capabilities, limits, maximum_window)?;
    reader.stream.webm_vp9_t35 = true;
    Ok(reader)
}

/// One step of adding up the storage the reader holds.
fn assess(
    pressure: &mut Pressure,
    stream: &Stream,
    control: &Control,
    work: &mut WorkBudget,
    cancel: &AtomicBool,
    source: Option<&dyn Storage>,
) -> Result<ParseStep<()>> {
    let graph = Graph { stream, control };
    match source {
        Some(source) => pressure.step(
            &crate::storage::usage::WithSource {
                graph: &graph,
                source,
            },
            &stream.pool,
            work,
            cancel,
        ),
        None => pressure.step(&graph, &stream.pool, work, cancel),
    }
}

/// One step of the controller that reads before blocks follow in source order: the
/// opening check, the opening metadata or a seek.
#[inline(never)]
fn step_activity(
    first: &ClusterCheckpoint,
    activity: &mut Activity,
    stream: &mut Stream,
    root: Option<&triergon_core::Bytes>,
    work: &mut WorkBudget,
    cancel: &AtomicBool,
) -> ReaderStep<Event> {
    match activity {
        // Nothing for a controller to do: the next step reads blocks.
        Activity::Blocks => {}
        Activity::Opening(opening) => match opening.step(stream, work, cancel)? {
            Opened::Pending => {}
            Opened::Blocks => *activity = Activity::Blocks,
            Opened::Unread => *activity = Activity::Metadata(Resolver::new(&stream.pool)),
        },
        Activity::Metadata(resolver) => {
            if let Some(step) = resolver.step(stream, work, cancel)? {
                return Ok(step);
            }
            begin_blocks(stream, first)?;
            *activity = Activity::Blocks;
        }
        Activity::Seek(seek) => {
            let result = seek.step(first, stream, root, work, cancel);
            if matches!(result, Ok(Step::Ready(Event::Seek(_)))) {
                *activity = Activity::Blocks;
            }
            return result;
        }
    }
    Ok(Step::Yield)
}

impl Reader {
    /// A reader at offset 0 of `source`, all storage charged to `pool`, asking for
    /// windows of at most `maximum_window` bytes. Fails with [`Error::Invalid`] for a
    /// zero `maximum_window`.
    #[inline(always)]
    pub fn new(
        pool: &Budget,
        source: SourceId,
        capabilities: SourceCapabilities,
        limits: Limits,
        maximum_window: usize,
    ) -> Result<Self> {
        Ok(Self {
            stream: Stream {
                cluster_time: None,
                state_coverage_contiguous: true,
                state_seen: false,
                webm_vp9_t35: false,
                envelope: ElementReader::new(pool, source, capabilities, limits, maximum_window)?,
                segment_end: None,
                scan_remaining: None,
                last_location: None,
                loaded: CopyVec::new(pool),
                seek_targets: CopyVec::new(pool),
                replay_prefix: None,
                configuration: None,
                catalogue: Catalogue::new(pool),
                cues: None,
                pool: pool.clone(),
                limits,
                scale: 1_000_000,
                segment_data: 0,
                revision: 0,
                metadata_used: 0,
                demux_read_bytes: 0,
                adapter_storage: 0,
                state_coverage_end: 0,
                seek_index: None,
                state: State::Envelope,
                reserve: Reserve([MaybeUninit::uninit(); RESERVE]),
            },
            control: Control::Head,
            gate: Gate::Open,
        })
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
        reader.stream.envelope.set_payload_ownership(policy);
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
            return Err(Error::ForeignPool);
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
        reader.stream.seek_index = Some(seek_index::ImmutableSeekIndex::new(
            bytes,
            pool,
            limits.index_entries,
        )?);
        Ok(reader)
    }
    /// The block-payload policy chosen at construction.
    pub fn payload_ownership(&self) -> crate::storage::PayloadOwnership {
        self.stream.envelope.payload_ownership()
    }
    /// The source this reader was created for.
    pub fn source(&self) -> SourceId {
        self.stream.envelope.input().source()
    }
    /// True until the reader has read or repositioned its input.
    pub fn is_unstarted(&self) -> bool {
        matches!(self.stream.state, State::Envelope) && self.stream.envelope.input().position() == 0
    }
    /// The pool all reader storage is charged to.
    pub fn budget(&self) -> &Budget {
        &self.stream.pool
    }
    /// The current input generation. A seek or reset starts a new one, after which
    /// answers to earlier requests are refused with [`Error::StaleGeneration`].
    pub fn generation(&self) -> u64 {
        self.stream.generation()
    }
    /// Source byte offset of the next byte the reader reads.
    pub fn position(&self) -> u64 {
        self.stream.position()
    }
    /// The request for the bytes at [`position`](Self::position), with the token an
    /// answer must carry.
    pub fn request(&self) -> InputRequest {
        self.stream.envelope.request()
    }
    /// The source capabilities; `length` becomes known once the caller
    /// [`finish`](Self::finish)es the source.
    pub fn capabilities(&self) -> SourceCapabilities {
        self.stream.envelope.input_capabilities()
    }
    /// The current track configuration; `None` until Tracks has been read.
    pub fn configuration(&self) -> Option<&Configuration> {
        self.stream.configuration.as_ref()
    }
    /// The seek in progress, until its [`Event::Seek`] receipt has been returned.
    pub fn pending_seek(&self) -> Option<SeekRequest> {
        self.control.seek().map(Seek::request)
    }
    /// The tracks of the current configuration; empty until Tracks has been read.
    pub fn tracks(&self) -> &[crate::track::Track] {
        self.stream.tracks()
    }
    /// TimestampScale: nanoseconds per Segment tick; 1000000 until Info sets it.
    pub fn timestamp_scale_ns(&self) -> u64 {
        self.stream.scale
    }
    /// The segment-metadata documents read so far, in reading order.
    pub fn catalogue(&self) -> &[SharedDocument] {
        self.stream.catalogue.documents()
    }
    /// The attachments of every Attachments document in the
    /// [`catalogue`](Self::catalogue).
    pub fn attachments(&self) -> impl Iterator<Item = Attachment> + '_ {
        self.stream
            .catalogue
            .documents()
            .iter()
            .flat_map(SharedDocument::attachments)
    }
    /// Whether opening is complete: Tracks read, the first Cluster found and every
    /// metadata element the SeekHead points to resolved.
    pub fn is_ready(&self) -> bool {
        !self.tracks().is_empty()
            && match &self.control {
                Control::Head => false,
                Control::Clusters { activity, .. } => match activity {
                    Activity::Opening(_) | Activity::Metadata(_) => false,
                    Activity::Blocks => true,
                    Activity::Seek(seek) => seek.resolver().is_none(),
                },
            }
    }
    /// Answers the current [`request`](Self::request) with `window`. A refused window
    /// leaves the reader unchanged: a stale token ([`Error::StaleGeneration`],
    /// [`Error::StaleRequest`]), another source or pool, more bytes than requested
    /// ([`Error::Limit`]), or a window at the wrong offset ([`Error::Invalid`]).
    pub fn provide(&mut self, window: &InputWindow) -> Result<()> {
        if let Some(index) = &self.stream.seek_index {
            index.verify(window)?;
        }
        self.stream.envelope.provide(window)?;
        self.restart_assessment();
        Ok(())
    }
    /// Answers the current request with the end of the source. A source that ends
    /// before its known length is [`Error::Truncated`]; a stale token is refused as
    /// by [`provide`](Self::provide).
    pub fn finish(&mut self, source: SourceId, token: InputToken) -> Result<()> {
        self.stream.envelope.finish(source, token)
    }
    /// Answers the current request with a failure; later steps return
    /// [`Error::Transport`] or [`Error::Cancelled`] to match it.
    pub fn fail(
        &mut self,
        source: SourceId,
        token: InputToken,
        failure: SourceFailure,
    ) -> Result<()> {
        self.stream.envelope.fail(source, token, failure)
    }
    /// Restarts at offset 0 in a new input generation, dropping the configuration,
    /// metadata and seek state, and returns the first request. Packets and documents
    /// the caller holds stay valid and charged.
    pub fn reset(&mut self) -> Result<InputRequest> {
        let stream = &mut self.stream;
        let request = stream.envelope.reset()?;
        stream.state = State::Envelope;
        self.gate = Gate::Open;
        self.control = Control::Head;
        stream.configuration = None;
        stream.catalogue = Catalogue::new(&stream.pool);
        stream.cues = None;
        stream.scale = 1_000_000;
        stream.cluster_time = None;
        stream.last_location = None;
        stream.segment_data = 0;
        stream.segment_end = None;
        stream.revision = 0;
        stream.state_coverage_end = 0;
        if let Some(index) = &mut stream.seek_index {
            index.reset(stream.limits.index_entries);
        }
        stream.state_coverage_contiguous = true;
        stream.state_seen = false;
        stream.metadata_used = 0;
        stream.loaded = CopyVec::new(&stream.pool);
        stream.seek_targets = CopyVec::new(&stream.pool);
        stream.replay_prefix = None;
        stream.scan_remaining = None;
        stream.demux_read_bytes = 0;
        Ok(request)
    }
    /// One bounded unit of work. `End` is the end of the Segment at the
    /// reader's current input position.
    #[inline]
    pub fn step(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> ReaderStep<Event> {
        let result = self.step_projected::<TypedProjection>(work, cancel);
        let stream = &mut self.stream;
        if stream.state_coverage_contiguous
            && let Some(index) = &mut stream.seek_index
            && index.is_collecting()
        {
            if stream.state_seen || stream.cues.as_ref().is_some_and(|cues| !cues.is_empty()) {
                index.discard();
            } else {
                match &result {
                    Ok(Step::Ready(Event::Packet(packet))) => {
                        if packet.configuration.tracks()[packet.packet.track].kind
                            == crate::track::Track::VIDEO
                            && let Some(location) = &stream.last_location
                        {
                            index.observe(packet, location.cluster_offset(), work);
                        }
                    }
                    Ok(Step::End(location)) => index.seal(location.offset),
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
    ) -> ReaderStep<D::Event> {
        self.step_projected_with_source::<D>(work, cancel, None, None)
    }
    /// One step with what a blocking adapter over an immutable source lends for it:
    /// `source`, the storage it holds, for the storage assessment, and `root`, its
    /// bytes, for a seek that restores state from them.
    #[inline]
    fn step_projected_with_source<D: PacketProjection>(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        source: Option<&dyn Storage>,
        root: Option<&triergon_core::Bytes>,
    ) -> ReaderStep<D::Event> {
        if let Err(error) = self.stream.envelope.healthy_for_job(work, cancel) {
            self.latch_failure();
            return Err(error);
        }
        match &self.gate {
            Gate::Open => {}
            Gate::Assessing(_) => {
                let result = self.continue_assessment(work, cancel, source);
                if result.is_err() {
                    self.latch_failure();
                }
                return result;
            }
            Gate::Failed => {
                return Err(Error::Invalid(
                    "reader requires explicit seek/reset after error",
                ));
            }
        }
        // The step is written into the place it is returned from: its common
        // outcomes, a packet or a yield, are inspected there and not moved.
        let mut result = match &mut self.control {
            Control::Clusters {
                activity: Activity::Blocks,
                ..
            } => {
                let result = self
                    .stream
                    .step_demux_after_health_check::<D>(work, cancel, None, false);
                self.stream.cover(&result);
                result
            }
            Control::Head => {
                let result = self
                    .stream
                    .step_demux_after_health_check::<D>(work, cancel, None, false);
                self.stream.cover(&result);
                if self.stream.envelope.in_cluster() {
                    self.enter_clusters();
                }
                result
            }
            Control::Clusters { first, activity } => D::progress(step_activity(
                first,
                activity,
                &mut self.stream,
                root,
                work,
                cancel,
            )),
        };
        match &mut result {
            Ok(Step::NeedInput(request)) => {
                if let Err(error) = self.widen_input_request(request) {
                    result = Err(error);
                }
            }
            Err(Error::Backpressure(Backpressure { requested, .. }))
            | Ok(Step::Backpressure(Backpressure { requested, .. })) => {
                let requested = *requested;
                result = self.begin_assessment(requested, work, cancel, source);
            }
            _ => {}
        }
        if result.is_err() {
            self.latch_failure();
        }
        result
    }
    /// The stream has reached its first Cluster: the controllers start from it.
    #[cold]
    #[inline(never)]
    fn enter_clusters(&mut self) {
        if let Some(first) = self.stream.envelope.cluster_checkpoint() {
            self.control = Control::Clusters {
                first,
                activity: Activity::Opening(Opening::new()),
            };
        }
    }
    /// The first step of the storage assessment that follows a refused allocation
    /// of `requested` bytes; the assessment stays at the gate until it is complete.
    #[cold]
    #[inline(never)]
    fn begin_assessment<U>(
        &mut self,
        requested: usize,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        source: Option<&dyn Storage>,
    ) -> ReaderStep<U> {
        self.stream.forget_seek_index();
        let mut pressure = Pressure::new(requested);
        match assess(
            &mut pressure,
            &self.stream,
            &self.control,
            work,
            cancel,
            source,
        )? {
            ParseStep::Event(()) => Ok(Step::Yield),
            step => {
                self.gate = Gate::Assessing(pressure);
                step.waiting()
            }
        }
    }
    /// One more step of the storage assessment at the gate.
    #[cold]
    #[inline(never)]
    fn continue_assessment<U>(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        source: Option<&dyn Storage>,
    ) -> ReaderStep<U> {
        let Gate::Assessing(pressure) = &mut self.gate else {
            return Ok(Step::Yield);
        };
        match assess(pressure, &self.stream, &self.control, work, cancel, source)? {
            ParseStep::Event(()) => {
                self.gate = Gate::Open;
                Ok(Step::Yield)
            }
            step => step.waiting(),
        }
    }
    // The ordinary reader and the blocking adapter use this header policy.
    // Seek and metadata transactions have separate refill boundaries.
    fn ordinary_header_refill(
        &self,
    ) -> Result<Option<crate::element::refill::HeaderRefillRequest>> {
        let stream = &self.stream;
        if !self.control.in_source_order()
            || stream.scan_remaining.is_some()
            || !matches!(&stream.state, State::Envelope)
        {
            return Ok(None);
        }
        let Some(exact) = stream.envelope.exact_header_request() else {
            return Ok(None);
        };
        stream.envelope.bounded_header_refill(&exact, 4096)
    }
    /// Widens the request a step is about to return to the canonical refill: a
    /// header may be answered with up to 4096 bytes, and over an immutable source a
    /// seek's reads may reach as far as its scan allows.
    fn widen_input_request(&self, request: &mut InputRequest) -> Result<()> {
        let mut plan = self.ordinary_header_refill()?;
        if plan.is_none() && self.stream.seek_index.is_some() {
            // These windows are checked against the immutable source. Read
            // ahead within the same scan/parent bounds as the blocking adapter.
            plan = if self.needs_header_input() {
                self.blocking_seek_header_refill(request)?
            } else if self.needs_discard_input() {
                self.blocking_seek_discard_refill(request)?
            } else {
                None
            };
        }
        let Some(plan) = plan else {
            return Ok(());
        };
        if !plan.matches_exact(request) {
            return Err(Error::Invalid("canonical refill request identity changed"));
        }
        request.clone_from(plan.expanded());
        Ok(())
    }
    // This validates an already issued canonical hint; it cannot widen it.
    // The adapter may supply a short prefix under pre-read pressure, exactly
    // as another caller may supply fewer than InputRequest::maximum bytes.
    fn header_refill_for_request(
        &self,
        request: &InputRequest,
    ) -> Result<Option<crate::element::refill::HeaderRefillRequest>> {
        let Some(plan) = self.ordinary_header_refill()? else {
            return Ok(None);
        };
        if !plan.matches_expanded(request) {
            return Err(Error::Invalid("canonical header supply identity changed"));
        }
        Ok(Some(plan))
    }

    /// How many bytes past `request` a seek's scan or state reconstruction lets an
    /// input answer reach; `None` outside those stages and at their stop offset.
    fn seek_read_ahead(&self, request: &InputRequest) -> Result<Option<usize>> {
        let Some(seek) = self.control.seek() else {
            return Ok(None);
        };
        Ok(match seek.read_ahead(&self.stream)? {
            ReadAhead::Exact => None,
            ReadAhead::Free => Some(usize::MAX),
            ReadAhead::Until(stop) => stop
                .checked_sub(request.offset)
                .map(|left| usize::try_from(left).unwrap_or(usize::MAX)),
        })
    }
    // Engine-only supply certificate, separate from the public canonical
    // NeedInput and the ordinary refill policy. This expanded response policy is
    // an explicit new blocking seek contract, not an ordinary-hint override.
    fn blocking_seek_header_refill(
        &self,
        request: &InputRequest,
    ) -> Result<Option<crate::element::refill::HeaderRefillRequest>> {
        let Some(left) = self.seek_read_ahead(request)? else {
            return Ok(None);
        };
        self.stream
            .envelope
            .bounded_header_refill(request, left.min(4096))
    }
    // The same engine-only certificate for a discarded body: bounded by the
    // active stop, parent/source ends and scan allowance. Immutable sources only.
    fn blocking_seek_discard_refill(
        &self,
        request: &InputRequest,
    ) -> Result<Option<crate::element::refill::HeaderRefillRequest>> {
        let Some(left) = self.seek_read_ahead(request)? else {
            return Ok(None);
        };
        self.stream.envelope.bounded_discard_refill(request, left)
    }

    fn needs_header_input(&self) -> bool {
        self.control.resolver().map_or_else(
            || self.stream.envelope.needs_header_input(),
            Resolver::needs_header_input,
        )
    }
    fn needs_discard_input(&self) -> bool {
        self.control.resolver().is_none() && self.stream.envelope.needs_discard_input()
    }
    /// Whether a step failed and no seek or reset has followed.
    fn failed(&self) -> bool {
        matches!(self.gate, Gate::Failed)
    }
    /// Closes the gate after a failure: of a step, or of the blocking adapter's input.
    #[cold]
    #[inline(never)]
    fn latch_failure(&mut self) {
        self.gate = Gate::Failed;
    }
    /// Starts the storage assessment for an allocation of `requested` bytes that the
    /// blocking adapter was refused.
    fn allocation_blocked(&mut self, requested: usize) {
        self.stream.forget_seek_index();
        if !self.failed() {
            self.gate = Gate::Assessing(Pressure::new(requested));
        }
    }
    /// An assessment in progress starts over: the storage it was adding up changed.
    fn restart_assessment(&mut self) {
        if let Gate::Assessing(pressure) = &mut self.gate {
            *pressure = Pressure::new(pressure.requested);
        }
    }
    fn set_adapter_storage(&mut self, bytes: usize) {
        if self.stream.adapter_storage != bytes {
            self.stream.adapter_storage = bytes;
            self.restart_assessment();
        }
    }
}

impl Stream {
    fn generation(&self) -> u64 {
        self.envelope.generation()
    }
    fn position(&self) -> u64 {
        self.envelope.position()
    }
    fn tracks(&self) -> &[crate::track::Track] {
        self.configuration
            .as_ref()
            .map_or(&[], Configuration::tracks)
    }
    /// The track configuration; the input's error until Tracks is read.
    fn configuration(&self) -> Result<&Configuration> {
        self.configuration.as_ref().required("missing Tracks")
    }
    /// Where the block of the packet just returned lies.
    fn block_location(&self) -> Result<&ReplayLocation> {
        self.last_location
            .as_ref()
            .required("missing scanned block location")
    }
    /// Extends the proof that sequential reading met no CodecState up to the input
    /// position, after a step that returned an event.
    #[inline(always)]
    fn cover<T>(&mut self, result: &ReaderStep<T>) {
        if self.state_coverage_contiguous && matches!(result, Ok(Step::Ready(_))) {
            self.state_coverage_end = self.state_coverage_end.max(self.envelope.position());
        }
    }
    /// Drops the keyframe index of an immutable source when storage runs short.
    fn forget_seek_index(&mut self) {
        if let Some(index) = &mut self.seek_index
            && (index.is_collecting() || index.is_complete())
        {
            index.discard();
        }
    }
    /// The element reader's step that carries no event, as the reader's step: the
    /// element reader ends where the source does.
    #[inline(always)]
    fn stalled<T, U>(&self, step: ParseStep<T>) -> ReaderStep<U> {
        Ok(match step {
            ParseStep::Yield => Step::Yield,
            ParseStep::NeedInput(request) => Step::NeedInput(request),
            ParseStep::Backpressure {
                requested,
                available,
            } => Step::Backpressure(Backpressure {
                requested,
                available,
            }),
            ParseStep::Finished => Step::End(self.envelope.location()),
            ParseStep::Event(_) => return Err(Error::Invalid("unexpected job event")),
        })
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
                .ok_or(Error::Policy("operation read bytes"))?;
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
            return Err(Error::Policy("metadata"));
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
            .ok_or(Error::Policy("metadata"))?;
        // Keep each large constructor tied to its selected destination arm,
        // avoiding a merged maximum-sized enum result in this path.
        match header.id {
            EBML => {
                self.state = State::Ebml {
                    header,
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
                    job: SeekHeadJob::new(
                        data,
                        self.segment_data,
                        self.seek_targets.len(),
                        &self.pool,
                    ),
                };
            }
            TIMESTAMP => {
                self.cluster_time = Some(ebml::uint(&data)?);
                self.state = State::Envelope;
            }
            BLOCK_GROUP | SIMPLE_BLOCK => {
                let configuration = self.configuration.as_ref().required("missing Tracks")?;
                let time = self
                    .cluster_time
                    .required("Block before Cluster Timestamp")?;
                self.envelope.refresh_block_replay_location(
                    &mut self.last_location,
                    header,
                    self.cluster_time,
                )?;
                if header.id == SIMPLE_BLOCK {
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
            Some(crate::packet::block_group::GroupLocation {
                source: self.envelope.request().source,
                segment_data: self.segment_data,
                header: header.start,
                data: header.data,
            }),
            time,
            self.scale,
            configuration,
            self.limits,
            &self.pool,
        )?);
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
    ) -> ReaderStep<Event> {
        self.step_demux_mode(work, cancel, stop, false)
    }
    fn step_demux_mode(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        stop: Option<u64>,
        scan_prefix: bool,
    ) -> ReaderStep<Event> {
        self.step_demux_projected::<TypedProjection>(work, cancel, stop, scan_prefix)
    }
    fn step_demux_projected<D: PacketProjection>(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        stop: Option<u64>,
        scan_prefix: bool,
    ) -> ReaderStep<D::Event> {
        self.envelope.healthy_for_job(work, cancel)?;
        self.step_demux_after_health_check::<D>(work, cancel, stop, scan_prefix)
    }

    fn step_demux_after_health_check<D: PacketProjection>(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        stop: Option<u64>,
        scan_prefix: bool,
    ) -> ReaderStep<D::Event> {
        // Every metadata commit returns its event before this call continues.
        // Limits are immutable; the remaining metadata allowance is therefore
        // constant across these pure transitions. Input errors also propagate
        // immediately, with no input-supply callback inside this exclusive call.
        let left = self
            .limits
            .metadata_bytes
            .checked_sub(self.metadata_used)
            .ok_or(Error::Policy("metadata"))?;
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
                        return Ok(Step::End(self.envelope.location()));
                    }
                    if let Some(remaining) = self.scan_remaining {
                        self.envelope.set_scan_remaining(remaining);
                    }
                    let before = self.envelope.position();
                    // A scan with a stop offset needs the header boundary to
                    // check whether the whole block fits before that offset.
                    let admit = match (stop, scan_prefix, self.scan_remaining) {
                        (None, true, _) => Admit::ScanSimpleBlocks,
                        (None, false, None) => Admit::OrdinaryBlocks,
                        _ => Admit::Pause,
                    };
                    let progress = self
                        .envelope
                        .step_typed_compact_after_input_health(admit, work, cancel);
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
                        EnvelopeProgress::Header(header) if !is_segment_metadata(header.id) => {
                            if !work.try_charge(1) {
                                self.state = State::Admission(Admission {
                                    header,
                                    phase: AdmitPhase::Admit { maximum: None },
                                    catalogue: CataloguePrepareJob::new(),
                                });
                                return Ok(Step::Yield);
                            }
                            self.envelope.admit_typed_leaf(header, None, false)?;
                            if scan_prefix
                                && header.id == SIMPLE_BLOCK
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
                                phase: AdmitPhase::Lookup { at: 0 },
                                catalogue: CataloguePrepareJob::new(),
                            });
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
                                }
                                return Ok(Step::Ready(D::envelope(ElementEvent::Start(header))?));
                            }
                            CompactElementEvent::End {
                                id,
                                offset,
                                generation,
                            } => {
                                if id == CLUSTER {
                                    self.cluster_time = None;
                                }
                                return Ok(Step::Ready(D::envelope(ElementEvent::End {
                                    id,
                                    offset,
                                    generation,
                                })?));
                            }
                            CompactElementEvent::Skipped(header) => {
                                return Ok(Step::Ready(D::envelope(ElementEvent::Skipped(
                                    header,
                                ))?));
                            }
                            CompactElementEvent::Leaf(header) => {
                                let data = self.envelope.take_compact_leaf(header)?;
                                self.start_leaf_projected::<D>(header, data)?;
                            }
                        },
                        EnvelopeProgress::Step(other) => {
                            return self.stalled(other);
                        }
                    }
                }
                State::Admission(_) => {
                    if let Some(step) =
                        self.step_admission(left, work, cancel, stop, scan_prefix)?
                    {
                        return Ok(match step {
                            Step::NeedInput(request) => Step::NeedInput(request),
                            Step::Yield => Step::Yield,
                            Step::Backpressure(pressure) => Step::Backpressure(pressure),
                            Step::End(location) => Step::End(location),
                        });
                    }
                }
                State::Ebml { .. }
                | State::Tracks { .. }
                | State::Document { .. }
                | State::Cues { .. } => return self.step_metadata::<D>(work, cancel),
                State::Head { .. } | State::MergeHead { .. } => {
                    if let Some(step) = self.step_seek_head::<D>(work, cancel)? {
                        return Ok(step);
                    }
                }
                // Kept in the loop: a seek scan passes here for every block, and as
                // a function of its own it ran up to 1% more instructions per seek.
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
                        return Ok(Step::Yield);
                    }
                    if job.draining() {
                        if !job.context_matches(&context) {
                            return Err(Error::Invalid("scan drain source context changed"));
                        }
                        if let Some(remaining) = self.scan_remaining {
                            self.envelope.set_scan_remaining(remaining);
                        }
                        let before = self.envelope.position();
                        let progress = self.envelope.step_typed_compact_after_input_health(
                            Admit::Pause,
                            work,
                            cancel,
                        );
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
                            return Ok(Step::Yield);
                        }
                        match progress {
                            EnvelopeProgress::Step(step) => {
                                return self.stalled(step);
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
                            if self
                                .envelope
                                .resolve_scan_prefix(header, drain, work, cancel)?
                            {
                                job.finish();
                                return Ok(Step::Yield);
                            }
                            if !drain {
                                self.state = State::Envelope;
                            }
                        }
                        step => return step.waiting(),
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
                            return Ok(Step::Ready(D::take_simple(job, configuration)?));
                        }
                        SimpleProgress::Finished => self.state = State::Envelope,
                        SimpleProgress::Yield => return Ok(Step::Yield),
                        SimpleProgress::Backpressure {
                            requested,
                            available,
                        } => {
                            return Ok(Step::Backpressure(Backpressure {
                                requested,
                                available,
                            }));
                        }
                    }
                }
                State::Packet(job) => {
                    if let Some(configuration) = job.take_configuration_change() {
                        self.state_seen |= configuration.has_codec_states();
                        self.revision = configuration.revision();
                        self.configuration = Some(configuration.clone());
                        return Ok(Step::Ready(D::configuration(configuration)?));
                    }
                    let prefix = packet_replay_prefix(
                        &mut self.replay_prefix,
                        self.last_location.as_ref(),
                        &self.envelope,
                    )?;
                    let step = job.step_ready(prefix, work, cancel)?;
                    match step {
                        ParseStep::Event(()) => {
                            return Ok(Step::Ready(D::packet(job.take_ready()?)?));
                        }
                        ParseStep::Finished => self.state = State::Envelope,
                        other => return other.waiting(),
                    }
                }
            }
        }
    }

    // The three metadata seams below are compiled into the demux step. Opening a
    // file passes through them once per element and phase; called out of line
    // they cost the metadata workloads up to 1.7% more instructions.

    /// One step of admitting a level-one leaf: the cached-metadata lookup, the
    /// position reservation, the catalogue preparation of a document and the
    /// envelope's admission. `Ok(None)` continues the demux loop.
    #[inline(always)]
    fn step_admission(
        &mut self,
        left: usize,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        stop: Option<u64>,
        scan_prefix: bool,
    ) -> Result<Option<Step<Infallible, Backpressure>>> {
        let State::Admission(Admission {
            header,
            phase,
            catalogue,
        }) = &mut self.state
        else {
            return Err(Error::Invalid("admission outside its state"));
        };
        match phase {
            AdmitPhase::Lookup { at } => {
                if *at == self.loaded.len() {
                    let maximum = leaf_allowance(&self.limits, &self.catalogue, header.id, left)?;
                    if header.size.is_some_and(|size| size > maximum as u64) {
                        return Err(Error::Policy("element bytes"));
                    }
                    *phase = AdmitPhase::Reserve;
                    return Ok(None);
                }
                if !work.try_charge(1) {
                    return Ok(Some(Step::Yield));
                }
                let (position, id) = self.loaded.as_slice()[*at];
                *at += 1;
                if position == header.start {
                    if id != header.id {
                        return Err(Error::Invalid("cached metadata identity changed"));
                    }
                    self.envelope.admit_typed_leaf(*header, None, true)?;
                    self.state = State::Envelope;
                }
            }
            AdmitPhase::Reserve => {
                self.loaded.reserve(1)?;
                *phase = AdmitPhase::WaitReserve;
            }
            AdmitPhase::WaitReserve => match self.loaded.step(work, cancel)? {
                ParseStep::Event(()) => {
                    let maximum = leaf_allowance(&self.limits, &self.catalogue, header.id, left)?;
                    *phase = if document(header.id) {
                        AdmitPhase::Catalogue { maximum }
                    } else {
                        AdmitPhase::Admit {
                            maximum: Some(maximum),
                        }
                    };
                }
                other => return other.waiting().map(Some),
            },
            AdmitPhase::Catalogue { maximum } => {
                match catalogue.step(&mut self.catalogue, self.limits, work, cancel)? {
                    ParseStep::Event(available) => {
                        *phase = AdmitPhase::Admit {
                            maximum: Some((*maximum).min(available)),
                        };
                    }
                    other => return other.waiting().map(Some),
                }
            }
            AdmitPhase::Admit { maximum } => {
                if !work.try_charge(1) {
                    return Ok(Some(Step::Yield));
                }
                self.envelope.admit_typed_leaf(*header, *maximum, false)?;
                // A block touching a finite scan stop keeps the ordinary
                // body/stop scheduling, including work-boundary cases.
                if scan_prefix
                    && header.id == SIMPLE_BLOCK
                    && stop.is_none_or(|stop| {
                        header
                            .size
                            .and_then(|size| header.data.checked_add(size))
                            .is_some_and(|end| end < stop)
                    })
                {
                    self.envelope.arm_scan_prefix(*header)?;
                }
                self.state = State::Envelope;
            }
        }
        Ok(None)
    }

    /// Completes the EBML header, Tracks, an Info, Chapters, Tags or Attachments
    /// document, or Cues, and returns its event.
    #[inline(always)]
    fn step_metadata<D: PacketProjection>(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> ReaderStep<D::Event> {
        match &mut self.state {
            State::Ebml { header, job } => match job.step(work, cancel)? {
                ParseStep::Event(doctype) => {
                    self.webm_vp9_t35 &= doctype == BlockAdditionContainer::WebM;
                    self.metadata_used = job.source().len();
                    let event = ElementEvent::Leaf {
                        header: *header,
                        data: job.source().clone(),
                    };
                    self.state = State::Envelope;
                    Ok(Step::Ready(D::envelope(event)?))
                }
                other => other.waiting(),
            },
            State::Tracks { header, job } => match job.step(work, cancel)? {
                ParseStep::Event(configuration) => {
                    let h = *header;
                    let structural = job.metadata_bytes()?;
                    self.commit_metadata(h, structural)?;
                    self.revision = configuration.revision();
                    self.configuration = Some(configuration.clone());
                    self.state = State::Envelope;
                    Ok(Step::Ready(D::configuration(configuration)?))
                }
                other => other.waiting(),
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
                        Ok(Step::Ready(D::document(commit.document)?))
                    }
                    other => other.waiting(),
                }
            }
            State::Cues { header, job } => match job.step(work, cancel)? {
                ParseStep::Event(cues) => {
                    let h = *header;
                    let source = cues.source;
                    self.commit_metadata(h, source.len())?;
                    self.cues = Some(cues.entries);
                    self.state = State::Envelope;
                    Ok(Step::Ready(D::envelope(ElementEvent::Leaf {
                        header: h,
                        data: source,
                    })?))
                }
                other => other.waiting(),
            },
            _ => Err(Error::Invalid("metadata step outside a metadata job")),
        }
    }

    /// Completes a SeekHead and merges its entries into the seek targets; returns the
    /// SeekHead's envelope event once every entry is in. `Ok(None)` continues the
    /// demux loop.
    #[inline(always)]
    fn step_seek_head<D: PacketProjection>(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<Step<D::Event, Backpressure>>> {
        match &mut self.state {
            State::Head { header, job } => match job.step(work, cancel)? {
                ParseStep::Event(entries) => {
                    let previous_len = self.seek_targets.len();
                    self.seek_targets.reserve(entries.len())?;
                    self.state = State::MergeHead {
                        header: *header,
                        source: job.source().clone(),
                        entries,
                        at: 0,
                        previous_len,
                    };
                }
                other => return other.waiting().map(Some),
            },
            State::MergeHead {
                header,
                source,
                entries,
                at,
                ..
            } => {
                match self.seek_targets.step(work, cancel)? {
                    ParseStep::Event(()) | ParseStep::Finished => (),
                    other => return other.waiting().map(Some),
                }
                if *at == entries.len() {
                    let h = *header;
                    let source = source.clone();
                    self.commit_metadata(h, source.len())?;
                    self.state = State::Envelope;
                    return Ok(Some(Step::Ready(D::envelope(ElementEvent::Leaf {
                        header: h,
                        data: source,
                    })?)));
                }
                if !work.try_charge(1) {
                    return Ok(Some(Step::Yield));
                }
                self.seek_targets.push_precharged(entries.as_slice()[*at])?;
                *at += 1;
            }
            _ => return Err(Error::Invalid("SeekHead step outside a SeekHead job")),
        }
        Ok(None)
    }
}

crate::storage::usage::storage!(Admission, |s| [s.catalogue]);
impl Storage for State {
    fn storage_slots(&self) -> Result<usize> {
        use crate::storage::usage::{Storage, slots};
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
    fn storage_owner(&self, index: usize) -> Result<Option<crate::storage::usage::Owner>> {
        use crate::storage::usage::{Storage, owner};
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
/// The storage the reader holds, in the order the assessment adds it up.
struct Graph<'a> {
    stream: &'a Stream,
    control: &'a Control,
}
crate::storage::usage::storage!(Graph<'_>, |s| [
    s.stream.envelope,
    s.stream.configuration,
    s.stream.catalogue,
    s.stream.cues,
    s.stream.loaded,
    s.stream.seek_targets,
    s.stream.state,
    s.control.resolver(),
    s.control.seek().and_then(Seek::scan),
    s.stream.seek_index.as_deref(),
    s.control.seek().and_then(Seek::plan_job),
    crate::storage::usage::Charge(s.stream.adapter_storage)
]);
impl Storage for Reader {
    fn storage_slots(&self) -> Result<usize> {
        Graph {
            stream: &self.stream,
            control: &self.control,
        }
        .storage_slots()
    }
    fn storage_owner(&self, index: usize) -> Result<Option<crate::storage::usage::Owner>> {
        Graph {
            stream: &self.stream,
            control: &self.control,
        }
        .storage_owner(index)
    }
}

#[cfg(test)]
impl Reader {
    /// The seek in progress.
    fn seek(&self) -> Option<&Seek> {
        self.control.seek()
    }
    /// The first Cluster, once the stream has reached it.
    fn first_cluster(&self) -> Option<&ClusterCheckpoint> {
        match &self.control {
            Control::Head => None,
            Control::Clusters { first, .. } => Some(first),
        }
    }
    /// Whether a storage assessment is in progress.
    fn assessing(&self) -> bool {
        matches!(self.gate, Gate::Assessing(_))
    }
}

#[cfg(test)]
mod tests;

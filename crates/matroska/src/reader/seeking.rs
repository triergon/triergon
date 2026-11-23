use super::*;
use crate::packet::batch::{
    BlockPrefix, FrameParameters, ScanPacketFields, block_prefix, block_track, scan_track_is_plain,
};
use crate::seek::plan::{LandingPlan, ScanPlan, SeekPlan};
use crate::step::check;
use triergon_core::Bytes;

/// A seek from its request to its receipt.
#[derive(Debug)]
pub(super) struct Seek {
    operation: Operation,
    stage: SeekStage,
}
/// What a seek carries from stage to stage.
#[derive(Debug)]
struct Operation {
    request: SeekRequest,
    audio: RecoveryAudio,
    /// Source bytes read for the metadata the SeekHead points at.
    metadata_bytes: u64,
    /// The landing a second scan recovers earlier subtitles and audio for.
    recovery: Option<SubtitleRecovery>,
    /// The Cue's CodecState reference a state reconstruction must reach.
    reference: Option<EpochReference>,
}
/// Where a seek stands. Every stage owns what only it needs and is stepped in place.
#[derive(Debug)]
enum SeekStage {
    /// Reading the metadata the SeekHead points at.
    Metadata(Resolver),
    /// Planning the landing from the Cues; `None` builds the job on the next step.
    Plan(Option<SeekPlanJob>),
    /// Validating the planned Cue in `State::Cue`.
    Cue {
        plan: SeekPlan,
        cue: crate::seek::Cue,
        scan_after_validation: Option<ScanPlan>,
        whole_cluster: bool,
    },
    /// Scanning Clusters for the landing keyframe.
    Scan(ScanState),
    /// Rebuilding codec state up to the landing in `State::Epoch`.
    Epoch(SeekReceipt),
    /// The landing's configuration is returned before the receipt.
    InitialConfiguration(SeekReceipt),
    Receipt(SeekReceipt),
}
/// How far past its request an input answer may reach.
pub(super) enum ReadAhead {
    /// Only what was asked for.
    Exact,
    /// Up to this source offset.
    Until(u64),
    /// As far as the source and the element allow.
    Free,
}
impl Seek {
    /// A seek that first reads the metadata the SeekHead points at.
    pub(super) fn new(request: SeekRequest, audio: RecoveryAudio, pool: &Budget) -> Self {
        Self {
            operation: Operation {
                request,
                audio,
                metadata_bytes: 0,
                recovery: None,
                reference: None,
            },
            stage: SeekStage::Metadata(Resolver::new(pool)),
        }
    }
    pub(super) fn request(&self) -> SeekRequest {
        self.operation.request
    }
    pub(super) fn audio(&self) -> RecoveryAudio {
        self.operation.audio
    }
    pub(super) fn resolver(&self) -> Option<&Resolver> {
        match &self.stage {
            SeekStage::Metadata(resolver) => Some(resolver),
            _ => None,
        }
    }
    pub(super) fn plan_job(&self) -> Option<&SeekPlanJob> {
        match &self.stage {
            SeekStage::Plan(job) => job.as_ref(),
            _ => None,
        }
    }
    pub(super) fn scan(&self) -> Option<&ScanState> {
        match &self.stage {
            SeekStage::Scan(scan) => Some(scan),
            _ => None,
        }
    }
    /// How far past its request an input answer may reach in this stage.
    pub(super) fn read_ahead(&self, stream: &Stream) -> Result<ReadAhead> {
        Ok(match &self.stage {
            SeekStage::Scan(scan) => scan.policy.stop.map_or(ReadAhead::Free, ReadAhead::Until),
            SeekStage::Epoch(_) => {
                let State::Epoch(job) = &stream.state else {
                    return Err(Error::Invalid("missing active epoch"));
                };
                ReadAhead::Until(job.read_ahead_stop())
            }
            // Cue validation and metadata ranges retain their exact path.
            _ => ReadAhead::Exact,
        })
    }
}
/// Destination is the actual validated video RAP. Replay can begin earlier to
/// supply other tracks; neither field changes any source packet timestamp.
#[derive(Clone, Copy, Debug)]
pub(super) struct SubtitleRecovery {
    actual_ns: i64,
    destination_block: u64,
    cutoff: RecoveryCutoff,
    used_index: bool,
    source: SourceId,
    generation: u64,
}

/// Applies only before the validated RAP's physical boundary.
/// The record holds no source owner, output packet, queue or independent cursor.
#[derive(Clone, Copy, Debug)]
pub(super) struct ReplayPrefix {
    pub until_block: u64,
    pub cutoff: RecoveryCutoff,
    pub source: SourceId,
    pub generation: u64,
}

/// A borrow of the controller's source/configuration context, not a
/// second owner, input cursor or timestamp implementation.
pub(super) struct ScanContext<'a> {
    pub configuration: Option<&'a Configuration>,
    pub pool: &'a Budget,
    pub source: SourceId,
    pub generation: u64,
    pub cluster_ticks: Option<u64>,
    pub scale: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SimplePhase {
    Header,
    Track,
    Clock,
    Drain,
    Complete,
}
/// Scalar progress only. Its actual byte owner stays in the envelope's
/// span/direct slots until fallback or successful admission of a discard.
#[derive(Debug)]
pub(super) struct ScanSimple {
    header: ElementHeader,
    source: SourceId,
    revision: Option<u64>,
    cluster_ticks: Option<u64>,
    scale: u64,
    prefix: Option<BlockPrefix>,
    track: usize,
    fields: Option<ScanPacketFields>,
    phase: SimplePhase,
}
impl ScanSimple {
    pub(super) fn new(header: ElementHeader, context: &ScanContext<'_>) -> Self {
        Self {
            header,
            source: context.source,
            revision: context.configuration.map(Configuration::revision),
            cluster_ticks: context.cluster_ticks,
            scale: context.scale,
            prefix: None,
            track: 0,
            fields: None,
            phase: SimplePhase::Header,
        }
    }
    pub(super) fn header(&self) -> ElementHeader {
        self.header
    }
    pub(super) fn context_matches(&self, context: &ScanContext<'_>) -> bool {
        self.source == context.source
            && self.header.generation == context.generation
            && self.cluster_ticks == context.cluster_ticks
            && self.cluster_ticks.is_some()
            && self.scale == context.scale
            && context.configuration.is_some_and(|configuration| {
                Some(configuration.revision()) == self.revision
                    && configuration.generation() == context.generation
                    && configuration.belongs_to(context.pool)
            })
    }
    pub(super) fn draining(&self) -> bool {
        self.phase == SimplePhase::Drain
    }
    pub(super) fn completed_fields(&self) -> Option<ScanPacketFields> {
        if self.phase == SimplePhase::Complete {
            self.fields
        } else {
            None
        }
    }
    pub(super) fn finish(&mut self) {
        self.phase = SimplePhase::Complete;
    }
    /// False means resume the whole body as it is, never an early packet error.
    pub(super) fn step_prefix(
        &mut self,
        bytes: &[u8],
        context: &ScanContext<'_>,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<bool>> {
        check(cancel)?;
        if !work.try_charge(1) {
            return Ok(ParseStep::Yield);
        }
        if !self.context_matches(context) {
            return Ok(ParseStep::Event(false));
        }
        let configuration = context
            .configuration
            .required("missing scan configuration")?;
        if self.phase == SimplePhase::Header {
            let Ok(prefix) = block_prefix(bytes) else {
                return Ok(ParseStep::Event(false));
            };
            if (prefix.flags >> 1) & 3 != 0 {
                return Ok(ParseStep::Event(false));
            }
            self.prefix = Some(prefix);
            self.phase = SimplePhase::Track;
            check(cancel)?;
            if !work.try_charge(1) {
                return Ok(ParseStep::Yield);
            }
        }
        let track = match self.phase {
            SimplePhase::Track => {
                let prefix = self.prefix.required("missing scan block prefix")?;
                loop {
                    let found = match block_track(configuration, self.track, prefix.number) {
                        Ok(Some(track)) if scan_track_is_plain(track) => {
                            self.phase = SimplePhase::Clock;
                            Some(track)
                        }
                        Ok(None) => {
                            self.track += 1;
                            None
                        }
                        _ => return Ok(ParseStep::Event(false)),
                    };
                    check(cancel)?;
                    if !work.try_charge(1) {
                        return Ok(ParseStep::Yield);
                    }
                    if let Some(track) = found {
                        break track;
                    }
                }
            }
            SimplePhase::Clock => configuration
                .tracks()
                .get(self.track)
                .required("missing scan track")?,
            _ => return Err(Error::Invalid("scan prefix already resolved")),
        };
        let prefix = self.prefix.required("missing scan block prefix")?;
        let fields = FrameParameters {
            cluster_ticks: context
                .cluster_ticks
                .required("missing scan Cluster timestamp")?,
            offset: prefix.offset,
            scale: context.scale,
            duration_ticks: None,
            frame: 0,
            frames: 1,
        }
        .scan_fields(self.track, track, prefix.flags & 0x80 != 0);
        let Ok(fields) = fields else {
            return Ok(ParseStep::Event(false));
        };
        self.fields = Some(fields);
        self.phase = SimplePhase::Drain;
        Ok(ParseStep::Event(true))
    }
}

/// Where a scan is with its range of Clusters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScanPhase {
    /// The range is entered on the next step.
    Begin,
    Read,
    /// The range ended with a landing, taken on the next step.
    Land,
}
#[derive(Debug)]
pub(super) struct ScanState {
    policy: ScanPlan,
    plan: SeekPlan,
    index_bytes: u64,
    used_index: bool,
    phase: ScanPhase,
    retried: bool,
    best: Option<(i64, ReplayLocation)>,
    previous: Option<u64>,
    prefix_from_initial: bool,
    cluster_configuration: Option<ScanBoundary>,
    best_configuration: Option<ScanBoundary>,
}
impl ScanState {
    fn new(policy: ScanPlan, plan: SeekPlan, index_bytes: u64, used_index: bool) -> Self {
        Self {
            policy,
            plan,
            index_bytes,
            used_index,
            phase: ScanPhase::Begin,
            retried: false,
            best: None,
            previous: None,
            prefix_from_initial: false,
            cluster_configuration: None,
            best_configuration: None,
        }
    }
}
// Constant-count, charged immutable configuration handles. These snapshots are
// valid only within this one seek/source/generation, never across an I/O entity.
#[derive(Clone, Debug)]
struct ScanBoundary {
    configuration: Configuration,
    source: SourceId,
    cluster: u64,
}
impl ScanBoundary {
    fn validate(
        &self,
        pool: &Budget,
        envelope: &ElementReader,
        location: &ReplayLocation,
    ) -> Result<()> {
        if !self.configuration.belongs_to(pool) {
            return Err(Error::ForeignPool);
        }
        if self.configuration.generation() != envelope.generation() {
            return Err(Error::StaleGeneration {
                expected: envelope.generation(),
                received: self.configuration.generation(),
            });
        }
        if self.source != envelope.request().source || self.cluster != location.cluster_offset() {
            return Err(Error::Invalid(
                "scanned configuration boundary identity changed",
            ));
        }
        Ok(())
    }
}
struct SeekCompletion<'a> {
    actual_ns: i64,
    scanned: u64,
    used_index: bool,
    location: &'a ReplayLocation,
    whole_cluster: bool,
    plan: SeekPlan,
    scanned_boundary: Option<ScanBoundary>,
}
crate::storage::usage::storage!(ScanBoundary, |s| [s.configuration]);
crate::storage::usage::storage!(ScanState, |s| [
    s.cluster_configuration,
    s.best_configuration
]);

impl ScanState {
    /// Notes one scanned packet of the block at `block`, keeping its location when
    /// it improves the landing for `track`.
    fn consider(
        &mut self,
        track: usize,
        pool: &Budget,
        envelope: &ElementReader,
        block: u64,
        fields: ScanPacketFields,
        locate: impl FnOnce() -> Result<ReplayLocation>,
    ) -> Result<()> {
        // Advance the physical boundary for every packet, including other
        // tracks and subsequent frames from the same block.
        let first = self.previous.replace(block) != Some(block);
        if let Some(pts) = fields.pts_ns
            && first
            && fields.track == track
            && fields.keyframe
        {
            let target = self.policy.target_ns;
            let improves = if pts <= target {
                self.best
                    .as_ref()
                    .is_none_or(|(old, _)| *old > target || pts > *old)
            } else {
                // A zero-clamped preroll retry takes precedence over a later keyframe.
                self.policy.allow_first_after_target
                    && (!self.policy.allow_first_after_negative_preroll || self.retried)
                    && self
                        .best
                        .as_ref()
                        .is_none_or(|(old, _)| *old > target && pts < *old)
            };
            if !improves {
                return Ok(());
            }
            let location = locate()?;
            // A scan from the initial configuration notes it at every Cluster start;
            // any other scan notes none.
            if let Some(boundary) = &self.cluster_configuration {
                boundary.validate(pool, envelope, &location)?;
            }
            self.best_configuration
                .clone_from(&self.cluster_configuration);
            self.best = Some((pts, location));
        }
        Ok(())
    }
}

impl Reader {
    /// Starts a seek of video track index `track` to its latest keyframe at or before
    /// `requested_ns` nanoseconds and returns the first input request; steps then
    /// return [`Event::Seek`] before any packet. Fails with [`Error::Unsupported`] for
    /// an unknown or non-video track or a source without random access, and with
    /// [`Error::Invalid`] before the first Cluster has been found.
    pub fn seek_previous(&mut self, track: usize, requested_ns: i64) -> Result<InputRequest> {
        self.begin_seek(
            SeekRequest {
                track,
                requested_ns,
                playback_preroll: false,
            },
            RecoveryAudio::Overlap,
        )
    }
    /// [`seek_previous`](Self::seek_previous) to the target less the largest track
    /// SeekPreRoll, replaying the landing Cluster's interleaved audio so every track
    /// can decode from the target.
    pub fn seek_for_playback(&mut self, track: usize, requested_ns: i64) -> Result<InputRequest> {
        self.begin_seek(
            SeekRequest {
                track,
                requested_ns,
                playback_preroll: true,
            },
            RecoveryAudio::Overlap,
        )
    }
    pub(super) fn begin_seek(
        &mut self,
        request: SeekRequest,
        audio: RecoveryAudio,
    ) -> Result<InputRequest> {
        let result = (|| {
            let stream = &mut self.stream;
            if stream
                .tracks()
                .get(request.track)
                .is_none_or(|track| track.kind != crate::track::Track::VIDEO)
            {
                return Err(Error::Unsupported("seek requires video track"));
            }
            let Control::Clusters { first, activity } = &mut self.control else {
                return Err(Error::Invalid("seek before first Cluster"));
            };
            let input = stream.envelope.reset_to_cluster(first)?;
            stream.cancel_transaction()?;
            stream.configuration = stream.configuration.as_ref().map(|configuration| {
                configuration.initial_rebind(input.token.generation(), configuration.revision())
            });
            stream.state = State::Envelope;
            self.gate = Gate::Open;
            stream.cluster_time = None;
            stream.last_location = None;
            stream.replay_prefix = None;
            stream.scan_remaining = None;
            stream.state_coverage_contiguous = false;
            if let Some(index) = &mut stream.seek_index
                && index.is_collecting()
            {
                index.discard();
            }
            *activity = Activity::Seek(Seek::new(request, audio, &stream.pool));
            Ok(input)
        })();
        if result.is_err() {
            self.gate = Gate::Failed;
        }
        result
    }
}

/// The receipt of a seek that has found its landing; the scan allowance ends here.
fn land(
    stream: &mut Stream,
    plan: SeekPlan,
    actual_ns: i64,
    scanned_bytes: u64,
    used_index: bool,
    replays_from_start: bool,
) -> SeekReceipt {
    stream.scan_remaining = None;
    SeekReceipt {
        requested_ns: plan.requested_ns,
        actual_ns,
        scanned_bytes,
        used_index,
        discard_until_ns: plan.discard_until_ns,
        preroll_ns: plan.preroll_ns,
        replays_from_start,
    }
}

impl Operation {
    fn plan_job(&self, stream: &Stream, first: &ClusterCheckpoint) -> Result<SeekPlanJob> {
        SeekPlanJob::new(
            stream.configuration()?.clone(),
            self.request.track,
            self.request.requested_ns,
            self.request.playback_preroll,
            stream.scale,
            first.offset(),
            self.audio,
        )
    }
    /// The scan that returns to the source's first validated Cluster to find an
    /// earlier real replay RAP, when the landing needs one. No index pointer,
    /// cached output packet or guessed end is substituted for input validation. This
    /// is a bounded correctness pass; its real I/O is included in the one operation's
    /// remaining scan allowance.
    fn recovery_scan(
        &mut self,
        first: &ClusterCheckpoint,
        stream: &mut Stream,
        completion: &SeekCompletion<'_>,
    ) -> Result<Option<ScanState>> {
        if self.recovery.is_some()
            || completion.plan.subtitle_recovery_delay_ns == 0
            || !completion.whole_cluster
        {
            return Ok(None);
        }
        if first.offset() == completion.location.cluster_offset() {
            // This is already complete beginning-of-source replay, including
            // negative CodecDelay and the initial-trim distinction.
            return Ok(None);
        }
        let delay_ns = completion.plan.subtitle_recovery_delay_ns;
        let lower_ns = i64::try_from(i128::from(completion.actual_ns) - i128::from(delay_ns))
            .map_err(|_| Error::Overflow)?;
        let block_start_ticks = if self.audio == RecoveryAudio::BlockStart {
            let video_delay_ns = stream.tracks()[self.request.track].codec_delay_ns;
            let ticks = (i128::from(completion.actual_ns) + i128::from(video_delay_ns))
                / i128::from(stream.scale);
            let start =
                crate::compat::clock::block_start_ticks(ticks, video_delay_ns, stream.scale)?;
            let cutoff = start - i128::from(delay_ns / stream.scale);
            Some(i64::try_from(cutoff).map_err(|_| Error::Overflow)?)
        } else {
            None
        };
        let used = completion
            .scanned
            .checked_add(self.metadata_bytes)
            .ok_or(Error::Overflow)?;
        stream
            .limits
            .scan_bytes
            .checked_sub(used)
            .ok_or(Error::Policy("operation read bytes"))?;
        self.recovery = Some(SubtitleRecovery {
            actual_ns: completion.actual_ns,
            destination_block: completion.location.block_offset(),
            cutoff: RecoveryCutoff {
                lower_ns,
                block_start_ticks,
            },
            used_index: completion.used_index,
            source: stream.envelope.request().source,
            generation: stream.generation(),
        });
        stream.configuration = stream.configuration.as_ref().map(|configuration| {
            configuration.initial_rebind(configuration.generation(), configuration.revision())
        });
        Ok(Some(ScanState::new(
            ScanPlan {
                start: first.offset(),
                stop: Some(completion.location.cluster_offset()),
                target_ns: lower_ns,
                allow_first_after_target: false,
                allow_first_after_negative_preroll: false,
                replay_whole_cluster: true,
            },
            completion.plan,
            completion.scanned,
            completion.used_index,
        )))
    }

    /// The stage that follows a landing: a recovery scan, the reconstruction of
    /// codec state, the landing's configuration, or the receipt.
    fn finish(
        &mut self,
        first: &ClusterCheckpoint,
        stream: &mut Stream,
        completion: SeekCompletion<'_>,
    ) -> Result<SeekStage> {
        if let Some(scan) = self.recovery_scan(first, stream, &completion)? {
            return Ok(SeekStage::Scan(scan));
        }
        let SeekCompletion {
            mut actual_ns,
            scanned,
            mut used_index,
            location,
            whole_cluster,
            plan,
            mut scanned_boundary,
        } = completion;
        let recovering = self.recovery.take();
        if let Some(recovery) = recovering {
            if recovery.source != stream.envelope.request().source
                || recovery.generation != stream.generation()
            {
                return Err(Error::Invalid("subtitle recovery source identity changed"));
            }
            actual_ns = recovery.actual_ns;
            used_index = recovery.used_index;
            stream.replay_prefix = Some(ReplayPrefix {
                until_block: recovery.destination_block,
                cutoff: recovery.cutoff,
                source: recovery.source,
                generation: recovery.generation,
            });
            // A scanned replay boundary cannot replace an outstanding proof
            // about the target CueCodecState. The epoch job
            // validates that reference and snapshots the earlier real boundary.
            if self.reference.is_some() {
                scanned_boundary = None;
            }
        }
        let scanned_bytes = scanned
            .checked_add(self.metadata_bytes)
            .ok_or(Error::Overflow)?;
        let at_first = first.offset() == location.cluster_offset();
        let from_start = recovering.is_none() && self.request.playback_preroll && at_first;
        let receipt = land(
            stream,
            plan,
            actual_ns,
            scanned_bytes,
            used_index,
            from_start,
        );
        if whole_cluster && at_first && self.reference.is_none() {
            // Replay begins before every Block/CodecState in the source. Reset
            // ALL tracks, including audio/subtitle state left by an earlier
            // read or a private seek scan. Cue state in this first Cluster was
            // checked by the forward validator; no prefix input is missing.
            let changed = stream.state_seen
                || stream
                    .configuration
                    .as_ref()
                    .is_some_and(Configuration::has_codec_states);
            if changed {
                stream.revision = stream.revision.checked_add(1).ok_or(Error::Overflow)?;
            }
            stream.configuration = stream.configuration.as_ref().map(|configuration| {
                configuration.initial_rebind(configuration.generation(), stream.revision)
            });
            return Ok(if changed {
                SeekStage::InitialConfiguration(receipt)
            } else {
                SeekStage::Receipt(receipt)
            });
        }
        let replay_offset = if whole_cluster {
            location.cluster_offset()
        } else {
            location.block_offset()
        };
        let needs_epoch = stream.state_seen
            || self.reference.is_some_and(|reference| {
                reference.state != 0 || reference.block >= stream.state_coverage_end
            })
            || replay_offset > stream.state_coverage_end;
        if !needs_epoch {
            return Ok(SeekStage::Receipt(receipt));
        }
        if let Some(boundary) = scanned_boundary {
            if !whole_cluster {
                return Err(Error::Invalid(
                    "scanned epoch requires whole Cluster replay",
                ));
            }
            boundary.validate(&stream.pool, &stream.envelope, location)?;
            stream.revision = stream.revision.checked_add(1).ok_or(Error::Overflow)?;
            stream.configuration = Some(
                boundary
                    .configuration
                    .rebind(stream.generation(), stream.revision),
            );
            // Same observable configuration-before-receipt boundary as an
            // epoch job; its source prefix has already validated in scan.
            return Ok(SeekStage::InitialConfiguration(receipt));
        }
        let remaining = stream
            .limits
            .scan_bytes
            .checked_sub(scanned_bytes)
            .ok_or(Error::Policy("operation read bytes"))?;
        stream.state = State::Epoch(EpochRestoreJob::new(
            stream.configuration()?.clone(),
            EpochTarget {
                anchor: first.clone(),
                replay: location.clone(),
                whole_cluster,
                reference: self.reference,
                segment_data: stream.segment_data,
                remaining,
            },
            stream.limits,
            &stream.pool,
        )?);
        Ok(SeekStage::Epoch(receipt))
    }

    /// The receipt of a landing the immutable source's keyframe index already knows.
    fn indexed_landing(
        &self,
        first: &ClusterCheckpoint,
        stream: &mut Stream,
        plan: SeekPlan,
        policy: ScanPlan,
    ) -> Result<Option<SeekReceipt>> {
        if !policy.replay_whole_cluster || plan.subtitle_recovery_delay_ns != 0 || stream.state_seen
        {
            return Ok(None);
        }
        let Some(landing) = stream
            .seek_index
            .as_ref()
            .and_then(|index| index.landing(self.request.track, policy))
        else {
            return Ok(None);
        };
        let from_start = first.offset() == landing.cluster;
        stream
            .envelope
            .begin_internal_cluster_range(first, landing.cluster)?;
        // begin_seek already reset the configuration to the initial revision.
        // This index exists only after an entire pass with no CodecState.
        Ok(Some(land(
            stream,
            plan,
            landing.timestamp_ns,
            self.metadata_bytes,
            true,
            from_start,
        )))
    }

    /// One step of a scan; `None` once its range has ended with a landing.
    fn advance_scan(
        &mut self,
        scan: &mut ScanState,
        first: &ClusterCheckpoint,
        stream: &mut Stream,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<Step<Event, Backpressure>>> {
        loop {
            check(cancel)?;
            match scan.phase {
                ScanPhase::Begin => {
                    if !work.try_charge(1) {
                        return Ok(Some(Step::Yield));
                    }
                    scan.prefix_from_initial = scan.policy.replay_whole_cluster
                        && scan.policy.start == first.offset()
                        && stream
                            .configuration
                            .as_ref()
                            .is_some_and(|configuration| !configuration.has_codec_states());
                    scan.cluster_configuration = None;
                    scan.best_configuration = None;
                    stream
                        .envelope
                        .begin_internal_cluster_range(first, scan.policy.start)?;
                    stream.state = State::Envelope;
                    stream.cluster_time = None;
                    stream.last_location = None;
                    stream.scan_remaining = Some(if self.recovery.is_some() {
                        stream
                            .limits
                            .scan_bytes
                            .checked_sub(
                                scan.index_bytes
                                    .checked_add(self.metadata_bytes)
                                    .ok_or(Error::Overflow)?,
                            )
                            .ok_or(Error::Policy("operation read bytes"))?
                    } else {
                        stream.limits.scan_bytes
                    });
                    stream.demux_read_bytes = 0;
                    scan.phase = ScanPhase::Read;
                }
                ScanPhase::Land => {
                    if !work.try_charge(1) {
                        return Ok(Some(Step::Yield));
                    }
                    return Ok(None);
                }
                ScanPhase::Read => {}
            }
            // The public step already checked the cursor. This loop
            // retains exclusive access and propagates any new read failure.
            let stop = scan.policy.stop;
            let progress = stream.step_demux_after_health_check::<TypedProjection>(
                work,
                cancel,
                stop,
                stream.replay_prefix.is_none(),
            )?;
            if let State::ScanSimple(job) = &stream.state
                && let Some(fields) = job.completed_fields()
            {
                // The drain checked the source context and completed in this
                // call. Retain a replay location only if it improves the landing.
                let header = job.header();
                stream.state = State::Envelope;
                scan.consider(
                    self.request.track,
                    &stream.pool,
                    &stream.envelope,
                    header.start,
                    fields,
                    || {
                        stream
                            .envelope
                            .block_replay_location(header, stream.cluster_time)
                    },
                )?;
                continue;
            }
            match progress {
                Step::Ready(Event::Packet(packet)) => {
                    let location = stream.block_location()?;
                    let fields = ScanPacketFields {
                        track: packet.packet.track,
                        pts_ns: packet.packet.pts_ns,
                        keyframe: packet.packet.keyframe,
                    };
                    let result = scan.consider(
                        self.request.track,
                        &stream.pool,
                        &stream.envelope,
                        location.block_offset(),
                        fields,
                        || Ok(location.clone()),
                    );
                    // Keep payload/configuration/group owners through every
                    // fallible consideration, then drop them here.
                    drop(packet);
                    result?;
                }
                Step::Ready(Event::Envelope(ElementEvent::Start(header)))
                    if header.id == CLUSTER =>
                {
                    scan.cluster_configuration = if scan.prefix_from_initial {
                        Some(ScanBoundary {
                            configuration: stream.configuration()?.clone(),
                            source: stream.envelope.request().source,
                            cluster: header.start,
                        })
                    } else {
                        None
                    };
                    return Ok(Some(Step::Ready(Event::Envelope(ElementEvent::Start(
                        header,
                    )))));
                }
                Step::Ready(Event::Configuration(configuration)) => drop(configuration),
                Step::End(_) => {
                    if scan.best.is_none() {
                        if scan.policy.allow_first_after_negative_preroll && !scan.retried {
                            scan.policy.target_ns = 0;
                            scan.retried = true;
                            scan.phase = ScanPhase::Begin;
                            scan.previous = None;
                            stream.configuration =
                                stream.configuration.as_ref().map(|configuration| {
                                    configuration.initial_rebind(
                                        configuration.generation(),
                                        configuration.revision(),
                                    )
                                });
                            continue;
                        }
                        return Err(Error::Unsupported("no previous random-access block"));
                    }
                    scan.phase = ScanPhase::Land;
                }
                Step::Ready(Event::Envelope(event)) => {
                    return Ok(Some(Step::Ready(Event::Envelope(event))));
                }
                Step::Ready(Event::Document(document)) => {
                    return Ok(Some(Step::Ready(Event::Document(document))));
                }
                Step::Ready(Event::Seek(receipt)) => {
                    return Ok(Some(Step::Ready(Event::Seek(receipt))));
                }
                Step::NeedInput(request) => return Ok(Some(Step::NeedInput(request))),
                Step::Yield => return Ok(Some(Step::Yield)),
                Step::Backpressure(pressure) => return Ok(Some(Step::Backpressure(pressure))),
            }
        }
    }
    /// Positions the stream at a finished scan's landing and gives the stage that
    /// follows. The scan stays in its stage until that stage exists, so a retry
    /// after a refused allocation finds it.
    fn land_scan(
        &mut self,
        scan: &mut ScanState,
        first: &ClusterCheckpoint,
        stream: &mut Stream,
    ) -> Result<SeekStage> {
        let (actual, location) = scan
            .best
            .as_ref()
            .ok_or(Error::Unsupported("no previous random-access block"))?;
        let bytes = stream
            .demux_read_bytes
            .checked_add(scan.index_bytes)
            .ok_or(Error::Overflow)?;
        stream.state = State::Envelope;
        if scan.policy.replay_whole_cluster {
            stream
                .envelope
                .begin_internal_cluster_range(location.checkpoint(), location.cluster_offset())?;
            stream.cluster_time = None;
        } else {
            stream.envelope.restore_internal_block(location)?;
            stream.cluster_time = location.cluster_time;
        }
        stream.last_location = Some(location.clone());
        self.finish(
            first,
            stream,
            SeekCompletion {
                actual_ns: *actual,
                scanned: bytes,
                used_index: scan.used_index,
                location,
                whole_cluster: scan.policy.replay_whole_cluster,
                plan: scan.plan,
                scanned_boundary: scan.best_configuration.take(),
            },
        )
    }
}

impl Seek {
    /// One step of the seek. Its receipt is its last event.
    pub(super) fn step(
        &mut self,
        first: &ClusterCheckpoint,
        stream: &mut Stream,
        root: Option<&Bytes>,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> ReaderStep<Event> {
        loop {
            check(cancel)?;
            let request = self.operation.request;
            match &mut self.stage {
                SeekStage::Metadata(resolver) => {
                    let Some(step) = resolver.step(stream, work, cancel)? else {
                        self.operation.metadata_bytes = resolver.scanned();
                        self.stage = SeekStage::Plan(Some(self.operation.plan_job(stream, first)?));
                        return Ok(Step::Yield);
                    };
                    return Ok(step);
                }
                SeekStage::Plan(job) => {
                    let job = match job {
                        Some(job) => job,
                        None => job.insert(self.operation.plan_job(stream, first)?),
                    };
                    let cues = stream.cues.as_ref().map_or(&[][..], BudgetVec::as_slice);
                    match job.step(cues, work, cancel)? {
                        ParseStep::Event(plan) => {
                            self.stage = SeekStage::Plan(None);
                            match plan.landing {
                                LandingPlan::Cue {
                                    cue,
                                    scan_after_validation,
                                    whole_cluster,
                                } => {
                                    stream.state = State::Cue(
                                        CueValidationJob::new(
                                            crate::seek::cue_validation::CueTarget {
                                                cue,
                                                track: request.track,
                                                scale: stream.scale,
                                                anchor: first.clone(),
                                                whole_cluster,
                                            },
                                            stream.configuration()?.clone(),
                                            stream.limits,
                                            &stream.pool,
                                        )?
                                        .with_segment_data(stream.segment_data),
                                    );
                                    self.stage = SeekStage::Cue {
                                        plan,
                                        cue,
                                        scan_after_validation,
                                        whole_cluster,
                                    };
                                }
                                LandingPlan::Scan(policy) => {
                                    self.stage = match self
                                        .operation
                                        .indexed_landing(first, stream, plan, policy)?
                                    {
                                        Some(receipt) => SeekStage::Receipt(receipt),
                                        None => {
                                            SeekStage::Scan(ScanState::new(policy, plan, 0, false))
                                        }
                                    };
                                }
                            }
                        }
                        other => return other.waiting(),
                    }
                }
                SeekStage::Cue {
                    plan,
                    cue,
                    scan_after_validation,
                    whole_cluster,
                } => {
                    let State::Cue(job) = &mut stream.state else {
                        return Err(Error::Invalid("missing cue validation job"));
                    };
                    match job.step(&mut stream.envelope, work, cancel)? {
                        ParseStep::Event(landing) => {
                            let (plan, cue, whole_cluster) = (*plan, *cue, *whole_cluster);
                            let scan_after_validation = *scan_after_validation;
                            self.operation.reference = if first.offset() == cue.cluster {
                                // The validator proved the reference while
                                // traversing the first Cluster from its start.
                                None
                            } else {
                                Some(EpochReference {
                                    block: landing.location.block_offset(),
                                    state: cue.codec_state,
                                    track: request.track,
                                    unchanged_cluster_prefix: landing
                                        .unchanged_state_prefix
                                        .then_some(landing.location.cluster_offset()),
                                })
                            };
                            stream.state = State::Envelope;
                            stream.cluster_time = landing.replay_cluster_time;
                            stream.last_location = Some(landing.location.clone());
                            self.stage = if let Some(policy) = scan_after_validation {
                                SeekStage::Scan(ScanState::new(
                                    policy,
                                    plan,
                                    landing.scanned_bytes,
                                    true,
                                ))
                            } else {
                                self.operation.finish(
                                    first,
                                    stream,
                                    SeekCompletion {
                                        actual_ns: landing.actual_ns,
                                        scanned: landing.scanned_bytes,
                                        used_index: true,
                                        location: &landing.location,
                                        whole_cluster,
                                        plan,
                                        scanned_boundary: None,
                                    },
                                )?
                            };
                        }
                        other => return other.waiting(),
                    }
                }
                SeekStage::Scan(scan) => {
                    if let Some(step) = self
                        .operation
                        .advance_scan(scan, first, stream, work, cancel)?
                    {
                        return Ok(step);
                    }
                    self.stage = self.operation.land_scan(scan, first, stream)?;
                    if matches!(self.stage, SeekStage::Scan(_)) {
                        return Ok(Step::Yield);
                    }
                }
                SeekStage::Epoch(receipt) => {
                    let State::Epoch(job) = &mut stream.state else {
                        return Err(Error::Invalid("missing state reconstruction job"));
                    };
                    if let Some(root) = root {
                        job.try_owned_prefix(&mut stream.envelope, root, cancel)?;
                    }
                    match job.step(&mut stream.envelope, work, cancel)? {
                        ParseStep::Event(epoch) => {
                            receipt.scanned_bytes = receipt
                                .scanned_bytes
                                .checked_add(epoch.scanned_bytes)
                                .ok_or(Error::Overflow)?;
                            let receipt = *receipt;
                            stream.revision =
                                stream.revision.checked_add(1).ok_or(Error::Overflow)?;
                            let configuration = epoch
                                .configuration
                                .rebind(stream.generation(), stream.revision);
                            stream.configuration = Some(configuration.clone());
                            stream.cluster_time = epoch.cluster_time;
                            stream.state = State::Envelope;
                            self.stage = SeekStage::Receipt(receipt);
                            return Ok(Step::Ready(Event::Configuration(configuration)));
                        }
                        other => return other.waiting(),
                    }
                }
                SeekStage::InitialConfiguration(receipt) => {
                    stream.envelope.healthy_for_job(work, cancel)?;
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    let configuration = stream.configuration()?.clone();
                    self.stage = SeekStage::Receipt(*receipt);
                    return Ok(Step::Ready(Event::Configuration(configuration)));
                }
                SeekStage::Receipt(receipt) => {
                    stream.envelope.healthy_for_job(work, cancel)?;
                    return Ok(Step::Ready(Event::Seek(*receipt)));
                }
            }
        }
    }
}

#[cfg(test)]
impl Seek {
    /// Whether a second scan is recovering earlier subtitles for a landing.
    pub(super) fn recovering(&self) -> bool {
        self.operation.recovery.is_some()
    }
    /// Whether the seek is rebuilding codec state.
    pub(super) fn rebuilding_state(&self) -> bool {
        matches!(self.stage, SeekStage::Epoch(_))
    }
}

#[cfg(test)]
mod tests;

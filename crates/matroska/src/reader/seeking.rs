use super::*;
use crate::packet::batch::{
    BlockPrefix, FrameParameters, ScanPacketFields, block_prefix, block_track, scan_track_is_plain,
};
use crate::seek::plan::{LandingPlan, ScanPlan, SeekPlan};
use crate::step::check;

/// Where a seek stands. `Idle` exactly while the reader has no pending seek; every
/// other stage owns what only it needs and is stepped in place.
#[derive(Debug)]
pub(super) enum SeekStage {
    Idle,
    /// Planning the landing from the Cues; the job is built once the resolver has
    /// read the metadata the SeekHead points to.
    Plan(Option<SeekPlanJob>),
    /// Validating the planned Cue in `State::Cue`.
    Cue(SeekPlan),
    /// Scanning Clusters for the landing keyframe.
    Scan(ScanState),
    /// A finished scan is on the stack of the step that completes it.
    Completing,
    /// Rebuilding codec state up to the landing in `State::Epoch`.
    Epoch(SeekReceipt),
    /// The landing's configuration is returned before the receipt.
    InitialConfiguration(SeekReceipt),
    Receipt(SeekReceipt),
}
impl SeekStage {
    pub(super) fn plan_job(&self) -> Option<&SeekPlanJob> {
        match self {
            Self::Plan(job) => job.as_ref(),
            _ => None,
        }
    }
    pub(super) fn scan(&self) -> Option<&ScanState> {
        match self {
            Self::Scan(scan) => Some(scan),
            _ => None,
        }
    }
    fn scan_mut(&mut self) -> Result<&mut ScanState> {
        match self {
            Self::Scan(scan) => Ok(scan),
            _ => Err(Error::Invalid("missing seek scan")),
        }
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

/// Applies only before the original validated RAP's real physical boundary.
/// The record holds no source owner, output packet, queue or independent cursor.
#[derive(Clone, Copy, Debug)]
pub(super) struct ReplayPrefix {
    pub until_block: u64,
    pub cutoff: RecoveryCutoff,
    pub source: SourceId,
    pub generation: u64,
}

/// A borrow of the existing controller's source/configuration context, not a
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
/// Scalar progress only. Its actual byte owner stays in the envelope's existing
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
    /// False means resume the unchanged full body, never an early packet error.
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

#[derive(Debug)]
pub(super) struct ScanState {
    policy: ScanPlan,
    plan: SeekPlan,
    index_bytes: u64,
    used_index: bool,
    begun: bool,
    finishing: bool,
    retried: bool,
    best: Option<(i64, ReplayLocation)>,
    previous: Option<u64>,
    read_start: u64,
    prefix_from_initial: bool,
    cluster_configuration: Option<ScanBoundary>,
    best_configuration: Option<ScanBoundary>,
}
impl ScanState {
    pub(super) fn read_ahead_stop(&self) -> Option<u64> {
        self.policy.stop
    }

    pub(super) fn new(
        policy: ScanPlan,
        plan: SeekPlan,
        index_bytes: u64,
        used_index: bool,
    ) -> Self {
        Self {
            policy,
            plan,
            index_bytes,
            used_index,
            begun: false,
            finishing: false,
            retried: false,
            best: None,
            previous: None,
            read_start: 0,
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
            self.best_configuration = if self.prefix_from_initial {
                let boundary = self
                    .cluster_configuration
                    .as_ref()
                    .required("missing scanned Cluster configuration")?;
                boundary.validate(pool, envelope, &location)?;
                Some(boundary.clone())
            } else {
                None
            };
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
            if self
                .tracks()
                .get(request.track)
                .is_none_or(|track| track.kind != 1)
            {
                return Err(Error::Unsupported("seek requires video track"));
            }
            let first = self
                .first_cluster
                .as_ref()
                .required("seek before first Cluster")?;
            let input = self.envelope.reset_to_cluster(first)?;
            self.cancel_transaction()?;
            self.configuration = self.configuration.as_ref().map(|configuration| {
                configuration.initial_rebind(input.token.generation(), configuration.revision())
            });
            self.state = State::Envelope;
            self.failed = false;
            self.pressure = None;
            self.cluster_time = None;
            self.last_location = None;
            self.opening = None;
            self.pending_seek = Some(request);
            self.recovery_audio = audio;
            self.subtitle_recovery = None;
            self.replay_prefix = None;
            self.scan_remaining = None;
            self.state_coverage_contiguous = false;
            if let Some(index) = &mut self.seek_index
                && index.is_collecting()
            {
                index.discard();
            }
            self.epoch_reference = None;
            self.seek_metadata_bytes = 0;
            self.seek_stage = SeekStage::Plan(None);
            self.resolver = Some(Resolver::new(ResolvePurpose::Seek, &self.pool));
            Ok(input)
        })();
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    pub(super) fn start_seek_plan(&mut self) -> Result<()> {
        let request = self.pending_seek.required("missing seek request")?;
        self.seek_stage = SeekStage::Plan(Some(Self::seek_plan_job(
            request,
            self.configuration.as_ref(),
            self.first_cluster.as_ref(),
            self.scale,
            self.recovery_audio,
        )?));
        Ok(())
    }
    fn seek_plan_job(
        request: SeekRequest,
        configuration: Option<&Configuration>,
        first_cluster: Option<&ClusterCheckpoint>,
        scale: u64,
        audio: RecoveryAudio,
    ) -> Result<SeekPlanJob> {
        let configuration = configuration.cloned().required("missing Tracks")?;
        let first = first_cluster.required("missing first Cluster")?.offset();
        SeekPlanJob::new(
            configuration,
            request.track,
            request.requested_ns,
            request.playback_preroll,
            scale,
            first,
            audio,
        )
    }
    /// Return to the same source's first validated Cluster to find an earlier
    /// real replay RAP. No index pointer, cached output packet or guessed end is
    /// substituted for input validation. This is a bounded correctness pass;
    /// its real I/O is included in the one operation's remaining scan allowance.
    fn start_subtitle_recovery(
        &mut self,
        request: SeekRequest,
        completion: &SeekCompletion<'_>,
    ) -> Result<bool> {
        if self.subtitle_recovery.is_some()
            || completion.plan.subtitle_recovery_delay_ns == 0
            || !completion.whole_cluster
        {
            return Ok(false);
        }
        let first = self
            .first_cluster
            .as_ref()
            .required("missing first Cluster")?;
        if first.offset() == completion.location.cluster_offset() {
            // This is already complete beginning-of-source replay, including
            // negative CodecDelay and the original initial-trim distinction.
            return Ok(false);
        }
        let delay_ns = completion.plan.subtitle_recovery_delay_ns;
        let lower_ns = i64::try_from(i128::from(completion.actual_ns) - i128::from(delay_ns))
            .map_err(|_| Error::Overflow)?;
        let block_start_ticks = if self.recovery_audio == RecoveryAudio::BlockStart {
            let video_delay_ns = self.tracks()[request.track].codec_delay_ns;
            let ticks = (i128::from(completion.actual_ns) + i128::from(video_delay_ns))
                / i128::from(self.scale);
            let start = crate::compat::clock::block_start_ticks(ticks, video_delay_ns, self.scale)?;
            let cutoff = start - i128::from(delay_ns / self.scale);
            Some(i64::try_from(cutoff).map_err(|_| Error::Overflow)?)
        } else {
            None
        };
        let used = completion
            .scanned
            .checked_add(self.seek_metadata_bytes)
            .ok_or(Error::Overflow)?;
        self.limits
            .scan_bytes
            .checked_sub(used)
            .ok_or(Error::Policy("operation read bytes"))?;
        self.subtitle_recovery = Some(SubtitleRecovery {
            actual_ns: completion.actual_ns,
            destination_block: completion.location.block_offset(),
            cutoff: RecoveryCutoff {
                lower_ns,
                block_start_ticks,
            },
            used_index: completion.used_index,
            source: self.envelope.request().source,
            generation: self.generation(),
        });
        self.configuration = self.configuration.as_ref().map(|configuration| {
            configuration.initial_rebind(configuration.generation(), configuration.revision())
        });
        self.seek_stage = SeekStage::Scan(ScanState::new(
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
        ));
        Ok(true)
    }

    fn finish_seek(&mut self, request: SeekRequest, completion: SeekCompletion<'_>) -> Result<()> {
        if self.start_subtitle_recovery(request, &completion)? {
            return Ok(());
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
        let recovering = self.subtitle_recovery.take();
        if let Some(recovery) = recovering {
            if recovery.source != self.envelope.request().source
                || recovery.generation != self.generation()
            {
                return Err(Error::Invalid("subtitle recovery source identity changed"));
            }
            actual_ns = recovery.actual_ns;
            used_index = recovery.used_index;
            self.replay_prefix = Some(ReplayPrefix {
                until_block: recovery.destination_block,
                cutoff: recovery.cutoff,
                source: recovery.source,
                generation: recovery.generation,
            });
            // A scanned replay boundary cannot replace an outstanding proof
            // about the original target CueCodecState. The existing epoch job
            // validates that reference and snapshots the earlier real boundary.
            if self.epoch_reference.is_some() {
                scanned_boundary = None;
            }
        }
        let scanned_bytes = scanned
            .checked_add(self.seek_metadata_bytes)
            .ok_or(Error::Overflow)?;
        let from_start = recovering.is_none()
            && request.playback_preroll
            && self
                .first_cluster
                .as_ref()
                .is_some_and(|first| first.offset() == location.cluster_offset());
        let receipt = self.land(plan, actual_ns, scanned_bytes, used_index, from_start);
        let starts_at_initial_configuration = whole_cluster
            && self
                .first_cluster
                .as_ref()
                .is_some_and(|first| first.offset() == location.cluster_offset())
            && self.epoch_reference.is_none();
        if starts_at_initial_configuration {
            // Replay begins before every Block/CodecState in the source. Reset
            // ALL tracks, including audio/subtitle state left by an earlier
            // read or a private seek scan. Cue state in this first Cluster was
            // checked by the forward validator; no prefix input is missing.
            let changed = self.state_seen
                || self
                    .configuration
                    .as_ref()
                    .is_some_and(Configuration::has_codec_states);
            if changed {
                self.revision = self.revision.checked_add(1).ok_or(Error::Overflow)?;
            }
            self.configuration = self.configuration.as_ref().map(|configuration| {
                configuration.initial_rebind(configuration.generation(), self.revision)
            });
            self.seek_stage = if changed {
                SeekStage::InitialConfiguration(receipt)
            } else {
                SeekStage::Receipt(receipt)
            };
            return Ok(());
        }
        let replay_offset = if whole_cluster {
            location.cluster_offset()
        } else {
            location.block_offset()
        };
        let needs_epoch = self.state_seen
            || self.epoch_reference.is_some_and(|reference| {
                reference.state != 0 || reference.block >= self.state_coverage_end
            })
            || replay_offset > self.state_coverage_end;
        if needs_epoch {
            if let Some(boundary) = scanned_boundary {
                if !whole_cluster {
                    return Err(Error::Invalid(
                        "scanned epoch requires whole Cluster replay",
                    ));
                }
                boundary.validate(&self.pool, &self.envelope, location)?;
                self.revision = self.revision.checked_add(1).ok_or(Error::Overflow)?;
                self.configuration = Some(
                    boundary
                        .configuration
                        .rebind(self.generation(), self.revision),
                );
                // Same observable configuration-before-receipt boundary as an
                // epoch job; its source prefix has already validated in scan.
                self.seek_stage = SeekStage::InitialConfiguration(receipt);
                return Ok(());
            }
            let remaining = self
                .limits
                .scan_bytes
                .checked_sub(scanned_bytes)
                .ok_or(Error::Policy("operation read bytes"))?;
            self.state = State::Epoch(EpochRestoreJob::new(
                self.configuration
                    .clone()
                    .required("missing state configuration")?,
                EpochTarget {
                    anchor: self
                        .first_cluster
                        .clone()
                        .required("missing first Cluster")?,
                    replay: location.clone(),
                    whole_cluster,
                    reference: self.epoch_reference,
                    segment_data: self.segment_data,
                    remaining,
                },
                self.limits,
                &self.pool,
            )?);
            self.seek_stage = SeekStage::Epoch(receipt);
        } else {
            self.seek_stage = SeekStage::Receipt(receipt);
        }
        Ok(())
    }
    /// The receipt of a seek that has found its landing; the scan allowance ends here.
    fn land(
        &mut self,
        plan: SeekPlan,
        actual_ns: i64,
        scanned_bytes: u64,
        used_index: bool,
        replays_from_start: bool,
    ) -> SeekReceipt {
        self.scan_remaining = None;
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

    fn try_immutable_seek(
        &mut self,
        request: SeekRequest,
        plan: SeekPlan,
        policy: ScanPlan,
    ) -> Result<bool> {
        if !policy.replay_whole_cluster || plan.subtitle_recovery_delay_ns != 0 || self.state_seen {
            return Ok(false);
        }
        let Some(landing) = self
            .seek_index
            .as_ref()
            .and_then(|index| index.landing(request.track, policy))
        else {
            return Ok(false);
        };
        let first = self
            .first_cluster
            .as_ref()
            .required("missing first Cluster")?;
        let from_start = first.offset() == landing.cluster;
        self.envelope
            .begin_internal_cluster_range(first, landing.cluster)?;
        // begin_seek already reset the configuration to the initial revision.
        // This index exists only after an entire pass with no CodecState.
        let receipt = self.land(
            plan,
            landing.timestamp_ns,
            self.seek_metadata_bytes,
            true,
            from_start,
        );
        self.seek_stage = SeekStage::Receipt(receipt);
        Ok(true)
    }

    pub(super) fn step_seeking(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<Event>> {
        loop {
            check(cancel)?;
            let request = self
                .pending_seek
                .required("missing pending seek operation")?;
            match &mut self.seek_stage {
                SeekStage::Idle => return Err(Error::Invalid("missing pending seek operation")),
                SeekStage::Plan(job) => {
                    let job = match job {
                        Some(job) => job,
                        None => job.insert(Self::seek_plan_job(
                            request,
                            self.configuration.as_ref(),
                            self.first_cluster.as_ref(),
                            self.scale,
                            self.recovery_audio,
                        )?),
                    };
                    let cues = self.cues.as_ref().map_or(&[][..], BudgetVec::as_slice);
                    match job.step(cues, work, cancel)? {
                        ParseStep::Event(plan) => {
                            self.seek_stage = SeekStage::Plan(None);
                            match plan.landing {
                                LandingPlan::Cue {
                                    cue, whole_cluster, ..
                                } => {
                                    let configuration =
                                        self.configuration.clone().required("missing Tracks")?;
                                    let first = self
                                        .first_cluster
                                        .clone()
                                        .required("missing first Cluster")?;
                                    self.state = State::Cue(
                                        CueValidationJob::new(
                                            crate::seek::cue_validation::CueTarget {
                                                cue,
                                                track: request.track,
                                                scale: self.scale,
                                                anchor: first,
                                                whole_cluster,
                                            },
                                            configuration,
                                            self.limits,
                                            &self.pool,
                                        )?
                                        .with_segment_data(self.segment_data),
                                    );
                                    self.seek_stage = SeekStage::Cue(plan);
                                }
                                LandingPlan::Scan(policy) => {
                                    if !self.try_immutable_seek(request, plan, policy)? {
                                        self.seek_stage =
                                            SeekStage::Scan(ScanState::new(policy, plan, 0, false));
                                    }
                                }
                            }
                        }
                        other => return crate::step::pending(other),
                    }
                }
                SeekStage::Cue(plan) => {
                    let State::Cue(job) = &mut self.state else {
                        return Err(Error::Invalid("missing cue validation job"));
                    };
                    match job.step(&mut self.envelope, work, cancel)? {
                        ParseStep::Event(landing) => {
                            let plan = *plan;
                            let LandingPlan::Cue {
                                cue,
                                scan_after_validation,
                                whole_cluster,
                            } = plan.landing
                            else {
                                return Err(Error::Invalid("invalid cue seek plan"));
                            };
                            self.epoch_reference = if self
                                .first_cluster
                                .as_ref()
                                .is_some_and(|first| first.offset() == cue.cluster)
                            {
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
                            self.state = State::Envelope;
                            self.cluster_time = landing.replay_cluster_time;
                            self.last_location = Some(landing.location.clone());
                            if let Some(policy) = scan_after_validation {
                                self.seek_stage = SeekStage::Scan(ScanState::new(
                                    policy,
                                    plan,
                                    landing.scanned_bytes,
                                    true,
                                ));
                            } else {
                                self.finish_seek(
                                    request,
                                    SeekCompletion {
                                        actual_ns: landing.actual_ns,
                                        scanned: landing.scanned_bytes,
                                        used_index: true,
                                        location: &landing.location,
                                        whole_cluster,
                                        plan,
                                        scanned_boundary: None,
                                    },
                                )?;
                            }
                        }
                        other => return crate::step::pending(other),
                    }
                }
                SeekStage::Scan(_) => return self.advance_scan(request, work, cancel),
                SeekStage::Completing => return Err(Error::Invalid("missing seek scan")),
                SeekStage::Epoch(receipt) => {
                    let State::Epoch(job) = &mut self.state else {
                        return Err(Error::Invalid("missing state reconstruction job"));
                    };
                    if let Some(root) = self.owned_root.as_ref() {
                        job.try_owned_prefix(&mut self.envelope, root, cancel)?;
                    }
                    match job.step(&mut self.envelope, work, cancel)? {
                        ParseStep::Event(epoch) => {
                            receipt.scanned_bytes = receipt
                                .scanned_bytes
                                .checked_add(epoch.scanned_bytes)
                                .ok_or(Error::Overflow)?;
                            let receipt = *receipt;
                            self.revision = self.revision.checked_add(1).ok_or(Error::Overflow)?;
                            let configuration =
                                epoch.configuration.rebind(self.generation(), self.revision);
                            self.configuration = Some(configuration.clone());
                            self.cluster_time = epoch.cluster_time;
                            self.state = State::Envelope;
                            self.seek_stage = SeekStage::Receipt(receipt);
                            return Ok(ParseStep::Event(Event::Configuration(configuration)));
                        }
                        other => return crate::step::pending(other),
                    }
                }
                SeekStage::InitialConfiguration(receipt) => {
                    self.envelope.healthy_for_job(work, cancel)?;
                    if !work.try_charge(1) {
                        return Ok(ParseStep::Yield);
                    }
                    let configuration = self
                        .configuration
                        .clone()
                        .required("missing initial replay configuration")?;
                    self.seek_stage = SeekStage::Receipt(*receipt);
                    return Ok(ParseStep::Event(Event::Configuration(configuration)));
                }
                SeekStage::Receipt(receipt) => {
                    self.envelope.healthy_for_job(work, cancel)?;
                    let receipt = *receipt;
                    self.pending_seek = None;
                    self.seek_stage = SeekStage::Idle;
                    return Ok(ParseStep::Event(Event::Seek(receipt)));
                }
            }
        }
    }
    fn advance_scan(
        &mut self,
        request: SeekRequest,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<Event>> {
        loop {
            check(cancel)?;
            let scan = self.seek_stage.scan_mut()?;
            if !scan.begun {
                if !work.try_charge(1) {
                    return Ok(ParseStep::Yield);
                }
                let first = self
                    .first_cluster
                    .as_ref()
                    .required("missing first Cluster")?;
                scan.prefix_from_initial = scan.policy.replay_whole_cluster
                    && scan.policy.start == first.offset()
                    && self
                        .configuration
                        .as_ref()
                        .is_some_and(|configuration| !configuration.has_codec_states());
                scan.cluster_configuration = None;
                scan.best_configuration = None;
                self.envelope
                    .begin_internal_cluster_range(first, scan.policy.start)?;
                self.state = State::Envelope;
                self.cluster_time = None;
                self.last_location = None;
                self.scan_remaining = Some(if self.subtitle_recovery.is_some() {
                    self.limits
                        .scan_bytes
                        .checked_sub(
                            scan.index_bytes
                                .checked_add(self.seek_metadata_bytes)
                                .ok_or(Error::Overflow)?,
                        )
                        .ok_or(Error::Policy("operation read bytes"))?
                } else {
                    self.limits.scan_bytes
                });
                self.demux_read_bytes = 0;
                scan.read_start = 0;
                scan.begun = true;
            }
            if scan.finishing {
                if !work.try_charge(1) {
                    return Ok(ParseStep::Yield);
                }
                return self.complete_scan(request, work, cancel);
            }
            // The public step already checked the cursor. This loop
            // retains exclusive access and propagates any new read failure.
            let stop = scan.policy.stop;
            let progress = self.step_demux_after_health_check::<TypedProjection>(
                work,
                cancel,
                stop,
                self.replay_prefix.is_none(),
            )?;
            let scan = self.seek_stage.scan_mut()?;
            if let State::ScanSimple(job) = &self.state
                && let Some(fields) = job.completed_fields()
            {
                // The drain checked the source context and completed in this
                // call. Retain a replay location only if it improves the landing.
                let header = job.header();
                self.state = State::Envelope;
                scan.consider(
                    request.track,
                    &self.pool,
                    &self.envelope,
                    header.start,
                    fields,
                    || {
                        self.envelope
                            .block_replay_location(header, self.cluster_time)
                    },
                )?;
                continue;
            }
            match progress {
                ParseStep::Event(Event::Packet(packet)) => {
                    let location = self
                        .last_location
                        .as_ref()
                        .required("missing scanned block location")?;
                    let fields = ScanPacketFields {
                        track: packet.packet.track,
                        pts_ns: packet.packet.pts_ns,
                        keyframe: packet.packet.keyframe,
                    };
                    let result = scan.consider(
                        request.track,
                        &self.pool,
                        &self.envelope,
                        location.block_offset(),
                        fields,
                        || Ok(location.clone()),
                    );
                    // Keep payload/configuration/group owners through every
                    // fallible consideration, then drop at the old arm boundary.
                    drop(packet);
                    result?;
                }
                ParseStep::Event(Event::Envelope(ElementEvent::Start(header)))
                    if header.id == CLUSTER =>
                {
                    scan.cluster_configuration = if scan.prefix_from_initial {
                        Some(ScanBoundary {
                            configuration: self
                                .configuration
                                .clone()
                                .required("missing scanned configuration")?,
                            source: self.envelope.request().source,
                            cluster: header.start,
                        })
                    } else {
                        None
                    };
                    return Ok(ParseStep::Event(Event::Envelope(ElementEvent::Start(
                        header,
                    ))));
                }
                ParseStep::Event(Event::Configuration(configuration)) => drop(configuration),
                ParseStep::Finished => {
                    if scan.best.is_none() {
                        if scan.policy.allow_first_after_negative_preroll && !scan.retried {
                            scan.policy.target_ns = 0;
                            scan.retried = true;
                            scan.begun = false;
                            scan.previous = None;
                            self.configuration = self.configuration.as_ref().map(|configuration| {
                                configuration.initial_rebind(
                                    configuration.generation(),
                                    configuration.revision(),
                                )
                            });
                            continue;
                        }
                        return Err(Error::Unsupported("no previous random-access block"));
                    }
                    scan.finishing = true;
                }
                ParseStep::Event(Event::Envelope(event)) => {
                    return Ok(ParseStep::Event(Event::Envelope(event)));
                }
                ParseStep::Event(Event::Document(document)) => {
                    return Ok(ParseStep::Event(Event::Document(document)));
                }
                ParseStep::Event(Event::Seek(receipt)) => {
                    return Ok(ParseStep::Event(Event::Seek(receipt)));
                }
                other => return crate::step::pending(other),
            }
        }
    }
    /// Positions the reader at a finished scan's landing and completes the seek
    /// from it. The scan leaves its stage for this step: it returns to the stage
    /// when the completion fails, so a retry finds it, and is otherwise dropped
    /// after the step that follows.
    fn complete_scan(
        &mut self,
        request: SeekRequest,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<Event>> {
        let SeekStage::Scan(mut scan) =
            std::mem::replace(&mut self.seek_stage, SeekStage::Completing)
        else {
            return Err(Error::Invalid("missing seek scan"));
        };
        if let Err(error) = self.land_scan(request, &mut scan) {
            if matches!(self.seek_stage, SeekStage::Completing) {
                self.seek_stage = SeekStage::Scan(scan);
            }
            return Err(error);
        }
        if matches!(self.seek_stage, SeekStage::Scan(_)) {
            return Ok(ParseStep::Yield);
        }
        self.step_seeking(work, cancel)
    }
    fn land_scan(&mut self, request: SeekRequest, scan: &mut ScanState) -> Result<()> {
        let (actual, location) = scan
            .best
            .as_ref()
            .ok_or(Error::Unsupported("no previous random-access block"))?;
        let bytes = self
            .demux_read_bytes
            .checked_sub(scan.read_start)
            .and_then(|bytes| bytes.checked_add(scan.index_bytes))
            .ok_or(Error::Overflow)?;
        self.state = State::Envelope;
        if scan.policy.replay_whole_cluster {
            self.envelope
                .begin_internal_cluster_range(location.checkpoint(), location.cluster_offset())?;
            self.cluster_time = None;
        } else {
            self.envelope.restore_internal_block(location)?;
            self.cluster_time = location.cluster_time;
        }
        self.last_location = Some(location.clone());
        self.finish_seek(
            request,
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

#[cfg(test)]
mod tests;

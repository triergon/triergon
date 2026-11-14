use super::*;
use crate::matroska::typed::syntax::Required;
use crate::matroska::typed::{
    packet::{
        BlockPrefix, FrameParameters, ScanPacketFields, block_prefix, block_track,
        scan_track_is_plain,
    },
    seek::{LandingPlan, ScanPlan, SeekPlan},
    syntax::check,
};

#[derive(Clone, Copy, Debug)]
pub(super) enum SeekMode {
    None,
    Plan,
    Cue,
    Scan,
    Epoch,
    InitialConfiguration,
    Receipt,
}
/// Destination is the actual validated video RAP. Replay can begin earlier to
/// supply other tracks; neither field changes any source packet timestamp.
#[derive(Clone, Copy, Debug)]
pub(super) struct SubtitleRecovery {
    actual_ns: i64,
    destination_block: u64,
    lower_ns: i64,
    used_index: bool,
    source: SourceId,
    generation: u64,
}

/// Applies only before the original validated RAP's real physical boundary.
/// The record holds no source owner, output packet, queue or independent cursor.
#[derive(Clone, Copy, Debug)]
pub(super) struct ReplayPrefix {
    pub until_block: u64,
    pub lower_ns: i64,
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

    fn new(policy: ScanPlan, plan: SeekPlan, index_bytes: u64, used_index: bool) -> Self {
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
    fn validate(&self, reader: &Reader, location: &ReplayLocation) -> Result<()> {
        if !self.configuration.belongs_to(&reader.pool) {
            return Err(Error::ForeignPool);
        }
        if self.configuration.generation() != reader.generation() {
            return Err(Error::StaleGeneration {
                expected: reader.generation(),
                received: self.configuration.generation(),
            });
        }
        if self.source != reader.envelope.request().source
            || self.cluster != location.cluster_offset()
        {
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
crate::matroska::typed::usage::storage!(ScanBoundary, |s| [s.configuration]);
crate::matroska::typed::usage::storage!(ScanState, |s| [
    s.cluster_configuration,
    s.best_configuration
]);

impl Reader {
    fn consider_scan_packet(
        &self,
        scan: &mut ScanState,
        block: u64,
        fields: ScanPacketFields,
        locate: impl FnOnce() -> Result<ReplayLocation>,
    ) -> Result<()> {
        let request = self.pending_seek.required("missing seek request")?;
        // Advance the physical boundary for every packet, including other
        // tracks and subsequent frames from the same block.
        let first = scan.previous.replace(block) != Some(block);
        if let Some(pts) = fields.pts_ns
            && first
            && fields.track == request.track
            && fields.keyframe
            && pts <= scan.policy.target_ns
            && scan.best.as_ref().is_none_or(|(old, _)| pts > *old)
        {
            let location = locate()?;
            scan.best_configuration = if scan.prefix_from_initial {
                let boundary = scan
                    .cluster_configuration
                    .as_ref()
                    .required("missing scanned Cluster configuration")?;
                boundary.validate(self, &location)?;
                Some(boundary.clone())
            } else {
                None
            };
            scan.best = Some((pts, location));
        }
        Ok(())
    }
    pub fn seek_previous(&mut self, track: usize, requested_ns: i64) -> Result<InputRequest> {
        self.begin_seek(SeekRequest {
            track,
            requested_ns,
            playback_preroll: false,
        })
    }
    pub fn seek_for_playback(&mut self, track: usize, requested_ns: i64) -> Result<InputRequest> {
        self.begin_seek(SeekRequest {
            track,
            requested_ns,
            playback_preroll: true,
        })
    }
    fn begin_seek(&mut self, request: SeekRequest) -> Result<InputRequest> {
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
            self.plan = None;
            self.seek_plan = None;
            self.scan = None;
            self.subtitle_recovery = None;
            self.replay_prefix = None;
            self.seek_receipt = None;
            self.scan_remaining = None;
            self.state_coverage_contiguous = false;
            if let Some(index) = &mut self.seek_index
                && index.is_collecting()
            {
                index.discard();
            }
            self.epoch_reference = None;
            self.seek_metadata_bytes = 0;
            self.seek_mode = SeekMode::Plan;
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
        let configuration = self.configuration.clone().required("missing Tracks")?;
        let first = self
            .first_cluster
            .as_ref()
            .required("missing first Cluster")?
            .offset();
        self.plan = Some(SeekPlanJob::new(
            configuration,
            request.track,
            request.requested_ns,
            request.playback_preroll,
            self.scale,
            first,
        )?);
        self.seek_mode = SeekMode::Plan;
        Ok(())
    }
    /// Return to the same source's first validated Cluster to find an earlier
    /// real replay RAP. No index pointer, cached output packet or guessed end is
    /// substituted for input validation. This is a bounded correctness pass;
    /// its real I/O is included in the one operation's remaining scan allowance.
    fn start_subtitle_recovery(&mut self, completion: &SeekCompletion<'_>) -> Result<bool> {
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
        let lower_ns = i64::try_from(
            i128::from(completion.actual_ns)
                - i128::from(completion.plan.subtitle_recovery_delay_ns),
        )
        .map_err(|_| Error::Overflow)?;
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
            lower_ns,
            used_index: completion.used_index,
            source: self.envelope.request().source,
            generation: self.generation(),
        });
        self.configuration = self.configuration.as_ref().map(|configuration| {
            configuration.initial_rebind(configuration.generation(), configuration.revision())
        });
        self.scan = Some(ScanState::new(
            ScanPlan {
                start: first.offset(),
                stop: Some(completion.location.cluster_offset()),
                target_ns: lower_ns,
                allow_first_after_negative_preroll: false,
                replay_whole_cluster: true,
            },
            completion.plan,
            completion.scanned,
            completion.used_index,
        ));
        self.seek_mode = SeekMode::Scan;
        Ok(true)
    }

    fn finish_seek(&mut self, completion: SeekCompletion<'_>) -> Result<()> {
        if self.start_subtitle_recovery(&completion)? {
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
                lower_ns: recovery.lower_ns,
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
        let request = self.pending_seek.required("missing seek request")?;
        let scanned_bytes = scanned
            .checked_add(self.seek_metadata_bytes)
            .ok_or(Error::Overflow)?;
        let from_start = recovering.is_none()
            && request.playback_preroll
            && self
                .first_cluster
                .as_ref()
                .is_some_and(|first| first.offset() == location.cluster_offset());
        self.record_seek_receipt(plan, actual_ns, scanned_bytes, used_index, from_start);
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
                    .is_some_and(|configuration| configuration.has_codec_states());
            if changed {
                self.revision = self.revision.checked_add(1).ok_or(Error::Overflow)?;
            }
            self.configuration = self.configuration.as_ref().map(|configuration| {
                configuration.initial_rebind(configuration.generation(), self.revision)
            });
            self.seek_mode = if changed {
                SeekMode::InitialConfiguration
            } else {
                SeekMode::Receipt
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
                boundary.validate(self, location)?;
                self.revision = self.revision.checked_add(1).ok_or(Error::Overflow)?;
                self.configuration = Some(
                    boundary
                        .configuration
                        .rebind(self.generation(), self.revision),
                );
                // Same observable configuration-before-receipt boundary as an
                // epoch job; its source prefix has already validated in scan.
                self.seek_mode = SeekMode::InitialConfiguration;
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
            self.seek_mode = SeekMode::Epoch;
        } else {
            self.seek_mode = SeekMode::Receipt;
        }
        Ok(())
    }
    fn record_seek_receipt(
        &mut self,
        plan: SeekPlan,
        actual_ns: i64,
        scanned_bytes: u64,
        used_index: bool,
        replays_from_start: bool,
    ) {
        self.seek_receipt = Some(crate::matroska::SeekReceipt {
            requested_ns: plan.requested_ns,
            actual_ns,
            scanned_bytes,
            used_index,
            discard_until_ns: plan.discard_until_ns,
            preroll_ns: plan.preroll_ns,
            replays_from_start,
        });
        self.scan_remaining = None;
    }

    fn try_immutable_seek(&mut self, plan: SeekPlan, policy: ScanPlan) -> Result<bool> {
        if !policy.replay_whole_cluster || plan.subtitle_recovery_delay_ns != 0 || self.state_seen {
            return Ok(false);
        }
        let request = self.pending_seek.required("missing seek request")?;
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
        self.record_seek_receipt(
            plan,
            landing.timestamp_ns,
            self.seek_metadata_bytes,
            true,
            from_start,
        );
        self.seek_mode = SeekMode::Receipt;
        Ok(true)
    }

    pub(super) fn step_seeking(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<Event>> {
        loop {
            check(cancel)?;
            match self.seek_mode {
                SeekMode::None => return Err(Error::Invalid("missing pending seek operation")),
                SeekMode::Plan => {
                    if self.plan.is_none() {
                        self.start_seek_plan()?;
                    }
                    let cues = self.cues.as_ref().map_or(&[][..], BudgetVec::as_slice);
                    match self
                        .plan
                        .as_mut()
                        .required("missing cue plan job")?
                        .step(cues, work, cancel)?
                    {
                        ParseStep::Event(plan) => {
                            self.plan = None;
                            self.seek_plan = Some(plan);
                            match plan.landing {
                                LandingPlan::Cue {
                                    cue, whole_cluster, ..
                                } => {
                                    let request =
                                        self.pending_seek.required("missing seek request")?;
                                    let configuration =
                                        self.configuration.clone().required("missing Tracks")?;
                                    let first = self
                                        .first_cluster
                                        .clone()
                                        .required("missing first Cluster")?;
                                    self.state = State::Cue(
                                        CueValidationJob::new(
                                            crate::matroska::typed::seek_validate::CueTarget {
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
                                    self.seek_mode = SeekMode::Cue;
                                }
                                LandingPlan::Scan(policy) => {
                                    if !self.try_immutable_seek(plan, policy)? {
                                        self.scan = Some(ScanState::new(policy, plan, 0, false));
                                        self.seek_mode = SeekMode::Scan;
                                    }
                                }
                            }
                        }
                        other => return crate::matroska::typed::pending(other),
                    }
                }
                SeekMode::Cue => match self.step_cue_validation(work, cancel)? {
                    ParseStep::Event(landing) => {
                        let plan = self.seek_plan.required("missing validated seek plan")?;
                        if let LandingPlan::Cue { cue, .. } = plan.landing {
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
                                    track: self
                                        .pending_seek
                                        .required("missing seek request")?
                                        .track,
                                    unchanged_cluster_prefix: landing
                                        .unchanged_state_prefix
                                        .then_some(landing.location.cluster_offset()),
                                })
                            };
                        }
                        self.state = State::Envelope;
                        self.cluster_time = landing.replay_cluster_time;
                        self.last_location = Some(landing.location.clone());
                        let LandingPlan::Cue {
                            scan_after_validation,
                            whole_cluster,
                            ..
                        } = plan.landing
                        else {
                            return Err(Error::Invalid("invalid cue seek plan"));
                        };
                        if let Some(policy) = scan_after_validation {
                            self.scan =
                                Some(ScanState::new(policy, plan, landing.scanned_bytes, true));
                            self.seek_mode = SeekMode::Scan;
                        } else {
                            self.finish_seek(SeekCompletion {
                                actual_ns: landing.actual_ns,
                                scanned: landing.scanned_bytes,
                                used_index: true,
                                location: &landing.location,
                                whole_cluster,
                                plan,
                                scanned_boundary: None,
                            })?;
                        }
                    }
                    other => return crate::matroska::typed::pending(other),
                },
                SeekMode::Scan => {
                    let mut scan = self.scan.take().required("missing seek scan")?;
                    let result = self.advance_scan(&mut scan, work, cancel);
                    if matches!(self.seek_mode, SeekMode::Scan) && self.scan.is_none() {
                        self.scan = Some(scan);
                    }
                    return result;
                }
                SeekMode::Epoch => {
                    let State::Epoch(job) = &mut self.state else {
                        return Err(Error::Invalid("missing state reconstruction job"));
                    };
                    if let Some(root) = self.owned_root.as_ref() {
                        job.try_owned_prefix(&mut self.envelope, root, cancel)?;
                    }
                    match job.step(&mut self.envelope, work, cancel)? {
                        ParseStep::Event(epoch) => {
                            let receipt = self
                                .seek_receipt
                                .as_mut()
                                .required("missing state seek receipt")?;
                            receipt.scanned_bytes = receipt
                                .scanned_bytes
                                .checked_add(epoch.scanned_bytes)
                                .ok_or(Error::Overflow)?;
                            self.revision = self.revision.checked_add(1).ok_or(Error::Overflow)?;
                            let configuration =
                                epoch.configuration.rebind(self.generation(), self.revision);
                            self.configuration = Some(configuration.clone());
                            self.cluster_time = epoch.cluster_time;
                            self.state = State::Envelope;
                            self.seek_mode = SeekMode::Receipt;
                            return Ok(ParseStep::Event(Event::Configuration(configuration)));
                        }
                        other => return crate::matroska::typed::pending(other),
                    }
                }
                SeekMode::InitialConfiguration => {
                    self.envelope.healthy_for_job(work, cancel)?;
                    if !work.try_charge(1) {
                        return Ok(ParseStep::Yield);
                    }
                    let configuration = self
                        .configuration
                        .clone()
                        .required("missing initial replay configuration")?;
                    self.seek_mode = SeekMode::Receipt;
                    return Ok(ParseStep::Event(Event::Configuration(configuration)));
                }
                SeekMode::Receipt => {
                    self.envelope.healthy_for_job(work, cancel)?;
                    let receipt = self
                        .seek_receipt
                        .take()
                        .required("missing completed seek receipt")?;
                    self.pending_seek = None;
                    self.seek_mode = SeekMode::None;
                    self.seek_plan = None;
                    return Ok(ParseStep::Event(Event::Seek(receipt)));
                }
            }
        }
    }
    #[inline]
    fn step_cue_validation(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<crate::matroska::typed::seek_validate::ValidatedCue>> {
        let State::Cue(job) = &mut self.state else {
            return Err(Error::Invalid("missing cue validation job"));
        };
        job.step(&mut self.envelope, work, cancel)
    }
    fn advance_scan(
        &mut self,
        scan: &mut ScanState,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<Event>> {
        loop {
            check(cancel)?;
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
                    self.envelope.begin_internal_cluster_range(
                        location.checkpoint(),
                        location.cluster_offset(),
                    )?;
                    self.cluster_time = None;
                } else {
                    self.envelope.restore_internal_block(location)?;
                    self.cluster_time = location.cluster_time;
                }
                self.last_location = Some(location.clone());
                self.finish_seek(SeekCompletion {
                    actual_ns: *actual,
                    scanned: bytes,
                    used_index: scan.used_index,
                    location,
                    whole_cluster: scan.policy.replay_whole_cluster,
                    plan: scan.plan,
                    scanned_boundary: scan.best_configuration.take(),
                })?;
                if self.scan.is_some() {
                    return Ok(ParseStep::Yield);
                }
                return self.step_seeking(work, cancel);
            }
            // The public step already checked the cursor. This loop
            // retains exclusive access and propagates any new read failure.
            let progress = self.step_demux_after_health_check::<TypedProjection>(
                work,
                cancel,
                scan.policy.stop,
                self.replay_prefix.is_none(),
            )?;
            if let State::ScanSimple(job) = &self.state
                && let Some(fields) = job.completed_fields()
            {
                // The drain checked the source context and completed in this
                // call. Retain a replay location only if it improves the landing.
                let header = job.header();
                self.state = State::Envelope;
                self.consider_scan_packet(scan, header.start, fields, || {
                    self.envelope
                        .block_replay_location(header, self.cluster_time)
                })?;
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
                    let result =
                        self.consider_scan_packet(scan, location.block_offset(), fields, || {
                            Ok(location.clone())
                        });
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
                other => return crate::matroska::typed::pending(other),
            }
        }
    }
}

#[cfg(test)]
mod scan_prefix_regressions;

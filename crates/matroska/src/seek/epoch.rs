//! Bounded state reconstruction on the envelope's InputCursor. A raw
//! CueCodecState offset is never accepted as an arbitrary binary leaf pointer.
//! Only a CodecState child of a structurally valid BlockGroup for the proper
//! Track can establish the referenced initialization (RFC9559 §§5.1.3.5.6,16,22.1).
use crate::configuration::{Configuration, StateChangeJob, StateCopyJob};
use crate::packet::block_group::{BlockGroupSource, GroupJob, GroupLocation};
use crate::step::{ParseStep, Required, check};
use crate::track::extensions::TrackExtensionsJob;
use crate::{
    Error, Limits, Result, ebml,
    element::{
        BLOCK_GROUP, ClusterCheckpoint, ElementEvent, ElementHeader, ElementReader, EpochAdmission,
        SIMPLE_BLOCK, control::EnvelopeProgress, replay::ReplayLocation,
    },
    storage::SharedBytes,
};
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, WorkBudget};

/// How often the owned-prefix shortcut replaced the ordinary scan (tests only).
#[cfg(test)]
pub(crate) static OWNED_PREFIX_HITS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[derive(Clone, Copy, Debug)]
pub(crate) struct EpochReference {
    pub block: u64,
    pub state: u64,
    pub track: usize,
    // Set only by completed Cue validation; binds the no-state-change proof
    // to its actual Cluster. It is never inferred from a zero CueCodecState.
    pub unchanged_cluster_prefix: Option<u64>,
}
#[derive(Debug)]
pub(crate) struct EpochTarget {
    pub anchor: ClusterCheckpoint,
    pub replay: ReplayLocation,
    pub whole_cluster: bool,
    pub reference: Option<EpochReference>,
    pub segment_data: u64,
    pub remaining: u64,
}
#[derive(Clone, Debug)]
pub(crate) struct RestoredEpoch {
    pub configuration: Configuration,
    pub scanned_bytes: u64,
    pub cluster_time: Option<u64>,
}
/// The bytes of the Block a CodecState may come with.
#[derive(Debug)]
enum Source {
    Group(BlockGroupSource),
    /// A SimpleBlock, which carries no CodecState but whose track matters at the
    /// referenced block.
    Simple(SharedBytes),
}
/// Which track the Block being examined belongs to.
#[derive(Clone, Copy, Debug)]
enum Lookup {
    /// Reading its track number.
    Number,
    /// Comparing that number with the track at index `at`.
    Track { number: u64, at: usize },
}
/// The jobs that take a BlockGroup's CodecState into the configuration.
#[expect(
    clippy::large_enum_variant,
    reason = "one job at a time, inline in the reconstruction; boxing it would allocate per group"
)]
#[derive(Debug)]
enum StateJob {
    /// Checking the group's additions against the track's mappings.
    Mapping(TrackExtensionsJob),
    Copy(StateCopyJob),
    Change(StateChangeJob),
}
/// Where the reconstruction stands. A phase owns what it reads: the Block being
/// examined, the jobs that take its CodecState, the configuration at the boundary.
// An explicit tag byte: in a niche of a job, the envelope loop decodes it on
// every element.
#[expect(
    clippy::large_enum_variant,
    reason = "one phase at a time, inline in the reconstruction; boxing it would allocate per group"
)]
#[derive(Debug)]
#[repr(u8)]
enum Phase {
    Begin,
    /// Elements from the anchor up to the boundary.
    Envelope,
    /// The Block of a BlockGroup or of the referenced SimpleBlock.
    Block {
        header: ElementHeader,
        source: Source,
        lookup: Lookup,
    },
    /// A BlockGroup of the track at index `track` whose additions or CodecState
    /// change the configuration.
    State {
        header: ElementHeader,
        group: BlockGroupSource,
        track: usize,
        job: StateJob,
    },
    /// Positioning the input at the boundary, with the configuration there.
    Replay(Configuration),
    Done,
}
#[derive(Debug)]
pub(crate) struct EpochRestoreJob {
    target: EpochTarget,
    configuration: Configuration,
    // The configuration at the boundary once reading has passed it.
    boundary: Option<Configuration>,
    limits: Limits,
    pool: Budget,
    remaining: u64,
    phase: Phase,
    /// The parser of a BlockGroup and its header; it finishes before the phase
    /// goes on.
    group: Option<(ElementHeader, GroupJob)>,
    reference_checked: bool,
    failed: bool,
}
impl EpochRestoreJob {
    pub(crate) fn new(
        configuration: Configuration,
        target: EpochTarget,
        limits: Limits,
        pool: &Budget,
    ) -> Result<Self> {
        if !configuration.belongs_to(pool) {
            return Err(Error::ForeignPool);
        }
        if target
            .reference
            .is_some_and(|reference| reference.track >= configuration.tracks().len())
        {
            return Err(Error::Invalid("state reference track"));
        }
        if let Some(reference) = target.reference {
            target
                .segment_data
                .checked_add(reference.state)
                .ok_or(Error::Overflow)?;
        }
        let remaining = target.remaining;
        let configuration =
            configuration.initial_rebind(configuration.generation(), configuration.revision());
        Ok(Self {
            target,
            configuration,
            boundary: None,
            limits,
            pool: pool.clone(),
            remaining,
            phase: Phase::Begin,
            group: None,
            reference_checked: false,
            failed: false,
        })
    }
    // Never prefetch over the next boundary whose identity decides whether
    // state reconstruction may finish. The exact boundary header stays exact.
    pub(crate) fn read_ahead_stop(&self) -> u64 {
        self.target
            .reference
            .map_or(self.boundary_offset(), |reference| {
                self.boundary_offset().min(reference.block)
            })
    }
    fn boundary_offset(&self) -> u64 {
        if self.target.whole_cluster {
            self.target.replay.cluster_offset()
        } else {
            self.target.replay.block_offset()
        }
    }
    /// Goes on to the replay from the boundary once reading has passed it and the
    /// referenced block, if any, is checked; reports whether it did.
    fn replay(&mut self) -> bool {
        if self.target.reference.is_some() && !self.reference_checked {
            return false;
        }
        let Some(configuration) = self.boundary.take() else {
            return false;
        };
        self.phase = Phase::Replay(configuration);
        true
    }
    fn check_reference(&mut self, header: ElementHeader) -> Result<()> {
        let Some(reference) = self.target.reference else {
            return Ok(());
        };
        if header.start != reference.block {
            return Ok(());
        }
        self.check_reference_state(reference)
    }
    fn check_reference_state(&mut self, reference: EpochReference) -> Result<()> {
        let actual = match self.configuration.codec_state(reference.track) {
            None => 0,
            Some(state) => {
                state
                    .position()
                    .required("unlocated reconstructed CodecState")?
                    .segment_position
            }
        };
        if actual != reference.state {
            return Err(Error::Invalid(
                "cue CodecState does not match referenced block",
            ));
        }
        self.reference_checked = true;
        Ok(())
    }
    /// The Block with this header is examined: on to the replay, or back to the
    /// envelope.
    fn after_block(&mut self, header: ElementHeader) -> Result<()> {
        self.check_reference(header)?;
        if !self.replay() {
            self.phase = Phase::Envelope;
        }
        Ok(())
    }
    /// An immutable in-memory source already holds the whole prefix. When it
    /// is a plain Cluster/SimpleBlock prefix ending at the validated boundary
    /// Cluster, the result of the ordinary scan follows directly from those
    /// bytes: no CodecState exists in it, the boundary snapshot is the initial
    /// configuration and the reference check runs on it. Every other prefix,
    /// and every error, stays with the ordinary scan below.
    pub(crate) fn try_owned_prefix(
        &mut self,
        reader: &mut ElementReader,
        root: &[u8],
        cancel: &AtomicBool,
    ) -> Result<bool> {
        // The ordinary step's own entry checks come first; any failure is left
        // for that step to report.
        if !matches!(self.phase, Phase::Begin)
            || self.failed
            || !self.target.whole_cluster
            || cancel.load(std::sync::atomic::Ordering::Relaxed)
            || !self.pool.same_pool(reader.budget())
            || self.configuration.generation() != reader.generation()
            || reader
                .healthy_for_job(&mut WorkBudget::new(0), cancel)
                .is_err()
        {
            return Ok(false);
        }
        let boundary = self.boundary_offset();
        let reference = self.target.reference;
        if reference.is_some_and(|reference| reference.unchanged_cluster_prefix != Some(boundary)) {
            return Ok(false);
        }
        let Some(found) = reader.owned_plain_prefix(
            root,
            self.target.anchor.offset(),
            boundary,
            self.target.anchor.segment_end(),
            self.remaining,
            cancel,
        ) else {
            return Ok(false);
        };
        if let Some(reference) = reference {
            let actual = self
                .configuration
                .codec_state(reference.track)
                .and_then(crate::configuration::CodecState::position)
                .map_or(0, |position| position.segment_position);
            if reference.block < found.data
                || reference.block >= found.end
                || actual != reference.state
                || self.configuration.codec_state(reference.track).is_some()
            {
                return Ok(false);
            }
        }
        if reader
            .begin_internal_cluster_range(&self.target.anchor, self.target.anchor.offset())
            .is_err()
        {
            return Ok(false);
        }
        #[cfg(test)]
        OWNED_PREFIX_HITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.remaining -= found.consumed;
        self.reference_checked = reference.is_some();
        self.phase = Phase::Replay(self.configuration.clone());
        Ok(true)
    }
    pub(crate) fn step(
        &mut self,
        reader: &mut ElementReader,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<RestoredEpoch>> {
        if self.failed {
            return Err(Error::Invalid(
                "state reconstruction requires replacement after error",
            ));
        }
        if !self.pool.same_pool(reader.budget()) {
            return Err(Error::ForeignPool);
        }
        if self.configuration.generation() != reader.generation() {
            return Err(Error::StaleGeneration {
                expected: reader.generation(),
                received: self.configuration.generation(),
            });
        }
        let result = (|| {
            reader.healthy_for_job(work, cancel)?;
            self.advance(reader, work, cancel)
        })();
        match result {
            Err(Error::Backpressure(triergon_core::Backpressure {
                requested,
                available,
            })) => Ok(ParseStep::Backpressure {
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
    fn advance(
        &mut self,
        reader: &mut ElementReader,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<RestoredEpoch>> {
        'epoch: loop {
            if let Some((header, job)) = &mut self.group {
                check(cancel)?;
                match job.step(work, cancel)? {
                    ParseStep::Event(group) => {
                        // The group parser consumed syntax/CRC work; parsing the
                        // fixed Block header is another retained bounded phase.
                        self.phase = Phase::Block {
                            header: *header,
                            source: Source::Group(group),
                            lookup: Lookup::Number,
                        };
                        self.group = None;
                    }
                    other => return crate::step::pending(other),
                }
            }
            loop {
                check(cancel)?;
                match &mut self.phase {
                    Phase::Begin => {
                        if !work.try_charge(1) {
                            return Ok(ParseStep::Yield);
                        }
                        reader.begin_internal_cluster_range(
                            &self.target.anchor,
                            self.target.anchor.offset(),
                        )?;
                        self.phase = Phase::Envelope;
                    }
                    Phase::Envelope => {
                        let admission = EpochAdmission {
                            keep: [
                                (!self.target.whole_cluster).then(|| self.boundary_offset()),
                                self.target.reference.map(|reference| reference.block),
                            ],
                        };
                        let step = match reader.step_typed_seek_operation(
                            &mut self.remaining,
                            Some(admission),
                            work,
                            cancel,
                        )? {
                            EnvelopeProgress::Header(header) => {
                                if !work.try_charge(1) {
                                    return Ok(ParseStep::Yield);
                                }
                                let replay_boundary = !self.target.whole_cluster
                                    && header.start == self.boundary_offset();
                                let reference = self
                                    .target
                                    .reference
                                    .is_some_and(|reference| reference.block == header.start);
                                // CodecState is a BlockGroup child. An unrelated
                                // SimpleBlock was never decoded by this job; consume
                                // its real bytes without collecting a useless owner.
                                let skip =
                                    header.id == SIMPLE_BLOCK && !replay_boundary && !reference;
                                reader.admit_typed_leaf(header, None, skip)?;
                                continue;
                            }
                            EnvelopeProgress::Step(step) => step,
                        };
                        match step {
                            ParseStep::Event(ElementEvent::Start(header)) => {
                                if self.target.whole_cluster
                                    && header.id == 0x1f43b675
                                    && header.start == self.boundary_offset()
                                {
                                    self.boundary = Some(self.configuration.clone());
                                    if let Some(reference) =
                                        self.target.reference.filter(|reference| {
                                            reference.unchanged_cluster_prefix == Some(header.start)
                                        })
                                    {
                                        let end = header
                                            .size
                                            .map(|size| {
                                                header.data.checked_add(size).ok_or(Error::Overflow)
                                            })
                                            .transpose()?;
                                        if reference.block < header.data
                                            || end.is_some_and(|end| reference.block >= end)
                                        {
                                            return Err(Error::Invalid(
                                                "validated state prefix outside Cluster",
                                            ));
                                        }
                                        self.check_reference_state(reference)?;
                                    }
                                    self.replay();
                                }
                            }
                            ParseStep::Event(ElementEvent::Leaf { header, data }) => {
                                if !self.target.whole_cluster
                                    && header.start == self.boundary_offset()
                                {
                                    self.boundary = Some(self.configuration.clone());
                                    if self.replay() {
                                        continue;
                                    }
                                }
                                match header.id {
                                    BLOCK_GROUP => {
                                        let job = GroupJob::new(
                                            data,
                                            u64::MAX,
                                            Some(GroupLocation {
                                                source: reader.request().source,
                                                segment_data: self.target.segment_data,
                                                header: header.start,
                                                data: header.data,
                                            }),
                                            self.limits,
                                            &self.pool,
                                        )?;
                                        self.group = Some((header, job));
                                        continue 'epoch;
                                    }
                                    // A SimpleBlock cannot carry CodecState. Its Track
                                    // membership still matters at the referenced RAP.
                                    SIMPLE_BLOCK
                                        if self.target.reference.is_some_and(|reference| {
                                            reference.block == header.start
                                        }) =>
                                    {
                                        self.phase = Phase::Block {
                                            header,
                                            source: Source::Simple(data),
                                            lookup: Lookup::Number,
                                        };
                                    }
                                    _ => (),
                                }
                            }
                            ParseStep::Event(
                                ElementEvent::End { .. } | ElementEvent::Skipped(_),
                            ) => (),
                            ParseStep::Finished => {
                                return Err(Error::Invalid(
                                    "state replay boundary absent from source",
                                ));
                            }
                            other => return crate::step::pending(other),
                        }
                    }
                    Phase::Block {
                        header,
                        source,
                        lookup,
                    } => match *lookup {
                        Lookup::Number => {
                            if !work.try_charge(1) {
                                return Ok(ParseStep::Yield);
                            }
                            let block: &[u8] = match &*source {
                                Source::Group(group) => &group.raw_body()[group.block()],
                                Source::Simple(data) => data,
                            };
                            let (number, width, _) = ebml::vint(block, false)?;
                            block.get(width..width + 3).ok_or(Error::Truncated)?;
                            *lookup = Lookup::Track { number, at: 0 };
                        }
                        Lookup::Track { number, at } => {
                            if !work.try_charge(1) {
                                return Ok(ParseStep::Yield);
                            }
                            let track = self
                                .configuration
                                .tracks()
                                .get(at)
                                .required("unregistered state block track")?;
                            if track.number != number {
                                *lookup = Lookup::Track { number, at: at + 1 };
                                continue;
                            }
                            let header = *header;
                            if self.target.reference.is_some_and(|reference| {
                                reference.block == header.start && reference.track != at
                            }) {
                                return Err(Error::Invalid("cue state reference block track"));
                            }
                            let (group, job) = match source {
                                Source::Group(group) if group.max_addition_id() != 0 => (
                                    group.clone(),
                                    StateJob::Mapping(
                                        self.configuration.track_extensions_job(at, self.limits)?,
                                    ),
                                ),
                                Source::Group(group) if group.codec_state().is_some() => (
                                    group.clone(),
                                    StateJob::Copy(StateCopyJob::new(
                                        group,
                                        self.limits.block_bytes,
                                        &self.pool,
                                    )?),
                                ),
                                _ => {
                                    self.after_block(header)?;
                                    continue;
                                }
                            };
                            self.phase = Phase::State {
                                header,
                                group,
                                track: at,
                                job,
                            };
                        }
                    },
                    Phase::State {
                        header,
                        group,
                        track,
                        job,
                    } => {
                        let (header, track) = (*header, *track);
                        match job {
                            StateJob::Mapping(mapping) => match mapping.step_job(work, cancel)? {
                                ParseStep::Event(mapping) => {
                                    self.configuration
                                        .validate_additions(track, group, &mapping)?;
                                    if group.codec_state().is_some() {
                                        *job = StateJob::Copy(StateCopyJob::new(
                                            group,
                                            self.limits.block_bytes,
                                            &self.pool,
                                        )?);
                                    } else {
                                        self.after_block(header)?;
                                    }
                                }
                                other => return crate::step::pending(other),
                            },
                            StateJob::Copy(copy) => match copy.step(work, cancel)? {
                                ParseStep::Event(state) => {
                                    *job = StateJob::Change(StateChangeJob::new(
                                        self.configuration.clone(),
                                        track,
                                        Some(state),
                                        &self.pool,
                                    )?);
                                }
                                other => return crate::step::pending(other),
                            },
                            StateJob::Change(change) => match change.step(work, cancel)? {
                                ParseStep::Event(configuration) => {
                                    self.configuration = configuration;
                                    self.after_block(header)?;
                                }
                                other => return crate::step::pending(other),
                            },
                        }
                    }
                    Phase::Replay(configuration) => {
                        if !work.try_charge(1) {
                            return Ok(ParseStep::Yield);
                        }
                        let configuration = configuration.clone();
                        let cluster_time = if self.target.whole_cluster {
                            reader.begin_internal_cluster_range(
                                self.target.replay.checkpoint(),
                                self.target.replay.cluster_offset(),
                            )?;
                            None
                        } else {
                            reader.restore_internal_block(&self.target.replay)?;
                            self.target.replay.cluster_time
                        };
                        self.phase = Phase::Done;
                        return Ok(ParseStep::Event(RestoredEpoch {
                            configuration,
                            scanned_bytes: self.target.remaining - self.remaining,
                            cluster_time,
                        }));
                    }
                    Phase::Done => return Ok(ParseStep::Finished),
                }
            }
        }
    }
}
crate::storage::usage::storage!(RestoredEpoch, |s| [s.configuration]);
crate::storage::usage::storage!(EpochRestoreJob, |s| [
    s.configuration,
    s.boundary,
    s.group.as_ref().map(|(_, job)| job),
    s.phase
]);
/// The storage a phase holds, in the order the jobs it runs were listed.
impl crate::storage::usage::Storage for Phase {
    fn storage_slots(&self) -> Result<usize> {
        crate::storage::usage::slots(&self.parts())
    }
    fn storage_owner(&self, index: usize) -> Result<Option<crate::storage::usage::Owner>> {
        crate::storage::usage::owner(&self.parts(), index)
    }
}
impl Phase {
    fn parts(&self) -> [&dyn crate::storage::usage::Storage; 3] {
        const NONE: Option<SharedBytes> = None;
        match self {
            Self::Block { source, .. } => match source {
                Source::Group(group) => [group, &NONE, &NONE],
                Source::Simple(data) => [data, &NONE, &NONE],
            },
            Self::State { group, job, .. } => [
                group,
                match job {
                    StateJob::Mapping(job) => job,
                    StateJob::Copy(job) => job,
                    StateJob::Change(job) => job,
                },
                &NONE,
            ],
            Self::Replay(configuration) => [configuration, &NONE, &NONE],
            Self::Begin | Self::Envelope | Self::Done => [&NONE, &NONE, &NONE],
        }
    }
}

#[cfg(test)]
mod tests;

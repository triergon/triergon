//! Bounded state reconstruction on the existing envelope/InputCursor. A raw
//! CueCodecState offset is never accepted as an arbitrary binary leaf pointer.
//! Only a CodecState child of a structurally valid BlockGroup for the proper
//! Track can establish the referenced initialization (RFC9559 §§5.1.3.5.6,16,22.1).
use super::{
    configuration::{Configuration, StateChangeJob, StateCopyJob},
    mapping::TrackExtensionsJob,
    side_data::{BlockGroupSource, GroupJob, GroupLocation},
    syntax::{Required, check},
};
use crate::{
    Error, Limits, Result, ebml,
    incremental::{
        ClusterCheckpoint, ElementEvent, ElementHeader, ElementReader, EpochAdmission, ParseStep,
        typed_bridge::{EnvelopeProgress, ReplayLocation},
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
#[derive(Debug)]
enum Phase {
    Begin,
    Envelope,
    Group,
    Header,
    Track,
    Mapping,
    Copy,
    Change,
    Replay,
    Done,
}
#[derive(Debug)]
pub(crate) struct EpochRestoreJob {
    target: EpochTarget,
    configuration: Configuration,
    boundary: Option<Configuration>,
    limits: Limits,
    pool: Budget,
    remaining: u64,
    phase: Phase,
    header: Option<ElementHeader>,
    group_job: Option<GroupJob>,
    group: Option<BlockGroupSource>,
    simple: Option<SharedBytes>,
    mapping: Option<TrackExtensionsJob>,
    copy: Option<StateCopyJob>,
    change: Option<StateChangeJob>,
    track_number: u64,
    track: usize,
    reference_checked: bool,
    result: Option<RestoredEpoch>,
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
            header: None,
            group_job: None,
            group: None,
            simple: None,
            mapping: None,
            copy: None,
            change: None,
            track_number: 0,
            track: 0,
            reference_checked: false,
            result: None,
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
    fn ready(&self) -> bool {
        self.boundary.is_some() && (self.target.reference.is_none() || self.reference_checked)
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
    fn after_group(&mut self) -> Result<()> {
        let header = self.header.required("missing state source header")?;
        self.check_reference(header)?;
        self.group = None;
        self.simple = None;
        self.header = None;
        self.phase = if self.ready() {
            Phase::Replay
        } else {
            Phase::Envelope
        };
        Ok(())
    }
    fn start_copy(&mut self) -> Result<()> {
        if let Some(group) = self
            .group
            .as_ref()
            .filter(|group| group.codec_state().is_some())
        {
            self.copy = Some(StateCopyJob::new(
                group,
                self.limits.block_bytes,
                &self.pool,
            )?);
            self.phase = Phase::Copy;
        } else {
            self.after_group()?;
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
                .and_then(|state| state.position())
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
        self.boundary = Some(self.configuration.clone());
        self.reference_checked = reference.is_some();
        self.phase = Phase::Replay;
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
            Err(Error::Backpressure {
                requested,
                available,
            }) => Ok(ParseStep::Backpressure {
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
        loop {
            check(cancel)?;
            match self.phase {
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
                            let skip = header.id == 0xa3 && !replay_boundary && !reference;
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
                                if let Some(reference) = self.target.reference.filter(|reference| {
                                    reference.unchanged_cluster_prefix == Some(header.start)
                                }) {
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
                                if self.ready() {
                                    self.phase = Phase::Replay;
                                }
                            }
                        }
                        ParseStep::Event(ElementEvent::Leaf { header, data }) => {
                            if !self.target.whole_cluster && header.start == self.boundary_offset()
                            {
                                self.boundary = Some(self.configuration.clone());
                                if self.ready() {
                                    self.phase = Phase::Replay;
                                    continue;
                                }
                            }
                            match header.id {
                                0xa0 => {
                                    self.header = Some(header);
                                    self.group_job = Some(GroupJob::new(
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
                                    )?);
                                    self.phase = Phase::Group;
                                }
                                // A SimpleBlock cannot carry CodecState. Its Track
                                // membership still matters at the referenced RAP.
                                0xa3 if self
                                    .target
                                    .reference
                                    .is_some_and(|reference| reference.block == header.start) =>
                                {
                                    self.header = Some(header);
                                    self.simple = Some(data);
                                    self.group = None;
                                    self.phase = Phase::Header;
                                }
                                _ => (),
                            }
                        }
                        ParseStep::Event(ElementEvent::End { .. } | ElementEvent::Skipped(_)) => (),
                        ParseStep::Finished => {
                            return Err(Error::Invalid("state replay boundary absent from source"));
                        }
                        other => return super::pending(other),
                    }
                }
                Phase::Group => match self
                    .group_job
                    .as_mut()
                    .required("missing state group parser")?
                    .step(work, cancel)?
                {
                    ParseStep::Event(group) => {
                        // The group parser consumed syntax/CRC work; parsing the
                        // fixed Block header is another retained bounded phase.
                        self.group = Some(group);
                        self.group_job = None;
                        self.phase = Phase::Header;
                    }
                    other => return super::pending(other),
                },
                Phase::Header => {
                    if !work.try_charge(1) {
                        return Ok(ParseStep::Yield);
                    }
                    let block = match &self.group {
                        Some(group) => &group.raw_body()[group.block()],
                        None => self
                            .simple
                            .as_ref()
                            .required("missing state source Block")?
                            .as_ref(),
                    };
                    let (number, width, _) = ebml::vint(block, false)?;
                    block.get(width..width + 3).ok_or(Error::Truncated)?;
                    self.track_number = number;
                    self.track = 0;
                    self.phase = Phase::Track;
                }
                Phase::Track => {
                    if !work.try_charge(1) {
                        return Ok(ParseStep::Yield);
                    }
                    let track = self
                        .configuration
                        .tracks()
                        .get(self.track)
                        .required("unregistered state block track")?;
                    if track.number != self.track_number {
                        self.track += 1;
                        continue;
                    }
                    let header = self.header.required("missing state source header")?;
                    if self.target.reference.is_some_and(|reference| {
                        reference.block == header.start && reference.track != self.track
                    }) {
                        return Err(Error::Invalid("cue state reference block track"));
                    }
                    if self
                        .group
                        .as_ref()
                        .is_some_and(|group| group.max_addition_id() != 0)
                    {
                        self.mapping = Some(
                            self.configuration
                                .track_extensions_job(self.track, self.limits)?,
                        );
                        self.phase = Phase::Mapping;
                    } else {
                        self.start_copy()?;
                    }
                }
                Phase::Mapping => match self
                    .mapping
                    .as_mut()
                    .required("missing state track extension job")?
                    .step(work, cancel)?
                {
                    ParseStep::Event(mapping) => {
                        self.configuration.validate_additions(
                            self.track,
                            self.group.as_ref().required("missing state source group")?,
                            &mapping,
                        )?;
                        self.mapping = None;
                        self.start_copy()?;
                    }
                    other => return super::pending(other),
                },
                Phase::Copy => match self
                    .copy
                    .as_mut()
                    .required("missing reconstructed state copy")?
                    .step(work, cancel)?
                {
                    ParseStep::Event(state) => {
                        self.change = Some(StateChangeJob::new(
                            self.configuration.clone(),
                            self.track,
                            Some(state),
                            &self.pool,
                        )?);
                        self.copy = None;
                        self.phase = Phase::Change;
                    }
                    other => return super::pending(other),
                },
                Phase::Change => match self
                    .change
                    .as_mut()
                    .required("missing reconstructed state transaction")?
                    .step(work, cancel)?
                {
                    ParseStep::Event(configuration) => {
                        self.configuration = configuration;
                        self.change = None;
                        self.after_group()?;
                    }
                    other => return super::pending(other),
                },
                Phase::Replay => {
                    if !work.try_charge(1) {
                        return Ok(ParseStep::Yield);
                    }
                    let configuration = self
                        .boundary
                        .as_ref()
                        .required("missing state boundary snapshot")?
                        .clone();
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
                    self.result = Some(RestoredEpoch {
                        configuration,
                        scanned_bytes: self.target.remaining - self.remaining,
                        cluster_time,
                    });
                    self.phase = Phase::Done;
                }
                Phase::Done => {
                    return Ok(ParseStep::Event(
                        self.result
                            .as_ref()
                            .required("missing restored epoch")?
                            .clone(),
                    ));
                }
            }
        }
    }
}
super::usage::storage!(RestoredEpoch, |s| [s.configuration]);
super::usage::storage!(EpochRestoreJob, |s| [
    s.configuration,
    s.boundary,
    s.group_job,
    s.group,
    s.simple,
    s.mapping,
    s.copy,
    s.change,
    s.result
]);

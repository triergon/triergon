//! Actual asynchronous Cue target validation through the EXISTING envelope and
//! its sole InputCursor. The complete referenced batch validates before a RAP
//! can qualify. A result positions a replay range. The root independently proves
//! CodecState history before publishing a Seek receipt; this job proves the RAP.
use super::{configuration::Configuration, packet::PacketBatchJob, syntax::check};
use crate::matroska::typed::syntax::Required;
use crate::{
    Error, Limits, Result,
    cues::Cue,
    ebml,
    incremental::{
        ClusterCheckpoint, ElementEvent, ElementHeader, ElementReader, ParseStep,
        typed_bridge::ReplayLocation,
    },
};
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Rounding, Time, TimeBase, WorkBudget};

#[derive(Debug, Clone)]
pub(crate) struct ValidatedCue {
    pub actual_ns: i64,
    pub scanned_bytes: u64,
    pub location: ReplayLocation,
    pub replay_cluster_time: Option<u64>,
    // Proof about the fully validated target Cluster prefix, not Cue trust.
    pub unchanged_state_prefix: bool,
}
#[derive(Debug)]
enum Phase {
    Begin,
    Envelope,
    Batch,
    Drain,
    Replay,
    Done,
}
#[derive(Debug)]
pub(crate) struct CueValidationJob {
    cue: Cue,
    configuration: Configuration,
    track: usize,
    scale: u64,
    anchor: ClusterCheckpoint,
    limits: Limits,
    pool: Budget,
    whole_cluster: bool,
    phase: Phase,
    remaining: u64,
    wanted: i64,
    header: Option<ElementHeader>,
    relative: Option<u64>,
    cluster_time: Option<u64>,
    blocks: u64,
    batch: Option<PacketBatchJob>,
    location: Option<ReplayLocation>,
    first: bool,
    result: Option<ValidatedCue>,
    failed: bool,
    segment_data: Option<u64>,
    saw_state_change: bool,
}
/// A source Cue and the exact requested playback/replay target context.
#[derive(Debug)]
pub(crate) struct CueTarget {
    pub cue: Cue,
    pub track: usize,
    pub scale: u64,
    pub anchor: ClusterCheckpoint,
    pub whole_cluster: bool,
}
impl CueValidationJob {
    pub fn new(
        target: CueTarget,
        configuration: Configuration,
        limits: Limits,
        pool: &Budget,
    ) -> Result<Self> {
        let CueTarget {
            cue,
            track,
            scale,
            anchor,
            whole_cluster,
        } = target;
        if !configuration.belongs_to(pool) {
            return Err(triergon_core::Error::ForeignPool.into());
        }
        if configuration
            .tracks()
            .get(track)
            .is_none_or(|t| t.kind != 1)
        {
            return Err(Error::Unsupported("seek requires video track"));
        }
        if configuration.tracks()[track].number != cue.track {
            return Err(Error::Invalid("cue/configuration track mismatch"));
        }
        if scale == 0 {
            return Err(Error::Invalid("zero TimestampScale"));
        }
        Ok(Self {
            cue,
            configuration,
            track,
            scale,
            anchor,
            limits,
            pool: pool.clone(),
            whole_cluster,
            phase: Phase::Begin,
            remaining: limits.scan_bytes,
            wanted: 0,
            header: None,
            relative: None,
            cluster_time: None,
            blocks: 0,
            batch: None,
            location: None,
            first: true,
            result: None,
            failed: false,
            segment_data: None,
            saw_state_change: false,
        })
    }
    pub(crate) fn with_segment_data(mut self, segment_data: u64) -> Self {
        self.segment_data = Some(segment_data);
        self
    }
    fn step_batch(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<super::packet::TypedPacket>> {
        let batch = self.batch.as_mut().required("missing cue packet batch")?;
        if let Some(configuration) = batch.take_configuration_change() {
            self.saw_state_change = true;
            self.configuration = configuration;
        }
        let step = batch.step(work, cancel)?;
        if matches!(&step, ParseStep::Event(packet)
            if packet.group.as_ref().is_some_and(|group| group.codec_state().is_some()))
        {
            self.saw_state_change = true;
        }
        Ok(step)
    }
    pub fn step(
        &mut self,
        reader: &mut ElementReader,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<ValidatedCue>> {
        if self.failed {
            return Err(Error::Invalid(
                "cue validation requires replacement after error",
            ));
        }
        if !self.pool.same_pool(reader.budget()) {
            return Err(triergon_core::Error::ForeignPool.into());
        }
        if self.configuration.generation != reader.generation() {
            return Err(triergon_core::Error::StaleGeneration {
                expected: reader.generation(),
                received: self.configuration.generation,
            }
            .into());
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
    ) -> Result<ParseStep<ValidatedCue>> {
        loop {
            check(cancel)?;
            match self.phase {
                Phase::Begin => {
                    if !work.try_charge(1) {
                        return Ok(ParseStep::Yield);
                    }
                    self.wanted = Time::from_ticks(
                        i128::from(self.cue.ticks),
                        TimeBase::new(self.scale, 1_000_000_000)?,
                    )?
                    .checked_sub(Time::new(
                        i128::from(self.configuration.tracks()[self.track].codec_delay_ns),
                        1_000_000_000,
                    )?)?
                    .to_nanoseconds(Rounding::Floor)?;
                    reader.begin_internal_cluster_range(&self.anchor, self.cue.cluster)?;
                    self.phase = Phase::Envelope;
                }
                Phase::Envelope => {
                    match reader.step_seek_operation(&mut self.remaining, work, cancel)? {
                        ParseStep::Event(ElementEvent::Start(header)) => {
                            if header.id != 0x1f43b675
                                || header.start != self.cue.cluster
                                || self.header.is_some()
                            {
                                return Err(Error::Invalid("cue timestamp absent from Cluster"));
                            }
                            let end = header
                                .size
                                .map(|size| header.data.checked_add(size).ok_or(Error::Overflow))
                                .transpose()?;
                            self.relative = self
                                .cue
                                .relative
                                .map(|relative| {
                                    header.data.checked_add(relative).ok_or(Error::Overflow)
                                })
                                .transpose()?;
                            if self
                                .relative
                                .is_some_and(|p| end.is_some_and(|end| p >= end))
                            {
                                return Err(Error::Invalid(
                                    "cue relative position outside Cluster",
                                ));
                            }
                            self.header = Some(header);
                        }
                        ParseStep::Event(ElementEvent::Leaf { header, data }) => {
                            if self.header.is_none() {
                                return Err(Error::Invalid("cue target is not Cluster"));
                            }
                            match header.id {
                                0xe7 => self.cluster_time = Some(ebml::uint(&data)?),
                                0xa0 | 0xa3 => {
                                    let ticks = self
                                        .cluster_time
                                        .required("Block before Cluster Timestamp")?;
                                    self.blocks =
                                        self.blocks.checked_add(1).ok_or(Error::Overflow)?;
                                    self.location = Some(
                                        reader.block_replay_location(header, self.cluster_time)?,
                                    );
                                    self.first = true;
                                    self.batch = Some(PacketBatchJob::new(
                                        data,
                                        header.id == 0xa3,
                                        ticks,
                                        self.scale,
                                        self.configuration.clone(),
                                        self.limits,
                                        &self.pool,
                                    )?);
                                    if let (0xa0, Some(segment_data)) =
                                        (header.id, self.segment_data)
                                    {
                                        self.batch
                                            .as_mut()
                                            .required("missing cue packet batch")?
                                            .set_location(super::side_data::GroupLocation {
                                            source: reader.request().source,
                                            segment_data,
                                            header: header.start,
                                            data: header.data,
                                        })?;
                                    }
                                    self.phase = Phase::Batch;
                                }
                                _ => (),
                            }
                        }
                        ParseStep::Event(ElementEvent::Skipped(_)) => (),
                        ParseStep::Event(ElementEvent::End { .. }) => {
                            return Err(Error::Invalid(
                                "cue does not reference a random-access block",
                            ));
                        }
                        ParseStep::Finished => {
                            return Err(Error::Invalid("cue beyond packet data"));
                        }
                        other => return super::pending(other),
                    }
                }
                Phase::Batch => match self.step_batch(work, cancel)? {
                    ParseStep::Event(packet) => {
                        if self.first
                            && packet.packet.track == self.track
                            && packet.packet.keyframe
                            && packet.packet.pts_ns == Some(self.wanted)
                        {
                            let location = self
                                .location
                                .as_ref()
                                .required("missing cue block location")?;
                            if self.relative.is_some_and(|p| p != location.block_offset())
                                || self.cue.block.is_some_and(|n| n != self.blocks)
                            {
                                return Err(Error::Invalid(
                                    "cue position disagrees with referenced block",
                                ));
                            }
                            // From the first physical Cluster there is no
                            // earlier per-track state history. This forward
                            // validation has already processed every preceding
                            // group and the complete target batch. Prove the
                            // Cue's state here so whole-first-Cluster replay can
                            // start with all tracks' initial configuration
                            // without repeating the same input range.
                            if self.cue.cluster == self.anchor.offset() {
                                let state = match packet.configuration.codec_state(self.track) {
                                    None => 0,
                                    Some(state) => {
                                        state
                                            .position()
                                            .required("unlocated validated CodecState")?
                                            .segment_position
                                    }
                                };
                                if state != self.cue.codec_state {
                                    return Err(Error::Invalid(
                                        "cue CodecState does not match referenced block",
                                    ));
                                }
                            }
                            self.phase = Phase::Drain;
                        } else {
                            self.first = false;
                        }
                    }
                    ParseStep::Finished => {
                        self.batch = None;
                        self.location = None;
                        self.phase = Phase::Envelope;
                    }
                    other => return super::pending(other),
                },
                Phase::Drain => match self.step_batch(work, cancel)? {
                    ParseStep::Event(_) => (),
                    ParseStep::Finished => {
                        self.batch = None;
                        self.phase = Phase::Replay;
                    }
                    other => return super::pending(other),
                },
                Phase::Replay => {
                    if !work.try_charge(1) {
                        return Ok(ParseStep::Yield);
                    }
                    let location = self
                        .location
                        .as_ref()
                        .required("missing validated cue location")?;
                    let cluster_time = if self.whole_cluster {
                        reader.begin_internal_cluster_range(
                            location.checkpoint(),
                            location.cluster_offset(),
                        )?;
                        None
                    } else {
                        reader.restore_internal_block(location)?;
                        location.cluster_time
                    };
                    self.result = Some(ValidatedCue {
                        actual_ns: self.wanted,
                        scanned_bytes: self.limits.scan_bytes - self.remaining,
                        location: location.clone(),
                        replay_cluster_time: cluster_time,
                        unchanged_state_prefix: !self.saw_state_change,
                    });
                    self.phase = Phase::Done;
                }
                Phase::Done => {
                    return Ok(ParseStep::Event(
                        self.result
                            .as_ref()
                            .required("missing validated cue result")?
                            .clone(),
                    ));
                }
            }
        }
    }
}

super::usage::storage!(CueValidationJob, |s| [s.configuration, s.batch]);

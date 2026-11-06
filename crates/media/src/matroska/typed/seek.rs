//! Bounded format policy planning over validated source Cue positions.
//! A plan is not a SeekReceipt. Range resolution, RAP validation and replay must
//! complete on the reader's sole core InputCursor before any receipt is emitted.
use super::{configuration::Configuration, syntax::check};
use crate::matroska::typed::syntax::Required;
use crate::{Error, Result, cues::Cue, incremental::ParseStep};
use std::sync::atomic::AtomicBool;
use triergon_core::{Rounding, Time, TimeBase, WorkBudget};

#[derive(Clone, Copy, Debug)]
pub(crate) struct ScanPlan {
    pub start: u64,
    pub stop: Option<u64>,
    pub target_ns: i64,
    pub allow_first_after_negative_preroll: bool,
    pub replay_whole_cluster: bool,
}
#[derive(Clone, Copy, Debug)]
pub(crate) enum LandingPlan {
    Cue {
        cue: Cue,
        whole_cluster: bool,
        scan_after_validation: Option<ScanPlan>,
    },
    Scan(ScanPlan),
}
#[derive(Clone, Copy, Debug)]
pub(crate) struct SeekPlan {
    pub requested_ns: i64,
    pub discard_until_ns: i64,
    pub preroll_ns: u64,
    pub subtitle_recovery_delay_ns: u64,
    pub landing: LandingPlan,
}
#[derive(Debug)]
enum Phase {
    Preroll,
    Clock,
    Index,
    Stop,
    Done,
}
#[derive(Debug)]
pub(crate) struct SeekPlanJob {
    configuration: Configuration,
    track: usize,
    requested: i64,
    preroll: bool,
    scale: u64,
    first_cluster: u64,
    phase: Phase,
    at: usize,
    roll: u64,
    subtitle_tracks: bool,
    audio_delay_ns: u64,
    target: i64,
    target_ticks: i128,
    requested_ticks: i128,
    first: Option<Cue>,
    best: Option<Cue>,
    track_count: usize,
    before: usize,
    stop: Option<u64>,
    result: Option<SeekPlan>,
    failed: bool,
}
impl SeekPlanJob {
    // Preserve every source Cue without sorting. For equal-time references in
    // different Clusters, replay starts at the earliest physical Cluster, matching
    // the observed native cross-Cluster suffix. This is an explicit format replay
    // policy, not a claim about native index internals or a normative tie winner.
    // Equal-Cluster reference ordering remains unchanged and separately unproven.
    pub(crate) fn new(
        configuration: Configuration,
        track: usize,
        requested: i64,
        preroll: bool,
        scale: u64,
        first_cluster: u64,
    ) -> Result<Self> {
        if configuration
            .tracks()
            .get(track)
            .is_none_or(|t| t.kind != 1)
        {
            return Err(Error::Unsupported("seek requires video track"));
        }
        if scale == 0 {
            return Err(Error::Invalid("zero TimestampScale"));
        }
        Ok(Self {
            configuration,
            track,
            requested,
            preroll,
            scale,
            first_cluster,
            phase: Phase::Preroll,
            at: 0,
            roll: 0,
            subtitle_tracks: false,
            audio_delay_ns: 0,
            target: 0,
            target_ticks: 0,
            requested_ticks: 0,
            first: None,
            best: None,
            track_count: 0,
            before: 0,
            stop: None,
            result: None,
            failed: false,
        })
    }
    pub(crate) fn step(
        &mut self,
        cues: &[Cue],
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<SeekPlan>> {
        if self.failed {
            return Err(Error::Invalid("seek plan requires replacement after error"));
        }
        let result = self.advance(cues, work, cancel);
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    fn advance(
        &mut self,
        cues: &[Cue],
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<SeekPlan>> {
        loop {
            check(cancel)?;
            if let Some(result) = self.result {
                return Ok(ParseStep::Event(result));
            }
            if !work.try_charge(1) {
                return Ok(ParseStep::Yield);
            }
            match self.phase {
                Phase::Preroll => {
                    if self.preroll && self.at < self.configuration.tracks().len() {
                        let track = &self.configuration.tracks()[self.at];
                        self.roll = self.roll.max(track.seek_preroll_ns);
                        self.subtitle_tracks |= track.kind == 17;
                        if track.kind == 2 && track.sample_rate != 0.0 {
                            self.audio_delay_ns = self.audio_delay_ns.max(track.codec_delay_ns);
                        }
                        self.at += 1;
                    } else {
                        self.phase = Phase::Clock;
                    }
                }
                Phase::Clock => {
                    // Preserve the checked i64 target view. Exact rescale
                    // itself uses core Time/TimeBase, including negative floor.
                    self.target = i64::try_from(i128::from(self.requested) - i128::from(self.roll))
                        .map_err(|_| Error::Overflow)?;
                    let delay = Time::new(
                        i128::from(self.configuration.tracks()[self.track].codec_delay_ns),
                        1_000_000_000,
                    )?;
                    let base = TimeBase::new(self.scale, 1_000_000_000)?;
                    self.target_ticks = Time::new(i128::from(self.target), 1_000_000_000)?
                        .checked_add(delay)?
                        .to_ticks(base, Rounding::Floor)?;
                    self.requested_ticks = Time::new(i128::from(self.requested), 1_000_000_000)?
                        .checked_add(delay)?
                        .to_ticks(base, Rounding::Floor)?;
                    self.at = 0;
                    self.phase = Phase::Index;
                }
                Phase::Index => {
                    if self.at == cues.len() {
                        let cue = self.best.or_else(|| {
                            self.first.filter(|cue| {
                                self.preroll && i128::from(cue.ticks) <= self.requested_ticks
                            })
                        });
                        self.best = cue;
                        if cue.is_none() {
                            self.finish(LandingPlan::Scan(ScanPlan {
                                start: self.first_cluster,
                                stop: None,
                                target_ns: self.target,
                                allow_first_after_negative_preroll: self.preroll && self.target < 0,
                                replay_whole_cluster: self.preroll,
                            }));
                            continue;
                        }
                        self.at = 0;
                        self.phase = Phase::Stop;
                        continue;
                    }
                    let cue = cues[self.at];
                    self.at += 1;
                    if cue.track != self.configuration.tracks()[self.track].number {
                        continue;
                    }
                    self.track_count += 1;
                    if self.first.is_none_or(|old| {
                        cue.ticks < old.ticks
                            || (cue.ticks == old.ticks && cue.cluster < old.cluster)
                    }) {
                        self.first = Some(cue);
                    }
                    // Never rescale unselected extreme CueTimes into i64 ns.
                    if i128::from(cue.ticks) <= self.target_ticks {
                        self.before += 1;
                        if self.best.is_none_or(|old| {
                            cue.ticks > old.ticks
                                || (cue.ticks == old.ticks && cue.cluster <= old.cluster)
                        }) {
                            self.best = Some(cue);
                        }
                    }
                }
                Phase::Stop => {
                    let cue = self.best.required("missing planned cue")?;
                    let tail_gap = self.preroll
                        && self.before == self.track_count
                        && i128::from(cue.ticks) < self.target_ticks;
                    if self.preroll {
                        let scan = tail_gap.then_some(ScanPlan {
                            start: cue.cluster,
                            stop: None,
                            target_ns: self.target,
                            allow_first_after_negative_preroll: false,
                            replay_whole_cluster: true,
                        });
                        self.finish(LandingPlan::Cue {
                            cue,
                            whole_cluster: true,
                            scan_after_validation: scan,
                        });
                        continue;
                    }
                    if self.at == cues.len() {
                        self.finish(LandingPlan::Cue {
                            cue,
                            whole_cluster: false,
                            scan_after_validation: Some(ScanPlan {
                                start: cue.cluster,
                                stop: self.stop,
                                target_ns: self.target,
                                allow_first_after_negative_preroll: false,
                                replay_whole_cluster: false,
                            }),
                        });
                        continue;
                    }
                    let next = cues[self.at];
                    self.at += 1;
                    if next.track == cue.track
                        && i128::from(next.ticks) > self.target_ticks
                        && next.cluster > cue.cluster
                    {
                        self.stop =
                            Some(self.stop.map_or(next.cluster, |old| old.min(next.cluster)));
                    }
                }
                Phase::Done => return Err(Error::Invalid("missing completed seek plan")),
            }
        }
    }
    fn finish(&mut self, landing: LandingPlan) {
        self.result = Some(SeekPlan {
            requested_ns: self.requested,
            discard_until_ns: self.requested,
            preroll_ns: self.roll,
            subtitle_recovery_delay_ns: if self.subtitle_tracks {
                self.audio_delay_ns
            } else {
                0
            },
            landing,
        });
        self.phase = Phase::Done;
    }
}

super::usage::storage!(SeekPlanJob, |s| [s.configuration]);

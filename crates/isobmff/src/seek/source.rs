//! Seek plans by sample ordinal. [`SourceSeekPlanner`] finds the container sync sample
//! at or before a target sample in a track's sample table and fragments, steps back the
//! number of preroll samples the caller asks for, and checks that every sample from
//! there to the target lies in the caller's media extents. Edit lists and sample groups
//! are not applied.
//!
//! Specification: Apple QuickTime File Format (seeking, `stss` and chunk tables).
use crate::{
    Access, BuildStatus, Next, Sample, fragment::FragmentLocator, interleave::SourceTrack,
    table::SampleLocator,
};
use std::{
    mem,
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};
use triergon_core::{Error, Result, WorkBudget};

/// The sample a [`SourceSeekPlanner`] lands on and the preroll it adds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceSeekRequest {
    /// 1-based ordinal of the sample to land on; zero fails
    /// [`SourceSeekPlanner::new`] with `Error::Invalid`.
    pub target_ordinal: u64,
    /// Samples to decode before the sync sample, counted in container samples. The
    /// planner takes as many as precede the sync sample and reports any shortfall; it
    /// does not derive the count from sample groups or the codec.
    pub preroll_samples: u16,
}

/// Bounds on the work of a [`SourceSeekPlanner`]; exceeding one fails with
/// `Error::Limit`.
#[derive(Clone, Copy, Debug)]
pub struct SourceSeekLimits {
    /// Most fragment indexes the track may have; 4096 by default.
    pub max_fragments: usize,
    /// Most media extents the caller may pass; 4096 by default.
    pub max_media_extents: usize,
    /// Most samples probed while looking back for a sync sample; 1,000,000 by default.
    pub max_sync_scan_samples: u64,
    /// Most samples from the decode start to the target, both included; 1,000,000 by
    /// default.
    pub max_range_samples: u64,
}
impl Default for SourceSeekLimits {
    fn default() -> Self {
        Self {
            max_fragments: 4096,
            max_media_extents: 4096,
            max_sync_scan_samples: 1_000_000,
            max_range_samples: 1_000_000,
        }
    }
}

/// Where a seek lands on one track: the target, the sync sample before it and the
/// first sample to decode, as the source states them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceSeekLanding {
    /// `track_ID` of the track; never zero.
    pub track_id: u32,
    /// The request's [`target_ordinal`](SourceSeekRequest::target_ordinal).
    pub requested_ordinal: u64,
    /// The sample at the requested ordinal.
    pub target: Sample,
    /// The latest container sync sample at or before the target.
    pub sync_sample: Sample,
    /// The first sample to decode: the earliest preroll sample, or the sync sample
    /// when no preroll is taken.
    pub decode_start: Sample,
    /// The request's [`preroll_samples`](SourceSeekRequest::preroll_samples).
    pub requested_preroll_samples: u16,
    /// Preroll samples taken: the requested count, or fewer where the track starts
    /// closer to the sync sample.
    pub available_preroll_samples: u16,
    /// Requested preroll samples the track cannot supply.
    pub missing_preroll_samples: u16,
    /// Samples whose byte ranges were checked against the media extents: every sample
    /// from the decode start to the target, both included.
    pub validated_samples: u64,
}

fn count_limit(count: usize, maximum: usize) -> Result<()> {
    if count > maximum {
        return Err(Error::Limit {
            requested: count,
            maximum,
        });
    }
    Ok(())
}
fn sample_limit(count: u64, maximum: u64) -> Result<()> {
    if count > maximum {
        return Err(Error::Limit {
            requested: usize::try_from(count).map_err(|_| Error::Overflow)?,
            maximum: usize::try_from(maximum).map_err(|_| Error::Overflow)?,
        });
    }
    Ok(())
}

#[derive(Debug)]
enum LocatedIn<'a> {
    Regular(SampleLocator<'a>),
    Fragment(FragmentLocator<'a>),
}
impl LocatedIn<'_> {
    fn poll(&mut self, work: &mut WorkBudget) -> Result<Next<Sample>> {
        match self {
            Self::Regular(cursor) => cursor.poll(work),
            Self::Fragment(cursor) => cursor.poll(work),
        }
    }
}

/// A borrowed lookup, not a second sample index. Fragment association was
/// checked by the planner; ordinal gaps remain unsupported, never filled.
#[derive(Debug)]
struct IndexedLookup<'a> {
    source: SourceTrack<'a>,
    ordinal: u64,
    fragment: usize,
    active: Option<LocatedIn<'a>>,
}
impl<'a> IndexedLookup<'a> {
    fn new(source: SourceTrack<'a>, ordinal: u64) -> Result<Self> {
        let active = source
            .regular_index()
            .filter(|table| ordinal <= table.sample_count())
            .map(|table| table.locate(ordinal).map(LocatedIn::Regular))
            .transpose()?;
        Ok(Self {
            source,
            ordinal,
            fragment: 0,
            active,
        })
    }
    /// The lookup in `lookup` for `ordinal`: the same lookup while it looks for
    /// `ordinal`, the same one continued where `ordinal` follows its located
    /// sample in the same fragment, or else a fresh one.
    fn locate<'l>(
        lookup: &'l mut Option<Self>,
        source: SourceTrack<'a>,
        ordinal: u64,
    ) -> Result<&'l mut Self> {
        let reuse = lookup
            .as_mut()
            .is_some_and(|current| current.continue_to(ordinal));
        match (reuse, lookup) {
            (true, Some(current)) => Ok(current),
            (_, lookup) => Ok(lookup.insert(Self::new(source, ordinal)?)),
        }
    }
    /// Whether this lookup looks for `ordinal`, or now does because `ordinal` follows
    /// its located sample in the same fragment.
    fn continue_to(&mut self, ordinal: u64) -> bool {
        if self.ordinal == ordinal {
            return true;
        }
        if let Some(LocatedIn::Fragment(locator)) = &mut self.active
            && locator.continue_to(ordinal)
        {
            self.ordinal = ordinal;
            return true;
        }
        false
    }
    /// The last sync sample before a located fragment sample in its run, where
    /// locating it stepped past one.
    fn passed_sync(&self) -> Option<u64> {
        match &self.active {
            Some(LocatedIn::Fragment(locator)) => locator.passed_sync(),
            _ => None,
        }
    }
    fn poll(&mut self, work: &mut WorkBudget) -> Result<Next<Sample>> {
        loop {
            if let Some(active) = &mut self.active {
                return active.poll(work);
            }
            let Some(index) = self.source.fragment_indexes().get(self.fragment) else {
                return Err(Error::Unsupported("source seek ordinal not indexed"));
            };
            if !work.try_charge(1) {
                return Ok(Next::Yield);
            }
            self.fragment = self.fragment.checked_add(1).ok_or(Error::Overflow)?;
            if index.track_id() != self.source.track_id() || index.sample_count() == 0 {
                continue;
            }
            let end = index
                .first_ordinal()
                .checked_add(index.sample_count())
                .ok_or(Error::Overflow)?;
            if self.ordinal >= index.first_ordinal() && self.ordinal < end {
                self.active = Some(LocatedIn::Fragment(index.locate(self.ordinal)?));
            }
        }
    }
}

/// The walk from the decode start to the target that checks each sample's byte range
/// against the media extents.
#[derive(Debug)]
struct RangeWalk<'a> {
    /// The ordinal of the sample being checked.
    ordinal: u64,
    lookup: Option<IndexedLookup<'a>>,
    /// The sample at `ordinal`, once located, and the extent it is checked against.
    pending: Option<Sample>,
    extent: usize,
    validated: u64,
}
impl<'a> RangeWalk<'a> {
    fn new(ordinal: u64) -> Self {
        Self {
            ordinal,
            lookup: None,
            pending: None,
            extent: 0,
            validated: 0,
        }
    }
    /// Locates the sample at `ordinal` and finds an extent that holds it, one extent per
    /// work unit; `Some` with the sample once one does, `None` when the work is spent.
    fn check(
        &mut self,
        source: SourceTrack<'a>,
        media: &[Range<u64>],
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<Sample>> {
        loop {
            let sample = if let Some(sample) = &self.pending {
                sample
            } else {
                match IndexedLookup::locate(&mut self.lookup, source, self.ordinal)?.poll(work)? {
                    Next::Yield => return Ok(None),
                    Next::End => return Err(Error::Invalid("seek range lookup exhausted")),
                    Next::Item(sample) => {
                        // The lookup stays to continue to the next sample.
                        self.extent = 0;
                        self.pending.insert(sample)
                    }
                }
            };
            let Some(extent) = media.get(self.extent) else {
                return Err(Error::Invalid("seek sample outside media extents"));
            };
            if !work.try_charge(1) {
                return Ok(None);
            }
            if sample.require_within(extent.clone()).is_ok() {
                self.validated = self.validated.checked_add(1).ok_or(Error::Overflow)?;
                return Ok(self.pending.take());
            }
            self.extent = self.extent.checked_add(1).ok_or(Error::Overflow)?;
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
        }
    }
}

/// Where a plan stands. Each phase owns what it reads and what the phases before it
/// found.
#[derive(Debug)]
enum Phase<'a> {
    /// Checking that no media extent is reversed, from extent `next`; `next_ordinal`
    /// follows the last sample of the sample table.
    Extents {
        next: usize,
        next_ordinal: u64,
    },
    /// Checking that the track's fragments do not overlap, from fragment `next`;
    /// `next_ordinal` follows the last sample indexed so far.
    Fragments {
        next: usize,
        next_ordinal: u64,
    },
    /// Finding the target sample.
    Target(IndexedLookup<'a>),
    /// Looking back from `sync_ordinal` for a container sync sample, after `probes`
    /// probes; `lookup` is the probe at hand.
    Sync {
        target: Sample,
        sync_ordinal: u64,
        probes: u64,
        lookup: Option<IndexedLookup<'a>>,
    },
    /// Checking the first sample to decode.
    DecodeStart {
        target: Sample,
        sync_sample: Sample,
        available_preroll: u16,
        walk: RangeWalk<'a>,
    },
    /// Checking the samples after the decode start, through the target.
    Ranges {
        landing: SourceSeekLanding,
        walk: RangeWalk<'a>,
    },
    Ready(SourceSeekLanding),
    Failed(Error),
}

/// Plans a seek to one sample of a track. It finds the latest container sync sample at
/// or before the target, adds the requested preroll, and checks the byte range of
/// every sample from the decode start to the target against the media extents before
/// [`finish`](Self::finish) returns a plan. Samples are reported as the source states
/// them.
#[derive(Debug)]
pub struct SourceSeekPlanner<'a> {
    source: SourceTrack<'a>,
    request: SourceSeekRequest,
    media: &'a [Range<u64>],
    limits: SourceSeekLimits,
    phase: Phase<'a>,
}
impl<'a> SourceSeekPlanner<'a> {
    /// A planner for `request` on `source`, whose samples must lie in
    /// `media_extents`, absolute byte ranges of the file's media. Fails with
    /// `Error::Invalid` for a zero target ordinal or a sample table whose timing runs
    /// do not cover its samples exactly, and `Error::Limit` for more fragments or
    /// extents than `limits` allow.
    pub fn new(
        source: SourceTrack<'a>,
        request: SourceSeekRequest,
        media_extents: &'a [Range<u64>],
        limits: SourceSeekLimits,
    ) -> Result<Self> {
        if request.target_ordinal == 0 {
            return Err(Error::Invalid("zero seek sample ordinal"));
        }
        source.require_exact_timing()?;
        count_limit(source.fragment_indexes().len(), limits.max_fragments)?;
        count_limit(media_extents.len(), limits.max_media_extents)?;
        let next_ordinal = source
            .regular_index()
            .map_or(0, |table| table.sample_count())
            .checked_add(1)
            .ok_or(Error::Overflow)?;
        Ok(Self {
            source,
            request,
            media: media_extents,
            limits,
            phase: Phase::Extents {
                next: 0,
                next_ordinal,
            },
        })
    }
    /// Advances the plan. Fails with `Error::Unsupported` when no container sync
    /// sample precedes the target or a planned ordinal lies in a gap between
    /// fragments, `Error::Invalid` for a reversed extent, fragments with overlapping
    /// ordinals, a target past the indexed samples or a planned sample outside every
    /// extent, and `Error::Limit` past `limits`. An error is kept and returned by every
    /// later poll.
    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<BuildStatus> {
        if let Phase::Failed(error) = self.phase {
            return Err(error);
        }
        let result = self.poll_inner(work, cancel);
        if let Err(error) = result {
            self.phase = Phase::Failed(error);
        }
        result
    }
    fn poll_inner(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<BuildStatus> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            // A sample the finished phase hands to the next one.
            let found = match &mut self.phase {
                Phase::Extents { next, next_ordinal } => {
                    if let Some(extent) = self.media.get(*next) {
                        if !work.try_charge(1) {
                            return Ok(BuildStatus::Yield);
                        }
                        if extent.start > extent.end {
                            return Err(Error::Invalid("reversed seek media extent"));
                        }
                        *next = next.checked_add(1).ok_or(Error::Overflow)?;
                    } else {
                        self.phase = Phase::Fragments {
                            next: 0,
                            next_ordinal: *next_ordinal,
                        };
                    }
                    continue;
                }
                Phase::Fragments { next, next_ordinal } => {
                    if let Some(index) = self.source.fragment_indexes().get(*next) {
                        if !work.try_charge(1) {
                            return Ok(BuildStatus::Yield);
                        }
                        if index.track_id() == self.source.track_id() && index.sample_count() != 0 {
                            if index.first_ordinal() < *next_ordinal {
                                return Err(Error::Invalid("overlapping seek sample ordinals"));
                            }
                            *next_ordinal = index
                                .first_ordinal()
                                .checked_add(index.sample_count())
                                .ok_or(Error::Overflow)?;
                        }
                        *next = next.checked_add(1).ok_or(Error::Overflow)?;
                    } else {
                        if self.request.target_ordinal >= *next_ordinal {
                            return Err(Error::Invalid("seek target outside indexed samples"));
                        }
                        self.phase = Phase::Target(IndexedLookup::new(
                            self.source,
                            self.request.target_ordinal,
                        )?);
                    }
                    continue;
                }
                Phase::Target(lookup) => match lookup.poll(work)? {
                    Next::Yield => return Ok(BuildStatus::Yield),
                    Next::End => {
                        return Err(Error::Invalid("seek target lookup exhausted"));
                    }
                    Next::Item(sample) => {
                        self.phase = Phase::Sync {
                            sync_ordinal: sample.ordinal,
                            target: sample,
                            probes: 0,
                            lookup: None,
                        };
                        continue;
                    }
                },
                Phase::Sync {
                    sync_ordinal,
                    probes,
                    lookup,
                    ..
                } => {
                    let probe = if let Some(probe) = lookup {
                        probe
                    } else {
                        if !work.try_charge(1) {
                            return Ok(BuildStatus::Yield);
                        }
                        let next = probes.checked_add(1).ok_or(Error::Overflow)?;
                        sample_limit(next, self.limits.max_sync_scan_samples)?;
                        // A validated regular stss uses <=32 probes, not a walk
                        // from the source's first sample. Fragments use bounded
                        // source flag lookups, retaining their locator progress.
                        if let Some(table) = self
                            .source
                            .regular_index()
                            .filter(|table| *sync_ordinal <= table.sample_count())
                        {
                            *sync_ordinal = table
                                .previous_sync(*sync_ordinal)?
                                .ok_or(Error::Unsupported("no preceding container sync sample"))?;
                        }
                        let probe = lookup.insert(IndexedLookup::new(self.source, *sync_ordinal)?);
                        *probes = next;
                        probe
                    };
                    let sample = match probe.poll(work)? {
                        Next::Yield => return Ok(BuildStatus::Yield),
                        Next::End => {
                            return Err(Error::Invalid("seek sync lookup exhausted"));
                        }
                        Next::Item(sample) => sample,
                    };
                    let passed_sync = probe.passed_sync();
                    *lookup = None;
                    if sample.access == Access::ContainerSync {
                        sample
                    } else {
                        *sync_ordinal =
                            sample
                                .ordinal
                                .checked_sub(1)
                                .filter(|value| *value != 0)
                                .ok_or(Error::Unsupported("no preceding container sync sample"))?;
                        // Locating this sample passed the run's samples
                        // before it: probe its last sync directly, counting
                        // the non-sync probes in between.
                        if let Some(sync) = passed_sync {
                            let between = *sync_ordinal - sync;
                            let passed = probes.checked_add(between).ok_or(Error::Overflow)?;
                            sample_limit(
                                passed.min(self.limits.max_sync_scan_samples.saturating_add(1)),
                                self.limits.max_sync_scan_samples,
                            )?;
                            *probes = passed;
                            *sync_ordinal = sync;
                        }
                        continue;
                    }
                }
                Phase::DecodeStart { walk, .. } => {
                    let Some(sample) = walk.check(self.source, self.media, work, cancel)? else {
                        return Ok(BuildStatus::Yield);
                    };
                    sample
                }
                Phase::Ranges { walk, .. } => {
                    let Some(sample) = walk.check(self.source, self.media, work, cancel)? else {
                        return Ok(BuildStatus::Yield);
                    };
                    if walk.ordinal != self.request.target_ordinal {
                        walk.ordinal = walk.ordinal.checked_add(1).ok_or(Error::Overflow)?;
                        continue;
                    }
                    sample
                }
                Phase::Ready(_) => return Ok(BuildStatus::Ready),
                Phase::Failed(error) => return Err(*error),
            };
            // The phase's job is done. The planner counts as failed until the next
            // phase is in place; `poll` records the error if that fails.
            let finished = mem::replace(&mut self.phase, Phase::Failed(Error::Cancelled));
            self.phase = self.next_phase(finished, found)?;
        }
    }
    /// The phase that follows `finished`, whose job found `sample`: the sync sample
    /// after [`Phase::Sync`], the decode start after [`Phase::DecodeStart`] and the
    /// target after [`Phase::Ranges`].
    fn next_phase(&self, finished: Phase<'a>, sample: Sample) -> Result<Phase<'a>> {
        Ok(match finished {
            Phase::Sync { target, .. } => {
                let preceding = sample
                    .ordinal
                    .checked_sub(1)
                    .ok_or(Error::Invalid("zero seek sync ordinal"))?;
                let available = preceding.min(u64::from(self.request.preroll_samples));
                let available_preroll = u16::try_from(available).map_err(|_| Error::Overflow)?;
                let range_start = sample
                    .ordinal
                    .checked_sub(available)
                    .ok_or(Error::Overflow)?;
                let range_count = self
                    .request
                    .target_ordinal
                    .checked_sub(range_start)
                    .and_then(|value| value.checked_add(1))
                    .ok_or(Error::Overflow)?;
                sample_limit(range_count, self.limits.max_range_samples)?;
                Phase::DecodeStart {
                    target,
                    sync_sample: sample,
                    available_preroll,
                    walk: RangeWalk::new(range_start),
                }
            }
            Phase::DecodeStart {
                target,
                sync_sample,
                available_preroll,
                mut walk,
            } => {
                let landing = SourceSeekLanding {
                    track_id: self.source.track_id(),
                    requested_ordinal: self.request.target_ordinal,
                    target,
                    sync_sample,
                    decode_start: sample,
                    requested_preroll_samples: self.request.preroll_samples,
                    available_preroll_samples: available_preroll,
                    missing_preroll_samples: self
                        .request
                        .preroll_samples
                        .checked_sub(available_preroll)
                        .ok_or(Error::Overflow)?,
                    validated_samples: walk.validated,
                };
                if walk.ordinal == self.request.target_ordinal {
                    Phase::Ready(landing)
                } else {
                    walk.ordinal = walk.ordinal.checked_add(1).ok_or(Error::Overflow)?;
                    Phase::Ranges { landing, walk }
                }
            }
            Phase::Ranges { landing, walk } => Phase::Ready(SourceSeekLanding {
                validated_samples: walk.validated,
                ..landing
            }),
            other => other,
        })
    }
    /// [`poll`](Self::poll), with the landing once the plan is ready.
    pub(crate) fn poll_landing(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<&SourceSeekLanding>> {
        self.poll(work, cancel)?;
        Ok(match &self.phase {
            Phase::Ready(landing) => Some(landing),
            _ => None,
        })
    }
    /// The plan, once [`poll`](Self::poll) has returned `Ready`; `Error::Invalid`
    /// before that, or the error a poll returned.
    pub fn finish(self) -> Result<SourceSeekPlan<'a>> {
        match self.phase {
            Phase::Ready(landing) => Ok(SourceSeekPlan {
                source: self.source,
                landing,
            }),
            Phase::Failed(error) => Err(error),
            _ => Err(Error::Invalid("source seek planner not ready")),
        }
    }
}

/// A checked seek plan for one track: its landing and the samples to read, from the
/// decode start through the target.
#[derive(Debug)]
pub struct SourceSeekPlan<'a> {
    source: SourceTrack<'a>,
    landing: SourceSeekLanding,
}
impl SourceSeekPlan<'_> {
    /// Where the plan lands.
    pub fn landing(&self) -> &SourceSeekLanding {
        &self.landing
    }
    /// A new cursor over the planned samples, starting at the decode start.
    pub fn cursor(&self) -> SourceSeekSamples<'_> {
        SourceSeekSamples {
            source: self.source,
            next: self.landing.decode_start.ordinal,
            last: self.landing.target.ordinal,
            lookup: None,
            done: false,
            failed: None,
        }
    }
}

/// The samples of a [`SourceSeekPlan`], from its decode start through its target, in
/// ordinal order. It reads only the indexes: reading the payloads, from a new input
/// position, is the caller's job. A plan can make any number of these cursors.
#[derive(Debug)]
pub struct SourceSeekSamples<'a> {
    source: SourceTrack<'a>,
    next: u64,
    last: u64,
    lookup: Option<IndexedLookup<'a>>,
    done: bool,
    failed: Option<Error>,
}
impl SourceSeekSamples<'_> {
    /// Returns the next planned sample. Fails with `Error::Invalid` if the indexes
    /// return a different sample than planned; an error is kept and returned by every
    /// later poll.
    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Next<Sample>> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = self.poll_inner(work, cancel);
        if let Err(error) = result {
            self.failed = Some(error);
        }
        result
    }
    fn poll_inner(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Next<Sample>> {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if self.done {
            return Ok(Next::End);
        }
        match IndexedLookup::locate(&mut self.lookup, self.source, self.next)?.poll(work)? {
            Next::Item(sample) => {
                if sample.ordinal != self.next {
                    return Err(Error::Invalid("planned sample ordinal changed"));
                }
                // The lookup stays to continue to the next sample.
                if self.next == self.last {
                    self.done = true;
                } else {
                    self.next = self.next.checked_add(1).ok_or(Error::Overflow)?;
                }
                Ok(Next::Item(sample))
            }
            Next::End => Err(Error::Invalid("planned sample lookup exhausted")),
            Next::Yield => Ok(Next::Yield),
        }
    }
}

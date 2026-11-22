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
    /// The requested 1-based ordinal.
    pub requested_ordinal: u64,
    /// The sample at the requested ordinal.
    pub target: Sample,
    /// The latest container sync sample at or before the target.
    pub sync_sample: Sample,
    /// The first sample to decode: the earliest preroll sample, or the sync sample
    /// when no preroll is taken.
    pub decode_start: Sample,
    /// Number of samples requested before the sync sample.
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
    /// Whether this lookup serves `ordinal`: it looks for `ordinal`, or it continues to
    /// `ordinal` where that follows its located sample in the same fragment.
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
    /// The lookup for `ordinal`: the one in `lookup` where it serves `ordinal`, or else
    /// a fresh one put there.
    fn locate<'l>(
        lookup: &'l mut Option<Self>,
        source: SourceTrack<'a>,
        ordinal: u64,
    ) -> Result<&'l mut Self> {
        let serves = lookup
            .as_mut()
            .is_some_and(|current| current.continue_to(ordinal));
        match (serves, lookup) {
            (true, Some(current)) => Ok(current),
            (_, lookup) => Ok(lookup.insert(Self::new(source, ordinal)?)),
        }
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

/// Where a plan stands. The phases after the target is found hold the samples found so
/// far; the walks' positions stay in the planner's fields.
// A tag byte of its own: the range walk matches the phase per sample, and a tag held in
// a niche of the samples cost up to 0.4% on long walks.
#[derive(Debug)]
#[repr(u8)]
enum Phase {
    /// Checking that no media extent is reversed.
    Extents,
    /// Checking that the track's fragments do not overlap.
    Fragments,
    /// Finding the target sample.
    Target,
    /// Looking back from the target for a container sync sample.
    Sync {
        target: Sample,
    },
    /// Checking the samples from the decode start through the target; `decode_start`
    /// once the walk has passed it.
    Ranges {
        target: Sample,
        sync_sample: Sample,
        decode_start: Option<Sample>,
    },
    Ready(SourceSeekLanding),
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
    phase: Phase,
    extent: usize,
    fragment: usize,
    next_ordinal: u64,
    lookup: Option<IndexedLookup<'a>>,
    sync_ordinal: u64,
    sync_probes: u64,
    range_ordinal: u64,
    range_start: u64,
    pending: Option<Sample>,
    available_preroll: u16,
    validated: u64,
    failed: Option<Error>,
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
            phase: Phase::Extents,
            extent: 0,
            fragment: 0,
            next_ordinal,
            lookup: None,
            sync_ordinal: 0,
            sync_probes: 0,
            range_ordinal: 0,
            range_start: 0,
            pending: None,
            available_preroll: 0,
            validated: 0,
            failed: None,
        })
    }
    /// Advances the plan. Fails with `Error::Unsupported` when no container sync
    /// sample precedes the target or a planned ordinal lies in a gap between
    /// fragments, `Error::Invalid` for a reversed extent, fragments with overlapping
    /// ordinals, a target past the indexed samples or a planned sample outside every
    /// extent, and `Error::Limit` past `limits`. An error is kept and returned by every
    /// later poll.
    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<BuildStatus> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = self.poll_inner(work, cancel);
        if let Err(error) = result {
            self.failed = Some(error);
        }
        result
    }
    /// Advances the plan as [`poll`](Self::poll) does; its landing once planned.
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
    fn poll_inner(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<BuildStatus> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            match &mut self.phase {
                Phase::Extents => {
                    if let Some(extent) = self.media.get(self.extent) {
                        if !work.try_charge(1) {
                            return Ok(BuildStatus::Yield);
                        }
                        if extent.start > extent.end {
                            return Err(Error::Invalid("reversed seek media extent"));
                        }
                        self.extent = self.extent.checked_add(1).ok_or(Error::Overflow)?;
                    } else {
                        self.phase = Phase::Fragments;
                    }
                }
                Phase::Fragments => {
                    if let Some(index) = self.source.fragment_indexes().get(self.fragment) {
                        if !work.try_charge(1) {
                            return Ok(BuildStatus::Yield);
                        }
                        if index.track_id() == self.source.track_id() && index.sample_count() != 0 {
                            if index.first_ordinal() < self.next_ordinal {
                                return Err(Error::Invalid("overlapping seek sample ordinals"));
                            }
                            self.next_ordinal = index
                                .first_ordinal()
                                .checked_add(index.sample_count())
                                .ok_or(Error::Overflow)?;
                        }
                        self.fragment = self.fragment.checked_add(1).ok_or(Error::Overflow)?;
                    } else {
                        if self.request.target_ordinal >= self.next_ordinal {
                            return Err(Error::Invalid("seek target outside indexed samples"));
                        }
                        self.lookup = Some(IndexedLookup::new(
                            self.source,
                            self.request.target_ordinal,
                        )?);
                        self.phase = Phase::Target;
                    }
                }
                Phase::Target => {
                    match IndexedLookup::locate(
                        &mut self.lookup,
                        self.source,
                        self.request.target_ordinal,
                    )?
                    .poll(work)?
                    {
                        Next::Yield => return Ok(BuildStatus::Yield),
                        Next::End => {
                            return Err(Error::Invalid("seek target lookup exhausted"));
                        }
                        Next::Item(sample) => {
                            self.sync_ordinal = sample.ordinal;
                            self.lookup = None;
                            self.phase = Phase::Sync { target: sample };
                        }
                    }
                }
                Phase::Sync { target } => {
                    let probe = if let Some(probe) = &mut self.lookup {
                        probe
                    } else {
                        if !work.try_charge(1) {
                            return Ok(BuildStatus::Yield);
                        }
                        let probes = self.sync_probes.checked_add(1).ok_or(Error::Overflow)?;
                        sample_limit(probes, self.limits.max_sync_scan_samples)?;
                        // A validated regular stss uses <=32 probes, not a walk
                        // from the source's first sample. Fragments use bounded
                        // source flag lookups, retaining their locator progress.
                        if let Some(table) = self
                            .source
                            .regular_index()
                            .filter(|table| self.sync_ordinal <= table.sample_count())
                        {
                            self.sync_ordinal = table
                                .previous_sync(self.sync_ordinal)?
                                .ok_or(Error::Unsupported("no preceding container sync sample"))?;
                        }
                        let probe = self
                            .lookup
                            .insert(IndexedLookup::new(self.source, self.sync_ordinal)?);
                        self.sync_probes = probes;
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
                    self.lookup = None;
                    if sample.access == Access::ContainerSync {
                        let preceding = sample
                            .ordinal
                            .checked_sub(1)
                            .ok_or(Error::Invalid("zero seek sync ordinal"))?;
                        let available = preceding.min(u64::from(self.request.preroll_samples));
                        self.available_preroll =
                            u16::try_from(available).map_err(|_| Error::Overflow)?;
                        self.range_start = sample
                            .ordinal
                            .checked_sub(available)
                            .ok_or(Error::Overflow)?;
                        let range_count = self
                            .request
                            .target_ordinal
                            .checked_sub(self.range_start)
                            .and_then(|value| value.checked_add(1))
                            .ok_or(Error::Overflow)?;
                        sample_limit(range_count, self.limits.max_range_samples)?;
                        self.range_ordinal = self.range_start;
                        self.phase = Phase::Ranges {
                            target: target.clone(),
                            sync_sample: sample,
                            decode_start: None,
                        };
                    } else {
                        self.sync_ordinal = sample
                            .ordinal
                            .checked_sub(1)
                            .filter(|value| *value != 0)
                            .ok_or(Error::Unsupported("no preceding container sync sample"))?;
                        // Locating this sample passed the run's samples
                        // before it: probe its last sync directly, counting
                        // the non-sync probes in between.
                        if let Some(sync) = passed_sync {
                            let between = self.sync_ordinal - sync;
                            let probes = self
                                .sync_probes
                                .checked_add(between)
                                .ok_or(Error::Overflow)?;
                            sample_limit(
                                probes.min(self.limits.max_sync_scan_samples.saturating_add(1)),
                                self.limits.max_sync_scan_samples,
                            )?;
                            self.sync_probes = probes;
                            self.sync_ordinal = sync;
                        }
                    }
                }
                Phase::Ranges {
                    target,
                    sync_sample,
                    decode_start,
                } => {
                    let sample = match &self.pending {
                        Some(sample) => sample,
                        None => match IndexedLookup::locate(
                            &mut self.lookup,
                            self.source,
                            self.range_ordinal,
                        )?
                        .poll(work)?
                        {
                            Next::Yield => return Ok(BuildStatus::Yield),
                            Next::End => {
                                return Err(Error::Invalid("seek range lookup exhausted"));
                            }
                            Next::Item(sample) => {
                                // The lookup stays to continue to the next sample.
                                self.extent = 0;
                                self.pending.insert(sample)
                            }
                        },
                    };
                    let Some(extent) = self.media.get(self.extent) else {
                        return Err(Error::Invalid("seek sample outside media extents"));
                    };
                    if !work.try_charge(1) {
                        return Ok(BuildStatus::Yield);
                    }
                    if sample.require_within(extent.clone()).is_err() {
                        self.extent = self.extent.checked_add(1).ok_or(Error::Overflow)?;
                        continue;
                    }
                    self.validated = self.validated.checked_add(1).ok_or(Error::Overflow)?;
                    if self.range_ordinal != self.request.target_ordinal {
                        if decode_start.is_none() {
                            *decode_start = Some(sample.clone());
                        }
                        self.pending = None;
                        self.range_ordinal =
                            self.range_ordinal.checked_add(1).ok_or(Error::Overflow)?;
                        continue;
                    }
                    // The target is the decode start where the walk began at it.
                    let decode_start = decode_start.take().unwrap_or_else(|| sample.clone());
                    let landing = SourceSeekLanding {
                        track_id: self.source.track_id(),
                        requested_ordinal: self.request.target_ordinal,
                        target: target.clone(),
                        sync_sample: sync_sample.clone(),
                        decode_start,
                        requested_preroll_samples: self.request.preroll_samples,
                        available_preroll_samples: self.available_preroll,
                        missing_preroll_samples: self
                            .request
                            .preroll_samples
                            .checked_sub(self.available_preroll)
                            .ok_or(Error::Overflow)?,
                        validated_samples: self.validated,
                    };
                    self.pending = None;
                    self.phase = Phase::Ready(landing);
                }
                Phase::Ready(_) => return Ok(BuildStatus::Ready),
            }
        }
    }
    /// The plan, once [`poll`](Self::poll) has returned `Ready`; `Error::Invalid`
    /// before that, or the error a poll returned.
    pub fn finish(self) -> Result<SourceSeekPlan<'a>> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        match self.phase {
            Phase::Ready(landing) => Ok(SourceSeekPlan {
                source: self.source,
                landing,
            }),
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
    /// return a sample of another ordinal than planned; an error is kept and returned by
    /// every later poll.
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

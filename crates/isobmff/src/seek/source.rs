//! Bounded source-sample seek plans over the existing compact format indexes.
//!
//! Apple QTFF seeking/stss/chunk tables supply the addressing basis. Preroll is
//! an explicit caller requirement in container samples, not inferred codec or
//! sample-group semantics. Demux edit occurrences and clocks are separate.
use crate::{
    Access, BuildStatus, Next, Sample, fragment::FragmentLocator, interleave::SourceTrack,
    table::SampleLocator,
};
use std::{
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};
use triergon_core::{Error, Result, WorkBudget};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceSeekRequest {
    pub target_ordinal: u64,
    /// Preceding container samples required before the selected sync sample.
    /// No group inheritance, PCM count or decoder recovery is implied here.
    pub preroll_samples: u16,
}

#[derive(Clone, Copy, Debug)]
pub struct SourceSeekLimits {
    pub max_fragments: usize,
    pub max_media_extents: usize,
    pub max_sync_scan_samples: u64,
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceSeekLanding {
    pub track_id: u32,
    pub requested_ordinal: u64,
    pub target: Sample,
    pub sync_sample: Sample,
    pub decode_start: Sample,
    pub requested_preroll_samples: u16,
    pub available_preroll_samples: u16,
    pub missing_preroll_samples: u16,
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
    /// Sets `lookup` to one for `ordinal`: the same lookup while it looks for
    /// `ordinal`, the same one continued where `ordinal` follows its located
    /// sample in the same fragment, or else a fresh one.
    fn locate(lookup: &mut Option<Self>, source: SourceTrack<'a>, ordinal: u64) -> Result<()> {
        if let Some(current) = lookup {
            if current.ordinal == ordinal {
                return Ok(());
            }
            if let Some(LocatedIn::Fragment(locator)) = &mut current.active
                && locator.continue_to(ordinal)
            {
                current.ordinal = ordinal;
                return Ok(());
            }
        }
        *lookup = Some(Self::new(source, ordinal)?);
        Ok(())
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Extents,
    Fragments,
    Target,
    Sync,
    Ranges,
    Ready,
}

/// Validates every requested byte range before making a confirmation available.
/// Only source declarations are used; no clock/flag/description is rewritten.
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
    target: Option<Sample>,
    sync_sample: Option<Sample>,
    decode_start: Option<Sample>,
    sync_ordinal: u64,
    sync_probes: u64,
    range_ordinal: u64,
    range_start: u64,
    pending: Option<Sample>,
    available_preroll: u16,
    validated: u64,
    landing: Option<SourceSeekLanding>,
    failed: Option<Error>,
}
impl<'a> SourceSeekPlanner<'a> {
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
            target: None,
            sync_sample: None,
            decode_start: None,
            sync_ordinal: 0,
            sync_probes: 0,
            range_ordinal: 0,
            range_start: 0,
            pending: None,
            available_preroll: 0,
            validated: 0,
            landing: None,
            failed: None,
        })
    }
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
    fn poll_inner(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<BuildStatus> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            match self.phase {
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
                    match self
                        .lookup
                        .as_mut()
                        .ok_or(Error::Invalid("missing seek target lookup"))?
                        .poll(work)?
                    {
                        Next::Yield => return Ok(BuildStatus::Yield),
                        Next::End => {
                            return Err(Error::Invalid("seek target lookup exhausted"));
                        }
                        Next::Item(sample) => {
                            self.sync_ordinal = sample.ordinal;
                            self.target = Some(sample);
                            self.lookup = None;
                            self.phase = Phase::Sync;
                        }
                    }
                }
                Phase::Sync => {
                    if self.lookup.is_none() {
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
                        self.lookup = Some(IndexedLookup::new(self.source, self.sync_ordinal)?);
                        self.sync_probes = probes;
                    }
                    match self
                        .lookup
                        .as_mut()
                        .ok_or(Error::Invalid("missing seek sync lookup"))?
                        .poll(work)?
                    {
                        Next::Yield => return Ok(BuildStatus::Yield),
                        Next::End => {
                            return Err(Error::Invalid("seek sync lookup exhausted"));
                        }
                        Next::Item(sample) => {
                            let located = self
                                .lookup
                                .take()
                                .ok_or(Error::Invalid("missing seek sync lookup"))?;
                            if sample.access == Access::ContainerSync {
                                let preceding = sample
                                    .ordinal
                                    .checked_sub(1)
                                    .ok_or(Error::Invalid("zero seek sync ordinal"))?;
                                let available =
                                    preceding.min(u64::from(self.request.preroll_samples));
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
                                self.sync_sample = Some(sample);
                                self.phase = Phase::Ranges;
                            } else {
                                self.sync_ordinal = sample
                                    .ordinal
                                    .checked_sub(1)
                                    .filter(|value| *value != 0)
                                    .ok_or(Error::Unsupported(
                                        "no preceding container sync sample",
                                    ))?;
                                // Locating this sample passed the run's samples
                                // before it: probe its last sync directly, counting
                                // the non-sync probes in between.
                                if let Some(sync) = located.passed_sync() {
                                    let between = self.sync_ordinal - sync;
                                    let probes = self
                                        .sync_probes
                                        .checked_add(between)
                                        .ok_or(Error::Overflow)?;
                                    sample_limit(
                                        probes.min(
                                            self.limits.max_sync_scan_samples.saturating_add(1),
                                        ),
                                        self.limits.max_sync_scan_samples,
                                    )?;
                                    self.sync_probes = probes;
                                    self.sync_ordinal = sync;
                                }
                            }
                        }
                    }
                }
                Phase::Ranges => {
                    if self.pending.is_none() {
                        IndexedLookup::locate(&mut self.lookup, self.source, self.range_ordinal)?;
                        match self
                            .lookup
                            .as_mut()
                            .ok_or(Error::Invalid("missing seek range lookup"))?
                            .poll(work)?
                        {
                            Next::Yield => return Ok(BuildStatus::Yield),
                            Next::End => {
                                return Err(Error::Invalid("seek range lookup exhausted"));
                            }
                            Next::Item(sample) => {
                                // The lookup stays to continue to the next sample.
                                self.pending = Some(sample);
                                self.extent = 0;
                            }
                        }
                    }
                    let sample = self
                        .pending
                        .as_ref()
                        .ok_or(Error::Invalid("missing seek range sample"))?;
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
                    if sample.ordinal == self.range_start {
                        self.decode_start = Some(sample.clone());
                    }
                    self.validated = self.validated.checked_add(1).ok_or(Error::Overflow)?;
                    self.pending = None;
                    if self.range_ordinal == self.request.target_ordinal {
                        self.landing = Some(SourceSeekLanding {
                            track_id: self.source.track_id(),
                            requested_ordinal: self.request.target_ordinal,
                            target: self
                                .target
                                .take()
                                .ok_or(Error::Invalid("missing seek target"))?,
                            sync_sample: self
                                .sync_sample
                                .take()
                                .ok_or(Error::Invalid("missing seek sync"))?,
                            decode_start: self
                                .decode_start
                                .take()
                                .ok_or(Error::Invalid("missing seek decode start"))?,
                            requested_preroll_samples: self.request.preroll_samples,
                            available_preroll_samples: self.available_preroll,
                            missing_preroll_samples: self
                                .request
                                .preroll_samples
                                .checked_sub(self.available_preroll)
                                .ok_or(Error::Overflow)?,
                            validated_samples: self.validated,
                        });
                        self.phase = Phase::Ready;
                    } else {
                        self.range_ordinal =
                            self.range_ordinal.checked_add(1).ok_or(Error::Overflow)?;
                    }
                }
                Phase::Ready => return Ok(BuildStatus::Ready),
            }
        }
    }
    pub fn finish(mut self) -> Result<SourceSeekPlan<'a>> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if self.phase != Phase::Ready {
            return Err(Error::Invalid("source seek planner not ready"));
        }
        Ok(SourceSeekPlan {
            source: self.source,
            landing: self
                .landing
                .take()
                .ok_or(Error::Invalid("missing source seek receipt"))?,
        })
    }
}

#[derive(Debug)]
pub struct SourceSeekPlan<'a> {
    source: SourceTrack<'a>,
    landing: SourceSeekLanding,
}
impl SourceSeekPlan<'_> {
    pub fn landing(&self) -> &SourceSeekLanding {
        &self.landing
    }
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

/// A plan can create fresh cursors after exhaustion without mutating an input
/// entity. Actual fresh/after-EOF range transactions still belong to core I/O.
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
        IndexedLookup::locate(&mut self.lookup, self.source, self.next)?;
        match self
            .lookup
            .as_mut()
            .ok_or(Error::Invalid("missing planned sample lookup"))?
            .poll(work)?
        {
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

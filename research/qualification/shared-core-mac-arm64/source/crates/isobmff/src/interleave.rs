//! Source-only interleaving over the independently parsed compact sample indexes.
//!
//! Apple QTFF establishes the chunk/sample byte addresses, not a mandated merged
//! demux packet schedule. This module implements an explicitly selected physical
//! address order. It does not claim the pinned MOV reader's default interleaving.
use crate::{FragmentCursor, FragmentIndex, Sample, SampleCursor, SampleStep, SampleTable};
use safe_media_core::{Budget, BudgetVec, Error, Result, WorkBudget};
use std::{
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};

/// Borrowed indexes for one track in one already established source entity.
/// The caller supplies fragments in source order and establishes the same-source
/// association. Data-reference resolution and source/request tokens remain core
/// input/caller responsibilities, not track IDs or configuration epochs.
#[derive(Clone, Copy, Debug)]
pub struct SourceTrack<'a> {
    track_id: u32,
    regular: Option<&'a SampleTable>,
    fragments: &'a [FragmentIndex],
}
impl<'a> SourceTrack<'a> {
    pub fn new(
        track_id: u32,
        regular: Option<&'a SampleTable>,
        fragments: &'a [FragmentIndex],
    ) -> Result<Self> {
        if track_id == 0 {
            return Err(Error::Invalid("zero source track ID"));
        }
        Ok(Self {
            track_id,
            regular,
            fragments,
        })
    }
    pub fn track_id(self) -> u32 {
        self.track_id
    }
}

/// One track's original sample sequence: regular samples, then the matching
/// fragment indexes in the caller's established order. No edit repetition,
/// source clock filling, cross-track ordering or expanded sample array.
#[derive(Debug)]
pub struct TrackSampleCursor<'a> {
    source: SourceTrack<'a>,
    regular: Option<SampleCursor<'a>>,
    next_fragment: usize,
    current: Option<FragmentCursor<'a>>,
    failed: Option<Error>,
}
impl<'a> TrackSampleCursor<'a> {
    pub fn new(source: SourceTrack<'a>) -> Self {
        Self {
            source,
            regular: source.regular.map(SampleTable::cursor),
            next_fragment: 0,
            current: None,
            failed: None,
        }
    }
    pub fn poll(&mut self, work: &mut WorkBudget) -> Result<SampleStep> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = self.poll_inner(work);
        if let Err(error) = result {
            self.failed = Some(error);
        }
        result
    }
    fn poll_inner(&mut self, work: &mut WorkBudget) -> Result<SampleStep> {
        loop {
            if let Some(regular) = &mut self.regular {
                match regular.poll(work)? {
                    SampleStep::End => self.regular = None,
                    other => return Ok(other),
                }
            }
            if let Some(current) = &mut self.current {
                match current.poll(work)? {
                    SampleStep::End => self.current = None,
                    other => return Ok(other),
                }
            }
            let Some(fragment) = self.source.fragments.get(self.next_fragment) else {
                return Ok(SampleStep::End);
            };
            if !work.try_charge(1) {
                return Ok(SampleStep::Yield);
            }
            self.next_fragment += 1;
            if fragment.track_id() == self.source.track_id {
                self.current = Some(fragment.cursor());
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct PhysicalInterleaveLimits {
    pub max_tracks: usize,
    pub max_samples: u64,
}
impl Default for PhysicalInterleaveLimits {
    fn default() -> Self {
        Self {
            max_tracks: 64,
            max_samples: 1_000_000,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InterleavedSample {
    pub track_id: u32,
    pub sample: Sample,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InterleaveStatus {
    Yield,
    Ready,
    /// All supplied source indexes ended. This is not an I/O EOF observation.
    End,
}

#[derive(Debug)]
struct Slot<'a> {
    source: SourceTrack<'a>,
    cursor: TrackSampleCursor<'a>,
    head: Option<Sample>,
    ended: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    ValidateMedia,
    ValidateEnter,
    ValidateRead,
    ValidateBounds,
    ValidateOrder,
    Fill,
    ReadHead,
    Select,
    End,
}

/// Bounded physical-order merge, suitable for feeding the existing SampleReader.
///
/// Before exposing the first sample, a work-bounded metadata pass verifies every
/// track's nondecreasing sample start address, increasing source ordinal and the
/// cumulative sample limit. A backward source track remains accessible through
/// TrackSampleCursor but is unsupported by this physical merge; no silently
/// incorrect prefix is exposed. This is a selected traversal limit, not a claim
/// that such file layout violates the format.
///
/// The second pass retains one metadata head per track and one waiting output;
/// all variable storage is charged to core BudgetVec. No sample/payload expansion
/// or timestamp sorting occurs. Equal addresses follow the supplied track order,
/// including zero-size samples. Per-sample selection is O(number of tracks).
/// `new` validates metadata traversal only; `new_with_media_extents` also checks
/// actual self-contained payload ranges before any physical-layout refusal.
#[derive(Debug)]
pub struct PhysicalSampleCursor<'a> {
    slots: BudgetVec<Slot<'a>>,
    media_extents: Option<&'a [Range<u64>]>,
    limits: PhysicalInterleaveLimits,
    phase: Phase,
    index: usize,
    best: Option<usize>,
    previous_start: Option<u64>,
    previous_ordinal: Option<u64>,
    validated: u64,
    pending_validation: Option<Sample>,
    extent_low: usize,
    extent_high: usize,
    layout_error: Option<Error>,
    ready: Option<InterleavedSample>,
    failed: Option<Error>,
}
impl<'a> PhysicalSampleCursor<'a> {
    /// Metadata-only source traversal. The caller retains responsibility for
    /// establishing self-contained media containment before accessing payloads.
    pub fn new(
        budget: &Budget,
        tracks: &[SourceTrack<'a>],
        limits: PhysicalInterleaveLimits,
    ) -> Result<Self> {
        Self::from_sources(budget, tracks, None, limits)
    }
    /// Borrow checked source-entity mdat payload extents without copying them.
    /// Their bounds, nonoverlap and ascending order are validated during poll.
    /// A resumable binary search checks every source sample's full containment;
    /// only after all supplied tracks pass may a backward physical-layout policy
    /// refusal be returned. Resource/structural failures still stop immediately.
    /// Empty samples at an extent endpoint use Sample::require_within semantics.
    pub fn new_with_media_extents(
        budget: &Budget,
        tracks: &[SourceTrack<'a>],
        media_extents: &'a [Range<u64>],
        limits: PhysicalInterleaveLimits,
    ) -> Result<Self> {
        Self::from_sources(budget, tracks, Some(media_extents), limits)
    }
    fn from_sources(
        budget: &Budget,
        tracks: &[SourceTrack<'a>],
        media_extents: Option<&'a [Range<u64>]>,
        limits: PhysicalInterleaveLimits,
    ) -> Result<Self> {
        if tracks.len() > limits.max_tracks {
            return Err(Error::Limit {
                requested: tracks.len(),
                maximum: limits.max_tracks,
            });
        }
        // Constructor work is bounded by the caller's explicit track policy.
        // Detect duplicates before allocating any retained per-track storage.
        for (at, track) in tracks.iter().enumerate() {
            if tracks[..at]
                .iter()
                .any(|prior| prior.track_id == track.track_id)
            {
                return Err(Error::Invalid("duplicate source track ID"));
            }
        }
        let mut slots = BudgetVec::new(budget);
        slots.try_reserve_exact(tracks.len())?;
        for source in tracks {
            slots
                .try_push(Slot {
                    source: *source,
                    cursor: TrackSampleCursor::new(*source),
                    head: None,
                    ended: false,
                })
                .map_err(|error| error.error)?;
        }
        Ok(Self {
            slots,
            media_extents,
            limits,
            phase: if media_extents.is_some() {
                Phase::ValidateMedia
            } else if tracks.is_empty() {
                Phase::End
            } else {
                Phase::ValidateEnter
            },
            index: 0,
            best: None,
            previous_start: None,
            previous_ordinal: None,
            validated: 0,
            pending_validation: None,
            extent_low: 0,
            extent_high: 0,
            layout_error: None,
            ready: None,
            failed: None,
        })
    }
    /// Metadata samples encountered by preflight, including a failing sample if
    /// polling returns an error. After successful preflight this is the full count.
    pub fn validated_samples(&self) -> u64 {
        self.validated
    }
    pub fn take_sample(&mut self) -> Result<InterleavedSample> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        self.ready
            .take()
            .ok_or(Error::Invalid("no ready interleaved sample"))
    }
    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<InterleaveStatus> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = self.poll_inner(work, cancel);
        if let Err(error) = result {
            self.failed = Some(error);
        }
        result
    }
    fn poll_inner(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<InterleaveStatus> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if self.ready.is_some() {
                return Ok(InterleaveStatus::Ready);
            }
            match self.phase {
                Phase::ValidateMedia => {
                    let media = self
                        .media_extents
                        .ok_or(Error::Invalid("missing media extents"))?;
                    if self.index == media.len() {
                        self.index = 0;
                        self.phase = Phase::ValidateEnter;
                        continue;
                    }
                    if !work.try_charge(1) {
                        return Ok(InterleaveStatus::Yield);
                    }
                    let extent = &media[self.index];
                    if extent.start > extent.end
                        || (self.index > 0 && media[self.index - 1].end > extent.start)
                    {
                        return Err(Error::Invalid(
                            "media extents reversed/overlapping/unordered",
                        ));
                    }
                    self.index += 1;
                }
                Phase::ValidateEnter => {
                    if self.index == self.slots.len() {
                        if let Some(error) = self.layout_error {
                            return Err(error);
                        }
                        self.index = 0;
                        self.phase = Phase::Fill;
                        continue;
                    }
                    if !work.try_charge(1) {
                        return Ok(InterleaveStatus::Yield);
                    }
                    self.phase = Phase::ValidateRead;
                }
                Phase::ValidateRead => {
                    let slot = &mut self.slots.as_mut_slice()[self.index];
                    match slot.cursor.poll(work)? {
                        SampleStep::Yield => return Ok(InterleaveStatus::Yield),
                        SampleStep::End => {
                            slot.cursor = TrackSampleCursor::new(slot.source);
                            self.index += 1;
                            self.previous_start = None;
                            self.previous_ordinal = None;
                            self.phase = Phase::ValidateEnter;
                        }
                        SampleStep::Sample(sample) => {
                            self.validated =
                                self.validated.checked_add(1).ok_or(Error::Overflow)?;
                            if self.validated > self.limits.max_samples {
                                return Err(Error::Limit {
                                    requested: usize::try_from(self.validated)
                                        .map_err(|_| Error::Overflow)?,
                                    maximum: usize::try_from(self.limits.max_samples)
                                        .map_err(|_| Error::Overflow)?,
                                });
                            }
                            if sample.range.start > sample.range.end || sample.ordinal == 0 {
                                return Err(Error::Invalid("source sample range/ordinal"));
                            }
                            self.pending_validation = Some(sample);
                            self.extent_low = 0;
                            self.extent_high = self.media_extents.map_or(0, |media| media.len());
                            self.phase = if self.media_extents.is_some() {
                                Phase::ValidateBounds
                            } else {
                                Phase::ValidateOrder
                            };
                        }
                    }
                }
                Phase::ValidateBounds => {
                    let media = self
                        .media_extents
                        .ok_or(Error::Invalid("missing media extents"))?;
                    let sample = self
                        .pending_validation
                        .as_ref()
                        .ok_or(Error::Invalid("missing media validation sample"))?;
                    if self.extent_low < self.extent_high {
                        if !work.try_charge(1) {
                            return Ok(InterleaveStatus::Yield);
                        }
                        let mid = self.extent_low + (self.extent_high - self.extent_low) / 2;
                        if media[mid].start <= sample.range.start {
                            self.extent_low = mid.checked_add(1).ok_or(Error::Overflow)?;
                        } else {
                            self.extent_high = mid;
                        }
                        continue;
                    }
                    let selected = self
                        .extent_low
                        .checked_sub(1)
                        .ok_or(Error::Invalid("sample outside self-contained mdat"))?;
                    sample.require_within(media[selected].clone())?;
                    self.phase = Phase::ValidateOrder;
                }
                Phase::ValidateOrder => {
                    let sample = self
                        .pending_validation
                        .take()
                        .ok_or(Error::Invalid("missing source validation sample"))?;
                    if self
                        .previous_start
                        .is_some_and(|start| sample.range.start < start)
                    {
                        let error = Error::Unsupported(
                            "physical merge needs nondecreasing per-track source offsets",
                        );
                        if self.media_extents.is_none() {
                            return Err(error);
                        }
                        // Continue checking known source bounds in every track.
                        // A selected layout limit must not hide malformed ranges.
                        self.layout_error = Some(error);
                    }
                    if self
                        .previous_ordinal
                        .is_some_and(|ordinal| sample.ordinal <= ordinal)
                    {
                        return Err(Error::Invalid("source sample ordinal not increasing"));
                    }
                    self.previous_start = Some(sample.range.start);
                    self.previous_ordinal = Some(sample.ordinal);
                    self.phase = Phase::ValidateRead;
                }
                Phase::Fill => {
                    if self.index == self.slots.len() {
                        self.index = 0;
                        self.best = None;
                        self.phase = Phase::Select;
                        continue;
                    }
                    if !work.try_charge(1) {
                        return Ok(InterleaveStatus::Yield);
                    }
                    let slot = &self.slots.as_slice()[self.index];
                    if slot.ended || slot.head.is_some() {
                        self.index += 1;
                    } else {
                        // Charge entry once, then preserve this phase across a
                        // cursor yield. A one-unit quantum must make progress.
                        self.phase = Phase::ReadHead;
                    }
                }
                Phase::ReadHead => {
                    let slot = &mut self.slots.as_mut_slice()[self.index];
                    match slot.cursor.poll(work)? {
                        SampleStep::Yield => return Ok(InterleaveStatus::Yield),
                        SampleStep::Sample(sample) => slot.head = Some(sample),
                        SampleStep::End => slot.ended = true,
                    }
                    self.index += 1;
                    self.phase = Phase::Fill;
                }
                Phase::Select => {
                    if self.index == self.slots.len() {
                        let Some(selected) = self.best else {
                            self.phase = Phase::End;
                            continue;
                        };
                        let slot = &mut self.slots.as_mut_slice()[selected];
                        let sample = slot
                            .head
                            .take()
                            .ok_or(Error::Invalid("missing selected source head"))?;
                        self.ready = Some(InterleavedSample {
                            track_id: slot.source.track_id,
                            sample,
                        });
                        self.index = 0;
                        self.phase = Phase::Fill;
                        continue;
                    }
                    if !work.try_charge(1) {
                        return Ok(InterleaveStatus::Yield);
                    }
                    let slots = self.slots.as_slice();
                    if let Some(head) = &slots[self.index].head {
                        let earlier = match self.best {
                            Some(best) => {
                                head.range.start
                                    < slots[best]
                                        .head
                                        .as_ref()
                                        .ok_or(Error::Invalid("missing comparison source head"))?
                                        .range
                                        .start
                            }
                            None => true,
                        };
                        if earlier {
                            self.best = Some(self.index);
                        }
                    }
                    self.index += 1;
                }
                Phase::End => return Ok(InterleaveStatus::End),
            }
        }
    }
}

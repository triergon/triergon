//! Samples across tracks. [`SourceTrack`] names one track's regular table and fragments,
//! [`TrackSampleCursor`] reads one track as stored, and [`PhysicalSampleCursor`] merges
//! tracks in ascending byte order. The format states each sample's address but no merged
//! order; this one is byte order, not FFmpeg's interleaving, which
//! [`compat::mov`](crate::compat::mov) reproduces.
//!
//! Specification: Apple QuickTime File Format (chunk and sample addresses).
use crate::{
    CursorStatus, Next, Sample,
    boxes::BoxHeader,
    fragment::{FragmentCursor, FragmentIndex, RunPosition},
    sample::SampleSpan,
    table::{SampleCursor, SampleRanges, SampleTable, TimingRunPolicy, TimingRunSurplus},
};
use std::{
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};
use triergon_core::{Budget, BudgetVec, Error, Result, WorkBudget};

/// One track's borrowed indexes: its regular sample table, if any, and fragment indexes
/// in file order, of which those of other tracks are passed over. The caller makes sure
/// all of them come from the same source.
#[derive(Clone, Copy, Debug)]
pub struct SourceTrack<'a> {
    track_id: u32,
    regular: Option<&'a SampleTable>,
    fragments: &'a [FragmentIndex],
}
impl<'a> SourceTrack<'a> {
    /// The track with `track_ID` `track_id`; `Error::Invalid` for zero.
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
    pub(crate) fn track_id(self) -> u32 {
        self.track_id
    }
    // The borrowed indexes, for source seek planning.
    pub(crate) fn regular_index(self) -> Option<&'a SampleTable> {
        self.regular
    }
    pub(crate) fn fragment_indexes(self) -> &'a [FragmentIndex] {
        self.fragments
    }
    pub(crate) fn require_exact_timing(self) -> Result<()> {
        if self
            .regular
            .is_some_and(|table| table.timing_run_surplus() != TimingRunSurplus::default())
        {
            return Err(Error::Invalid("source view requires exact timing coverage"));
        }
        Ok(())
    }
}

/// Reads one track's samples as stored: the regular table's, then those of its fragments
/// in the order given. Edits are not applied and unknown times stay unknown. A failure is
/// kept for later polls.
#[derive(Clone, Debug)]
pub struct TrackSampleCursor<'a> {
    source: SourceTrack<'a>,
    regular: Option<SampleCursor<'a>>,
    next_fragment: usize,
    current: Option<FragmentCursor<'a>>,
    failed: Option<Error>,
}
impl<'a> TrackSampleCursor<'a> {
    /// A cursor from the track's first sample.
    pub fn new(source: SourceTrack<'a>) -> Self {
        Self {
            source,
            regular: source.regular.map(SampleTable::cursor),
            next_fragment: 0,
            current: None,
            failed: None,
        }
    }
    /// The cursor whose regular samples continue from `regular`, a cursor over
    /// the track's table.
    pub(crate) fn from_regular(source: SourceTrack<'a>, regular: SampleCursor<'a>) -> Self {
        Self {
            regular: Some(regular),
            ..Self::new(source)
        }
    }
    /// The cursor whose next sample comes from `current`, a cursor over
    /// fragment `next_fragment - 1` of the track's fragments, after the regular
    /// samples have ended.
    pub(crate) fn from_fragment(
        source: SourceTrack<'a>,
        next_fragment: usize,
        current: FragmentCursor<'a>,
    ) -> Self {
        Self {
            source,
            regular: None,
            next_fragment,
            current: Some(current),
            failed: None,
        }
    }
    /// Where the last fragment sample came from: its 0-based position in the given
    /// fragments and its 0-based run in that index; `None` outside the fragments.
    pub(crate) fn active_fragment_run(&self) -> Option<(usize, usize)> {
        self.current.as_ref().and_then(|cursor| {
            self.next_fragment
                .checked_sub(1)
                .map(|fragment| (fragment, cursor.run_index()))
        })
    }
    /// The next sample, for one work unit per sample and per fragment index passed.
    #[inline]
    pub fn poll(&mut self, work: &mut WorkBudget) -> Result<Next<Sample>> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = self.poll_inner(work);
        if let Err(error) = result {
            self.failed = Some(error);
        }
        result
    }
    fn poll_inner(&mut self, work: &mut WorkBudget) -> Result<Next<Sample>> {
        loop {
            if let Some(regular) = &mut self.regular {
                match regular.poll(work)? {
                    Next::End => self.regular = None,
                    other => return Ok(other),
                }
            }
            if let Some(current) = &mut self.current {
                match current.poll(work)? {
                    Next::End => self.current = None,
                    other => return Ok(other),
                }
            }
            let Some(fragment) = self.source.fragments.get(self.next_fragment) else {
                return Ok(Next::End);
            };
            if !work.try_charge(1) {
                return Ok(Next::Yield);
            }
            self.next_fragment += 1;
            if fragment.track_id() == self.source.track_id {
                self.current = Some(fragment.cursor());
            }
        }
    }
}

/// The byte ranges of one track's samples in source order, as spans of
/// consecutive samples with contiguous bytes: its regular samples a chunk at a
/// time, then each fragment run. They are the ranges [`TrackSampleCursor`]
/// reports, with the same errors.
#[derive(Clone, Copy, Debug)]
pub(crate) struct TrackSpans<'a> {
    source: SourceTrack<'a>,
    regular: Option<SampleRanges<'a>>,
    next_fragment: usize,
    current: Option<(&'a FragmentIndex, RunPosition)>,
}
impl<'a> TrackSpans<'a> {
    pub(crate) fn new(source: SourceTrack<'a>) -> Self {
        Self {
            source,
            regular: source.regular.map(SampleTable::ranges),
            next_fragment: 0,
            current: None,
        }
    }
    /// The next span of at most `maximum` samples, charging a work unit for each
    /// sample and for each run and fragment index passed.
    pub(crate) fn poll(&mut self, maximum: u64, work: &mut WorkBudget) -> Result<Next<SampleSpan>> {
        loop {
            if let Some(regular) = &mut self.regular {
                match regular.next_span(maximum, work)? {
                    Next::End => self.regular = None,
                    other => return Ok(other),
                }
            }
            if let Some((index, position)) = &mut self.current {
                match index.next_span(position, maximum, work)? {
                    Next::End => self.current = None,
                    other => return Ok(other),
                }
            }
            let Some(fragment) = self.source.fragments.get(self.next_fragment) else {
                return Ok(Next::End);
            };
            if !work.try_charge(1) {
                return Ok(Next::Yield);
            }
            self.next_fragment += 1;
            if fragment.track_id() == self.source.track_id {
                self.current = Some((fragment, RunPosition::default()));
            }
        }
    }
}

/// Bounds of a [`PhysicalSampleCursor`].
#[derive(Clone, Copy, Debug)]
pub struct PhysicalInterleaveLimits {
    /// Most tracks merged; default 64.
    pub max_tracks: usize,
    /// Most samples across all tracks; default 1,000,000.
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

/// A sample of a merge, with its track.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InterleavedSample {
    /// `track_ID` of the sample's track.
    pub track_id: u32,
    /// The sample as its track's cursor reports it.
    pub sample: Sample,
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
    TrackOrder,
    End,
}

/// Merges tracks into one sequence in ascending byte order of sample start, so that
/// payloads can be read one after another with
/// [`SampleReader`](crate::payload::SampleReader).
///
/// Before the first sample, a work-bounded pass over the metadata checks that each track's
/// samples start at nondecreasing offsets with increasing ordinals, the sample limit and,
/// with media extents, that every sample lies in an `mdat`. A track whose samples go
/// backwards in the file is valid but cannot be merged: polling returns
/// `Error::Unsupported` before any sample, and a cursor with media extents can then
/// continue track by track with [`resume_in_track_order`](Self::resume_in_track_order).
///
/// The merge then keeps one sample per track and one ready sample, charged to the budget,
/// and sorts nothing by time. Equal start offsets, zero-size samples included, follow the
/// given track order. Each sample costs two work units per track on top of its own
/// track's read. Poll until [`CursorStatus::Ready`], then take the sample with
/// [`take_sample`](Self::take_sample).
#[derive(Debug)]
pub struct PhysicalSampleCursor<'a> {
    slots: BudgetVec<Slot<'a>>,
    media_extents: Option<&'a [Range<u64>]>,
    forward_media: Option<ForwardRegularMdat>,
    deferred_media: bool,
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

/// The first `mdat` of a forward-only source whose movie comes first, with the source
/// length if known. Samples after it can be checked only once their own `mdat` is read.
#[derive(Clone, Copy, Debug)]
pub struct ForwardRegularMdat {
    first_start: u64,
    first_end: u64,
    source_length: Option<u64>,
}
impl ForwardRegularMdat {
    /// From the first `mdat`'s header. `Error::Invalid` for another box type or an extent
    /// past `source_length`, `Error::Unsupported` for an `mdat` of unknown size.
    pub fn new(header: BoxHeader, source_length: Option<u64>) -> Result<Self> {
        if header.kind != *b"mdat" {
            return Err(Error::Invalid("forward regular media header"));
        }
        let extent = header.payload_range()?;
        if extent.start > extent.end || source_length.is_some_and(|end| extent.end > end) {
            return Err(Error::Invalid("forward regular media extent"));
        }
        Ok(Self {
            first_start: extent.start,
            first_end: extent.end,
            source_length,
        })
    }
    pub(crate) fn require_initial_extent(self, media: &[Range<u64>]) -> Result<()> {
        if let [extent] = media
            && extent.start == self.first_start
            && extent.end == self.first_end
        {
            return Ok(());
        }
        Err(Error::Invalid("forward regular initial media extent"))
    }
    pub(crate) fn can_defer(self, range: &Range<u64>) -> bool {
        range.start >= self.first_end
            && range.end > self.first_end
            && self.source_length.is_none_or(|end| range.end <= end)
    }
}

impl<'a> PhysicalSampleCursor<'a> {
    /// A merge that checks the metadata only: the caller checks that samples lie inside
    /// an `mdat` before reading them. Fails as
    /// [`with_timing_policy`](Self::with_timing_policy) does under the strict policy.
    pub fn new(
        budget: &Budget,
        tracks: &[SourceTrack<'a>],
        limits: PhysicalInterleaveLimits,
    ) -> Result<Self> {
        Self::with_timing_policy(budget, tracks, None, limits, TimingRunPolicy::Strict)
    }
    /// A merge that also checks every sample against `media_extents`, the payload extents
    /// of the source's `mdat` boxes, which must ascend without overlap. A sample outside
    /// them is `Error::Invalid` (as [`Sample::require_within`] decides). A backward layout
    /// is refused only after every track's samples are checked, so it never hides a
    /// malformed range.
    pub fn new_with_media_extents(
        budget: &Budget,
        tracks: &[SourceTrack<'a>],
        media_extents: &'a [Range<u64>],
        limits: PhysicalInterleaveLimits,
    ) -> Result<Self> {
        Self::with_timing_policy(
            budget,
            tracks,
            Some(media_extents),
            limits,
            TimingRunPolicy::Strict,
        )
    }
    /// A merge for a forward-only source whose movie comes first: `media` must be exactly
    /// the first `mdat`'s extent that `bounds` records, and samples after it that fit the
    /// source are deferred (see [`deferred_media`](Self::deferred_media)). The timing
    /// policy is as for [`with_timing_policy`](Self::with_timing_policy);
    /// `Error::Unsupported` for tracks with fragments.
    pub fn new_forward_regular_with_timing_policy(
        budget: &Budget,
        tracks: &[SourceTrack<'a>],
        media: &'a [Range<u64>],
        bounds: ForwardRegularMdat,
        limits: PhysicalInterleaveLimits,
        timing_policy: TimingRunPolicy,
    ) -> Result<Self> {
        bounds.require_initial_extent(media)?;
        let mut cursor =
            Self::with_timing_policy(budget, tracks, Some(media), limits, timing_policy)?;
        if tracks.iter().any(|track| !track.fragments.is_empty()) {
            return Err(Error::Unsupported("forward regular media with fragments"));
        }
        cursor.forward_media = Some(bounds);
        Ok(cursor)
    }
    /// Whether the metadata pass found samples after the first `mdat`, whose ranges are
    /// checked only when their `mdat` is read.
    pub fn deferred_media(&self) -> bool {
        self.deferred_media
    }
    /// A merge with optional `media_extents` and a chosen timing policy; under
    /// [`TimingRunPolicy::BoundedPrefix`] the samples of a table with surplus timing runs
    /// are merged too. `Error::Limit` above `max_tracks`; `Error::Invalid` for a repeated
    /// track ID or, under the strict policy, a table whose timing runs are not exact.
    pub fn with_timing_policy(
        budget: &Budget,
        tracks: &[SourceTrack<'a>],
        media_extents: Option<&'a [Range<u64>]>,
        limits: PhysicalInterleaveLimits,
        timing_policy: TimingRunPolicy,
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
            if timing_policy == TimingRunPolicy::Strict {
                track.require_exact_timing()?;
            }
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
            forward_media: None,
            deferred_media: false,
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
    /// Samples the metadata pass has visited, one that failed it included; after the
    /// pass, the total across tracks.
    pub fn validated_samples(&self) -> u64 {
        self.validated
    }
    /// After the backward-layout refusal of a cursor with media extents, continues with
    /// each track's samples, one track after another in the order given; `false`, with
    /// nothing changed, in any other state. A cursor without media extents refuses at the
    /// first backward sample, before the rest are checked, and cannot continue; nor can one
    /// with deferred media. The samples keep their ordinals but are not in byte order.
    pub fn resume_in_track_order(&mut self) -> bool {
        if self.phase != Phase::ValidateEnter
            || self.index != self.slots.len()
            || self.layout_error.is_none()
            || self.failed != self.layout_error
            || self.deferred_media
        {
            return false;
        }
        self.failed = None;
        self.index = 0;
        self.phase = Phase::TrackOrder;
        true
    }
    /// The sample a [`CursorStatus::Ready`] poll announced; `Error::Invalid` when none is
    /// ready.
    pub fn take_sample(&mut self) -> Result<InterleavedSample> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        self.ready
            .take()
            .ok_or(Error::Invalid("no ready interleaved sample"))
    }
    /// One bounded step; `Error::Cancelled` once `cancel` is set. A failure is kept for
    /// later polls.
    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<CursorStatus> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = self.poll_inner(work, cancel);
        if let Err(error) = result {
            self.failed = Some(error);
        }
        result
    }
    // One loop over the phases: `Fill`, `ReadHead` and `Select` run per sample, and moving
    // the validation phases out of the loop changes their code.
    fn poll_inner(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<CursorStatus> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if self.ready.is_some() {
                return Ok(CursorStatus::Ready);
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
                        return Ok(CursorStatus::Yield);
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
                        return Ok(CursorStatus::Yield);
                    }
                    self.phase = Phase::ValidateRead;
                }
                Phase::ValidateRead => {
                    let slot = &mut self.slots.as_mut_slice()[self.index];
                    match slot.cursor.poll(work)? {
                        Next::Yield => return Ok(CursorStatus::Yield),
                        Next::End => {
                            slot.cursor = TrackSampleCursor::new(slot.source);
                            self.index += 1;
                            self.previous_start = None;
                            self.previous_ordinal = None;
                            self.phase = Phase::ValidateEnter;
                        }
                        Next::Item(sample) => {
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
                            return Ok(CursorStatus::Yield);
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
                    if self
                        .forward_media
                        .is_some_and(|bounds| bounds.can_defer(&sample.range))
                    {
                        self.deferred_media = true;
                    } else {
                        sample.require_within(media[selected].clone())?;
                    }
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
                        return Ok(CursorStatus::Yield);
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
                        Next::Yield => return Ok(CursorStatus::Yield),
                        Next::Item(sample) => slot.head = Some(sample),
                        Next::End => slot.ended = true,
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
                        return Ok(CursorStatus::Yield);
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
                Phase::TrackOrder => {
                    let Some(slot) = self.slots.as_mut_slice().get_mut(self.index) else {
                        self.phase = Phase::End;
                        continue;
                    };
                    match slot.cursor.poll(work)? {
                        Next::Yield => return Ok(CursorStatus::Yield),
                        Next::End => self.index += 1,
                        Next::Item(sample) => {
                            self.ready = Some(InterleavedSample {
                                track_id: slot.source.track_id,
                                sample,
                            });
                        }
                    }
                }
                Phase::End => return Ok(CursorStatus::End),
            }
        }
    }
}

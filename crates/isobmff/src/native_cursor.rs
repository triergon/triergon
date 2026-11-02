//! Bounded all-stream occurrence iterator for the explicitly pinned MOV view.
//!
//! Source grammar/clocks come from the independent source indexes. Selection,
//! regular raw32 clock projection and fragment bit4 are separate observation
//! rules, challenged by the original, first42 and follow-up39 native captures.
//! The cp18 native30 controls additionally challenge signed-raw fragment CTO
//! compensation and explicit complete-source/predecessor origin derivation.
//! The native4 plus run-partition discriminator refute the lifetime-maximum
//! extrapolation: bit4 uses the previous run's tail, frozen while appending a run.
//! No native implementation code, expanded sample array, I/O or payload storage.
use crate::{
    EditList, FragmentIndex, Mov9CursorStatus, Mov9DurationBasis, Mov9EditCursor, Mov9EditLimits,
    Mov9MediaClock, Mov9PacketView, Mov9SchedulingClock, Mov9View, Sample, SampleStep, SampleTable,
    SourceTrack, TrackSampleCursor,
};
use std::{
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};
use triergon_core::{Budget, BudgetVec, Error, Result, TimeBase, WorkBudget};

/// Opt-in compatibility operation, not a source/presentation conformance rule.
/// Forced mov, seekable input, no stream-info, NOPARSE|NOFILLIN (48), pinned n9.0.
#[derive(Clone, Copy, Debug)]
pub enum Mov9Profile {
    PinnedNoParseNoFillin,
}
impl Mov9Profile {
    pub const WORKER_SHA256: &'static str =
        "d97ce08126efe876cf92dfaac3655073da87fd8c590793a7d37ffd9e1fbea2d2";
    /// Same upstream revision, separately built Linux ARM64 observation worker.
    pub const LINUX_BOUNDARY_WORKER_SHA256: &'static str =
        "e8fcc2429e2086b5d0edf4929c098e0150c715ffa3faafc00e9b691e0128c14f";
}

/// Source association is established by the caller, independently of I/O request
/// tokens and codec epochs. Supply these in actual movie catalogue order.
#[derive(Clone, Copy, Debug)]
pub struct Mov9Track<'a> {
    pub track_id: u32,
    pub table: &'a SampleTable,
    pub fragments: &'a [FragmentIndex],
    pub media_base: TimeBase,
    pub edits: Option<(&'a EditList, TimeBase)>,
}
/// Zero-based native occurrence position, not a source sample ordinal or I/O token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mov9ResumePoint {
    pub track_id: u32,
    pub occurrence: u64,
}
#[derive(Clone, Copy, Debug)]
pub struct Mov9PacketLimits {
    pub max_tracks: usize,
    pub max_media_extents: usize,
    pub max_source_samples: u64,
    pub max_occurrences: u64,
    pub edit: Mov9EditLimits,
}
impl Default for Mov9PacketLimits {
    fn default() -> Self {
        Self {
            max_tracks: 64,
            max_media_extents: 1_000_000,
            max_source_samples: 1_000_000,
            max_occurrences: 1_000_000,
            edit: Mov9EditLimits::default(),
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mov9EditIdentity {
    pub sequence: u64,
    pub edit_ordinal: u32,
    pub duration_basis: Mov9DurationBasis,
}
/// Provenance for a native clock. Derived variants never fill source Sample time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mov9ClockOrigin {
    SourceClock,
    PredecessorEnd(Mov9PredecessorClock),
    /// Only the explicit complete-source constructor may establish this native
    /// observation baseline at source ordinal1 with an empty regular table.
    CompleteMediaStart,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mov9PredecessorClock {
    pub predecessor_sample: u64,
    pub predecessor_native_dts_ticks: i128,
    pub predecessor_duration_ticks: u32,
    pub predecessor_source_dts_known: bool,
    pub derived_dts_ticks: i128,
}
#[derive(Clone, Debug)]
pub struct Mov9PacketOccurrence {
    pub track_id: u32,
    pub source: Sample,
    pub native: Mov9PacketView,
    pub edit: Option<Mov9EditIdentity>,
    pub clock_origin: Mov9ClockOrigin,
    pub media_clock: Mov9MediaClock,
    pub scheduling_clock: Mov9SchedulingClock,
}

#[derive(Clone, Copy, Debug)]
struct PreviousClock {
    sample: u64,
    dts: i128,
    signed_dts: i64,
    duration: u32,
    source_known: bool,
}
#[derive(Debug)]
struct Slot<'a> {
    resume_remaining: u64,
    track: Mov9Track<'a>,
    cursor: TrackSampleCursor<'a>,
    edited: Option<Mov9EditCursor<'a>>,
    head: Option<Mov9PacketOccurrence>,
    clock: Mov9MediaClock,
    scheduling_clock: Mov9SchedulingClock,
    shift: i64,
    append_run: Option<(usize, usize)>,
    discard_boundary: Option<i128>,
    fragment_cto_minimum: i64,
    previous_clock: Option<PreviousClock>,
    saw_fragment: bool,
    ended: bool,
}
impl<'a> Slot<'a> {
    fn source(&self) -> Result<SourceTrack<'a>> {
        SourceTrack::new(
            self.track.track_id,
            Some(self.track.table),
            self.track.fragments,
        )
    }
    fn fragment_origin(
        &self,
        sample: &Sample,
        complete_source: bool,
    ) -> Result<(i128, Mov9ClockOrigin)> {
        if sample.dts_ticks.is_some() != sample.cts_ticks.is_some() {
            return Err(Error::Invalid("inconsistent source clock knownness"));
        }
        if let Some(dts) = sample.dts_ticks {
            return Ok((dts, Mov9ClockOrigin::SourceClock));
        }
        if let Some(previous) = self.previous_clock {
            if previous.sample.checked_add(1) != Some(sample.ordinal) {
                return Err(Error::Unsupported("native predecessor source ordinal gap"));
            }
            let dts = previous
                .dts
                .checked_add(i128::from(previous.duration))
                .ok_or(Error::Overflow)?;
            return Ok((
                dts,
                Mov9ClockOrigin::PredecessorEnd(Mov9PredecessorClock {
                    predecessor_sample: previous.sample,
                    predecessor_native_dts_ticks: previous.dts,
                    predecessor_duration_ticks: previous.duration,
                    predecessor_source_dts_known: previous.source_known,
                    derived_dts_ticks: dts,
                }),
            ));
        }
        if complete_source && self.track.table.sample_count() == 0 && sample.ordinal == 1 {
            return Ok((0, Mov9ClockOrigin::CompleteMediaStart));
        }
        Err(Error::Unsupported(
            "native first origin needs complete-source context",
        ))
    }
    fn fill(
        &mut self,
        work: &mut WorkBudget,
        complete_source: bool,
        global_schedule: bool,
    ) -> Result<Mov9CursorStatus> {
        let mut occurrence = if let Some(cursor) = &mut self.edited {
            match cursor.poll(work)? {
                Mov9CursorStatus::Ready => {
                    let occurrence = cursor.take_occurrence()?;
                    Mov9PacketOccurrence {
                        track_id: self.track.track_id,
                        source: occurrence.source,
                        native: occurrence.native,
                        edit: Some(Mov9EditIdentity {
                            sequence: occurrence.sequence,
                            edit_ordinal: occurrence.edit_ordinal,
                            duration_basis: occurrence.duration_basis,
                        }),
                        clock_origin: Mov9ClockOrigin::SourceClock,
                        media_clock: self.clock,
                        scheduling_clock: Mov9SchedulingClock::SingleTrack,
                    }
                }
                other => return Ok(other),
            }
        } else {
            let sample = match self.cursor.poll(work)? {
                SampleStep::Sample(sample) => sample,
                SampleStep::Yield => return Ok(Mov9CursorStatus::Yield),
                SampleStep::End => return Ok(Mov9CursorStatus::End),
            };
            let view = Mov9View::new(
                self.track.media_base,
                if self.saw_fragment {
                    None
                } else {
                    self.track.table.sync_entry_count()
                },
            );
            let (dts, clock_origin) = if sample.fragment_flags.is_some() {
                self.fragment_origin(&sample, complete_source)?
            } else {
                (
                    sample
                        .dts_ticks
                        .ok_or(Error::Unsupported("native source DTS unknown"))?,
                    Mov9ClockOrigin::SourceClock,
                )
            };
            let mut native = if sample.fragment_flags.is_some() {
                view.project_fragment(&sample, dts, self.fragment_cto_minimum)?
            } else {
                view.project_regular(&sample, self.shift)?
            };
            if sample.fragment_flags.is_some() {
                let run = self
                    .cursor
                    .active_fragment_run()
                    .ok_or(Error::Invalid("native fragment sample has no source run"))?;
                if self.append_run != Some(run) {
                    // Actual pinned-native partition controls distinguish trun
                    // appends from a whole traf or lifetime peak. Snapshot the
                    // immediately preceding index tail once, even after rollback.
                    // Current-run samples never raise this discard threshold.
                    self.discard_boundary = Some(
                        self.previous_clock
                            .map_or(i128::from(i64::MIN), |previous| {
                                i128::from(previous.signed_dts)
                            }),
                    );
                    self.append_run = Some(run);
                }
                if self
                    .discard_boundary
                    .is_some_and(|previous| i128::from(native.dts_field.signed_ticks()) <= previous)
                {
                    native.flags |= 4;
                }
                self.previous_clock = Some(PreviousClock {
                    sample: sample.ordinal,
                    dts,
                    signed_dts: native.dts_field.signed_ticks(),
                    duration: sample.duration_ticks,
                    source_known: sample.dts_ticks.is_some(),
                });
            }
            Mov9PacketOccurrence {
                track_id: self.track.track_id,
                source: sample,
                native,
                edit: None,
                clock_origin,
                media_clock: self.clock,
                scheduling_clock: Mov9SchedulingClock::SingleTrack,
            }
        };
        self.scheduling_clock = if global_schedule {
            Mov9SchedulingClock::from_native(occurrence.native.dts_field, self.clock.native_base)?
        } else {
            Mov9SchedulingClock::SingleTrack
        };
        occurrence.scheduling_clock = self.scheduling_clock;
        self.head = Some(occurrence);
        Ok(Mov9CursorStatus::Ready)
    }
}

#[derive(Clone, Copy, Debug)]
enum Phase {
    Media,
    Validate,
    Bounds,
    Clock,
    Initialize,
    Fill,
    Select,
    End,
}

/// One charged slot/head per track (including the existing edit cursor's bounded
/// lookahead), plus one pending validation sample and one ready occurrence.
/// Polling is cancellable/work-bounded, with sticky failures.
/// Preflight validates *all* source sample extents before native policy refusals.
/// Backward source addresses remain legal: the consumer performs checked ranges.
#[derive(Debug)]
pub struct Mov9PacketCursor<'a> {
    slots: BudgetVec<Slot<'a>>,
    media: &'a [Range<u64>],
    limits: Mov9PacketLimits,
    phase: Phase,
    at: usize,
    row: u32,
    previous_ordinal: u64,
    validated: u64,
    generated: u64,
    checking: Option<Sample>,
    low: usize,
    high: usize,
    best: Option<usize>,
    ready: Option<Mov9PacketOccurrence>,
    failed: Option<Error>,
    complete_source: bool,
}

fn count(value: u64, maximum: u64) -> Result<()> {
    if value > maximum {
        return Err(Error::Limit {
            requested: usize::try_from(value).map_err(|_| Error::Overflow)?,
            maximum: usize::try_from(maximum).map_err(|_| Error::Overflow)?,
        });
    }
    Ok(())
}
impl<'a> Mov9PacketCursor<'a> {
    pub fn new(
        budget: &Budget,
        tracks: &[Mov9Track<'a>],
        media: &'a [Range<u64>],
        limits: Mov9PacketLimits,
        _profile: Mov9Profile,
    ) -> Result<Self> {
        let max_tracks = limits.max_tracks.min(64);
        if tracks.len() > max_tracks {
            return Err(Error::Limit {
                requested: tracks.len(),
                maximum: max_tracks,
            });
        }
        if media.len() > limits.max_media_extents {
            return Err(Error::Limit {
                requested: media.len(),
                maximum: limits.max_media_extents,
            });
        }
        // At most64 tracks: bound constructor work independently of input size.
        for (at, track) in tracks.iter().enumerate() {
            if tracks[..at]
                .iter()
                .any(|prior| prior.track_id == track.track_id)
            {
                return Err(Error::Invalid("duplicate native track ID"));
            }
        }
        let regular_samples = tracks.iter().try_fold(0u64, |total, track| {
            total
                .checked_add(track.table.sample_count())
                .ok_or(Error::Overflow)
        })?;
        count(regular_samples, limits.max_source_samples)?;
        let mut slots = BudgetVec::new(budget);
        slots.try_reserve_exact(tracks.len())?;
        for track in tracks {
            let source = SourceTrack::new(track.track_id, Some(track.table), track.fragments)?;
            slots
                .try_push(Slot {
                    resume_remaining: 0,
                    track: *track,
                    cursor: TrackSampleCursor::new(source),
                    edited: None,
                    head: None,
                    clock: Mov9MediaClock::from_source(track.media_base)?,
                    scheduling_clock: Mov9SchedulingClock::SingleTrack,
                    shift: 0,
                    append_run: None,
                    discard_boundary: None,
                    fragment_cto_minimum: 0,
                    previous_clock: None,
                    saw_fragment: false,
                    ended: false,
                })
                .map_err(|error| error.error)?;
        }
        Ok(Self {
            slots,
            media,
            limits,
            phase: Phase::Media,
            at: 0,
            row: 0,
            previous_ordinal: 0,
            validated: 0,
            generated: 0,
            checking: None,
            low: 0,
            high: 0,
            best: None,
            ready: None,
            failed: None,
            complete_source: false,
        })
    }
    /// The caller establishes one complete original media source and all its
    /// preceding track samples. This enables the pinned native start-zero rule
    /// for an unknown first fragment clock; source timestamps remain unknown.
    /// Do not use for isolated/range fragments, missing prior segments or a
    /// continuation whose earlier samples are absent. `new` keeps that refusal.
    pub fn new_complete_source(
        budget: &Budget,
        tracks: &[Mov9Track<'a>],
        media: &'a [Range<u64>],
        limits: Mov9PacketLimits,
        profile: Mov9Profile,
    ) -> Result<Self> {
        let mut cursor = Self::new(budget, tracks, media, limits, profile)?;
        cursor.complete_source = true;
        Ok(cursor)
    }
    pub fn validated_samples(&self) -> u64 {
        self.validated
    }
    /// Reconstruct each track's real native state up to its checked occurrence
    /// position before the existing global selector begins delivery. Full source
    /// extent validation still runs first; skipped metadata reads no payload.
    pub fn new_complete_source_at(
        budget: &Budget,
        tracks: &[Mov9Track<'a>],
        media: &'a [Range<u64>],
        limits: Mov9PacketLimits,
        profile: Mov9Profile,
        starts: &[Mov9ResumePoint],
    ) -> Result<Self> {
        let mut cursor = Self::new_complete_source(budget, tracks, media, limits, profile)?;
        if starts.len() != tracks.len() {
            return Err(Error::Invalid("native resume track coverage"));
        }
        let skipped = starts.iter().try_fold(0u64, |sum, p| {
            sum.checked_add(p.occurrence).ok_or(Error::Overflow)
        })?;
        count(skipped, limits.max_occurrences)?;
        for (i, start) in starts.iter().enumerate() {
            if starts[..i].iter().any(|p| p.track_id == start.track_id) {
                return Err(Error::Invalid("duplicate native resume track"));
            }
            count(start.occurrence, limits.max_occurrences)?;
            let slot = cursor
                .slots
                .as_mut_slice()
                .iter_mut()
                .find(|s| s.track.track_id == start.track_id)
                .ok_or(Error::Invalid("unknown native resume track"))?;
            slot.resume_remaining = start.occurrence;
        }
        Ok(cursor)
    }
    /// Provenance of the currently held source run, accessed before take. O(1)
    /// in the selected single-track index cursor; no run/sample array expansion.
    pub(crate) fn ready_source_run(&self) -> Result<Option<(usize, usize, u32)>> {
        let ready = self
            .ready
            .as_ref()
            .ok_or(Error::Invalid("no ready native occurrence"))?;
        if ready.source.fragment_flags.is_none() {
            return Ok(None);
        }
        let slot = self
            .slots
            .as_slice()
            .iter()
            .find(|s| s.track.track_id == ready.track_id)
            .ok_or(Error::Invalid("native source slot absent"))?;
        let (fragment, run) = slot
            .append_run
            .ok_or(Error::Invalid("native source run absent"))?;
        let index = slot
            .track
            .fragments
            .get(fragment)
            .ok_or(Error::Invalid("native source fragment absent"))?;
        Ok(Some((fragment, run, index.run_flags(run)?)))
    }
    pub fn take_occurrence(&mut self) -> Result<Mov9PacketOccurrence> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        self.ready
            .take()
            .ok_or(Error::Invalid("no ready native packet"))
    }
    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Mov9CursorStatus> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = self.poll_inner(work, cancel);
        if let Err(error) = result {
            self.failed = Some(error);
            self.ready = None;
        }
        result
    }
    fn poll_inner(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Mov9CursorStatus> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if self.ready.is_some() {
                return Ok(Mov9CursorStatus::Ready);
            }
            match self.phase {
                Phase::Media => {
                    if self.at == self.media.len() {
                        self.at = 0;
                        self.phase = Phase::Validate;
                        continue;
                    }
                    if !work.try_charge(1) {
                        return Ok(Mov9CursorStatus::Yield);
                    }
                    let range = &self.media[self.at];
                    if range.start > range.end
                        || (self.at > 0 && self.media[self.at - 1].end > range.start)
                    {
                        return Err(Error::Invalid("native media extents order/overlap"));
                    }
                    self.at += 1;
                }
                Phase::Validate => {
                    if self.at == self.slots.len() {
                        self.at = 0;
                        self.phase = Phase::Clock;
                        continue;
                    }
                    let slot = &mut self.slots.as_mut_slice()[self.at];
                    match slot.cursor.poll(work)? {
                        SampleStep::Yield => return Ok(Mov9CursorStatus::Yield),
                        SampleStep::End => {
                            if !work.try_charge(1) {
                                return Ok(Mov9CursorStatus::Yield);
                            }
                            self.at += 1;
                            self.previous_ordinal = 0;
                        }
                        SampleStep::Sample(sample) => {
                            self.validated =
                                self.validated.checked_add(1).ok_or(Error::Overflow)?;
                            count(self.validated, self.limits.max_source_samples)?;
                            if sample.ordinal <= self.previous_ordinal {
                                return Err(Error::Invalid("native source ordinal order"));
                            }
                            self.previous_ordinal = sample.ordinal;
                            slot.saw_fragment |= sample.fragment_flags.is_some();
                            if let Some(offset) = sample
                                .composition_offset
                                .filter(|_| sample.fragment_flags.is_some())
                            {
                                let signed = i32::from_be_bytes(offset.raw_bits().to_be_bytes());
                                slot.fragment_cto_minimum =
                                    slot.fragment_cto_minimum.min(i64::from(signed));
                            }
                            self.checking = Some(sample);
                            self.low = 0;
                            self.high = self.media.len();
                            self.phase = Phase::Bounds;
                        }
                    }
                }
                Phase::Bounds => {
                    if !work.try_charge(1) {
                        return Ok(Mov9CursorStatus::Yield);
                    }
                    let sample = self
                        .checking
                        .as_ref()
                        .ok_or(Error::Invalid("native pending validation"))?;
                    if self.low < self.high {
                        let mid = self.low + (self.high - self.low) / 2;
                        if self.media[mid].start <= sample.range.start {
                            self.low = mid + 1;
                        } else {
                            self.high = mid;
                        }
                    } else {
                        let extent = self
                            .low
                            .checked_sub(1)
                            .and_then(|at| self.media.get(at))
                            .ok_or(Error::Invalid("native sample outside mdat"))?;
                        sample.require_within(extent.clone())?;
                        self.checking = None;
                        self.phase = Phase::Validate;
                    }
                }
                Phase::Clock => {
                    if self.at == self.slots.len() {
                        self.at = 0;
                        self.phase = Phase::Initialize;
                        continue;
                    }
                    if !work.try_charge(1) {
                        return Ok(Mov9CursorStatus::Yield);
                    }
                    let slot = &mut self.slots.as_mut_slice()[self.at];
                    if let Some(runs) = slot
                        .track
                        .table
                        .composition_runs()
                        .filter(|runs| self.row < runs.count.saturating_sub(2))
                    {
                        // Explicit pinned native rule, regardless of the
                        // source ctts version. Source runs are never changed.
                        let offset = i32::from_be_bytes(runs.word(self.row, 4)?.to_be_bytes());
                        slot.shift = slot.shift.min(i64::from(offset));
                        self.row += 1;
                        continue;
                    }
                    self.at += 1;
                    self.row = 0;
                }
                Phase::Initialize => {
                    if self.at == self.slots.len() {
                        self.at = 0;
                        self.phase = Phase::Fill;
                        continue;
                    }
                    if !work.try_charge(1) {
                        return Ok(Mov9CursorStatus::Yield);
                    }
                    let slot = &mut self.slots.as_mut_slice()[self.at];
                    if slot.saw_fragment && slot.track.table.sample_count() != 0 {
                        return Err(Error::Unsupported(
                            "native mixed regular/fragment clock profile",
                        ));
                    }
                    slot.edited = if let Some((edits, movie_base)) = slot.track.edits {
                        if slot.saw_fragment {
                            return Err(Error::Unsupported(
                                "native edited fragment occurrence profile",
                            ));
                        }
                        Some(Mov9EditCursor::new(
                            slot.track.table,
                            edits,
                            slot.track.media_base,
                            movie_base,
                            self.limits.edit,
                        )?)
                    } else {
                        None
                    };
                    slot.cursor = TrackSampleCursor::new(slot.source()?);
                    self.at += 1;
                }
                Phase::Fill => {
                    if self.at == self.slots.len() {
                        self.at = 0;
                        self.best = None;
                        self.phase = Phase::Select;
                        continue;
                    }
                    let global_schedule = self.slots.len() > 1;
                    let slot = &mut self.slots.as_mut_slice()[self.at];
                    if slot.ended {
                        if !work.try_charge(1) {
                            return Ok(Mov9CursorStatus::Yield);
                        }
                        self.at += 1;
                        continue;
                    }
                    if slot.head.is_some() {
                        if !work.try_charge(1) {
                            return Ok(Mov9CursorStatus::Yield);
                        }
                        if slot.resume_remaining != 0 {
                            slot.resume_remaining -= 1;
                            slot.head = None;
                            continue;
                        }
                        self.at += 1;
                        continue;
                    }
                    match slot.fill(work, self.complete_source, global_schedule)? {
                        Mov9CursorStatus::Yield => return Ok(Mov9CursorStatus::Yield),
                        Mov9CursorStatus::End => {
                            if slot.resume_remaining != 0 {
                                return Err(Error::Invalid(
                                    "native resume position outside occurrences",
                                ));
                            }
                            slot.ended = true;
                        }
                        Mov9CursorStatus::Ready => {
                            continue;
                        }
                    }
                    if !work.try_charge(1) {
                        return Ok(Mov9CursorStatus::Yield);
                    }
                    self.at += 1;
                }
                Phase::Select => {
                    if self.at == self.slots.len() {
                        let Some(best) = self.best else {
                            self.phase = Phase::End;
                            continue;
                        };
                        if !work.try_charge(1) {
                            return Ok(Mov9CursorStatus::Yield);
                        }
                        self.generated = self.generated.checked_add(1).ok_or(Error::Overflow)?;
                        count(self.generated, self.limits.max_occurrences)?;
                        self.ready = self.slots.as_mut_slice()[best].head.take();
                        self.at = 0;
                        self.phase = Phase::Fill;
                        continue;
                    }
                    if !work.try_charge(1) {
                        return Ok(Mov9CursorStatus::Yield);
                    }
                    let slots = self.slots.as_slice();
                    if let Some(contender) = slots[self.at].head.as_ref() {
                        let prefer = if let Some(best) = self.best {
                            let incumbent = slots[best]
                                .head
                                .as_ref()
                                .ok_or(Error::Invalid("native selector head missing"))?;
                            match (
                                slots[self.at].scheduling_clock,
                                slots[best].scheduling_clock,
                            ) {
                                (
                                    Mov9SchedulingClock::ExactMicroseconds(contender_clock),
                                    Mov9SchedulingClock::ExactMicroseconds(incumbent_clock),
                                ) => {
                                    let distance = (i128::from(contender_clock)
                                        - i128::from(incumbent_clock))
                                    .abs();
                                    if distance > 1_000_000 {
                                        contender_clock < incumbent_clock
                                    } else {
                                        contender.native.source_range.start
                                            < incumbent.native.source_range.start
                                    }
                                }
                                // Observed undefined/native-overflow scheduling
                                // heads retain the incumbent catalogue slot.
                                // No source timestamp/key is synthesized.
                                _ => false,
                            }
                        } else {
                            true
                        };
                        if prefer {
                            self.best = Some(self.at);
                        }
                    }
                    self.at += 1;
                }
                Phase::End => return Ok(Mov9CursorStatus::End),
            }
        }
    }
}

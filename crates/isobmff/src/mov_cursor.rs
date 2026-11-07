//! Bounded all-stream occurrence iterator for the explicitly selected MOV view.
//!
//! Source grammar/clocks come from the independent source indexes. Selection,
//! regular raw32 clock projection and fragment bit4 are separate observation
//! rules, checked against demux captures, including signed-raw fragment CTO
//! compensation and explicit complete-source/predecessor origin derivation.
//! bit4 uses the previous run's tail, not a lifetime maximum, frozen while
//! appending a run.
//! No demux implementation code, expanded sample array, I/O or payload storage.
use crate::{
    CursorStatus, DurationBasis, EditCursor, EditLimits, EditList, FragmentIndex, MediaClock,
    PacketProjector, PacketView, Sample, SampleStep, SampleTable, SchedulingClock, SourceTrack,
    TrackSampleCursor,
};
use std::{
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use triergon_core::{Budget, BudgetVec, Error, Result, TimeBase, WorkBudget};

mod range_seek;
use range_seek::Checkpoints;
pub use range_seek::RangeSeekJob;

/// Opt-in compatibility operation, not a source/presentation conformance rule.
/// The original constructors use forced MOV, seekable input, no stream-info and
/// NOPARSE|NOFILLIN (48). `new_front_source` explicitly adds the
/// separately observed nonseekable, from-start profile; it does not change those
/// default operations or establish a complete-entity/EOF Source fact.
#[derive(Clone, Copy, Debug)]
pub enum DemuxProfile {
    NoParseNoFillin,
}
#[cfg(feature = "reference-trace")]
impl DemuxProfile {
    pub const WORKER_SHA256: &'static str =
        "d97ce08126efe876cf92dfaac3655073da87fd8c590793a7d37ffd9e1fbea2d2";
    /// Same upstream revision, separately built Linux ARM64 observation worker.
    pub const LINUX_BOUNDARY_WORKER_SHA256: &'static str =
        "e8fcc2429e2086b5d0edf4929c098e0150c715ffa3faafc00e9b691e0128c14f";
}

/// Source association is established by the caller, independently of I/O request
/// tokens and codec epochs. Supply these in actual movie catalogue order.
#[derive(Clone, Copy, Debug)]
pub struct DemuxTrack<'a> {
    pub track_id: u32,
    pub table: &'a SampleTable,
    pub fragments: &'a [FragmentIndex],
    pub media_base: TimeBase,
    pub edits: Option<(&'a EditList, TimeBase)>,
}
/// Zero-based demux occurrence position, not a source sample ordinal or I/O token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResumePoint {
    pub track_id: u32,
    pub occurrence: u64,
}
#[derive(Clone, Copy, Debug)]
pub struct PacketLimits {
    pub max_tracks: usize,
    pub max_media_extents: usize,
    pub max_source_samples: u64,
    pub max_occurrences: u64,
    pub edit: EditLimits,
}
impl Default for PacketLimits {
    fn default() -> Self {
        Self {
            max_tracks: 64,
            max_media_extents: 1_000_000,
            max_source_samples: 1_000_000,
            max_occurrences: 1_000_000,
            edit: EditLimits::default(),
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EditIdentity {
    pub sequence: u64,
    pub edit_ordinal: u32,
    pub duration_basis: DurationBasis,
}
/// Provenance for a demux clock. Derived variants never fill source Sample time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClockOrigin {
    SourceClock,
    PredecessorEnd(PredecessorClock),
    /// Only the explicit complete-source constructor establishes this origin.
    CompleteMediaStart,
    /// Explicit from-start front metadata establishes a demux origin, without
    /// claiming that the entity has reached physical EOF or filling Source time.
    FrontSourceMediaStart,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PredecessorClock {
    pub predecessor_sample: u64,
    pub predecessor_demux_dts_ticks: i128,
    pub predecessor_duration_ticks: u32,
    pub predecessor_source_dts_known: bool,
    pub derived_dts_ticks: i128,
}
#[derive(Clone, Debug)]
pub struct PacketOccurrence {
    pub track_id: u32,
    pub source: Sample,
    pub demux: PacketView,
    pub edit: Option<EditIdentity>,
    pub clock_origin: ClockOrigin,
    pub media_clock: MediaClock,
    pub scheduling_clock: SchedulingClock,
}

/// Whether this seekable MOV head replaces the incumbent during a
/// left-to-right catalogue scan. Incomparable and tied heads keep the incumbent.
pub fn seekable_head_precedes(contender: &PacketOccurrence, incumbent: &PacketOccurrence) -> bool {
    match (contender.scheduling_clock, incumbent.scheduling_clock) {
        (
            SchedulingClock::ExactMicroseconds(contender_clock),
            SchedulingClock::ExactMicroseconds(incumbent_clock),
        ) => {
            let distance = (i128::from(contender_clock) - i128::from(incumbent_clock)).abs();
            if distance > 1_000_000 {
                contender_clock < incumbent_clock
            } else {
                contender.demux.source_range.start < incumbent.demux.source_range.start
            }
        }
        // Undefined/demux-overflow heads retain the incumbent catalogue slot.
        _ => false,
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PreviousClock {
    pub(crate) sample: u64,
    pub(crate) dts: i128,
    pub(crate) signed_dts: i64,
    pub(crate) duration: u32,
    pub(crate) source_known: bool,
}
#[derive(Clone, Debug)]
struct Slot<'a> {
    resume_remaining: u64,
    track: DemuxTrack<'a>,
    cursor: TrackSampleCursor<'a>,
    edited: Option<EditCursor<'a>>,
    encoded_edit: Option<&'a crate::EncodedEditPlan>,
    head: Option<PacketOccurrence>,
    clock: MediaClock,
    shift: i64,
    append_run: Option<(usize, usize)>,
    discard_boundary: Option<i128>,
    fragment_cto_minimum: i64,
    previous_clock: Option<PreviousClock>,
    saw_fragment: bool,
    ended: bool,
}
impl<'a> Slot<'a> {
    fn new(track: DemuxTrack<'a>) -> Result<Self> {
        let source = SourceTrack::new(track.track_id, Some(track.table), track.fragments)?;
        Ok(Self {
            resume_remaining: 0,
            track,
            cursor: TrackSampleCursor::new(source),
            edited: None,
            encoded_edit: None,
            head: None,
            clock: MediaClock::from_source(track.media_base)?,
            shift: 0,
            append_run: None,
            discard_boundary: None,
            fragment_cto_minimum: 0,
            previous_clock: None,
            saw_fragment: false,
            ended: false,
        })
    }

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
        front_source: bool,
    ) -> Result<(i128, ClockOrigin)> {
        if sample.dts_ticks.is_some() != sample.cts_ticks.is_some() {
            return Err(Error::Invalid("inconsistent source clock knownness"));
        }
        if let Some(dts) = sample.dts_ticks {
            return Ok((dts, ClockOrigin::SourceClock));
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
                ClockOrigin::PredecessorEnd(PredecessorClock {
                    predecessor_sample: previous.sample,
                    predecessor_demux_dts_ticks: previous.dts,
                    predecessor_duration_ticks: previous.duration,
                    predecessor_source_dts_known: previous.source_known,
                    derived_dts_ticks: dts,
                }),
            ));
        }
        if complete_source && self.track.table.sample_count() == 0 && sample.ordinal == 1 {
            return Ok((0, ClockOrigin::CompleteMediaStart));
        }
        if front_source && self.track.table.sample_count() == 0 && sample.ordinal == 1 {
            return Ok((0, ClockOrigin::FrontSourceMediaStart));
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
        front_source: bool,
    ) -> Result<CursorStatus> {
        let mut occurrence = if let Some(plan) = self.encoded_edit {
            let sample = match self.cursor.poll(work)? {
                SampleStep::Yield => return Ok(CursorStatus::Yield),
                SampleStep::End => return Ok(CursorStatus::End),
                SampleStep::Sample(s) => s,
            };
            let Some(demux) = plan.project(&sample)? else {
                return Ok(CursorStatus::Yield);
            };
            let edit = EditIdentity {
                sequence: sample.ordinal,
                edit_ordinal: plan.edit_ordinal(),
                duration_basis: DurationBasis::OriginalDecodeTableOrdinal,
            };
            PacketOccurrence {
                track_id: self.track.track_id,
                source: sample,
                demux,
                edit: Some(edit),
                clock_origin: ClockOrigin::SourceClock,
                media_clock: self.clock,
                scheduling_clock: SchedulingClock::SingleTrack,
            }
        } else if let Some(cursor) = &mut self.edited {
            match cursor.poll(work)? {
                CursorStatus::Ready => {
                    let occurrence = cursor.take_occurrence()?;
                    PacketOccurrence {
                        track_id: self.track.track_id,
                        source: occurrence.source,
                        demux: occurrence.demux,
                        edit: Some(EditIdentity {
                            sequence: occurrence.sequence,
                            edit_ordinal: occurrence.edit_ordinal,
                            duration_basis: occurrence.duration_basis,
                        }),
                        clock_origin: ClockOrigin::SourceClock,
                        media_clock: self.clock,
                        scheduling_clock: SchedulingClock::SingleTrack,
                    }
                }
                other => return Ok(other),
            }
        } else {
            let sample = match self.cursor.poll(work)? {
                SampleStep::Sample(sample) => sample,
                SampleStep::Yield => return Ok(CursorStatus::Yield),
                SampleStep::End => return Ok(CursorStatus::End),
            };
            let view = PacketProjector::new(
                self.track.media_base,
                if self.saw_fragment {
                    None
                } else {
                    self.track.table.sync_entry_count()
                },
            );
            let (dts, clock_origin) = if sample.fragment_flags.is_some() {
                self.fragment_origin(&sample, complete_source, front_source)?
            } else {
                (
                    sample
                        .dts_ticks
                        .ok_or(Error::Unsupported("native source DTS unknown"))?,
                    ClockOrigin::SourceClock,
                )
            };
            let mut demux = if sample.fragment_flags.is_some() {
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
                    // Actual reference-demux partition controls distinguish trun
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
                    .is_some_and(|previous| i128::from(demux.dts_field.signed_ticks()) <= previous)
                {
                    demux.flags |= 4;
                }
            }
            if sample.fragment_flags.is_some() || front_source {
                self.previous_clock = Some(PreviousClock {
                    sample: sample.ordinal,
                    dts,
                    signed_dts: demux.dts_field.signed_ticks(),
                    duration: sample.duration_ticks,
                    source_known: sample.dts_ticks.is_some(),
                });
            }
            PacketOccurrence {
                track_id: self.track.track_id,
                source: sample,
                demux,
                edit: None,
                clock_origin,
                media_clock: self.clock,
                scheduling_clock: SchedulingClock::SingleTrack,
            }
        };
        occurrence.scheduling_clock = if front_source {
            SchedulingClock::NonseekableSourcePosition(occurrence.demux.source_range.start)
        } else if global_schedule {
            SchedulingClock::from_demux_clock(occurrence.demux.dts_field, self.clock.demux_base)?
        } else {
            SchedulingClock::SingleTrack
        };
        self.head = Some(occurrence);
        Ok(CursorStatus::Ready)
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
/// Preflight validates *all* source sample extents before demux policy refusals.
/// Backward source addresses remain legal: the consumer performs checked ranges.
#[derive(Debug)]
pub struct PacketCursor<'a> {
    slots: BudgetVec<Slot<'a>>,
    media: &'a [Range<u64>],
    limits: PacketLimits,
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
    ready: Option<PacketOccurrence>,
    failed: Option<Error>,
    complete_source: bool,
    front_source: bool,
    terminal: Option<crate::TerminalMdat>,
    forward_regular: Option<crate::ForwardRegularMdat>,
    recording_seek_checkpoints: bool,
    seek_checkpoints: Option<Arc<Checkpoints<'a>>>,
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
impl<'a> PacketCursor<'a> {
    pub fn new(
        budget: &Budget,
        tracks: &[DemuxTrack<'a>],
        media: &'a [Range<u64>],
        limits: PacketLimits,
        _profile: DemuxProfile,
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
            slots
                .try_push(Slot::new(*track)?)
                .map_err(|error| error.error)?;
        }
        Ok(Self::from_slots(slots, media, limits))
    }

    fn from_slots(
        slots: BudgetVec<Slot<'a>>,
        media: &'a [Range<u64>],
        limits: PacketLimits,
    ) -> Self {
        Self {
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
            front_source: false,
            terminal: None,
            forward_regular: None,
            recording_seek_checkpoints: false,
            seek_checkpoints: None,
        }
    }
    /// The caller establishes one complete original media source and all its
    /// preceding track samples. This enables the reference demux start-zero rule
    /// for an unknown first fragment clock; source timestamps remain unknown.
    /// Do not use for isolated/range fragments, missing prior segments or a
    /// continuation whose earlier samples are absent. `new` keeps that refusal.
    pub fn new_complete_source(
        budget: &Budget,
        tracks: &[DemuxTrack<'a>],
        media: &'a [Range<u64>],
        limits: PacketLimits,
        profile: DemuxProfile,
    ) -> Result<Self> {
        let mut cursor = Self::new(budget, tracks, media, limits, profile)?;
        cursor.complete_source = true;
        Ok(cursor)
    }
    /// Explicit front-metadata, from-start nonseekable demux operation. A
    /// terminal declaration remains open; it is never a fabricated finite range.
    /// Only the source-bound playback opener should establish this context.
    pub fn new_front_source(
        budget: &Budget,
        tracks: &[DemuxTrack<'a>],
        media: &'a [Range<u64>],
        terminal: Option<crate::TerminalMdat>,
        limits: PacketLimits,
        profile: DemuxProfile,
    ) -> Result<Self> {
        let media_count = media
            .len()
            .checked_add(usize::from(terminal.is_some()))
            .ok_or(Error::Overflow)?;
        if media_count > limits.max_media_extents {
            return Err(Error::Limit {
                requested: media_count,
                maximum: limits.max_media_extents,
            });
        }
        if let Some(tail) = terminal
            && media
                .last()
                .is_some_and(|range| range.end > tail.payload_start())
        {
            return Err(Error::Invalid("front terminal overlaps finite media"));
        }
        let mut cursor = Self::new(budget, tracks, media, limits, profile)?;
        cursor.front_source = true;
        cursor.terminal = terminal;
        Ok(cursor)
    }
    /// Project unedited regular declarations for a forward-only front movie.
    /// An occurrence outside the first extent still needs actual later media
    /// admission before it may carry a packet payload.
    pub fn new_front_regular(
        budget: &Budget,
        tracks: &[DemuxTrack<'a>],
        media: &'a [Range<u64>],
        bounds: crate::ForwardRegularMdat,
        limits: PacketLimits,
        profile: DemuxProfile,
    ) -> Result<Self> {
        bounds.require_initial_extent(media)?;
        let mut cursor = Self::new_front_source(budget, tracks, media, None, limits, profile)?;
        if tracks
            .iter()
            .any(|track| !track.fragments.is_empty() || track.edits.is_some())
        {
            return Err(Error::Unsupported(
                "forward regular media with edits or fragments",
            ));
        }
        cursor.forward_regular = Some(bounds);
        Ok(cursor)
    }
    /// Snapshot only an exhausted, checked front-source track. The private state
    /// cannot be manufactured from a caller's expected clock or source token.
    pub fn front_track(&self, at: usize) -> Result<crate::FrontTrack> {
        if !self.front_source
            || !matches!(self.phase, Phase::End)
            || self.failed.is_some()
            || self.ready.is_some()
        {
            return Err(Error::Invalid("front native index not exhausted"));
        }
        let slot = self
            .slots
            .as_slice()
            .get(at)
            .ok_or(Error::Invalid("front native track"))?;
        Ok(crate::FrontTrack::from_index(
            slot.track.track_id,
            slot.track.media_base,
            slot.clock,
            slot.previous_clock,
            slot.fragment_cto_minimum,
            slot.track.table.sample_count(),
            slot.edited.is_some(),
        ))
    }
    /// Explicit encoded edit admission; the default/demux-source constructors
    /// keep their original policy and every raw source/edit field.
    pub fn new_encoded_regular(
        budget: &Budget,
        tracks: &[DemuxTrack<'a>],
        media: &'a [Range<u64>],
        limits: PacketLimits,
        plans: &'a [Option<crate::EncodedEditPlan>],
        _profile: crate::EncodedEditProfile,
    ) -> Result<Self> {
        if plans.len() != tracks.len() {
            return Err(Error::Invalid("encoded edit track coverage"));
        }
        let mut cursor = Self::new_complete_source(
            budget,
            tracks,
            media,
            limits,
            DemuxProfile::NoParseNoFillin,
        )?;
        for (slot, plan) in cursor.slots.as_mut_slice().iter_mut().zip(plans) {
            if let Some(plan) = plan {
                if !plan.source_table().bytes().belongs_to(budget) {
                    return Err(Error::ForeignPool);
                }
                let Some((edits, movie_base)) = slot.track.edits else {
                    return Err(Error::Invalid("encoded edit plan without source edit"));
                };
                if !slot.track.fragments.is_empty()
                    || !plan.matches(slot.track.table, edits, slot.track.media_base, movie_base)
                {
                    return Err(Error::Invalid("encoded edit source association"));
                }
                slot.encoded_edit = Some(plan);
            }
        }
        Ok(cursor)
    }
    pub fn ready_encoded_skip(&self) -> Result<Option<[u8; 10]>> {
        let item = self.ready.as_ref().ok_or(Error::Invalid(
            "encoded skip requires ready source occurrence",
        ))?;
        let slot = self
            .slots
            .as_slice()
            .iter()
            .find(|s| s.track.track_id == item.track_id)
            .ok_or(Error::Invalid("encoded occurrence track"))?;
        match slot.encoded_edit {
            Some(plan) => plan.skip_samples(&item.source),
            None => Ok(None),
        }
    }
    pub fn validated_samples(&self) -> u64 {
        self.validated
    }
    /// Reconstruct each track's real demux state up to its checked occurrence
    /// position before the existing global selector begins delivery. Full source
    /// extent validation still runs first; skipped metadata reads no payload.
    pub fn new_complete_source_at(
        budget: &Budget,
        tracks: &[DemuxTrack<'a>],
        media: &'a [Range<u64>],
        limits: PacketLimits,
        profile: DemuxProfile,
        starts: &[ResumePoint],
    ) -> Result<Self> {
        Self::new_complete_source(budget, tracks, media, limits, profile)?
            .with_resume_points(starts, limits)
    }
    pub(crate) fn with_resume_points(
        mut self,
        starts: &[ResumePoint],
        limits: PacketLimits,
    ) -> Result<Self> {
        self.set_resume_points(starts, limits)?;
        Ok(self)
    }

    fn set_resume_points(&mut self, starts: &[ResumePoint], limits: PacketLimits) -> Result<u64> {
        if starts.len() != self.slots.len() {
            return Err(Error::Invalid("native resume track coverage"));
        }
        if self.recording_seek_checkpoints && starts.iter().any(|start| start.occurrence != 0) {
            self.discard_seek_checkpoints();
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
            let slot = self
                .slots
                .as_mut_slice()
                .iter_mut()
                .find(|s| s.track.track_id == start.track_id)
                .ok_or(Error::Invalid("unknown native resume track"))?;
            slot.resume_remaining = start.occurrence;
        }
        Ok(skipped)
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
    /// Inspect the current occurrence without consuming its place in the
    /// demux schedule.
    pub fn occurrence(&self) -> Option<&PacketOccurrence> {
        self.ready.as_ref()
    }

    /// Start a separate replay of these immutable complete-source tables.
    /// Reuse the verified extents and initial clock facts with the same limits.
    pub fn fork_checked_replay(&self) -> Result<Self> {
        if !self.complete_source
            || self.front_source
            || self.failed.is_some()
            || !matches!(
                self.phase,
                Phase::Initialize | Phase::Fill | Phase::Select | Phase::End
            )
        {
            return Err(Error::Invalid(
                "native replay needs complete checked tables",
            ));
        }
        let mut replay = self.fork_checked_slots(self.slots.as_slice())?;
        if !self.recording_seek_checkpoints {
            replay.seek_checkpoints.clone_from(&self.seek_checkpoints);
        }
        Ok(replay)
    }

    pub(crate) fn fork_index_track(&self, track_id: u32) -> Result<(Self, DemuxTrack<'a>)> {
        if !matches!(self.phase, Phase::End)
            || self.failed.is_some()
            || !self.complete_source
            || self.front_source
            || self.terminal.is_some()
            || self.forward_regular.is_some()
        {
            return Err(Error::Invalid(
                "index replay requires complete sample tables",
            ));
        }
        let slot = self
            .slots
            .as_slice()
            .iter()
            .find(|slot| slot.track.track_id == track_id)
            .ok_or(Error::Invalid("index replay track is absent"))?;
        if slot.encoded_edit.is_some() {
            return Err(Error::Invalid("index replay requires a source track"));
        }
        Ok((
            self.fork_checked_slots(std::slice::from_ref(slot))?,
            slot.track,
        ))
    }

    fn fork_checked_slots(&self, checked_slots: &[Slot<'a>]) -> Result<Self> {
        let mut slots = BudgetVec::new(self.slots.budget());
        slots.try_reserve_exact(checked_slots.len())?;
        for checked in checked_slots {
            let mut slot = Slot::new(checked.track)?;
            slot.encoded_edit = checked.encoded_edit;
            slot.shift = checked.shift;
            slot.fragment_cto_minimum = checked.fragment_cto_minimum;
            slot.saw_fragment = checked.saw_fragment;
            slots.try_push(slot).map_err(|error| error.error)?;
        }
        let mut replay = Self::from_slots(slots, self.media, self.limits);
        replay.complete_source = true;
        replay.phase = Phase::Initialize;
        replay.validated = self.validated;
        Ok(replay)
    }

    /// Choose initial per-track occurrence positions on a fresh checked replay.
    pub fn resume_checked_replay(mut self, starts: &[ResumePoint]) -> Result<Self> {
        if !matches!(self.phase, Phase::Initialize) || self.at != 0 {
            return Err(Error::Invalid(
                "native resume positions need a fresh replay",
            ));
        }
        self.generated = self.set_resume_points(starts, self.limits)?;
        Ok(self)
    }

    /// Replay the same checked sample tables from their beginning after End.
    /// The extent and clock preflight still applies to these borrowed tables.
    pub fn rewind_at_end(&mut self) -> Result<()> {
        if !matches!(self.phase, Phase::End) || self.failed.is_some() {
            return Err(Error::Invalid("native occurrence rewind needs a clean end"));
        }
        for slot in self.slots.as_mut_slice() {
            slot.append_run = None;
            slot.discard_boundary = None;
            slot.previous_clock = None;
            slot.ended = false;
        }
        self.recording_seek_checkpoints = false;
        self.at = 0;
        self.generated = 0;
        self.phase = Phase::Initialize;
        Ok(())
    }

    pub fn take_occurrence(&mut self) -> Result<PacketOccurrence> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        self.ready
            .take()
            .ok_or(Error::Invalid("no ready native packet"))
    }
    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<CursorStatus> {
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
    fn poll_inner(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<CursorStatus> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if self.ready.is_some() {
                return Ok(CursorStatus::Ready);
            }
            match self.phase {
                Phase::Media => {
                    if self.at == self.media.len() {
                        self.at = 0;
                        self.phase = Phase::Validate;
                        continue;
                    }
                    if !work.try_charge(1) {
                        return Ok(CursorStatus::Yield);
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
                        SampleStep::Yield => return Ok(CursorStatus::Yield),
                        SampleStep::End => {
                            if !work.try_charge(1) {
                                return Ok(CursorStatus::Yield);
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
                        return Ok(CursorStatus::Yield);
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
                        if let Some(terminal) = self
                            .terminal
                            .filter(|tail| sample.range.start >= tail.payload_start())
                        {
                            terminal.require_sample(sample)?;
                        } else if !self
                            .forward_regular
                            .is_some_and(|bounds| bounds.can_defer(sample))
                        {
                            let extent = self
                                .low
                                .checked_sub(1)
                                .and_then(|at| self.media.get(at))
                                .ok_or(Error::Invalid("native sample outside mdat"))?;
                            sample.require_within(extent.clone())?;
                        }
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
                        return Ok(CursorStatus::Yield);
                    }
                    let slot = &mut self.slots.as_mut_slice()[self.at];
                    if let Some(runs) = slot
                        .track
                        .table
                        .composition_runs()
                        .filter(|runs| self.row < runs.count.saturating_sub(2))
                    {
                        // Explicit reference demux rule, regardless of the
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
                        return Ok(CursorStatus::Yield);
                    }
                    let slot = &mut self.slots.as_mut_slice()[self.at];
                    if slot.saw_fragment && slot.track.table.sample_count() != 0 {
                        return Err(Error::Unsupported(
                            "native mixed regular/fragment clock profile",
                        ));
                    }
                    slot.edited = if slot.encoded_edit.is_some() {
                        None
                    } else if let Some((edits, movie_base)) = slot.track.edits {
                        if slot.saw_fragment {
                            return Err(Error::Unsupported(
                                "native edited fragment occurrence profile",
                            ));
                        }
                        Some(EditCursor::new(
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
                            return Ok(CursorStatus::Yield);
                        }
                        self.at += 1;
                        continue;
                    }
                    if slot.head.is_some() {
                        if !work.try_charge(1) {
                            return Ok(CursorStatus::Yield);
                        }
                        if slot.resume_remaining != 0 {
                            slot.resume_remaining -= 1;
                            slot.head = None;
                            continue;
                        }
                        self.at += 1;
                        continue;
                    }
                    match slot.fill(
                        work,
                        self.complete_source,
                        global_schedule,
                        self.front_source,
                    )? {
                        CursorStatus::Yield => return Ok(CursorStatus::Yield),
                        CursorStatus::End => {
                            if slot.resume_remaining != 0 {
                                return Err(Error::Invalid(
                                    "native resume position outside occurrences",
                                ));
                            }
                            slot.ended = true;
                        }
                        CursorStatus::Ready => {
                            continue;
                        }
                    }
                    if !work.try_charge(1) {
                        return Ok(CursorStatus::Yield);
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
                            return Ok(CursorStatus::Yield);
                        }
                        self.generated = self.generated.checked_add(1).ok_or(Error::Overflow)?;
                        count(self.generated, self.limits.max_occurrences)?;
                        if self.recording_seek_checkpoints {
                            self.record_seek_checkpoint(best);
                        }
                        self.ready = self.slots.as_mut_slice()[best].head.take();
                        self.at = 0;
                        self.phase = Phase::Fill;
                        continue;
                    }
                    if !work.try_charge(1) {
                        return Ok(CursorStatus::Yield);
                    }
                    let slots = self.slots.as_slice();
                    if let Some(contender) = slots[self.at].head.as_ref() {
                        let prefer = if let Some(best) = self.best {
                            let incumbent = slots[best]
                                .head
                                .as_ref()
                                .ok_or(Error::Invalid("native selector head missing"))?;
                            if self.front_source {
                                contender.demux.source_range.start
                                    < incumbent.demux.source_range.start
                            } else {
                                seekable_head_precedes(contender, incumbent)
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
                Phase::End => return Ok(CursorStatus::End),
            }
        }
    }
}

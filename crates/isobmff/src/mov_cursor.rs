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
    CursorStatus, DurationBasis, EditCursor, EditLimits, EditList, FragmentCursor, FragmentIndex,
    MediaClock, PacketProjector, PacketView, Sample, SampleCursor, SampleStep, SampleTable,
    SchedulingClock, SourceTrack, SpanStep, TrackSampleCursor,
    interleave::TrackSpans,
    mov_edits::{EmitPrepare, EmitStart},
    require_range_within,
};
use std::{
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use triergon_core::{Budget, BudgetVec, Error, Result, Rounding, TickScale, TimeBase, WorkBudget};

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
#[derive(Debug)]
struct Slot<'a> {
    resume_remaining: u64,
    track: DemuxTrack<'a>,
    cursor: TrackSampleCursor<'a>,
    edited: Option<BudgetVec<EditCursor<'a>>>,
    encoded_edit: Option<&'a crate::EncodedEditPlan>,
    head: Option<PacketOccurrence>,
    clock: Option<MediaClock>,
    shift: i64,
    append_run: Option<(usize, usize)>,
    discard_boundary: Option<i128>,
    fragment_cto_minimum: i64,
    fragment_edit: Option<FragmentEdit>,
    front_fragment: bool,
    previous_clock: Option<PreviousClock>,
    saw_fragment: bool,
    starts_fragment_run: bool,
    ended: bool,
}
impl<'a> Slot<'a> {
    fn new(track: DemuxTrack<'a>, project_clock: bool) -> Result<Self> {
        let source = SourceTrack::new(track.track_id, Some(track.table), track.fragments)?;
        let clock = project_clock
            .then(|| MediaClock::from_source(track.media_base))
            .transpose()?;
        Ok(Self {
            resume_remaining: 0,
            track,
            cursor: TrackSampleCursor::new(source),
            edited: None,
            encoded_edit: None,
            head: None,
            clock,
            shift: 0,
            append_run: None,
            discard_boundary: None,
            fragment_cto_minimum: 0,
            fragment_edit: None,
            front_fragment: false,
            previous_clock: None,
            saw_fragment: false,
            starts_fragment_run: false,
            ended: false,
        })
    }

    fn clock(&self) -> Result<MediaClock> {
        self.clock
            .ok_or(Error::Invalid("missing projected media clock"))
    }

    fn try_clone(&self) -> Result<Self> {
        Ok(Self {
            resume_remaining: self.resume_remaining,
            track: self.track,
            cursor: self.cursor.clone(),
            edited: self.edited.as_ref().map(BudgetVec::try_clone).transpose()?,
            encoded_edit: self.encoded_edit,
            head: self.head.clone(),
            clock: self.clock,
            shift: self.shift,
            append_run: self.append_run,
            discard_boundary: self.discard_boundary,
            fragment_cto_minimum: self.fragment_cto_minimum,
            fragment_edit: self.fragment_edit,
            front_fragment: self.front_fragment,
            previous_clock: self.previous_clock,
            saw_fragment: self.saw_fragment,
            starts_fragment_run: self.starts_fragment_run,
            ended: self.ended,
        })
    }

    /// Makes `target`, a slot cloned from the same track before, a copy of this
    /// one in place, keeping its edit cursor storage. A slot is several hundred
    /// bytes, so this spares a seek that keeps replacing its best state from
    /// building each one apart and moving it in.
    fn try_clone_into(&self, target: &mut Self) -> Result<()> {
        let Self {
            resume_remaining,
            track,
            cursor,
            edited,
            encoded_edit,
            head,
            clock,
            shift,
            append_run,
            discard_boundary,
            fragment_cto_minimum,
            fragment_edit,
            front_fragment,
            previous_clock,
            saw_fragment,
            starts_fragment_run,
            ended,
        } = self;
        match (edited, &mut target.edited) {
            (None, kept) => *kept = None,
            (Some(source), Some(kept)) => kept.as_mut_slice()[0].clone_from(&source.as_slice()[0]),
            (Some(source), kept @ None) => *kept = Some(source.try_clone()?),
        }
        target.resume_remaining = *resume_remaining;
        target.track = *track;
        target.cursor.clone_from(cursor);
        target.encoded_edit = *encoded_edit;
        target.head.clone_from(head);
        target.clock = *clock;
        target.shift = *shift;
        target.append_run = *append_run;
        target.discard_boundary = *discard_boundary;
        target.fragment_cto_minimum = *fragment_cto_minimum;
        target.fragment_edit = *fragment_edit;
        target.front_fragment = *front_fragment;
        target.previous_clock = *previous_clock;
        target.saw_fragment = *saw_fragment;
        target.starts_fragment_run = *starts_fragment_run;
        target.ended = *ended;
        Ok(())
    }

    fn fragment_edit_offset(&self) -> i64 {
        self.fragment_edit.map_or(0, |edit| edit.packet_offset)
    }

    fn seek_offset(&self) -> i64 {
        if let Some(edit) = self.encoded_edit {
            edit.seek_offset()
        } else {
            self.fragment_edit.map_or(0, |edit| edit.media_start)
        }
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
            // Keep decode accumulation in media ticks. The index checks the
            // witness against the projected DTS of the two adjacent packets.
            let edit_start = i128::from(self.fragment_edit_offset());
            let predecessor_dts = previous
                .dts
                .checked_sub(edit_start)
                .ok_or(Error::Overflow)?;
            let derived_dts = dts.checked_sub(edit_start).ok_or(Error::Overflow)?;
            return Ok((
                dts,
                ClockOrigin::PredecessorEnd(PredecessorClock {
                    predecessor_sample: previous.sample,
                    predecessor_demux_dts_ticks: predecessor_dts,
                    predecessor_duration_ticks: previous.duration,
                    predecessor_source_dts_known: previous.source_known,
                    derived_dts_ticks: derived_dts,
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
    /// Project the next occurrence into `head`. `micros`, when given, converts
    /// the demux base to the microseconds of a global schedule.
    fn fill(
        &mut self,
        work: &mut WorkBudget,
        complete_source: bool,
        global_schedule: bool,
        front_source: bool,
        micros: Option<&TickScale>,
    ) -> Result<CursorStatus> {
        let clock = self.clock()?;
        let (source, demux, edit, clock_origin) = if let Some(plan) = self.encoded_edit {
            let sample = match self.cursor.poll(work)? {
                SampleStep::Yield => return Ok(CursorStatus::Yield),
                SampleStep::End => return Ok(CursorStatus::End),
                SampleStep::Sample(s) => s,
            };
            let Some((demux, sequence)) = plan.project(&sample)? else {
                return Ok(CursorStatus::Yield);
            };
            let edit = EditIdentity {
                sequence,
                edit_ordinal: plan.edit_ordinal(),
                duration_basis: DurationBasis::OriginalDecodeTableOrdinal,
            };
            (sample, demux, Some(edit), ClockOrigin::SourceClock)
        } else if let Some(edited) = &mut self.edited {
            let cursor = &mut edited.as_mut_slice()[0];
            match cursor.poll(work)? {
                CursorStatus::Ready => {
                    let occurrence = cursor.take_occurrence()?;
                    (
                        occurrence.source,
                        occurrence.demux,
                        Some(EditIdentity {
                            sequence: occurrence.sequence,
                            edit_ordinal: occurrence.edit_ordinal,
                            duration_basis: occurrence.duration_basis,
                        }),
                        ClockOrigin::SourceClock,
                    )
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
                let offset = i128::from(self.fragment_edit_offset());
                let presentation_dts = dts.checked_sub(offset).ok_or(Error::Overflow)?;
                view.project_fragment(&sample, presentation_dts, self.fragment_cto_minimum)?
            } else {
                view.project_regular(&sample, self.shift)?
            };
            if sample.fragment_flags.is_some() {
                let run = self
                    .cursor
                    .active_fragment_run()
                    .ok_or(Error::Invalid("native fragment sample has no source run"))?;
                self.starts_fragment_run = self.append_run != Some(run);
                if self.starts_fragment_run {
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
            let edit = self.fragment_edit.map(|edit| edit.identity(sample.ordinal));
            (sample, demux, edit, clock_origin)
        };
        let scheduling_clock = if front_source {
            SchedulingClock::NonseekableSourcePosition(demux.source_range.start)
        } else if global_schedule {
            match micros {
                Some(micros) => SchedulingClock::from_scaled_clock(demux.dts_field, micros)?,
                None => SchedulingClock::from_demux_clock(demux.dts_field, clock.demux_base)?,
            }
        } else {
            SchedulingClock::SingleTrack
        };
        self.head = Some(PacketOccurrence {
            track_id: self.track.track_id,
            source,
            demux,
            edit,
            clock_origin,
            media_clock: clock,
            scheduling_clock,
        });
        Ok(CursorStatus::Ready)
    }
}

/// A track whose occurrences are its samples in order, each once; see
/// [`PacketCursor::prepare_plain`].
#[derive(Clone, Copy, Debug)]
pub(crate) enum PlainSource<'a> {
    /// A regular table, projected with its composition shift.
    Regular { table: &'a SampleTable, shift: i64 },
    /// Fragments alone, whose decode times the fragment edit offsets.
    Fragments {
        track_id: u32,
        fragments: &'a [FragmentIndex],
        edit_offset: i64,
    },
    /// A regular table under one media edit, after at most an empty one.
    Edited(EmitStart<'a>),
}

/// One step of [`PacketCursor::prepare_plain`].
#[derive(Debug)]
pub(crate) enum PlainPrepare<'a> {
    Ready(Option<PlainSource<'a>>),
    Yield,
}

/// Where a [`PlainSource`] track continues from.
#[derive(Clone, Copy, Debug)]
pub(crate) enum PlainStart<'a> {
    Regular(SampleCursor<'a>),
    /// The sample with this ordinal, under the track's edit.
    Edited(u64),
    /// The cursor over fragment `next_fragment - 1` that produces the sample,
    /// the sample before it, if any, and the discard boundary of its run.
    Fragment {
        next_fragment: usize,
        cursor: FragmentCursor<'a>,
        previous: Option<PreviousSample>,
        discard_boundary: Option<i128>,
    },
}

/// A fragment sample's clock and the run that holds it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PreviousSample {
    pub(crate) clock: PreviousClock,
    pub(crate) run: (usize, usize),
}

/// One track's packet state at an occurrence its index produced: what a
/// complete-source cursor over the same tracks holds for this track once a
/// resume point has replayed it to that occurrence.
#[derive(Debug)]
pub(crate) struct TrackResume<'a> {
    /// The occurrence's position in the track, from zero.
    occurrence: u64,
    /// The samples whose extents the index checked.
    validated: u64,
    /// The track's slot, with the occurrence as its head.
    slot: Slot<'a>,
}

#[derive(Clone, Copy, Debug)]
enum Phase {
    Media,
    Validate,
    Bounds,
    Clock,
    Initialize,
    CheckProjections,
    Fill,
    Select,
    End,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct TrackProjection(u64);

impl TrackProjection {
    pub(crate) fn new(count: usize, mut include: impl FnMut(usize) -> bool) -> Result<Self> {
        if count > 64 {
            return Err(Error::Limit {
                requested: count,
                maximum: 64,
            });
        }
        let mut excluded = 0;
        for index in 0..count {
            if !include(index) {
                excluded |= 1 << index;
            }
        }
        Ok(Self(excluded))
    }

    pub(crate) fn is_all(self) -> bool {
        self.0 == 0
    }

    pub(crate) fn includes(self, index: usize) -> bool {
        self.0 & (1 << index) == 0
    }

    pub(crate) fn len(self, count: usize) -> usize {
        count - self.0.count_ones() as usize
    }

    pub(crate) fn retain<T>(self, entries: &mut BudgetVec<T>) {
        if self.is_all() {
            return;
        }
        let mut kept = 0;
        for index in 0..entries.len() {
            if self.includes(index) {
                entries.as_mut_slice().swap(kept, index);
                kept += 1;
            }
        }
        entries.truncate(kept);
    }
}

/// One charged slot/head per track (including the existing edit cursor's bounded
/// lookahead), plus one pending validation sample. The ready occurrence stays in
/// its track's slot until it is taken.
/// Polling is cancellable/work-bounded, with sticky failures.
/// Preflight validates *all* source sample extents before demux policy refusals.
/// Backward source addresses remain legal: the consumer performs checked ranges.
#[derive(Debug)]
pub struct PacketCursor<'a> {
    slots: BudgetVec<Slot<'a>>,
    source_tracks: usize,
    media: &'a [Range<u64>],
    limits: PacketLimits,
    phase: Phase,
    at: usize,
    row: u32,
    previous_ordinal: u64,
    validated: u64,
    generated: u64,
    checking: Option<Range<u64>>,
    /// The track whose sample extents are being checked.
    spans: Option<TrackSpans<'a>>,
    /// Samples of a span outside one extent left to check one at a time.
    stepping: u64,
    low: usize,
    high: usize,
    best: Option<usize>,
    /// The slot whose head is the ready occurrence.
    ready: Option<usize>,
    failed: Option<Error>,
    complete_source: bool,
    front_source: bool,
    forward_encoded_end: Option<u64>,
    terminal: Option<crate::TerminalMdat>,
    forward_regular: Option<crate::ForwardRegularMdat>,
    check_projections: bool,
    checked_duration_us: Option<i64>,
    duration_checkpoints: usize,
    projection: TrackProjection,
    recording_seek_checkpoints: bool,
    seek_checkpoints: Option<Arc<Checkpoints<'a>>>,
    /// The demux base of each of the first tracks in microseconds, for the
    /// scheduling clock and the checked duration of every packet; prepared on
    /// first use. Slots and their checkpoint copies stay as they are.
    micros: [Option<TickScale>; SCALED_TRACKS],
}

/// How many tracks keep a prepared microsecond scale; later tracks convert
/// through [`TimeBase::rescale`] per packet.
const SCALED_TRACKS: usize = 4;

impl PacketCursor<'_> {
    /// Prepare track `at`'s demux base in microseconds, for the first
    /// [`SCALED_TRACKS`] tracks once they have a projected clock; read it from
    /// `micros`.
    fn prepare_micros(&mut self, at: usize) {
        if let Some(cached @ None) = self.micros.get_mut(at)
            && let Some(clock) = &self.slots.as_slice()[at].clock
        {
            *cached = Some(TickScale::new(clock.demux_base, TimeBase::MICROSECONDS));
        }
    }
}

pub(crate) fn fragment_media_start(edits: &EditList) -> Result<i64> {
    if edits.len() != 1 {
        return Err(UNSUPPORTED_FRAGMENT_EDIT);
    }
    fragment_media_start_at(edits, 0)
}

const UNSUPPORTED_FRAGMENT_EDIT: Error =
    Error::Unsupported("fragment edit needs open-ended rate-one media");

fn fragment_media_start_at(edits: &EditList, at: u32) -> Result<i64> {
    let edit = edits.entry(at)?;
    // An initialization edit with zero duration spans every later fragment.
    if edit.segment_duration != 0
        || edit.media_time < 0
        || edit.rate_integer != 1
        || edit.rate_fraction != 0
    {
        return Err(UNSUPPORTED_FRAGMENT_EDIT);
    }
    Ok(edit.media_time)
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct FragmentEdit {
    pub(crate) packet_offset: i64,
    media_start: i64,
    leading_empty: bool,
}

impl FragmentEdit {
    fn new(edits: &EditList, movie_base: TimeBase, media_base: TimeBase) -> Result<Self> {
        let (at, delay) = match edits.len() {
            1 => (0, 0),
            2 => {
                let empty = edits.entry(0)?;
                if !empty.is_empty() || empty.rate_integer != 1 || empty.rate_fraction != 0 {
                    return Err(UNSUPPORTED_FRAGMENT_EDIT);
                }
                // The empty interval is in movie ticks. Project its duration
                // once onto the track clock before applying the media start.
                let delay = empty
                    .duration(movie_base)?
                    .to_i64_ticks(media_base, Rounding::NearestAway)?;
                (1, delay)
            }
            _ => return Err(UNSUPPORTED_FRAGMENT_EDIT),
        };
        let media_start = fragment_media_start_at(edits, at)?;
        Ok(Self {
            packet_offset: media_start.checked_sub(delay).ok_or(Error::Overflow)?,
            media_start,
            leading_empty: at != 0,
        })
    }

    pub(crate) fn identity(self, sequence: u64) -> EditIdentity {
        EditIdentity {
            sequence,
            edit_ordinal: 1 + u32::from(self.leading_empty),
            duration_basis: DurationBasis::FragmentDecodeDelta,
        }
    }
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
    pub(crate) fn index_edit_offset(&self) -> i64 {
        self.slots.as_slice().first().map_or(0, Slot::seek_offset)
    }

    pub fn new(
        budget: &Budget,
        tracks: &[DemuxTrack<'a>],
        media: &'a [Range<u64>],
        limits: PacketLimits,
        _profile: DemuxProfile,
    ) -> Result<Self> {
        Self::build(budget, tracks, media, limits, TrackProjection::default())
    }

    fn build(
        budget: &Budget,
        tracks: &[DemuxTrack<'a>],
        media: &'a [Range<u64>],
        limits: PacketLimits,
        projection: TrackProjection,
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
        for (index, track) in tracks.iter().enumerate() {
            slots
                .try_push(Slot::new(*track, projection.includes(index))?)
                .map_err(|error| error.error)?;
        }
        let mut cursor = Self::from_slots(slots, media, limits);
        cursor.projection = projection;
        Ok(cursor)
    }

    /// Project initialization edits for tracks declared by the front movie's
    /// fragment defaults, including tracks whose first fragment arrives later.
    pub fn new_front_fragmented(
        budget: &Budget,
        tracks: &[DemuxTrack<'a>],
        media: &'a [Range<u64>],
        terminal: Option<crate::TerminalMdat>,
        defaults: &[crate::Trex],
        limits: PacketLimits,
    ) -> Result<Self> {
        let mut cursor = Self::new_front_source(
            budget,
            tracks,
            media,
            terminal,
            limits,
            DemuxProfile::NoParseNoFillin,
        )?;
        for slot in cursor.slots.as_mut_slice() {
            slot.front_fragment = defaults
                .iter()
                .any(|entry| entry.track_id == slot.track.track_id);
        }
        Ok(cursor)
    }

    fn from_slots(
        slots: BudgetVec<Slot<'a>>,
        media: &'a [Range<u64>],
        limits: PacketLimits,
    ) -> Self {
        Self {
            source_tracks: slots.len(),
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
            spans: None,
            stepping: 0,
            low: 0,
            high: 0,
            best: None,
            ready: None,
            failed: None,
            complete_source: false,
            front_source: false,
            forward_encoded_end: None,
            terminal: None,
            forward_regular: None,
            check_projections: false,
            checked_duration_us: None,
            duration_checkpoints: 0,
            projection: TrackProjection::default(),
            recording_seek_checkpoints: false,
            seek_checkpoints: None,
            micros: [None; SCALED_TRACKS],
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

    /// Project validated regular edits in ascending source order. All sample
    /// declarations must belong to the media extent already found by the opener.
    pub fn new_front_with_encoded_edits(
        budget: &Budget,
        tracks: &[DemuxTrack<'a>],
        media: &'a [Range<u64>],
        limits: PacketLimits,
        plans: &'a [Option<crate::EncodedEditPlan>],
    ) -> Result<Self> {
        if plans.len() != tracks.len() {
            return Err(Error::Invalid("encoded edit track coverage"));
        }
        let [extent] = media else {
            return Err(Error::Unsupported(
                "front encoded edits require regular tracks and one media extent",
            ));
        };
        let mut cursor = Self::new_front_source(
            budget,
            tracks,
            media,
            None,
            limits,
            DemuxProfile::NoParseNoFillin,
        )?;
        if tracks.iter().any(|track| !track.fragments.is_empty()) {
            return Err(Error::Unsupported(
                "front encoded edits require regular tracks and one media extent",
            ));
        }
        cursor.bind_encoded_edits(budget, plans)?;
        cursor.forward_encoded_end = Some(extent.start);
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
            slot.clock()?,
            slot.previous_clock,
            slot.fragment_cto_minimum,
            slot.track.table.sample_count(),
            slot.fragment_edit,
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
        cursor.bind_encoded_edits(budget, plans)?;
        Ok(cursor)
    }

    fn bind_encoded_edits(
        &mut self,
        budget: &Budget,
        plans: &'a [Option<crate::EncodedEditPlan>],
    ) -> Result<()> {
        for (slot, plan) in self.slots.as_mut_slice().iter_mut().zip(plans) {
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
        Ok(())
    }
    pub fn ready_encoded_skip(&self) -> Result<Option<[u8; 10]>> {
        let (slot, item) = self.ready_slot().ok_or(Error::Invalid(
            "encoded skip requires ready source occurrence",
        ))?;
        match slot.encoded_edit {
            Some(plan) => plan.skip_samples(&item.source),
            None => Ok(None),
        }
    }
    pub fn validated_samples(&self) -> u64 {
        self.validated
    }

    /// Validate every source extent and project only selected track ordinals.
    /// Demux clocks and edits are required only for the selected tracks.
    /// Supply any edit plans in source order.
    pub fn new_projected(
        budget: &Budget,
        tracks: &[DemuxTrack<'a>],
        media: &'a [Range<u64>],
        limits: PacketLimits,
        plans: Option<&'a [Option<crate::EncodedEditPlan>]>,
        include: impl FnMut(usize) -> bool,
    ) -> Result<Self> {
        if plans.is_some_and(|plans| plans.len() != tracks.len()) {
            return Err(Error::Invalid("encoded edit track coverage"));
        }
        let projection = TrackProjection::new(tracks.len(), include)?;
        let mut cursor = Self::build(budget, tracks, media, limits, projection)?;
        cursor.complete_source = true;
        if let Some(plans) = plans {
            cursor.bind_encoded_edits(budget, plans)?;
        }
        Ok(cursor)
    }

    /// Check source ranges and packet projections before playback without
    /// merging the track heads. Poll to End, then rewind for packet delivery.
    /// Use a complete indexed source from its beginning.
    pub fn check_projections_before_replay(&mut self) -> Result<()> {
        if !self.complete_source
            || self.front_source
            || !matches!(self.phase, Phase::Media)
            || self.at != 0
            || self
                .slots
                .as_slice()
                .iter()
                .any(|slot| slot.resume_remaining != 0)
        {
            return Err(Error::Invalid(
                "range check requires a fresh complete cursor",
            ));
        }
        self.check_projections = true;
        Ok(())
    }

    /// During the complete projection check, collect the latest packet
    /// presentation end. The maximum is independent of inter-track order.
    /// Preserve merged order if seek restarts can reach a count or storage limit.
    pub fn check_duration_before_replay(&mut self) -> Result<()> {
        self.check_projections_before_replay()?;
        self.checked_duration_us = Some(0);
        Ok(())
    }

    /// Return the checked presentation end in microseconds after the scan.
    pub fn checked_duration_us(&self) -> Option<i64> {
        if matches!(self.phase, Phase::End) && self.failed.is_none() {
            self.checked_duration_us
        } else {
            None
        }
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
        let projection = self.projection;
        if starts.len() != projection.len(self.slots.len()) {
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
                .enumerate()
                .find_map(|(index, slot)| {
                    (projection.includes(index) && slot.track.track_id == start.track_id)
                        .then_some(slot)
                })
                .ok_or(Error::Invalid("unknown native resume track"))?;
            slot.resume_remaining = start.occurrence;
        }
        Ok(skipped)
    }
    /// Provenance of the currently held source run, accessed before take. O(1)
    /// in the selected single-track index cursor; no run/sample array expansion.
    pub(crate) fn ready_source_run(&self) -> Result<Option<(usize, usize, u32)>> {
        let (slot, ready) = self
            .ready_slot()
            .ok_or(Error::Invalid("no ready native occurrence"))?;
        if ready.source.fragment_flags.is_none() {
            return Ok(None);
        }
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
        self.ready_slot().map(|(_, occurrence)| occurrence)
    }

    fn ready_slot(&self) -> Option<(&Slot<'a>, &PacketOccurrence)> {
        let slot = self.slots.as_slice().get(self.ready?)?;
        Some((slot, slot.head.as_ref()?))
    }

    /// Drop the ready occurrence without moving it out of its slot.
    pub(crate) fn discard_occurrence(&mut self) {
        if let Some(at) = self.ready.take() {
            self.slots.as_mut_slice()[at].head = None;
        }
    }

    /// Stores this one-track complete-source cursor's state at its ready
    /// occurrence, the `occurrence`-th from zero, in `resume`, in place of and
    /// in the storage of a state this method stored there before.
    pub(crate) fn store_track_resume(
        &self,
        occurrence: u64,
        resume: &mut Option<TrackResume<'a>>,
    ) -> Result<()> {
        let ([slot], Some(0), true) = (self.slots.as_slice(), self.ready, self.complete_source)
        else {
            return Err(Error::Invalid(
                "track resume needs a ready one-track complete cursor",
            ));
        };
        if let Some(kept) = resume {
            kept.occurrence = occurrence;
            kept.validated = self.validated;
            return slot.try_clone_into(&mut kept.slot);
        }
        *resume = Some(TrackResume {
            occurrence,
            validated: self.validated,
            slot: slot.try_clone()?,
        });
        Ok(())
    }

    /// The slot of a prepared one-track complete-source cursor that has produced
    /// nothing yet.
    fn prepared_slot(&self) -> Option<&Slot<'a>> {
        let [slot] = self.slots.as_slice() else {
            return None;
        };
        let prepared = matches!(self.phase, Phase::Fill)
            && self.at == 0
            && self.generated == 0
            && self.ready.is_none()
            && self.complete_source
            && !self.front_source
            && self.terminal.is_none()
            && self.forward_regular.is_none()
            && self.forward_encoded_end.is_none()
            && !self.check_projections
            && !self.recording_seek_checkpoints
            && self.checked_duration_us.is_none()
            && self.projection.is_all()
            && slot.head.is_none()
            && slot.resume_remaining == 0
            && slot.clock.is_some()
            && slot.encoded_edit.is_none()
            && !slot.front_fragment
            && slot.previous_clock.is_none()
            && slot.append_run.is_none()
            && self.validated != 0
            && self.validated <= self.limits.max_occurrences;
        prepared.then_some(slot)
    }

    /// A prepared one-track cursor's track when its occurrences are its samples
    /// in order with unedited clocks: a regular table with no edit list, partial
    /// sync or dependency samples, or fragments alone.
    fn plain_track(&self) -> Option<PlainSource<'a>> {
        let slot = self.prepared_slot().filter(|slot| slot.edited.is_none())?;
        let table = slot.track.table;
        if !slot.saw_fragment
            && slot.track.edits.is_none()
            && slot.fragment_edit.is_none()
            && !table.has_partial_sync_samples()
            && !table.has_dependency_table()
        {
            return Some(PlainSource::Regular {
                table,
                shift: slot.shift,
            });
        }
        (slot.saw_fragment && table.sample_count() == 0).then_some(PlainSource::Fragments {
            track_id: slot.track.track_id,
            fragments: slot.track.fragments,
            edit_offset: slot.fragment_edit_offset(),
        })
    }

    /// Prepares a one-track complete-source cursor and finds whether its
    /// occurrences can be read from its tables: those of a [`PlainSource`].
    pub(crate) fn prepare_plain(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<PlainPrepare<'a>> {
        match self.prepare(work, cancel)? {
            CursorStatus::Yield => return Ok(PlainPrepare::Yield),
            CursorStatus::End => return Ok(PlainPrepare::Ready(None)),
            CursorStatus::Ready => {}
        }
        if let Some(source) = self.plain_track() {
            return Ok(PlainPrepare::Ready(Some(source)));
        }
        // A regular track under a media edit emits each sample once.
        if self.prepared_slot().is_none_or(|slot| slot.saw_fragment) {
            return Ok(PlainPrepare::Ready(None));
        }
        let Some(edited) = self.slots.as_mut_slice()[0].edited.as_mut() else {
            return Ok(PlainPrepare::Ready(None));
        };
        Ok(match edited.as_mut_slice()[0].prepare_emit(work)? {
            EmitPrepare::Ready(start) => PlainPrepare::Ready(Some(PlainSource::Edited(start))),
            EmitPrepare::Yield => PlainPrepare::Yield,
            EmitPrepare::Unsuited => PlainPrepare::Ready(None),
        })
    }

    /// Continue a cursor that [`prepare_plain`](Self::prepare_plain) found plain
    /// from `start`, as though the occurrences before it had been produced.
    /// `false` when the work is spent.
    pub(crate) fn start_plain_at(
        &mut self,
        start: PlainStart<'a>,
        work: &mut WorkBudget,
    ) -> Result<bool> {
        if let PlainStart::Edited(ordinal) = start {
            if self.prepared_slot().is_none() {
                return Err(Error::Invalid("plain start needs a prepared edited track"));
            }
            let edited = self.slots.as_mut_slice()[0]
                .edited
                .as_mut()
                .ok_or(Error::Invalid("plain start needs a prepared edited track"))?;
            return edited.as_mut_slice()[0].skip_to(ordinal, work);
        }
        let source = self
            .plain_track()
            .ok_or(Error::Invalid("plain start needs a prepared plain track"))?;
        let slot = &mut self.slots.as_mut_slice()[0];
        match (source, start) {
            (PlainSource::Regular { table, .. }, PlainStart::Regular(cursor))
                if std::ptr::eq(cursor.table(), table) =>
            {
                slot.cursor = TrackSampleCursor::from_regular(slot.source()?, cursor);
            }
            (
                PlainSource::Fragments { fragments, .. },
                PlainStart::Fragment {
                    next_fragment,
                    cursor,
                    previous,
                    discard_boundary,
                },
            ) if next_fragment > 0 && next_fragment <= fragments.len() => {
                slot.cursor =
                    TrackSampleCursor::from_fragment(slot.source()?, next_fragment, cursor);
                slot.previous_clock = previous.map(|previous| previous.clock);
                slot.append_run = previous.map(|previous| previous.run);
                slot.discard_boundary = discard_boundary;
            }
            _ => return Err(Error::Invalid("plain start from another track")),
        }
        Ok(true)
    }

    /// The complete-source cursor over these tracks, in catalogue order, that
    /// [`with_resume_points`](Self::with_resume_points) reaches by replaying each
    /// track to its resumed occurrence. Every track's extents were checked when
    /// its state was kept.
    pub(crate) fn from_track_resumes(
        budget: &Budget,
        resumes: &[TrackResume<'a>],
        media: &'a [Range<u64>],
        limits: PacketLimits,
    ) -> Result<Self> {
        let max_tracks = limits.max_tracks.min(64);
        if resumes.len() > max_tracks {
            return Err(Error::Limit {
                requested: resumes.len(),
                maximum: max_tracks,
            });
        }
        if media.len() > limits.max_media_extents {
            return Err(Error::Limit {
                requested: media.len(),
                maximum: limits.max_media_extents,
            });
        }
        let total = |value: fn(&TrackResume<'_>) -> u64| {
            resumes.iter().try_fold(0u64, |sum, resume| {
                sum.checked_add(value(resume)).ok_or(Error::Overflow)
            })
        };
        count(
            total(|resume| resume.slot.track.table.sample_count())?,
            limits.max_source_samples,
        )?;
        count(total(|resume| resume.occurrence)?, limits.max_occurrences)?;
        for resume in resumes {
            count(resume.occurrence, limits.max_occurrences)?;
        }
        // A replay checks the extents again, failing at the first sample past the limit.
        let validated = total(|resume| resume.validated)?;
        count(
            validated.min(limits.max_source_samples.saturating_add(1)),
            limits.max_source_samples,
        )?;
        let mut slots = BudgetVec::new(budget);
        slots.try_reserve_exact(resumes.len())?;
        for resume in resumes {
            let mut slot = resume.slot.try_clone()?;
            // The index scheduled its one track alone.
            if resumes.len() > 1 {
                let base = slot.clock()?.demux_base;
                if let Some(head) = &mut slot.head {
                    head.scheduling_clock =
                        SchedulingClock::from_demux_clock(head.demux.dts_field, base)?;
                }
            }
            slots.try_push(slot).map_err(|error| error.error)?;
        }
        let mut cursor = Self::from_slots(slots, media, limits);
        cursor.complete_source = true;
        cursor.validated = validated;
        cursor.phase = Phase::Fill;
        Ok(cursor)
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
            let mut slot = Slot::new(checked.track, true)?;
            slot.encoded_edit = checked.encoded_edit;
            slot.shift = checked.shift;
            slot.fragment_cto_minimum = checked.fragment_cto_minimum;
            slot.saw_fragment = checked.saw_fragment;
            slots.try_push(slot).map_err(|error| error.error)?;
        }
        let mut replay = Self::from_slots(slots, self.media, self.limits);
        replay.source_tracks = self.source_tracks;
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
        self.check_projections = false;
        self.checked_duration_us = None;
        if let (Some(end), [extent]) = (&mut self.forward_encoded_end, self.media) {
            *end = extent.start;
        }
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
            .and_then(|at| self.slots.as_mut_slice()[at].head.take())
            .ok_or(Error::Invalid("no ready native packet"))
    }
    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<CursorStatus> {
        self.poll_until::<false>(work, cancel)
    }
    /// Runs the checks that precede the first occurrence: the sample extents,
    /// the clocks and the edit setup. `Ready` once the cursor would fill.
    pub(crate) fn prepare(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<CursorStatus> {
        self.poll_until::<true>(work, cancel)
    }
    /// Polls; with `PREPARE`, only up to the first fill.
    fn poll_until<const PREPARE: bool>(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<CursorStatus> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = self.poll_inner::<PREPARE>(work, cancel);
        if let Err(error) = result {
            self.failed = Some(error);
            self.ready = None;
        }
        result
    }
    fn poll_inner<const PREPARE: bool>(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<CursorStatus> {
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
                    let spans = match &mut self.spans {
                        Some(spans) => spans,
                        None => self.spans.insert(TrackSpans::new(slot.source()?)),
                    };
                    let before = *spans;
                    // A terminal mdat or a forward regular source checks every
                    // sample on its own, so its spans are one sample long.
                    let whole = self.stepping == 0
                        && self.terminal.is_none()
                        && self.forward_regular.is_none();
                    let maximum = if whole { u64::MAX } else { 1 };
                    let span = match spans.poll(maximum, work)? {
                        SpanStep::Yield => return Ok(CursorStatus::Yield),
                        SpanStep::End => {
                            if !work.try_charge(1) {
                                return Ok(CursorStatus::Yield);
                            }
                            if self.checked_duration_us.is_some() {
                                let points =
                                    range_seek::track_checkpoint_bound(slot, self.previous_ordinal);
                                self.duration_checkpoints =
                                    self.duration_checkpoints.saturating_add(points);
                            }
                            self.at += 1;
                            self.previous_ordinal = 0;
                            self.spans = None;
                            self.stepping = 0;
                            continue;
                        }
                        SpanStep::Span(span) => span,
                    };
                    if whole {
                        // A span that lies in one extent passes every sample's
                        // check at once. Any other is checked a sample at a time,
                        // which reports the first failure.
                        let fits = span.first > self.previous_ordinal
                            && self
                                .validated
                                .checked_add(span.count)
                                .is_some_and(|total| total <= self.limits.max_source_samples)
                            && work.try_charge(u64::from(
                                self.media.len().checked_ilog2().unwrap_or(0),
                            ))
                            && within_one_extent(self.media, &span.range);
                        if !fits {
                            *spans = before;
                            self.stepping = span.count;
                            continue;
                        }
                    }
                    if span.fragment {
                        slot.saw_fragment = true;
                        if let Some(minimum) = span.offset_minimum {
                            slot.fragment_cto_minimum =
                                slot.fragment_cto_minimum.min(i64::from(minimum));
                        }
                    }
                    if whole {
                        self.validated += span.count;
                        self.previous_ordinal = span.first + (span.count - 1);
                        continue;
                    }
                    self.stepping = self.stepping.saturating_sub(1);
                    let (ordinal, range) = (span.first, span.range);
                    self.validated = self.validated.checked_add(1).ok_or(Error::Overflow)?;
                    count(self.validated, self.limits.max_source_samples)?;
                    if ordinal <= self.previous_ordinal {
                        return Err(Error::Invalid("native source ordinal order"));
                    }
                    self.previous_ordinal = ordinal;
                    if let [extent] = self.media
                        && self.terminal.is_none()
                        && self.forward_regular.is_none()
                        && work.try_charge(2)
                    {
                        // A single extent needs no search state. Charge the same
                        // lookup and bounds check as the resumable path.
                        if range.start < extent.start {
                            return Err(Error::Invalid("native sample outside mdat"));
                        }
                        require_range_within(&range, extent.clone())?;
                        continue;
                    }
                    if self.media.len() > 1
                        && self.terminal.is_none()
                        && self.forward_regular.is_none()
                        && work.try_charge(u64::from(self.media.len().ilog2()) + 3)
                    {
                        // Reserve the full partition search and bounds check.
                        // Small grants use the resumable search below.
                        let end = self
                            .media
                            .partition_point(|extent| extent.start <= range.start);
                        let extent = end
                            .checked_sub(1)
                            .and_then(|at| self.media.get(at))
                            .ok_or(Error::Invalid("native sample outside mdat"))?;
                        require_range_within(&range, extent.clone())?;
                        continue;
                    }
                    self.checking = Some(range);
                    self.low = 0;
                    self.high = self.media.len();
                    self.phase = Phase::Bounds;
                }
                Phase::Bounds => {
                    if !work.try_charge(1) {
                        return Ok(CursorStatus::Yield);
                    }
                    let range = self
                        .checking
                        .as_ref()
                        .ok_or(Error::Invalid("native pending validation"))?;
                    if self.low < self.high {
                        let mid = self.low + (self.high - self.low) / 2;
                        if self.media[mid].start <= range.start {
                            self.low = mid + 1;
                        } else {
                            self.high = mid;
                        }
                    } else {
                        if let Some(terminal) = self
                            .terminal
                            .filter(|tail| range.start >= tail.payload_start())
                        {
                            terminal.require_range(range)?;
                        } else if !self
                            .forward_regular
                            .is_some_and(|bounds| bounds.can_defer(range))
                        {
                            let extent = self
                                .low
                                .checked_sub(1)
                                .and_then(|at| self.media.get(at))
                                .ok_or(Error::Invalid("native sample outside mdat"))?;
                            require_range_within(range, extent.clone())?;
                        }
                        self.checking = None;
                        self.phase = Phase::Validate;
                    }
                }
                Phase::Clock => {
                    if self.at == self.slots.len() {
                        let projection = std::mem::take(&mut self.projection);
                        if !projection.is_all() {
                            // The clock pass charged one unit for visiting each slot.
                            projection.retain(&mut self.slots);
                        }
                        self.at = 0;
                        self.phase = Phase::Initialize;
                        continue;
                    }
                    if !work.try_charge(1) {
                        return Ok(CursorStatus::Yield);
                    }
                    let slot = &mut self.slots.as_mut_slice()[self.at];
                    if self.projection.includes(self.at)
                        && let Some(runs) = slot
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
                        if self.recording_seek_checkpoints {
                            // Reserve optional restarts after initializing the required cursors.
                            self.initialize_range_seek_checkpoints();
                            if self.recording_seek_checkpoints
                                && self.checked_duration_us.is_some()
                                && !range_seek::can_record_by_track(
                                    self.duration_checkpoints,
                                    self.slots.budget(),
                                )
                            {
                                self.check_projections = false;
                            }
                        }
                        self.at = 0;
                        self.phase = if self.check_projections {
                            Phase::CheckProjections
                        } else {
                            Phase::Fill
                        };
                        continue;
                    }
                    if !work.try_charge(1) {
                        return Ok(CursorStatus::Yield);
                    }
                    let budget = self.slots.budget().clone();
                    let slot = &mut self.slots.as_mut_slice()[self.at];
                    if slot.saw_fragment && slot.track.table.sample_count() != 0 {
                        return Err(Error::Unsupported(
                            "native mixed regular/fragment clock profile",
                        ));
                    }
                    slot.edited = if slot.encoded_edit.is_some() {
                        None
                    } else if let Some((edits, movie_base)) = slot.track.edits {
                        if (slot.saw_fragment && self.complete_source)
                            || (slot.front_fragment && slot.track.table.sample_count() == 0)
                        {
                            slot.fragment_edit =
                                Some(FragmentEdit::new(edits, movie_base, slot.track.media_base)?);
                            None
                        } else if slot.saw_fragment {
                            return Err(Error::Unsupported(
                                "fragment edit needs a complete source",
                            ));
                        } else {
                            let cursor = EditCursor::new(
                                slot.track.table,
                                edits,
                                slot.track.media_base,
                                movie_base,
                                self.limits.edit,
                            )?;
                            // The checked replay keeps its edit cursor allocation.
                            if let Some(mut storage) = slot.edited.take() {
                                storage.as_mut_slice()[0] = cursor;
                                Some(storage)
                            } else {
                                Some(BudgetVec::try_filled(&budget, cursor, 1)?)
                            }
                        }
                    } else {
                        None
                    };
                    slot.cursor = TrackSampleCursor::new(slot.source()?);
                    self.at += 1;
                }
                Phase::CheckProjections => {
                    if self.at == self.slots.len() {
                        self.phase = Phase::End;
                        continue;
                    }
                    let global_schedule = self.slots.len() > 1;
                    self.prepare_micros(self.at);
                    let micros = self.micros.get(self.at).and_then(Option::as_ref);
                    let slot = &mut self.slots.as_mut_slice()[self.at];
                    if slot.head.is_none() {
                        match slot.fill(work, true, global_schedule, false, micros)? {
                            CursorStatus::Yield => return Ok(CursorStatus::Yield),
                            CursorStatus::Ready => {}
                            CursorStatus::End => {
                                if !work.try_charge(1) {
                                    return Ok(CursorStatus::Yield);
                                }
                                self.at += 1;
                                continue;
                            }
                        }
                    }
                    if !work.try_charge(1) {
                        return Ok(CursorStatus::Yield);
                    }
                    self.generated = self.generated.checked_add(1).ok_or(Error::Overflow)?;
                    count(self.generated, self.limits.max_occurrences)?;
                    if let Some(duration) = &mut self.checked_duration_us {
                        let row = slot
                            .head
                            .as_ref()
                            .ok_or(Error::Invalid("missing projected packet"))?;
                        record_duration(duration, row, micros)?;
                    }
                    if self.recording_seek_checkpoints {
                        self.record_seek_checkpoint(self.at);
                    }
                    self.slots.as_mut_slice()[self.at].head = None;
                }
                Phase::Fill => {
                    if PREPARE {
                        return Ok(CursorStatus::Ready);
                    }
                    if self.at == self.slots.len() {
                        self.at = 0;
                        self.best = None;
                        self.phase = Phase::Select;
                        continue;
                    }
                    let global_schedule = self.slots.len() > 1;
                    if global_schedule {
                        self.prepare_micros(self.at);
                    }
                    let micros = self.micros.get(self.at).and_then(Option::as_ref);
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
                        micros,
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
                            // A lone track's next occurrence is the selection.
                            if !global_schedule && slot.resume_remaining == 0 {
                                self.at = 1;
                                self.best = Some(0);
                                self.phase = Phase::Select;
                            }
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
                        self.prepare_micros(best);
                        let micros = self.micros.get(best).and_then(Option::as_ref);
                        let slot = &mut self.slots.as_mut_slice()[best];
                        if let Some(end) = &mut self.forward_encoded_end
                            && let Some(packet) = &slot.head
                        {
                            if *end > packet.source.range.start {
                                return Err(Error::Unsupported(
                                    "forward encoded edits revisit media",
                                ));
                            }
                            *end = packet.source.range.end;
                        }
                        if let Some(duration) = &mut self.checked_duration_us {
                            let row = slot
                                .head
                                .take()
                                .ok_or(Error::Invalid("missing projected packet"))?;
                            record_duration(duration, &row, micros)?;
                            self.at = 0;
                            self.phase = Phase::Fill;
                            continue;
                        }
                        // The occurrence stays in its slot until it is taken.
                        self.ready = slot.head.is_some().then_some(best);
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

/// Whether `range` lies in the extent that holds its start.
fn within_one_extent(media: &[Range<u64>], range: &Range<u64>) -> bool {
    let end = media.partition_point(|extent| extent.start <= range.start);
    end.checked_sub(1)
        .and_then(|at| media.get(at))
        .is_some_and(|extent| require_range_within(range, extent.clone()).is_ok())
}

/// Raise `duration` to the end of `packet` in microseconds; `micros`, when
/// given, is the packet's demux base in microseconds.
fn record_duration(
    duration: &mut i64,
    packet: &PacketOccurrence,
    micros: Option<&TickScale>,
) -> Result<()> {
    if let Some(pts) = packet.demux.pts {
        let end = pts
            .checked_add(packet.demux.duration)
            .ok_or(Error::Overflow)?;
        let micros = match micros {
            Some(micros) => micros.rescale(end, Rounding::NearestAway)?,
            None => packet.media_clock.demux_base.rescale(
                end,
                TimeBase::MICROSECONDS,
                Rounding::NearestAway,
            )?,
        };
        *duration = (*duration).max(micros);
    }
    Ok(())
}

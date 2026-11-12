//! Explicit reference-public-MOV index/backward-seek view, never source timing.
//! Original Samples and clock/config/edit provenance travel with every entry.
//! Compact streaming metadata passes; no per-sample array or payload cache.
//! Regression guards were observed failing before implementation activation.
use crate::mov_cursor::TrackProjection;
use crate::{
    BuildStatus, ClockOrigin, CursorStatus, DemuxProfile, DemuxTrack, MediaClock,
    OpusRecoveryLanding, PacketCursor, PacketLimits, PacketOccurrence, ResumePoint,
    opus_seek::OpusRecoverySeek,
};
use std::{
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};
use triergon_core::{Budget, BudgetVec, Error, Result, Rounding, TimeBase, WorkBudget};

#[derive(Clone, Copy, Debug)]
pub struct IndexLimits {
    pub packets: PacketLimits,
    pub max_entries: u64,
}
impl Default for IndexLimits {
    fn default() -> Self {
        Self {
            packets: PacketLimits::default(),
            max_entries: 1_000_000,
        }
    }
}
#[derive(Clone, Debug)]
pub struct IndexEntry {
    /// Zero-based demux entry, distinct from one-based unique source ordinal.
    pub ordinal: u64,
    pub track_id: u32,
    pub position: i64,
    pub timestamp: i64,
    /// Reference public signed2-bit C field plus its exact low two bits. This is
    /// distinct from positive AVPacket key/discard flags and original sample flags.
    pub flags: i32,
    pub raw_flag_bits: u8,
    pub size: u32,
    pub min_distance: i32,
    pub occurrence: PacketOccurrence,
}
fn bound(value: u64, maximum: u64) -> Result<()> {
    if value > maximum {
        return Err(Error::Limit {
            requested: usize::try_from(value).map_err(|_| Error::Overflow)?,
            maximum: usize::try_from(maximum).map_err(|_| Error::Overflow)?,
        });
    }
    Ok(())
}
#[derive(Debug, Clone, Copy)]
struct RunCarry {
    flags: u32,
    source_ordinal: u64,
    source_duration: u32,
    source_dts: Option<i128>,
    source_cts_unknown: bool,
    demux_dts: Option<i64>,
    demux_exact_dts: i128,
}
impl RunCarry {
    fn clocks_continue(self, current: &PacketOccurrence) -> bool {
        match (self.source_dts, current.source.dts_ticks) {
            // A demux API null is not an unknown source time. Use the
            // existing exact demux projection input for this known-source
            // continuity check; retain every signed/raw/null output field.
            (Some(previous), Some(next)) => {
                next > previous && current.demux.dts_field.exact_ticks > self.demux_exact_dts
            }
            (None, None) if self.source_cts_unknown && current.source.cts_ticks.is_none() => {
                let (Some(previous_demux), Some(current_demux)) =
                    (self.demux_dts, current.demux.dts)
                else {
                    return false;
                };
                if current_demux <= previous_demux {
                    return false;
                }
                // Missing-tfdt entries retain unknown source times.
                // Accept only the existing explicit predecessor witness, tied
                // to this exact previous sample/duration and representable clocks.
                match current.clock_origin {
                    ClockOrigin::PredecessorEnd(witness) => {
                        witness.predecessor_sample == self.source_ordinal
                            && witness.predecessor_duration_ticks == self.source_duration
                            && !witness.predecessor_source_dts_known
                            && witness.predecessor_demux_dts_ticks == i128::from(previous_demux)
                            && witness.derived_dts_ticks == i128::from(current_demux)
                            && witness
                                .predecessor_demux_dts_ticks
                                .checked_add(i128::from(witness.predecessor_duration_ticks))
                                == Some(witness.derived_dts_ticks)
                    }
                    _ => false,
                }
            }
            _ => false,
        }
    }
}
/// One demux table stream, keeping a single ready projection. A complete
/// source is mandatory for the existing explicit absent-tfdt demux origin.
#[derive(Debug)]
pub struct IndexCursor<'a> {
    cursor: PacketCursor<'a>,
    max_entries: u64,
    next: u64,
    distance: u64,
    run: Option<(usize, usize)>,
    carry: Option<RunCarry>,
    ready: Option<IndexEntry>,
    failed: Option<Error>,
    recovery_initial_nonkey: bool,
    dependency_table: bool,
}
impl<'a> IndexCursor<'a> {
    fn from_cursor(
        cursor: PacketCursor<'a>,
        max_entries: u64,
        recovery_initial_nonkey: bool,
        dependency_table: bool,
    ) -> Self {
        Self {
            cursor,
            max_entries,
            next: 0,
            distance: 0,
            run: None,
            carry: None,
            ready: None,
            failed: None,
            recovery_initial_nonkey,
            dependency_table,
        }
    }

    fn starts_before_sync(track: DemuxTrack<'_>) -> bool {
        track.fragments.is_empty()
            && track.edits.is_none()
            && track.table.unused_sync_entries() != 0
    }

    pub fn new_complete_source(
        pool: &Budget,
        track: DemuxTrack<'a>,
        media: &'a [Range<u64>],
        limits: IndexLimits,
        profile: DemuxProfile,
    ) -> Result<Self> {
        // A regular table with an explicitly bounded sync prefix may start
        // before any usable sync marker. The first index distance is zero;
        // this does not grant a seek point.
        let cursor =
            PacketCursor::new_complete_source(pool, &[track], media, limits.packets, profile)?;
        Ok(Self::from_cursor(
            cursor,
            limits.max_entries,
            Self::starts_before_sync(track),
            false,
        ))
    }

    /// Replay one track from a completed packet cursor using its checked sample
    /// extents and initial clock facts. The index has its own entry limit.
    pub fn replay_track(
        checked: &PacketCursor<'a>,
        track_id: u32,
        max_entries: u64,
    ) -> Result<Self> {
        let (cursor, track) = checked.fork_index_track(track_id)?;
        Ok(Self::from_cursor(
            cursor,
            max_entries,
            Self::starts_before_sync(track),
            false,
        ))
    }
    pub fn new_encoded_regular(
        pool: &Budget,
        track: DemuxTrack<'a>,
        media: &'a [Range<u64>],
        limits: IndexLimits,
        plans: &'a [Option<crate::EncodedEditPlan>],
        profile: crate::EncodedEditProfile,
    ) -> Result<Self> {
        let cursor = PacketCursor::new_encoded_regular(
            pool,
            &[track],
            media,
            limits.packets,
            plans,
            profile,
        )?;
        Ok(Self::from_cursor(
            cursor,
            limits.max_entries,
            false,
            plans.iter().flatten().any(|p| p.has_dependency_view()),
        ))
    }
    pub(crate) fn allow_recovery_initial_nonkey(&mut self) {
        self.recovery_initial_nonkey = true;
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
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if self.ready.is_some() {
            return Ok(CursorStatus::Ready);
        }
        match self.cursor.poll(work, cancel)? {
            CursorStatus::Yield => Ok(CursorStatus::Yield),
            CursorStatus::End => Ok(CursorStatus::End),
            CursorStatus::Ready => {
                if !work.try_charge(1) {
                    return Ok(CursorStatus::Yield);
                }
                let next = self.next.checked_add(1).ok_or(Error::Overflow)?;
                bound(next, self.max_entries)?;
                let source_run = self.cursor.ready_source_run()?;
                let occurrence = self.cursor.take_occurrence()?;
                // Public AVIndexEntry has key/discard bits, independently of
                // AVPacket's disposable hint. Preserve that hint on the retained
                // occurrence, while the separately typed index keeps its own ABI.
                let admitted = 5 | if self.dependency_table { 16 } else { 0 };
                if occurrence.demux.flags & !admitted != 0 {
                    return Err(Error::Unsupported("native index packet flag projection"));
                }
                let size = occurrence
                    .source
                    .range
                    .end
                    .checked_sub(occurrence.source.range.start)
                    .ok_or(Error::Overflow)?;
                // A conservative positive signed30-bit view. Source size stays
                // u64 and is never truncated to fit a demux bitfield.
                if size > 0x1fff_ffff {
                    return Err(Error::Unsupported(
                        "native index signed size representation",
                    ));
                }
                let key = occurrence.demux.flags & 1 != 0;
                let run = source_run.map(|(fragment, run, _)| (fragment, run));
                // Demux controls reset the distance at the first
                // entry of a following traf, including non-key/discard rows
                // after rollback or an explicit demux predecessor origin.
                // Source ordinal continuity and forward supplied-index order
                // identify this boundary without inventing source timestamps.
                let starts_fragment = match (self.run, self.carry, source_run) {
                    (
                        Some((previous_fragment, _)),
                        Some(previous),
                        Some((fragment, current_run, _)),
                    ) => {
                        fragment > previous_fragment
                            && current_run == 0
                            && previous.source_ordinal.checked_add(1)
                                == Some(occurrence.source.ordinal)
                    }
                    _ => false,
                };
                // The f-multitrack input has consecutive [1,2] truns
                // within each traf. Its second run starts non-key and carries
                // the last distance at the append boundary: 0,0,1. This does not
                // establish cross-fragment, mixed-layout or unwitnessed-clock carry.
                let continued_run = match (self.run, self.carry, source_run) {
                    (
                        Some((previous_fragment, previous_run)),
                        Some(previous),
                        Some((fragment, current_run, flags)),
                    ) => {
                        previous_fragment == fragment
                            && previous_run.checked_add(1) == Some(current_run)
                            // CTS presence changes no sample-flag or decode
                            // distance state. Preserve both raw layouts while
                            // requiring every other optional-field bit to match.
                            && (previous.flags ^ flags) & !0x800 == 0
                            && flags & 0x400 != 0
                            && occurrence.demux.flags & 4 == 0
                            && previous.clocks_continue(&occurrence)
                    }
                    _ => false,
                };
                let first_fragment_sample =
                    self.next == 0 && source_run.is_some() && occurrence.source.ordinal == 1;
                let starts_at_zero = key
                    || starts_fragment
                    || first_fragment_sample
                    || (self.next == 0 && self.recovery_initial_nonkey);
                if !starts_at_zero
                    && (self.next == 0 || (run.is_some() && run != self.run && !continued_run))
                {
                    return Err(Error::Unsupported(
                        "native index no-key append distance carry",
                    ));
                }
                // Demux partition controls distinguish within-run counting
                // from append initialization: [3] gives 0,1,2; [1,2] gives
                // 0,0,1. Keep the last emitted distance, carry it once at an
                // accepted append boundary, then increment inside that run.
                // Flag presence alone does not introduce a per-sample lag.
                let distance = if starts_at_zero {
                    0
                } else if run.is_some() && run != self.run {
                    self.distance
                } else {
                    self.distance.checked_add(1).ok_or(Error::Overflow)?
                };
                let raw_flag_bits = u8::from(key)
                    | if occurrence.demux.flags & 4 != 0 {
                        2
                    } else {
                        0
                    };
                let flags = match raw_flag_bits {
                    0 => 0,
                    1 => 1,
                    2 => -2,
                    3 => -1,
                    _ => return Err(Error::Invalid("native index flags")),
                };
                let carry = source_run.map(|(_, _, flags)| RunCarry {
                    flags,
                    source_ordinal: occurrence.source.ordinal,
                    source_duration: occurrence.source.duration_ticks,
                    source_dts: occurrence.source.dts_ticks,
                    source_cts_unknown: occurrence.source.cts_ticks.is_none(),
                    demux_dts: occurrence.demux.dts,
                    demux_exact_dts: occurrence.demux.dts_field.exact_ticks,
                });
                let entry = IndexEntry {
                    ordinal: self.next,
                    track_id: occurrence.track_id,
                    position: i64::try_from(occurrence.source.range.start)
                        .map_err(|_| Error::Unsupported("native index signed position"))?,
                    timestamp: occurrence.demux.dts_field.signed_ticks(),
                    flags,
                    raw_flag_bits,
                    size: u32::try_from(size).map_err(|_| Error::Overflow)?,
                    min_distance: i32::try_from(distance)
                        .map_err(|_| Error::Unsupported("native index signed distance"))?,
                    occurrence,
                };
                self.next = next;
                self.distance = distance;
                self.run = run;
                self.carry = carry;
                self.ready = Some(entry);
                Ok(CursorStatus::Ready)
            }
        }
    }
    pub fn take_entry(&mut self) -> Result<IndexEntry> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        self.ready
            .take()
            .ok_or(Error::Invalid("no ready native index entry"))
    }
}

#[derive(Clone, Copy, Debug)]
pub struct SeekLimits {
    pub index: IndexLimits,
    pub max_replayed_occurrences: u64,
}
impl Default for SeekLimits {
    fn default() -> Self {
        Self {
            index: IndexLimits::default(),
            max_replayed_occurrences: 1_000_000,
        }
    }
}
#[derive(Clone, Debug)]
pub struct SeekLanding {
    pub requested_timestamp: i64,
    pub selected: IndexEntry,
    pub scanned_entries: u64,
    pub used_first_fallback: bool,
    /// Decoder preroll when the caller requests source Opus recovery.
    pub opus_recovery: Option<OpusRecoveryLanding>,
}
#[derive(Debug, Clone)]
struct Context<'a> {
    pool: Budget,
    track: DemuxTrack<'a>,
    media: &'a [Range<u64>],
    limits: SeekLimits,
    profile: DemuxProfile,
    encoded_edits: Option<EncodedEdits<'a>>,
}
type EncodedEdits<'a> = &'a [Option<crate::EncodedEditPlan>];
/// Demux signed ticks are a separately named API argument. No rounding,
/// source-time wrap or conversion through SourceTime is performed here.
#[derive(Debug)]
pub struct SeekJob<'a> {
    context: Context<'a>,
    index: IndexCursor<'a>,
    timestamp: i64,
    index_timestamp: i128,
    first: Option<IndexEntry>,
    best: Option<IndexEntry>,
    previous: Option<i64>,
    unordered_minimum: Option<i64>,
    scanned: u64,
    allow_fragment_prefix: bool,
    ready: bool,
    failed: Option<Error>,
}
#[derive(Debug)]
pub struct SeekPlan<'a> {
    context: Context<'a>,
    landing: SeekLanding,
}
/// Resumes the real demux occurrence state by a bounded metadata replay from
/// original tables. Skipped metadata causes no payload reads or caller output.
/// This is a production cursor operation, not comparator filtering or PTS sort.
#[derive(Debug)]
pub struct SeekCursor<'a> {
    cursor: PacketCursor<'a>,
    selected: IndexEntry,
    next: u64,
    maximum: u64,
    reached: bool,
    ended: bool,
    ready: Option<PacketOccurrence>,
    failed: Option<Error>,
}
impl<'a> SeekJob<'a> {
    pub fn new_complete_source(
        pool: &Budget,
        track: DemuxTrack<'a>,
        media: &'a [Range<u64>],
        timestamp: i64,
        limits: SeekLimits,
        profile: DemuxProfile,
    ) -> Result<Self> {
        Self::build(
            Context {
                pool: pool.clone(),
                track,
                media,
                limits,
                profile,
                encoded_edits: None,
            },
            timestamp,
        )
    }
    fn build(context: Context<'a>, timestamp: i64) -> Result<Self> {
        if timestamp == i64::MIN {
            return Err(Error::Unsupported("native seek null timestamp argument"));
        }
        let index = if let Some(plans) = context.encoded_edits {
            IndexCursor::new_encoded_regular(
                &context.pool,
                context.track,
                context.media,
                context.limits.index,
                plans,
                crate::EncodedEditProfile,
            )?
        } else {
            IndexCursor::new_complete_source(
                &context.pool,
                context.track,
                context.media,
                context.limits.index,
                context.profile,
            )?
        };
        Ok(Self {
            context,
            index,
            timestamp,
            index_timestamp: i128::from(timestamp),
            first: None,
            best: None,
            previous: None,
            unordered_minimum: None,
            scanned: 0,
            allow_fragment_prefix: false,
            ready: false,
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
            if self.ready {
                return Ok(BuildStatus::Ready);
            }
            match self.index.poll(work, cancel)? {
                CursorStatus::Yield => return Ok(BuildStatus::Yield),
                CursorStatus::Ready => {
                    if !work.try_charge(1) {
                        return Ok(BuildStatus::Yield);
                    }
                    let entry = self.index.take_entry()?;
                    if entry.timestamp == i64::MIN {
                        return Err(Error::Unsupported("native seek null index clock"));
                    }
                    if self.scanned == 0 {
                        // A fragment seek subtracts the media start from its
                        // query. A leading empty edit delays index timestamps.
                        self.index_timestamp -= i128::from(self.index.cursor.index_edit_offset());
                        if entry.occurrence.source.fragment_flags.is_some() && entry.flags & 1 == 0
                        {
                            self.first = Some(entry.clone());
                        }
                    }
                    if self.previous.is_some_and(|prior| entry.timestamp <= prior)
                        || self.unordered_minimum.is_some()
                    {
                        self.unordered_minimum = Some(
                            self.unordered_minimum
                                .map_or(entry.timestamp, |v| v.min(entry.timestamp)),
                        );
                    }
                    self.previous = Some(entry.timestamp);
                    self.scanned = self.scanned.checked_add(1).ok_or(Error::Overflow)?;
                    if entry.flags & 1 != 0 {
                        if self.first.is_none() {
                            self.first = Some(entry.clone());
                        }
                        if i128::from(entry.timestamp) <= self.index_timestamp {
                            self.best = Some(entry);
                        }
                    }
                }
                CursorStatus::End => {
                    if !work.try_charge(1) {
                        return Ok(BuildStatus::Yield);
                    }
                    // The observed repeated-edit prefix is separated from all
                    // later rollback entries by this query. Overlapping/late
                    // searches stay refused until the demux search is established.
                    if self
                        .unordered_minimum
                        .is_some_and(|minimum| self.index_timestamp >= i128::from(minimum))
                    {
                        return Err(Error::Unsupported(
                            "native seek overlapping index clock query",
                        ));
                    }
                    if self.best.is_none()
                        && self
                            .first
                            .as_ref()
                            .is_none_or(|entry| entry.flags & 1 == 0 && !self.allow_fragment_prefix)
                    {
                        return Err(Error::Unsupported("native seek no observed key fallback"));
                    }
                    self.ready = true;
                    return Ok(BuildStatus::Ready);
                }
            }
        }
    }
    pub fn finish(mut self) -> Result<SeekPlan<'a>> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if !self.ready {
            return Err(Error::Invalid("native seek plan not ready"));
        }
        let used_first_fallback = self.best.is_none();
        let selected = self
            .best
            .take()
            .or_else(|| self.first.take())
            .ok_or(Error::Invalid("missing native seek selection"))?;
        Ok(SeekPlan {
            context: self.context,
            landing: SeekLanding {
                requested_timestamp: self.timestamp,
                selected,
                scanned_entries: self.scanned,
                used_first_fallback,
                opus_recovery: None,
            },
        })
    }
}
impl<'a> SeekPlan<'a> {
    pub fn landing(&self) -> &SeekLanding {
        &self.landing
    }
    pub fn cursor(&self) -> Result<SeekCursor<'a>> {
        let c = &self.context;
        let cursor = if let Some(plans) = c.encoded_edits {
            PacketCursor::new_encoded_regular(
                &c.pool,
                &[c.track],
                c.media,
                c.limits.index.packets,
                plans,
                crate::EncodedEditProfile,
            )?
        } else {
            PacketCursor::new_complete_source(
                &c.pool,
                &[c.track],
                c.media,
                c.limits.index.packets,
                c.profile,
            )?
        };
        Ok(SeekCursor {
            cursor,
            selected: self.landing.selected.clone(),
            next: 0,
            maximum: c.limits.max_replayed_occurrences,
            reached: false,
            ended: false,
            ready: None,
            failed: None,
        })
    }
}
impl SeekCursor<'_> {
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
            if self.ended {
                return Ok(CursorStatus::End);
            }
            match self.cursor.poll(work, cancel)? {
                CursorStatus::Yield => return Ok(CursorStatus::Yield),
                CursorStatus::End => {
                    if !self.reached {
                        return Err(Error::Invalid(
                            "native seek selection vanished during replay",
                        ));
                    }
                    self.ended = true;
                    return Ok(CursorStatus::End);
                }
                CursorStatus::Ready => {
                    if !work.try_charge(1) {
                        return Ok(CursorStatus::Yield);
                    }
                    let next = self.next.checked_add(1).ok_or(Error::Overflow)?;
                    bound(next, self.maximum)?;
                    let row = self.cursor.take_occurrence()?;
                    let ordinal = self.next;
                    self.next = next;
                    if ordinal < self.selected.ordinal {
                        continue;
                    }
                    if !self.reached {
                        let selected = &self.selected.occurrence;
                        if ordinal != self.selected.ordinal
                            || row.track_id != selected.track_id
                            || row.source.ordinal != selected.source.ordinal
                            || row.source.description_index != selected.source.description_index
                            || row.source.range != selected.source.range
                            || row.demux != selected.demux
                            || row.clock_origin != selected.clock_origin
                        {
                            return Err(Error::Invalid(
                                "native seek replay selection disagreement",
                            ));
                        }
                        self.reached = true;
                    }
                    self.ready = Some(row);
                    return Ok(CursorStatus::Ready);
                }
            }
        }
    }
    pub fn take_occurrence(&mut self) -> Result<PacketOccurrence> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        self.ready
            .take()
            .ok_or(Error::Invalid("no ready native seek occurrence"))
    }
}

/// Aligns every stream to the selected reference entry's demux clock, not the
/// original request or its physical position. The rescaled profile converts
/// that anchor to each other track's demux base, rounding ties away from zero.
#[derive(Debug)]
pub struct GlobalSeekJob<'a> {
    pool: Budget,
    tracks: BudgetVec<DemuxTrack<'a>>,
    media: &'a [Range<u64>],
    limits: SeekLimits,
    profile: DemuxProfile,
    encoded_edits: Option<EncodedEdits<'a>>,
    projection: TrackProjection,
    reference_index: usize,
    reference_base: TimeBase,
    reference: Option<SeekLanding>,
    anchors: BudgetVec<ResumePoint>,
    child: Option<SeekJob<'a>>,
    opus_track: Option<usize>,
    opus_recovery: Option<Box<OpusRecoverySeek<'a>>>,
    at: usize,
    scanned: u64,
    ready: bool,
    failed: Option<Error>,
}
#[derive(Debug)]
pub struct GlobalSeekPlan<'a> {
    pool: Budget,
    tracks: BudgetVec<DemuxTrack<'a>>,
    media: &'a [Range<u64>],
    limits: SeekLimits,
    profile: DemuxProfile,
    encoded_edits: Option<EncodedEdits<'a>>,
    projection: TrackProjection,
    reference: SeekLanding,
    anchors: BudgetVec<ResumePoint>,
}
enum GlobalSeekMode<'a> {
    SameBase,
    Rescaled,
    Encoded(EncodedEdits<'a>),
    Projected {
        plans: Option<EncodedEdits<'a>>,
        projection: TrackProjection,
    },
}
impl<'a> GlobalSeekJob<'a> {
    pub fn new_complete_source(
        pool: &Budget,
        tracks: &[DemuxTrack<'a>],
        media: &'a [Range<u64>],
        track_id: u32,
        timestamp: i64,
        limits: SeekLimits,
        profile: DemuxProfile,
    ) -> Result<Self> {
        Self::build(
            pool,
            tracks,
            media,
            (track_id, timestamp),
            limits,
            profile,
            GlobalSeekMode::SameBase,
        )
    }

    /// Admit unequal demux timebases using the reference MOV seek rescale.
    pub fn new_complete_source_rescaled(
        pool: &Budget,
        tracks: &[DemuxTrack<'a>],
        media: &'a [Range<u64>],
        track_id: u32,
        timestamp: i64,
        limits: SeekLimits,
        profile: DemuxProfile,
    ) -> Result<Self> {
        Self::build(
            pool,
            tracks,
            media,
            (track_id, timestamp),
            limits,
            profile,
            GlobalSeekMode::Rescaled,
        )
    }

    /// Use the same encoded edit plans that produced the indexed packet
    /// occurrences. The selected anchor is rescaled for each other track.
    pub fn new_encoded_regular_rescaled(
        pool: &Budget,
        tracks: &[DemuxTrack<'a>],
        media: &'a [Range<u64>],
        reference: (u32, i64),
        limits: SeekLimits,
        plans: EncodedEdits<'a>,
    ) -> Result<Self> {
        Self::build(
            pool,
            tracks,
            media,
            reference,
            limits,
            DemuxProfile::NoParseNoFillin,
            GlobalSeekMode::Encoded(plans),
        )
    }

    /// Plan a rescaled seek for the same track ordinals selected for playback.
    /// Edit plans are indexed by the complete source track order.
    pub fn new_projected(
        pool: &Budget,
        tracks: &[DemuxTrack<'a>],
        media: &'a [Range<u64>],
        reference: (u32, i64),
        limits: SeekLimits,
        plans: Option<EncodedEdits<'a>>,
        include: impl FnMut(usize) -> bool,
    ) -> Result<Self> {
        Self::build(
            pool,
            tracks,
            media,
            reference,
            limits,
            DemuxProfile::NoParseNoFillin,
            GlobalSeekMode::Projected {
                plans,
                projection: TrackProjection::new(tracks.len(), include)?,
            },
        )
    }

    fn build(
        pool: &Budget,
        tracks: &[DemuxTrack<'a>],
        media: &'a [Range<u64>],
        request: (u32, i64),
        limits: SeekLimits,
        profile: DemuxProfile,
        mode: GlobalSeekMode<'a>,
    ) -> Result<Self> {
        let (track_id, timestamp) = request;
        let (encoded_edits, projection) = match mode {
            GlobalSeekMode::Encoded(plans) => (Some(plans), TrackProjection::default()),
            GlobalSeekMode::Projected { plans, projection } => (plans, projection),
            _ => (None, TrackProjection::default()),
        };
        if encoded_edits.is_some_and(|plans| plans.len() != tracks.len()) {
            return Err(Error::Invalid("encoded edit track coverage"));
        }
        let maximum = limits.index.packets.max_tracks.min(64);
        if tracks.len() > maximum {
            return Err(Error::Limit {
                requested: tracks.len(),
                maximum,
            });
        }
        let reference_index = tracks
            .iter()
            .position(|t| t.track_id == track_id)
            .filter(|&at| projection.includes(at))
            .ok_or(Error::Invalid("native seek reference track absent"))?;
        let base = MediaClock::from_source(tracks[reference_index].media_base)?.demux_base;
        let mut owned = BudgetVec::new(pool);
        owned.try_reserve_exact(tracks.len())?;
        let mut anchors = BudgetVec::new(pool);
        anchors.try_reserve_exact(tracks.len())?;
        for (i, track) in tracks.iter().enumerate() {
            if tracks[..i].iter().any(|t| t.track_id == track.track_id) {
                return Err(Error::Invalid("duplicate native seek track"));
            }
            if projection.includes(i) {
                let track_base = MediaClock::from_source(track.media_base)?.demux_base;
                if matches!(mode, GlobalSeekMode::SameBase) && track_base != base {
                    return Err(Error::Unsupported(
                        "native global seek unequal timebase rescale",
                    ));
                }
            }
            owned.try_push(*track).map_err(|e| e.error)?;
            anchors
                .try_push(ResumePoint {
                    track_id: track.track_id,
                    occurrence: 0,
                })
                .map_err(|e| e.error)?;
        }
        let mut job = Self {
            pool: pool.clone(),
            tracks: owned,
            media,
            limits,
            profile,
            encoded_edits,
            projection,
            reference_index,
            reference_base: base,
            reference: None,
            anchors,
            child: None,
            opus_track: None,
            opus_recovery: None,
            at: 0,
            scanned: 0,
            ready: false,
            failed: None,
        };
        job.child = Some(SeekJob::build(
            job.track_context(reference_index),
            timestamp,
        )?);
        Ok(job)
    }
    fn track_context(&self, at: usize) -> Context<'a> {
        Context {
            pool: self.pool.clone(),
            track: self.tracks.as_slice()[at],
            media: self.media,
            limits: self.limits,
            profile: self.profile,
            encoded_edits: self.encoded_edits.map(|plans| &plans[at..at + 1]),
        }
    }
    /// Use a secondary regular Opus track's source roll groups for decoder
    /// recovery. Select this before the seek lands on the reference track.
    pub fn with_opus_recovery(mut self, track_id: u32) -> Result<Self> {
        if self.reference.is_some() || self.opus_track.is_some() || self.failed.is_some() {
            return Err(Error::Invalid(
                "Opus recovery must be selected before the seek",
            ));
        }
        let at = self
            .tracks
            .as_slice()
            .iter()
            .position(|track| track.track_id == track_id)
            .filter(|&at| at != self.reference_index && self.projection.includes(at))
            .ok_or(Error::Invalid("missing secondary Opus track"))?;
        self.opus_edit(at)?.opus_roll_scope()?;
        self.opus_track = Some(at);
        Ok(self)
    }

    fn opus_edit(&self, at: usize) -> Result<&'a crate::EncodedEditPlan> {
        self.encoded_edits
            .and_then(|plans| plans[at].as_ref())
            .ok_or(Error::Unsupported("Opus recovery needs an Opus edit"))
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
            if self.ready {
                return Ok(BuildStatus::Ready);
            }
            if let Some(recovery) = &mut self.opus_recovery {
                let Some(landing) = recovery.poll(work, cancel)? else {
                    return Ok(BuildStatus::Yield);
                };
                self.anchors.as_mut_slice()[self.at].occurrence = self
                    .opus_edit(self.at)?
                    .opus_occurrence(landing.first_sample)?;
                self.reference
                    .as_mut()
                    .expect("Opus recovery follows the reference seek")
                    .opus_recovery = Some(landing);
                self.opus_recovery = None;
                self.at += 1;
                continue;
            }
            if let Some(child) = &mut self.child {
                if child.poll(work, cancel)? == BuildStatus::Yield {
                    return Ok(BuildStatus::Yield);
                }
                if !work.try_charge(1) {
                    return Ok(BuildStatus::Yield);
                }
                let plan = self
                    .child
                    .take()
                    .ok_or(Error::Invalid("native seek child absent"))?
                    .finish()?;
                let landing = plan.landing().clone();
                self.scanned = self
                    .scanned
                    .checked_add(landing.scanned_entries)
                    .ok_or(Error::Overflow)?;
                bound(self.scanned, self.limits.index.max_entries)?;
                let at = if self.reference.is_none() {
                    self.reference_index
                } else {
                    self.at
                };
                if self.opus_track == Some(at) {
                    let sample = &landing.selected.occurrence.source;
                    let edit = self.opus_edit(at)?;
                    if landing.selected.ordinal != edit.opus_occurrence(sample.ordinal)? {
                        return Err(Error::Invalid("Opus seek occurrence order"));
                    }
                    self.opus_recovery = Some(OpusRecoverySeek::new(
                        &self.pool,
                        self.tracks.as_slice()[at],
                        edit.opus_roll_scope()?,
                        sample,
                    )?);
                } else {
                    self.anchors.as_mut_slice()[at].occurrence = landing.selected.ordinal;
                    if self.reference.is_none() {
                        self.reference = Some(landing);
                    } else {
                        self.at = self.at.checked_add(1).ok_or(Error::Overflow)?;
                    }
                }
                continue;
            }
            if !work.try_charge(1) {
                return Ok(BuildStatus::Yield);
            }
            if self.at == self.tracks.len() {
                self.ready = true;
                return Ok(BuildStatus::Ready);
            }
            if self.at == self.reference_index || !self.projection.includes(self.at) {
                self.at = self.at.checked_add(1).ok_or(Error::Overflow)?;
                continue;
            }
            let selected = &self
                .reference
                .as_ref()
                .ok_or(Error::Invalid("native reference selection absent"))?
                .selected;
            let recovering_opus = self.opus_track == Some(self.at);
            let track = self.tracks.as_slice()[self.at];
            let base = MediaClock::from_source(track.media_base)?.demux_base;
            let timestamp = if recovering_opus {
                let pts = selected
                    .occurrence
                    .demux
                    .pts
                    .ok_or(Error::Invalid("missing video landing PTS"))?;
                self.opus_edit(self.at)?
                    .opus_seek_timestamp(pts, self.reference_base, base)?
            } else {
                let timestamp =
                    self.reference_base
                        .rescale(selected.timestamp, base, Rounding::NearestAway)?;
                match self.encoded_edits.and_then(|plans| plans[self.at].as_ref()) {
                    Some(edit) => edit.align_seek_timestamp(timestamp, base)?,
                    None => timestamp,
                }
            };
            // Before the first sync on a secondary fragment track, a fresh
            // range still has its leading non-sync packets available to replay.
            let mut child = SeekJob::build(self.track_context(self.at), timestamp)?;
            child.allow_fragment_prefix = true;
            self.child = Some(child);
        }
    }
    pub fn finish(mut self) -> Result<GlobalSeekPlan<'a>> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if !self.ready {
            return Err(Error::Invalid("native global seek plan not ready"));
        }
        self.projection.retain(&mut self.anchors);
        Ok(GlobalSeekPlan {
            pool: self.pool,
            tracks: self.tracks,
            media: self.media,
            limits: self.limits,
            profile: self.profile,
            encoded_edits: self.encoded_edits,
            projection: self.projection,
            reference: self
                .reference
                .ok_or(Error::Invalid("native reference selection absent"))?,
            anchors: self.anchors,
        })
    }
}
impl<'a> GlobalSeekPlan<'a> {
    pub fn landing(&self) -> &SeekLanding {
        &self.reference
    }
    pub fn anchors(&self) -> &[ResumePoint] {
        self.anchors.as_slice()
    }
    pub fn cursor(&self) -> Result<PacketCursor<'a>> {
        let cursor = if !self.projection.is_all() {
            PacketCursor::new_projected(
                &self.pool,
                self.tracks.as_slice(),
                self.media,
                self.limits.index.packets,
                self.encoded_edits,
                |index| self.projection.includes(index),
            )?
        } else if let Some(plans) = self.encoded_edits {
            PacketCursor::new_encoded_regular(
                &self.pool,
                self.tracks.as_slice(),
                self.media,
                self.limits.index.packets,
                plans,
                crate::EncodedEditProfile,
            )?
        } else {
            PacketCursor::new_complete_source(
                &self.pool,
                self.tracks.as_slice(),
                self.media,
                self.limits.index.packets,
                self.profile,
            )?
        };
        cursor.with_resume_points(self.anchors.as_slice(), self.limits.index.packets)
    }
}

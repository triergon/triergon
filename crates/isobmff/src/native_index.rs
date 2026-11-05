//! Explicit pinned-public-MOV index/backward-seek view, never source timing.
//! Original Samples and clock/config/edit provenance travel with every entry.
//! Compact streaming metadata passes; no per-sample array or payload cache.
//! Regression guards were observed failing before implementation activation.
use crate::{
    BuildStatus, Mov9ClockOrigin, Mov9CursorStatus, Mov9MediaClock, Mov9PacketCursor,
    Mov9PacketLimits, Mov9PacketOccurrence, Mov9Profile, Mov9ResumePoint, Mov9Track,
};
use std::{
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};
use triergon_core::{Budget, BudgetVec, Error, Result, Rounding, TimeBase, WorkBudget};

#[derive(Clone, Copy, Debug)]
pub struct Mov9NativeIndexLimits {
    pub packets: Mov9PacketLimits,
    pub max_entries: u64,
}
impl Default for Mov9NativeIndexLimits {
    fn default() -> Self {
        Self {
            packets: Mov9PacketLimits::default(),
            max_entries: 1_000_000,
        }
    }
}
#[derive(Clone, Debug)]
pub struct Mov9NativeIndexEntry {
    /// Zero-based native entry, distinct from one-based unique source ordinal.
    pub ordinal: u64,
    pub track_id: u32,
    pub position: i64,
    pub timestamp: i64,
    /// Pinned public signed2-bit C field plus its exact low two bits. This is
    /// distinct from positive AVPacket key/discard flags and original sample flags.
    pub flags: i32,
    pub raw_flag_bits: u8,
    pub size: u32,
    pub min_distance: i32,
    pub occurrence: Mov9PacketOccurrence,
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
#[derive(Clone, Copy)]
struct RunCarry {
    flags: u32,
    source_ordinal: u64,
    source_duration: u32,
    source_dts: Option<i128>,
    source_cts_unknown: bool,
    native_dts: Option<i64>,
    native_exact_dts: i128,
}
impl RunCarry {
    fn clocks_continue(self, current: &Mov9PacketOccurrence) -> bool {
        match (self.source_dts, current.source.dts_ticks) {
            // A native API null is not an unknown source time. Use the
            // existing exact native projection input for this known-source
            // continuity check; retain every signed/raw/null output field.
            (Some(previous), Some(next)) => {
                next > previous
                    && current.native.dts_field.exact_native_ticks > self.native_exact_dts
            }
            (None, None) if self.source_cts_unknown && current.source.cts_ticks.is_none() => {
                let (Some(previous_native), Some(current_native)) =
                    (self.native_dts, current.native.dts)
                else {
                    return false;
                };
                if current_native <= previous_native {
                    return false;
                }
                // Native27 missing-tfdt entries retain unknown source times.
                // Accept only the existing explicit predecessor witness, tied
                // to this exact previous sample/duration and representable clocks.
                match current.clock_origin {
                    Mov9ClockOrigin::PredecessorEnd(witness) => {
                        witness.predecessor_sample == self.source_ordinal
                            && witness.predecessor_duration_ticks == self.source_duration
                            && !witness.predecessor_source_dts_known
                            && witness.predecessor_native_dts_ticks == i128::from(previous_native)
                            && witness.derived_dts_ticks == i128::from(current_native)
                            && witness
                                .predecessor_native_dts_ticks
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
/// One native table stream, keeping a single ready projection. A complete
/// source is mandatory for the existing explicit absent-tfdt native origin.
pub struct Mov9NativeIndexCursor<'a> {
    cursor: Mov9PacketCursor<'a>,
    limits: Mov9NativeIndexLimits,
    next: u64,
    distance: u64,
    run: Option<(usize, usize)>,
    carry: Option<RunCarry>,
    ready: Option<Mov9NativeIndexEntry>,
    failed: Option<Error>,
    recovery_initial_nonkey: bool,
    dependency_v4: bool,
}
impl<'a> Mov9NativeIndexCursor<'a> {
    pub fn new_complete_source(
        pool: &Budget,
        track: Mov9Track<'a>,
        media: &'a [Range<u64>],
        limits: Mov9NativeIndexLimits,
        profile: Mov9Profile,
    ) -> Result<Self> {
        let cursor =
            Mov9PacketCursor::new_complete_source(pool, &[track], media, limits.packets, profile)?;
        Ok(Self {
            cursor,
            limits,
            next: 0,
            distance: 0,
            run: None,
            carry: None,
            ready: None,
            failed: None,
            recovery_initial_nonkey: false,
            dependency_v4: false,
        })
    }
    pub fn new_encoded_regular_v2(
        pool: &Budget,
        track: Mov9Track<'a>,
        media: &'a [Range<u64>],
        limits: Mov9NativeIndexLimits,
        plans: &'a [Option<crate::Mov9EncodedEditPlanV2>],
        profile: crate::PinnedMovD32EncodedEditsV2,
    ) -> Result<Self> {
        let cursor = Mov9PacketCursor::new_encoded_regular_v2(
            pool,
            &[track],
            media,
            limits.packets,
            plans,
            profile,
        )?;
        Ok(Self {
            cursor,
            limits,
            next: 0,
            distance: 0,
            run: None,
            carry: None,
            ready: None,
            failed: None,
            recovery_initial_nonkey: false,
            dependency_v4: plans.iter().flatten().any(|p| p.has_dependency_view_v4()),
        })
    }
    pub(crate) fn allow_recovery_initial_nonkey(&mut self) {
        self.recovery_initial_nonkey = true;
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
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if self.ready.is_some() {
            return Ok(Mov9CursorStatus::Ready);
        }
        match self.cursor.poll(work, cancel)? {
            Mov9CursorStatus::Yield => Ok(Mov9CursorStatus::Yield),
            Mov9CursorStatus::End => Ok(Mov9CursorStatus::End),
            Mov9CursorStatus::Ready => {
                if !work.try_charge(1) {
                    return Ok(Mov9CursorStatus::Yield);
                }
                let next = self.next.checked_add(1).ok_or(Error::Overflow)?;
                bound(next, self.limits.max_entries)?;
                let source_run = self.cursor.ready_source_run()?;
                let occurrence = self.cursor.take_occurrence()?;
                // Public AVIndexEntry has key/discard bits, independently of
                // AVPacket's disposable hint. Preserve that hint on the retained
                // occurrence, while the separately typed index keeps its own ABI.
                let admitted = 5 | if self.dependency_v4 { 16 } else { 0 };
                if occurrence.native.flags & !admitted != 0 {
                    return Err(Error::Unsupported("native index packet flag projection"));
                }
                let size = occurrence
                    .source
                    .range
                    .end
                    .checked_sub(occurrence.source.range.start)
                    .ok_or(Error::Overflow)?;
                // A conservative positive signed30-bit view. Source size stays
                // u64 and is never truncated to fit a native bitfield.
                if size > 0x1fff_ffff {
                    return Err(Error::Unsupported(
                        "native index signed size representation",
                    ));
                }
                let key = occurrence.native.flags & 1 != 0;
                let run = source_run.map(|(fragment, run, _)| (fragment, run));
                // Actual v36 C34 controls reset the distance at the first
                // entry of a following traf, including non-key/discard rows
                // after rollback or an explicit native predecessor origin.
                // Source ordinal continuity and forward supplied-index order
                // identify this boundary without inventing source timestamps.
                let starts_fragment = match (self.run, self.carry, source_run) {
                    (
                        Some((previous_fragment, _)),
                        Some(previous),
                        Some((fragment, current_run, flags)),
                    ) => {
                        fragment > previous_fragment
                            && current_run == 0
                            && previous.source_ordinal.checked_add(1)
                                == Some(occurrence.source.ordinal)
                            && previous.flags & 0x400 != 0
                            && flags & 0x400 != 0
                    }
                    _ => false,
                };
                // Actual native27 f-multitrack has consecutive [1,2] truns
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
                            && occurrence.native.flags & 4 == 0
                            && previous.clocks_continue(&occurrence)
                    }
                    _ => false,
                };
                if !key
                    && ((self.next == 0 && !self.recovery_initial_nonkey)
                        || (run.is_some() && run != self.run && !continued_run && !starts_fragment))
                {
                    return Err(Error::Unsupported(
                        "native index no-key append distance carry",
                    ));
                }
                // Actual C34 partition controls distinguish within-run counting
                // from append initialization: [3] gives 0,1,2; [1,2] gives
                // 0,0,1. Keep the last emitted distance, carry it once at an
                // accepted append boundary, then increment inside that run.
                // Flag presence alone does not introduce a per-sample lag.
                let distance =
                    if key || starts_fragment || (self.next == 0 && self.recovery_initial_nonkey) {
                        0
                    } else if run.is_some() && run != self.run {
                        self.distance
                    } else {
                        self.distance.checked_add(1).ok_or(Error::Overflow)?
                    };
                let raw_flag_bits = u8::from(key)
                    | if occurrence.native.flags & 4 != 0 {
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
                    native_dts: occurrence.native.dts,
                    native_exact_dts: occurrence.native.dts_field.exact_native_ticks,
                });
                let entry = Mov9NativeIndexEntry {
                    ordinal: self.next,
                    track_id: occurrence.track_id,
                    position: i64::try_from(occurrence.source.range.start)
                        .map_err(|_| Error::Unsupported("native index signed position"))?,
                    timestamp: occurrence.native.dts_field.signed_ticks(),
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
                Ok(Mov9CursorStatus::Ready)
            }
        }
    }
    pub fn take_entry(&mut self) -> Result<Mov9NativeIndexEntry> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        self.ready
            .take()
            .ok_or(Error::Invalid("no ready native index entry"))
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Mov9NativeSeekLimits {
    pub index: Mov9NativeIndexLimits,
    pub max_replayed_occurrences: u64,
}
impl Default for Mov9NativeSeekLimits {
    fn default() -> Self {
        Self {
            index: Mov9NativeIndexLimits::default(),
            max_replayed_occurrences: 1_000_000,
        }
    }
}
#[derive(Clone, Debug)]
pub struct Mov9NativeSeekReceipt {
    pub requested_timestamp: i64,
    pub selected: Mov9NativeIndexEntry,
    pub scanned_entries: u64,
    pub used_first_fallback: bool,
}
#[derive(Clone)]
struct Context<'a> {
    pool: Budget,
    track: Mov9Track<'a>,
    media: &'a [Range<u64>],
    limits: Mov9NativeSeekLimits,
    profile: Mov9Profile,
    encoded_edits: Option<EncodedEdits<'a>>,
}
type EncodedEdits<'a> = &'a [Option<crate::Mov9EncodedEditPlanV2>];
/// Native signed ticks are a separately named API argument. No rounding,
/// source-time wrap or conversion through SourceTime is performed here.
pub struct Mov9NativeSeekJob<'a> {
    context: Context<'a>,
    index: Mov9NativeIndexCursor<'a>,
    timestamp: i64,
    first: Option<Mov9NativeIndexEntry>,
    best: Option<Mov9NativeIndexEntry>,
    previous: Option<i64>,
    unordered_minimum: Option<i64>,
    scanned: u64,
    ready: bool,
    failed: Option<Error>,
}
pub struct Mov9NativeSeekPlan<'a> {
    context: Context<'a>,
    receipt: Mov9NativeSeekReceipt,
}
/// Resumes the real native occurrence state by a bounded metadata replay from
/// original tables. Skipped metadata causes no payload reads or caller output.
/// This is a production cursor operation, not comparator filtering or PTS sort.
pub struct Mov9NativeSeekCursor<'a> {
    cursor: Mov9PacketCursor<'a>,
    selected: Mov9NativeIndexEntry,
    next: u64,
    maximum: u64,
    reached: bool,
    ended: bool,
    ready: Option<Mov9PacketOccurrence>,
    failed: Option<Error>,
}
impl<'a> Mov9NativeSeekJob<'a> {
    pub fn new_complete_source(
        pool: &Budget,
        track: Mov9Track<'a>,
        media: &'a [Range<u64>],
        timestamp: i64,
        limits: Mov9NativeSeekLimits,
        profile: Mov9Profile,
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
            Mov9NativeIndexCursor::new_encoded_regular_v2(
                &context.pool,
                context.track,
                context.media,
                context.limits.index,
                plans,
                crate::PinnedMovD32EncodedEditsV2,
            )?
        } else {
            Mov9NativeIndexCursor::new_complete_source(
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
            first: None,
            best: None,
            previous: None,
            unordered_minimum: None,
            scanned: 0,
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
                Mov9CursorStatus::Yield => return Ok(BuildStatus::Yield),
                Mov9CursorStatus::Ready => {
                    if !work.try_charge(1) {
                        return Ok(BuildStatus::Yield);
                    }
                    let entry = self.index.take_entry()?;
                    if entry.timestamp == i64::MIN {
                        return Err(Error::Unsupported("native seek null index clock"));
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
                        if entry.timestamp <= self.timestamp {
                            self.best = Some(entry);
                        }
                    }
                }
                Mov9CursorStatus::End => {
                    if !work.try_charge(1) {
                        return Ok(BuildStatus::Yield);
                    }
                    // The observed repeated-edit prefix is separated from all
                    // later rollback entries by this query. Overlapping/late
                    // searches stay refused until the native search is established.
                    if self
                        .unordered_minimum
                        .is_some_and(|minimum| self.timestamp >= minimum)
                    {
                        return Err(Error::Unsupported(
                            "native seek overlapping index clock query",
                        ));
                    }
                    if self.first.is_none() {
                        return Err(Error::Unsupported("native seek no observed key fallback"));
                    }
                    self.ready = true;
                    return Ok(BuildStatus::Ready);
                }
            }
        }
    }
    pub fn finish(mut self) -> Result<Mov9NativeSeekPlan<'a>> {
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
        Ok(Mov9NativeSeekPlan {
            context: self.context,
            receipt: Mov9NativeSeekReceipt {
                requested_timestamp: self.timestamp,
                selected,
                scanned_entries: self.scanned,
                used_first_fallback,
            },
        })
    }
}
impl<'a> Mov9NativeSeekPlan<'a> {
    pub fn receipt(&self) -> &Mov9NativeSeekReceipt {
        &self.receipt
    }
    pub fn cursor(&self) -> Result<Mov9NativeSeekCursor<'a>> {
        let c = &self.context;
        let cursor = if let Some(plans) = c.encoded_edits {
            Mov9PacketCursor::new_encoded_regular_v2(
                &c.pool,
                &[c.track],
                c.media,
                c.limits.index.packets,
                plans,
                crate::PinnedMovD32EncodedEditsV2,
            )?
        } else {
            Mov9PacketCursor::new_complete_source(
                &c.pool,
                &[c.track],
                c.media,
                c.limits.index.packets,
                c.profile,
            )?
        };
        Ok(Mov9NativeSeekCursor {
            cursor,
            selected: self.receipt.selected.clone(),
            next: 0,
            maximum: c.limits.max_replayed_occurrences,
            reached: false,
            ended: false,
            ready: None,
            failed: None,
        })
    }
}
impl Mov9NativeSeekCursor<'_> {
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
            if self.ended {
                return Ok(Mov9CursorStatus::End);
            }
            match self.cursor.poll(work, cancel)? {
                Mov9CursorStatus::Yield => return Ok(Mov9CursorStatus::Yield),
                Mov9CursorStatus::End => {
                    if !self.reached {
                        return Err(Error::Invalid(
                            "native seek selection vanished during replay",
                        ));
                    }
                    self.ended = true;
                    return Ok(Mov9CursorStatus::End);
                }
                Mov9CursorStatus::Ready => {
                    if !work.try_charge(1) {
                        return Ok(Mov9CursorStatus::Yield);
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
                            || row.native != selected.native
                            || row.clock_origin != selected.clock_origin
                        {
                            return Err(Error::Invalid(
                                "native seek replay selection disagreement",
                            ));
                        }
                        self.reached = true;
                    }
                    self.ready = Some(row);
                    return Ok(Mov9CursorStatus::Ready);
                }
            }
        }
    }
    pub fn take_occurrence(&mut self) -> Result<Mov9PacketOccurrence> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        self.ready
            .take()
            .ok_or(Error::Invalid("no ready native seek occurrence"))
    }
}

/// Aligns every stream to the selected reference entry's native clock, not the
/// original request or its physical position. The rescaled profile converts
/// that anchor to each other track's native base, rounding ties away from zero.
pub struct Mov9NativeGlobalSeekJob<'a> {
    pool: Budget,
    tracks: BudgetVec<Mov9Track<'a>>,
    media: &'a [Range<u64>],
    limits: Mov9NativeSeekLimits,
    profile: Mov9Profile,
    encoded_edits: Option<EncodedEdits<'a>>,
    reference_index: usize,
    reference_base: TimeBase,
    reference: Option<Mov9NativeSeekReceipt>,
    anchors: BudgetVec<Mov9ResumePoint>,
    child: Option<Mov9NativeSeekJob<'a>>,
    at: usize,
    scanned: u64,
    ready: bool,
    failed: Option<Error>,
}
pub struct Mov9NativeGlobalSeekPlan<'a> {
    pool: Budget,
    tracks: BudgetVec<Mov9Track<'a>>,
    media: &'a [Range<u64>],
    limits: Mov9NativeSeekLimits,
    profile: Mov9Profile,
    encoded_edits: Option<EncodedEdits<'a>>,
    reference: Mov9NativeSeekReceipt,
    anchors: BudgetVec<Mov9ResumePoint>,
}
enum GlobalSeekMode<'a> {
    SameBase,
    Rescaled,
    Encoded(EncodedEdits<'a>),
}
impl<'a> Mov9NativeGlobalSeekJob<'a> {
    pub fn new_complete_source(
        pool: &Budget,
        tracks: &[Mov9Track<'a>],
        media: &'a [Range<u64>],
        track_id: u32,
        timestamp: i64,
        limits: Mov9NativeSeekLimits,
        profile: Mov9Profile,
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

    /// Admit unequal native timebases using the pinned MOV seek rescale.
    pub fn new_complete_source_rescaled(
        pool: &Budget,
        tracks: &[Mov9Track<'a>],
        media: &'a [Range<u64>],
        track_id: u32,
        timestamp: i64,
        limits: Mov9NativeSeekLimits,
        profile: Mov9Profile,
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
    pub fn new_encoded_regular_v2_rescaled(
        pool: &Budget,
        tracks: &[Mov9Track<'a>],
        media: &'a [Range<u64>],
        reference: (u32, i64),
        limits: Mov9NativeSeekLimits,
        plans: EncodedEdits<'a>,
    ) -> Result<Self> {
        Self::build(
            pool,
            tracks,
            media,
            reference,
            limits,
            Mov9Profile::PinnedNoParseNoFillin,
            GlobalSeekMode::Encoded(plans),
        )
    }

    fn build(
        pool: &Budget,
        tracks: &[Mov9Track<'a>],
        media: &'a [Range<u64>],
        request: (u32, i64),
        limits: Mov9NativeSeekLimits,
        profile: Mov9Profile,
        mode: GlobalSeekMode<'a>,
    ) -> Result<Self> {
        let (track_id, timestamp) = request;
        let encoded_edits = match mode {
            GlobalSeekMode::Encoded(plans) if plans.len() == tracks.len() => Some(plans),
            GlobalSeekMode::Encoded(_) => {
                return Err(Error::Invalid("encoded edit track coverage"));
            }
            _ => None,
        };
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
            .ok_or(Error::Invalid("native seek reference track absent"))?;
        let base = Mov9MediaClock::from_source(tracks[reference_index].media_base)?.native_base;
        let mut owned = BudgetVec::new(pool);
        owned.try_reserve_exact(tracks.len())?;
        let mut anchors = BudgetVec::new(pool);
        anchors.try_reserve_exact(tracks.len())?;
        for (i, track) in tracks.iter().enumerate() {
            if tracks[..i].iter().any(|t| t.track_id == track.track_id) {
                return Err(Error::Invalid("duplicate native seek track"));
            }
            let track_base = Mov9MediaClock::from_source(track.media_base)?.native_base;
            if matches!(mode, GlobalSeekMode::SameBase) && track_base != base {
                return Err(Error::Unsupported(
                    "native global seek unequal timebase rescale",
                ));
            }
            owned.try_push(*track).map_err(|e| e.error)?;
            anchors
                .try_push(Mov9ResumePoint {
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
            reference_index,
            reference_base: base,
            reference: None,
            anchors,
            child: None,
            at: 0,
            scanned: 0,
            ready: false,
            failed: None,
        };
        job.child = Some(Mov9NativeSeekJob::build(
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
                let receipt = plan.receipt().clone();
                self.scanned = self
                    .scanned
                    .checked_add(receipt.scanned_entries)
                    .ok_or(Error::Overflow)?;
                bound(self.scanned, self.limits.index.max_entries)?;
                let at = if self.reference.is_none() {
                    self.reference_index
                } else {
                    self.at
                };
                self.anchors.as_mut_slice()[at].occurrence = receipt.selected.ordinal;
                if self.reference.is_none() {
                    self.reference = Some(receipt);
                } else {
                    self.at = self.at.checked_add(1).ok_or(Error::Overflow)?;
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
            if self.at == self.reference_index {
                self.at = self.at.checked_add(1).ok_or(Error::Overflow)?;
                continue;
            }
            let timestamp = self
                .reference
                .as_ref()
                .ok_or(Error::Invalid("native reference selection absent"))?
                .selected
                .timestamp;
            let track = self.tracks.as_slice()[self.at];
            let base = Mov9MediaClock::from_source(track.media_base)?.native_base;
            let timestamp = if self.reference_base == base {
                timestamp
            } else {
                self.reference_base
                    .rescale(timestamp, base, Rounding::NearestAway)?
            };
            self.child = Some(Mov9NativeSeekJob::build(
                self.track_context(self.at),
                timestamp,
            )?);
        }
    }
    pub fn finish(self) -> Result<Mov9NativeGlobalSeekPlan<'a>> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if !self.ready {
            return Err(Error::Invalid("native global seek plan not ready"));
        }
        Ok(Mov9NativeGlobalSeekPlan {
            pool: self.pool,
            tracks: self.tracks,
            media: self.media,
            limits: self.limits,
            profile: self.profile,
            encoded_edits: self.encoded_edits,
            reference: self
                .reference
                .ok_or(Error::Invalid("native reference selection absent"))?,
            anchors: self.anchors,
        })
    }
}
impl<'a> Mov9NativeGlobalSeekPlan<'a> {
    pub fn receipt(&self) -> &Mov9NativeSeekReceipt {
        &self.reference
    }
    pub fn anchors(&self) -> &[Mov9ResumePoint] {
        self.anchors.as_slice()
    }
    pub fn cursor(&self) -> Result<Mov9PacketCursor<'a>> {
        if let Some(plans) = self.encoded_edits {
            return Mov9PacketCursor::new_encoded_regular_v2(
                &self.pool,
                self.tracks.as_slice(),
                self.media,
                self.limits.index.packets,
                plans,
                crate::PinnedMovD32EncodedEditsV2,
            )?
            .with_resume_points(self.anchors.as_slice(), self.limits.index.packets);
        }
        Mov9PacketCursor::new_complete_source_at(
            &self.pool,
            self.tracks.as_slice(),
            self.media,
            self.limits.index.packets,
            self.profile,
            self.anchors.as_slice(),
        )
    }
}

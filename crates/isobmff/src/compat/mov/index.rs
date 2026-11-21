//! The index and backward seek of FFmpeg's MOV demuxer. `IndexCursor` builds a
//! track's index entries as the demuxer exposes them, `SeekJob` and `GlobalSeekJob`
//! select an entry for one track or all of them, and their plans replay the packets
//! from there. Every entry carries its source sample and packet; the walks keep no
//! per-sample array and read no payload.
use crate::compat::mov::cursor::{PlainPrepare, PlainStart, TrackProjection, TrackResume};
use crate::{
    BuildStatus, CursorStatus,
    compat::mov::{
        clocks::MediaClock,
        cursor::{
            ClockOrigin, DemuxProfile, DemuxTrack, PacketCursor, PacketLimits, PacketOccurrence,
            ResumePoint,
        },
    },
    seek::{OpusRecoveryLanding, OpusRecoverySeek},
};
use plain_scan::{PlainChoice, PlainScan, PlainStep, PlainTrack};
use std::{
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};
use triergon_core::{Budget, BudgetVec, Error, Result, Rounding, TimeBase, WorkBudget};

mod plain_scan;

/// Limits of an [`IndexCursor`]; passing one is `Error::Limit`.
#[derive(Clone, Copy, Debug)]
pub struct IndexLimits {
    /// Limits of the packet cursor the index reads.
    pub packets: PacketLimits,
    /// Entries the index may hold.
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
/// One entry of a track's index as FFmpeg's MOV demuxer exposes it, with the packet
/// it was built from.
#[derive(Clone, Debug)]
pub struct IndexEntry {
    /// 0-based position of the entry in the track's index; not the 1-based sample
    /// ordinal.
    pub ordinal: u64,
    /// `track_ID` of the entry's track.
    pub track_id: u32,
    /// Absolute file offset of the packet's bytes.
    pub position: i64,
    /// The packet's decode time as the demuxer's signed field holds it, in ticks of
    /// the track's demux time base.
    pub timestamp: i64,
    /// The flags as the demuxer's two-bit signed field reads them: 1 key, -2
    /// discard, -1 key and discard, 0 neither. These differ from the packet flags
    /// and from the source sample's flags.
    pub flags: i32,
    /// The same two bits unsigned: bit 0 key, bit 1 discard.
    pub raw_flag_bits: u8,
    /// Size of the packet's bytes; below 2^29.
    pub size: u32,
    /// The demuxer's distance in entries from the last key entry; 0 at a key and at
    /// the first entry of a fragment.
    pub min_distance: i32,
    /// The packet the entry was built from.
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
/// The fields of a ready [`IndexEntry`] other than its occurrence, which stays in
/// the packet cursor until the entry is taken.
#[derive(Clone, Copy, Debug)]
struct EntryFields {
    ordinal: u64,
    position: i64,
    timestamp: i64,
    flags: i32,
    raw_flag_bits: u8,
    size: u32,
    min_distance: i32,
}

/// Builds one track's index as FFmpeg's MOV demuxer does, an [`IndexEntry`] per
/// packet in decode order, holding one entry at a time. It reads the track over the
/// complete source, so a fragment-only track without `tfdt` starts at zero.
#[derive(Debug)]
pub struct IndexCursor<'a> {
    cursor: PacketCursor<'a>,
    max_entries: u64,
    next: u64,
    distance: u64,
    run: Option<(usize, usize)>,
    carry: Option<RunCarry>,
    ready: Option<EntryFields>,
    failed: Option<Error>,
    allow_initial_nonkey: bool,
    dependency_table: bool,
}
impl<'a> IndexCursor<'a> {
    fn from_cursor(
        cursor: PacketCursor<'a>,
        max_entries: u64,
        allow_initial_nonkey: bool,
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
            allow_initial_nonkey,
            dependency_table,
        }
    }

    fn starts_before_sync(track: DemuxTrack<'_>) -> bool {
        track.fragments.is_empty()
            && track.edits.is_none()
            && track.table.unused_sync_entries() != 0
    }

    /// The index of `track`, whose samples lie in `media`, built as
    /// [`PacketCursor::new_complete_source`] reads it. A regular track whose `stss`
    /// lists samples past its end may start with a non-key entry.
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
    /// `Error::Invalid` unless the cursor has ended cleanly over a complete,
    /// seekable source and holds the track without an encoded edit.
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
    /// The index of a regular `track` whose edit follows its plan, as
    /// [`PacketCursor::new_encoded_regular`] reads it; `plans` holds the one entry
    /// for this track. An edited track may start with a non-key entry.
    pub fn new_encoded_regular(
        pool: &Budget,
        track: DemuxTrack<'a>,
        media: &'a [Range<u64>],
        limits: IndexLimits,
        plans: &'a [Option<crate::compat::mov::encoded_edits::EncodedEditPlan>],
        profile: crate::compat::mov::encoded_edits::EncodedEditProfile,
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
            plans[0].is_some(),
            plans.iter().flatten().any(|p| p.has_dependency_view()),
        ))
    }
    pub(crate) fn allow_recovery_initial_nonkey(&mut self) {
        self.allow_initial_nonkey = true;
    }
    /// Advances to the next entry; failures are sticky. `Error::Unsupported` for an
    /// entry the demuxer's index would hold in a form not reproduced here: packet
    /// flags other than key, discard and disposable, a size of 2^29 bytes or more,
    /// or a non-key entry whose distance would carry over from another run.
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
                let occurrence = self
                    .cursor
                    .occurrence()
                    .ok_or(Error::Invalid("no ready packet"))?;
                // Public AVIndexEntry has key/discard bits, independently of
                // AVPacket's disposable hint. Preserve that hint on the retained
                // occurrence, while the separately typed index keeps its own ABI.
                let admitted = 5 | if self.dependency_table { 16 } else { 0 };
                if occurrence.demux.flags & !admitted != 0 {
                    return Err(Error::Unsupported("index packet flags"));
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
                    return Err(Error::Unsupported("index signed size"));
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
                            && previous.clocks_continue(occurrence)
                    }
                    _ => false,
                };
                let first_fragment_sample =
                    self.next == 0 && source_run.is_some() && occurrence.source.ordinal == 1;
                let starts_at_zero = key
                    || starts_fragment
                    || first_fragment_sample
                    || (self.next == 0 && self.allow_initial_nonkey);
                if !starts_at_zero
                    && (self.next == 0 || (run.is_some() && run != self.run && !continued_run))
                {
                    return Err(Error::Unsupported("index no-key append distance carry"));
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
                    _ => return Err(Error::Invalid("index flags")),
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
                let entry = EntryFields {
                    ordinal: self.next,
                    position: i64::try_from(occurrence.source.range.start)
                        .map_err(|_| Error::Unsupported("index signed position"))?,
                    timestamp: occurrence.demux.dts_field.signed_ticks(),
                    flags,
                    raw_flag_bits,
                    size: u32::try_from(size).map_err(|_| Error::Overflow)?,
                    min_distance: i32::try_from(distance)
                        .map_err(|_| Error::Unsupported("index signed distance"))?,
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
    /// Takes the ready entry; `Error::Invalid` when none is ready.
    pub fn take_entry(&mut self) -> Result<IndexEntry> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let fields = self
            .ready
            .take()
            .ok_or(Error::Invalid("no ready index entry"))?;
        let occurrence = self.cursor.take_occurrence()?;
        Ok(IndexEntry {
            ordinal: fields.ordinal,
            track_id: occurrence.track_id,
            position: fields.position,
            timestamp: fields.timestamp,
            flags: fields.flags,
            raw_flag_bits: fields.raw_flag_bits,
            size: fields.size,
            min_distance: fields.min_distance,
            occurrence,
        })
    }

    /// The ready entry, before it is taken or skipped.
    fn ready_entry(&self) -> Result<(EntryFields, &PacketOccurrence)> {
        let fields = self.ready.ok_or(Error::Invalid("no ready index entry"))?;
        let occurrence = self
            .cursor
            .occurrence()
            .ok_or(Error::Invalid("no ready packet"))?;
        Ok((fields, occurrence))
    }

    /// Drop the ready entry without moving its occurrence.
    fn skip_entry(&mut self) {
        self.ready = None;
        self.cursor.discard_occurrence();
    }

    /// Prepares the index and, before its first entry, finds whether a seek to
    /// `query` can read its entries from the track's tables: still `Untried`
    /// when the work is spent.
    fn prepare_plain(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        query: i128,
        allow_initial_prefix: bool,
    ) -> Result<Plain<'a>> {
        if self.next != 0 || self.ready.is_some() || self.dependency_table {
            return Ok(Plain::Off);
        }
        let PlainPrepare::Ready(source) = self.cursor.prepare_plain(work, cancel)? else {
            return Ok(Plain::Untried);
        };
        Ok(source.map_or(Plain::Off, |source| {
            Plain::Scanning(PlainScan::new(PlainTrack {
                source,
                allow_initial_nonkey: self.allow_initial_nonkey,
                allow_initial_prefix,
                max_entries: self.max_entries,
                // As the first entry does, take a fragment edit's media start
                // from the query.
                target: query - i128::from(self.cursor.index_edit_offset()),
            }))
        }))
    }

    /// Continue a prepared plain index from `start`, whose sample is entry
    /// `ordinal`. `false` when the work is spent.
    fn start_plain_at(
        &mut self,
        start: PlainStart<'a>,
        ordinal: u64,
        work: &mut WorkBudget,
    ) -> Result<bool> {
        self.next = ordinal;
        self.cursor.start_plain_at(start, work)
    }
}

/// Limits of a seek; passing one is `Error::Limit`.
#[derive(Clone, Copy, Debug)]
pub struct SeekLimits {
    /// Limits of each track's index; `max_entries` also bounds the entries a global
    /// seek reads across tracks.
    pub index: IndexLimits,
    /// Packets a [`SeekCursor`] may produce, those replayed before the landing
    /// included, and packets a range seek may skip before its landings.
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
/// Where a seek landed on its track.
#[derive(Clone, Debug)]
pub struct SeekLanding {
    /// The requested timestamp, in ticks of the track's demux time base.
    pub requested_timestamp: i64,
    /// The index entry the seek selected.
    pub selected: IndexEntry,
    /// How many of the track's index entries the seek read.
    pub scanned_entries: u64,
    /// `true` when no key entry lies at or before the target, so the seek took the
    /// track's first entry.
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
type EncodedEdits<'a> = &'a [Option<crate::compat::mov::encoded_edits::EncodedEditPlan>];
/// A backward seek on one track as FFmpeg's MOV demuxer does it: reads the track's
/// index and selects the last key entry whose timestamp is at or before the target,
/// or the first key entry when there is none. The target is in ticks of the track's
/// demux time base, as the demuxer's seek takes it, and is not rounded.
#[derive(Debug)]
pub struct SeekJob<'a> {
    context: Context<'a>,
    index: IndexCursor<'a>,
    timestamp: i64,
    index_timestamp: i128,
    first: Option<IndexEntry>,
    best: Option<IndexEntry>,
    /// The track's packet state at `first` and `best`, kept when a global seek
    /// starts its packets there.
    first_resume: Option<TrackResume<'a>>,
    best_resume: Option<TrackResume<'a>>,
    keep_resume: bool,
    plain: Plain<'a>,
    previous: Option<i64>,
    unordered_minimum: Option<i64>,
    scanned: u64,
    allow_initial_prefix: bool,
    ready: bool,
    failed: Option<Error>,
}
/// Where a seek stands with reading a track's entries from its tables.
// One per seek job.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
enum Plain<'a> {
    /// The index has not been prepared yet.
    Untried,
    Scanning(PlainScan<'a>),
    /// The scan chose this entry; the index moves to it.
    Starting(PlainChoice<'a>),
    /// The index produces the chosen entry next.
    Selecting(PlainChoice<'a>),
    /// The seek reads every entry.
    Off,
}
/// A finished one-track seek: its landing and what is needed to replay the track
/// to it.
#[derive(Debug)]
pub struct SeekPlan<'a> {
    context: Context<'a>,
    landing: SeekLanding,
}
/// Yields a track's packets from a seek's landing on. It replays the packets
/// before the landing from the tables, so their clock state carries over, without
/// yielding them or reading any payload.
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
    /// A seek of `track`, whose samples lie in `media`, to `timestamp`, reading
    /// the track as [`IndexCursor::new_complete_source`] does. `Error::Unsupported`
    /// for `i64::MIN`, the demuxer's null value.
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
            return Err(Error::Unsupported("seek timestamp is the null value"));
        }
        let index = if let Some(plans) = context.encoded_edits {
            IndexCursor::new_encoded_regular(
                &context.pool,
                context.track,
                context.media,
                context.limits.index,
                plans,
                crate::compat::mov::encoded_edits::EncodedEditProfile,
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
            first_resume: None,
            best_resume: None,
            keep_resume: false,
            plain: Plain::Untried,
            previous: None,
            unordered_minimum: None,
            scanned: 0,
            allow_initial_prefix: false,
            ready: false,
            failed: None,
        })
    }
    /// Advances the seek; failures are sticky. `Error::Unsupported` where the
    /// demuxer's choice is not reproduced: a null index timestamp, index timestamps
    /// out of order around the target, or no key entry to land on.
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
            if !matches!(self.plain, Plain::Off) && self.poll_plain(work, cancel)? {
                return Ok(BuildStatus::Yield);
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
                    let (fields, occurrence) = self.index.ready_entry()?;
                    let (timestamp, key) = (fields.timestamp, fields.flags & 1 != 0);
                    if timestamp == i64::MIN {
                        return Err(Error::Unsupported("seek over a null index clock"));
                    }
                    // A secondary track can replay its non-sync prefix. The
                    // primary needs a preceding key.
                    let nonkey_first = self.scanned == 0
                        && !key
                        && (occurrence.source.fragment_flags.is_some()
                            || self.context.encoded_edits.is_some());
                    if self.scanned == 0 {
                        // Convert the query to the packet index clock before
                        // searching. Packet and presentation clocks can differ.
                        self.index_timestamp -= i128::from(self.index.cursor.index_edit_offset());
                    }
                    if self.previous.is_some_and(|prior| timestamp <= prior)
                        || self.unordered_minimum.is_some()
                    {
                        self.unordered_minimum = Some(
                            self.unordered_minimum
                                .map_or(timestamp, |v| v.min(timestamp)),
                        );
                    }
                    self.previous = Some(timestamp);
                    self.scanned = self.scanned.checked_add(1).ok_or(Error::Overflow)?;
                    let first = nonkey_first || (key && self.first.is_none());
                    let best = key && i128::from(timestamp) <= self.index_timestamp;
                    if !first && !best {
                        // Only selectable entries leave the index cursor.
                        self.index.skip_entry();
                        continue;
                    }
                    if self.keep_resume {
                        let cursor = &self.index.cursor;
                        if first {
                            cursor.store_track_resume(fields.ordinal, &mut self.first_resume)?;
                        }
                        if best {
                            cursor.store_track_resume(fields.ordinal, &mut self.best_resume)?;
                        }
                    }
                    let entry = self.index.take_entry()?;
                    match (first, best) {
                        (true, true) => {
                            self.first = Some(entry.clone());
                            self.best = Some(entry);
                        }
                        (true, false) => self.first = Some(entry),
                        _ => self.best = Some(entry),
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
                            "seek query between overlapping index clocks",
                        ));
                    }
                    if self.best.is_none()
                        && self
                            .first
                            .as_ref()
                            .is_none_or(|entry| entry.flags & 1 == 0 && !self.allow_initial_prefix)
                    {
                        return Err(Error::Unsupported("seek fallback is not a key packet"));
                    }
                    self.ready = true;
                    return Ok(BuildStatus::Ready);
                }
            }
        }
    }
    /// Seeks a plain regular track from its table where it can. `true` when the
    /// work is spent; otherwise the seek is ready or reads every entry.
    fn poll_plain(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<bool> {
        loop {
            match &mut self.plain {
                Plain::Off => return Ok(false),
                Plain::Untried => {
                    let next = self.index.prepare_plain(
                        work,
                        cancel,
                        self.index_timestamp,
                        self.allow_initial_prefix,
                    )?;
                    if matches!(next, Plain::Untried) {
                        return Ok(true);
                    }
                    self.plain = next;
                }
                Plain::Scanning(scan) => match scan.poll(work) {
                    PlainStep::Yield => return Ok(true),
                    PlainStep::GiveUp => self.plain = Plain::Off,
                    PlainStep::Done(choice) => self.plain = Plain::Starting(choice),
                },
                Plain::Starting(choice) => {
                    let choice = *choice;
                    if !self
                        .index
                        .start_plain_at(choice.start, choice.ordinal, work)?
                    {
                        return Ok(true);
                    }
                    self.plain = Plain::Selecting(choice);
                }
                Plain::Selecting(choice) => {
                    let choice = *choice;
                    match self.index.poll(work, cancel)? {
                        CursorStatus::Yield => return Ok(true),
                        CursorStatus::End => {
                            return Err(Error::Invalid("plain seek selection vanished"));
                        }
                        CursorStatus::Ready => {}
                    }
                    let resume = if choice.first_fallback {
                        &mut self.first_resume
                    } else {
                        &mut self.best_resume
                    };
                    if self.keep_resume {
                        self.index
                            .cursor
                            .store_track_resume(choice.ordinal, resume)?;
                    } else {
                        *resume = None;
                    }
                    let entry = self.index.take_entry()?;
                    if entry.ordinal != choice.ordinal
                        || (entry.flags & 1 == 0 && !choice.first_fallback)
                    {
                        return Err(Error::Invalid("plain seek selection disagreement"));
                    }
                    if choice.first_fallback {
                        self.first = Some(entry);
                    } else {
                        self.best = Some(entry);
                    }
                    self.scanned = choice.scanned;
                    self.plain = Plain::Off;
                    self.ready = true;
                    return Ok(false);
                }
            }
        }
    }
    /// The plan of a ready seek; `Error::Invalid` before it is ready.
    pub fn finish(mut self) -> Result<SeekPlan<'a>> {
        let (landing, _) = self.take_landing()?;
        Ok(SeekPlan {
            context: self.context,
            landing,
        })
    }
    /// The landing of a ready seek and the track's packet state there, when
    /// it was kept. Takes them out of the job, which is spent afterwards.
    fn take_landing(&mut self) -> Result<(SeekLanding, Option<TrackResume<'a>>)> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if !self.ready {
            return Err(Error::Invalid("seek plan not ready"));
        }
        let used_first_fallback = self.best.is_none();
        let selected = self
            .best
            .take()
            .or_else(|| self.first.take())
            .ok_or(Error::Invalid("missing seek selection"))?;
        let resume = if used_first_fallback {
            self.first_resume.take()
        } else {
            self.best_resume.take()
        };
        let landing = SeekLanding {
            requested_timestamp: self.timestamp,
            selected,
            scanned_entries: self.scanned,
            used_first_fallback,
            opus_recovery: None,
        };
        Ok((landing, resume))
    }
}
impl<'a> SeekPlan<'a> {
    /// Where the seek landed.
    pub fn landing(&self) -> &SeekLanding {
        &self.landing
    }
    /// A cursor that replays the track from its start and yields its packets from
    /// the landing on.
    pub fn cursor(&self) -> Result<SeekCursor<'a>> {
        let c = &self.context;
        let cursor = if let Some(plans) = c.encoded_edits {
            PacketCursor::new_encoded_regular(
                &c.pool,
                &[c.track],
                c.media,
                c.limits.index.packets,
                plans,
                crate::compat::mov::encoded_edits::EncodedEditProfile,
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
    /// Advances to the next packet; failures are sticky. `Error::Invalid` when the
    /// replay does not reach a packet equal to the selected entry's.
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
                        return Err(Error::Invalid("seek selection vanished during replay"));
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
                            return Err(Error::Invalid("seek replay selects another entry"));
                        }
                        self.reached = true;
                    }
                    self.ready = Some(row);
                    return Ok(CursorStatus::Ready);
                }
            }
        }
    }
    /// Takes the ready packet; `Error::Invalid` when none is ready.
    pub fn take_occurrence(&mut self) -> Result<PacketOccurrence> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        self.ready
            .take()
            .ok_or(Error::Invalid("no ready seek occurrence"))
    }
}

/// A backward seek on every track as FFmpeg's MOV demuxer does it: seeks the
/// reference track as [`SeekJob`] does, then each other track to the selected
/// entry's timestamp, not the requested one or the entry's file position. The
/// rescaled constructors convert that timestamp to each track's demux time base,
/// rounding to nearest with ties away from zero.
#[derive(Debug)]
pub struct GlobalSeekJob<'a> {
    setup: GlobalSeekSetup<'a>,
    anchors: BudgetVec<ResumePoint>,
    /// Each track's packet state at its anchor, when its seek kept one.
    resumes: BudgetVec<Option<TrackResume<'a>>>,
    scanned: u64,
    stage: GlobalStage<'a>,
}
/// What a global seek works over, settled before it is polled.
#[derive(Debug)]
struct GlobalSeekSetup<'a> {
    pool: Budget,
    tracks: BudgetVec<DemuxTrack<'a>>,
    media: &'a [Range<u64>],
    limits: SeekLimits,
    profile: DemuxProfile,
    encoded_edits: Option<EncodedEdits<'a>>,
    projection: TrackProjection,
    reference_index: usize,
    reference_base: TimeBase,
    opus_track: Option<usize>,
}
/// Where a global seek stands. A track's seek is boxed: it is several kilobytes, and
/// the global job moves whole when it is built, returned and finished.
// Only `Failed` is small; boxing the landing would allocate once more per seek.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
enum GlobalStage<'a> {
    /// Seeking the reference track.
    Reference(Box<SeekJob<'a>>),
    /// The reference track has landed; the other tracks are sought from track `at` on.
    Tracks {
        reference: SeekLanding,
        at: usize,
        step: TrackStep<'a>,
    },
    Failed(Error),
}
/// What a global seek does on track `at` once the reference track has landed.
#[derive(Debug)]
enum TrackStep<'a> {
    /// Starting the track's seek.
    Start,
    Seek(Box<SeekJob<'a>>),
    /// Finding the decoder recovery point of the secondary Opus track.
    OpusRecovery(Box<OpusRecoverySeek<'a>>),
    /// Every track has landed.
    Done,
}
/// A finished global seek: the reference track's landing and where each track
/// resumes.
#[derive(Debug)]
pub struct GlobalSeekPlan<'a> {
    setup: GlobalSeekSetup<'a>,
    reference: SeekLanding,
    anchors: BudgetVec<ResumePoint>,
    /// Every track's packet state at its anchor, when all were kept.
    resumes: Option<BudgetVec<TrackResume<'a>>>,
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
    /// A seek of all `tracks` to `timestamp` on the track with ID `track_id`.
    /// `Error::Unsupported` when the tracks' demux time bases differ (see
    /// [`new_complete_source_rescaled`](Self::new_complete_source_rescaled)) and
    /// for `i64::MIN`; `Error::Invalid` for an absent or repeated track ID.
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

    /// [`new_complete_source`](Self::new_complete_source) for tracks whose demux
    /// time bases may differ.
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

    /// A rescaled seek to `reference`, a track ID and timestamp, over tracks whose
    /// edits follow `plans`, one per track, as
    /// [`PacketCursor::new_encoded_regular`] reads them. Each edited track aligns
    /// the rescaled timestamp to its plan.
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

    /// A rescaled seek of the tracks whose catalogue position passes `include`, as
    /// [`PacketCursor::new_projected`] plays them. `plans`, when given, has one
    /// entry per track in `tracks`, selected or not.
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
            .ok_or(Error::Invalid("seek reference track absent"))?;
        let base = MediaClock::from_source(tracks[reference_index].media_base)?.demux_base;
        let mut owned = BudgetVec::new(pool);
        owned.try_reserve_exact(tracks.len())?;
        let mut anchors = BudgetVec::new(pool);
        anchors.try_reserve_exact(tracks.len())?;
        let mut resumes = BudgetVec::new(pool);
        resumes.try_reserve_exact(tracks.len())?;
        for (i, track) in tracks.iter().enumerate() {
            if tracks[..i].iter().any(|t| t.track_id == track.track_id) {
                return Err(Error::Invalid("duplicate seek track"));
            }
            if projection.includes(i) {
                let track_base = MediaClock::from_source(track.media_base)?.demux_base;
                if matches!(mode, GlobalSeekMode::SameBase) && track_base != base {
                    return Err(Error::Unsupported("global seek across unequal time bases"));
                }
            }
            owned.try_push(*track).map_err(|e| e.error)?;
            anchors
                .try_push(ResumePoint {
                    track_id: track.track_id,
                    occurrence: 0,
                })
                .map_err(|e| e.error)?;
            resumes.try_push(None).map_err(|e| e.error)?;
        }
        let setup = GlobalSeekSetup {
            pool: pool.clone(),
            tracks: owned,
            media,
            limits,
            profile,
            encoded_edits,
            projection,
            reference_index,
            reference_base: base,
            opus_track: None,
        };
        let mut child = Box::new(SeekJob::build(
            setup.track_context(reference_index),
            timestamp,
        )?);
        child.keep_resume = setup.projection.is_all();
        Ok(Self {
            setup,
            anchors,
            resumes,
            scanned: 0,
            stage: GlobalStage::Reference(child),
        })
    }
    /// Use a secondary regular Opus track's source roll groups for decoder
    /// recovery. Select this before the seek lands on the reference track.
    pub fn with_opus_recovery(mut self, track_id: u32) -> Result<Self> {
        let setup = &mut self.setup;
        if !matches!(self.stage, GlobalStage::Reference(_)) || setup.opus_track.is_some() {
            return Err(Error::Invalid(
                "Opus recovery must be selected before the seek",
            ));
        }
        let at = setup
            .tracks
            .as_slice()
            .iter()
            .position(|track| track.track_id == track_id)
            .filter(|&at| at != setup.reference_index && setup.projection.includes(at))
            .ok_or(Error::Invalid("missing secondary Opus track"))?;
        setup.opus_edit(at)?.opus_roll_scope()?;
        setup.opus_track = Some(at);
        Ok(self)
    }
    /// Advances the seek, track by track; failures are sticky. Fails as
    /// [`SeekJob::poll`] does for any track, and with `Error::Limit` when the
    /// entries read across tracks pass the index limit.
    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<BuildStatus> {
        if let GlobalStage::Failed(error) = self.stage {
            return Err(error);
        }
        let result = self.poll_inner(work, cancel);
        if let Err(error) = result {
            self.stage = GlobalStage::Failed(error);
        }
        result
    }
    fn poll_inner(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<BuildStatus> {
        let setup = &self.setup;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            match &mut self.stage {
                GlobalStage::Failed(error) => return Err(*error),
                GlobalStage::Reference(child) => {
                    if child.poll(work, cancel)? == BuildStatus::Yield || !work.try_charge(1) {
                        return Ok(BuildStatus::Yield);
                    }
                    // The landing and the packet state are taken in place: the state
                    // is kilobytes, and an extra move of it costs more than the seek.
                    let (landing, resume) = child.take_landing()?;
                    self.scanned = scanned(self.scanned, &landing, setup.limits)?;
                    let at = setup.reference_index;
                    self.anchors.as_mut_slice()[at].occurrence = landing.selected.ordinal;
                    self.resumes.as_mut_slice()[at] = resume;
                    self.stage = GlobalStage::Tracks {
                        reference: landing,
                        at: 0,
                        step: TrackStep::Start,
                    };
                }
                GlobalStage::Tracks {
                    reference,
                    at,
                    step,
                } => match step {
                    TrackStep::Done => return Ok(BuildStatus::Ready),
                    TrackStep::OpusRecovery(recovery) => {
                        let Some(landing) = recovery.poll(work, cancel)? else {
                            return Ok(BuildStatus::Yield);
                        };
                        self.anchors.as_mut_slice()[*at].occurrence = setup
                            .opus_edit(*at)?
                            .opus_occurrence(landing.first_sample)?;
                        reference.opus_recovery = Some(landing);
                        *at += 1;
                        *step = TrackStep::Start;
                    }
                    TrackStep::Seek(child) => {
                        if child.poll(work, cancel)? == BuildStatus::Yield || !work.try_charge(1) {
                            return Ok(BuildStatus::Yield);
                        }
                        let (landing, resume) = child.take_landing()?;
                        self.scanned = scanned(self.scanned, &landing, setup.limits)?;
                        if setup.opus_track == Some(*at) {
                            let sample = &landing.selected.occurrence.source;
                            let edit = setup.opus_edit(*at)?;
                            if landing.selected.ordinal != edit.opus_occurrence(sample.ordinal)? {
                                return Err(Error::Invalid("Opus seek occurrence order"));
                            }
                            *step = TrackStep::OpusRecovery(OpusRecoverySeek::new(
                                &setup.pool,
                                setup.tracks.as_slice()[*at],
                                edit.opus_roll_scope()?,
                                sample,
                            )?);
                        } else {
                            self.anchors.as_mut_slice()[*at].occurrence = landing.selected.ordinal;
                            self.resumes.as_mut_slice()[*at] = resume;
                            *at = at.checked_add(1).ok_or(Error::Overflow)?;
                            *step = TrackStep::Start;
                        }
                    }
                    TrackStep::Start => {
                        if !work.try_charge(1) {
                            return Ok(BuildStatus::Yield);
                        }
                        if *at == setup.tracks.len() {
                            *step = TrackStep::Done;
                            return Ok(BuildStatus::Ready);
                        }
                        if *at == setup.reference_index || !setup.projection.includes(*at) {
                            *at = at.checked_add(1).ok_or(Error::Overflow)?;
                            continue;
                        }
                        let selected = &reference.selected;
                        let recovering_opus = setup.opus_track == Some(*at);
                        let track = setup.tracks.as_slice()[*at];
                        let base = MediaClock::from_source(track.media_base)?.demux_base;
                        let timestamp = if recovering_opus {
                            let pts = selected
                                .occurrence
                                .demux
                                .pts
                                .ok_or(Error::Invalid("missing video landing PTS"))?;
                            setup.opus_edit(*at)?.opus_seek_timestamp(
                                pts,
                                setup.reference_base,
                                base,
                            )?
                        } else {
                            let timestamp = setup.reference_base.rescale(
                                selected.timestamp,
                                base,
                                Rounding::NearestAway,
                            )?;
                            match setup.encoded_edits.and_then(|plans| plans[*at].as_ref()) {
                                Some(edit) => edit.align_seek_timestamp(timestamp, base)?,
                                None => timestamp,
                            }
                        };
                        // If a secondary track has no preceding sync sample, replay
                        // its verified non-sync prefix from the beginning.
                        let mut child =
                            Box::new(SeekJob::build(setup.track_context(*at), timestamp)?);
                        child.allow_initial_prefix = true;
                        child.keep_resume = setup.projection.is_all() && !recovering_opus;
                        *step = TrackStep::Seek(child);
                    }
                },
            }
        }
    }
    /// The plan of a ready seek; `Error::Invalid` before it is ready.
    pub fn finish(self) -> Result<GlobalSeekPlan<'a>> {
        let reference = match self.stage {
            GlobalStage::Tracks {
                reference,
                step: TrackStep::Done,
                ..
            } => reference,
            GlobalStage::Failed(error) => return Err(error),
            _ => return Err(Error::Invalid("global seek plan not ready")),
        };
        let setup = self.setup;
        let mut anchors = self.anchors;
        let mut resumes = self.resumes;
        setup.projection.retain(&mut anchors);
        let resumes = if resumes.as_slice().iter().all(Option::is_some) {
            let mut kept = BudgetVec::new(&setup.pool);
            kept.try_reserve_exact(resumes.len())?;
            for resume in resumes.as_mut_slice() {
                kept.try_push(resume.take().ok_or(Error::Invalid("seek resume"))?)
                    .map_err(|e| e.error)?;
            }
            Some(kept)
        } else {
            None
        };
        Ok(GlobalSeekPlan {
            setup,
            reference,
            anchors,
            resumes,
        })
    }
}
/// `scanned` with the entries `landing`'s seek read added; `Error::Limit` past the index
/// limit.
fn scanned(scanned: u64, landing: &SeekLanding, limits: SeekLimits) -> Result<u64> {
    let scanned = scanned
        .checked_add(landing.scanned_entries)
        .ok_or(Error::Overflow)?;
    bound(scanned, limits.index.max_entries)?;
    Ok(scanned)
}
impl<'a> GlobalSeekSetup<'a> {
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
    fn opus_edit(
        &self,
        at: usize,
    ) -> Result<&'a crate::compat::mov::encoded_edits::EncodedEditPlan> {
        self.encoded_edits
            .and_then(|plans| plans[at].as_ref())
            .ok_or(Error::Unsupported("Opus recovery needs an Opus edit"))
    }
}
impl<'a> GlobalSeekPlan<'a> {
    /// Where the reference track landed.
    pub fn landing(&self) -> &SeekLanding {
        &self.reference
    }
    /// Where each yielded track resumes, in catalogue order.
    pub fn anchors(&self) -> &[ResumePoint] {
        self.anchors.as_slice()
    }
    /// A packet cursor over the tracks that starts each at its anchor, from the
    /// packet state its seek kept where it can and by replay otherwise.
    pub fn cursor(&self) -> Result<PacketCursor<'a>> {
        // Start where each track's seek left its packet state, as a replay from
        // the beginning would reach it.
        if let Some(resumes) = &self.resumes {
            return PacketCursor::from_track_resumes(
                &self.setup.pool,
                resumes.as_slice(),
                self.setup.media,
                self.setup.limits.index.packets,
            );
        }
        let cursor = if !self.setup.projection.is_all() {
            PacketCursor::new_projected(
                &self.setup.pool,
                self.setup.tracks.as_slice(),
                self.setup.media,
                self.setup.limits.index.packets,
                self.setup.encoded_edits,
                |index| self.setup.projection.includes(index),
            )?
        } else if let Some(plans) = self.setup.encoded_edits {
            PacketCursor::new_encoded_regular(
                &self.setup.pool,
                self.setup.tracks.as_slice(),
                self.setup.media,
                self.setup.limits.index.packets,
                plans,
                crate::compat::mov::encoded_edits::EncodedEditProfile,
            )?
        } else {
            PacketCursor::new_complete_source(
                &self.setup.pool,
                self.setup.tracks.as_slice(),
                self.setup.media,
                self.setup.limits.index.packets,
                self.setup.profile,
            )?
        };
        cursor.with_resume_points(self.anchors.as_slice(), self.setup.limits.index.packets)
    }
}

//! Sparse restart states for tracks that passed the demux range scan.
use super::{PacketCursor, Phase, Slot, count};
use crate::{
    BuildStatus, CursorStatus, IndexEntry, SchedulingClock, SeekLanding, SeekLimits,
    opus_seek::OpusRecoverySeek,
};
use std::{
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use triergon_core::{Budget, BudgetVec, Error, Reservation, Result, Rounding, WorkBudget};

const CHECKPOINT_INTERVAL: u64 = 8192;
const SHORT_CHECKPOINT_INTERVAL: u64 = 32;
const MAX_SHORT_CHECKPOINTS_PER_TRACK: usize = 4;
const MAX_CHECKPOINTS: usize = 256;

#[derive(Clone, Debug)]
struct Checkpoint<'a> {
    ordinal: u64,
    timestamp: i64,
    previous_sync: u64,
    slot: Slot<'a>,
}

#[derive(Debug)]
struct Track<'a> {
    points: BudgetVec<Checkpoint<'a>>,
    previous: Option<i64>,
    previous_sync: u64,
    entries: u64,
}

#[derive(Debug)]
pub(super) struct Checkpoints<'a> {
    tracks: BudgetVec<Track<'a>>,
    total: u64,
    stored: usize,
    use_spare_capacity: bool,
    _charge: Reservation,
}

impl<'a> Checkpoints<'a> {
    fn new(budget: &Budget, tracks: &[Slot<'a>]) -> Result<Self> {
        let use_spare_capacity = tracks.iter().all(|slot| {
            slot.track.table.sample_count() <= CHECKPOINT_INTERVAL
                && (slot.track.edits.is_none() || slot.encoded_edit.is_some())
        });
        let charge = budget.reserve(size_of::<Self>() + 2 * size_of::<usize>())?;
        let mut slots = BudgetVec::new(budget);
        slots.try_reserve_exact(tracks.len())?;
        for _ in tracks {
            slots
                .try_push(Track {
                    points: BudgetVec::new(budget),
                    previous: None,
                    previous_sync: 0,
                    entries: 0,
                })
                .map_err(|e| e.error)?;
        }
        Ok(Self {
            tracks: slots,
            total: 0,
            stored: 0,
            use_spare_capacity,
            _charge: charge,
        })
    }

    fn record(&mut self, at: usize, slot: &Slot<'a>) -> Option<()> {
        let track = &mut self.tracks.as_mut_slice()[at];
        let row = slot.head.as_ref()?;
        let timestamp = row.demux.dts?;
        let flags = row.demux.flags;
        let key = flags & 1 != 0;
        let admitted = 5 | if slot
            .encoded_edit
            .is_some_and(|edit| edit.has_dependency_view())
        {
            16
        } else {
            0
        };
        // A fragment run beginning at a key needs no distance from a
        // preceding run. Let the full index handle other append boundaries.
        if track.previous.is_some_and(|before| timestamp <= before)
            || (!key && (track.entries == 0 || slot.starts_fragment_run))
            || flags & !admitted != 0
            || row.source.range.start > i64::MAX as u64
            || row.source.range.end.checked_sub(row.source.range.start)? > 0x1fff_ffff
        {
            return None;
        }
        if key {
            track.previous_sync = track.entries;
        }
        // Short regular tracks can use their existing checkpoint capacity
        // for earlier restarts without growing the index.
        let spare = self.use_spare_capacity
            && track.entries.is_multiple_of(SHORT_CHECKPOINT_INTERVAL)
            && track.points.len() < track.points.capacity().min(MAX_SHORT_CHECKPOINTS_PER_TRACK);
        if track.entries.is_multiple_of(CHECKPOINT_INTERVAL) || spare {
            if self.stored == MAX_CHECKPOINTS {
                return None;
            }
            if slot.saw_fragment && track.points.is_empty() {
                track.points.try_reserve_exact(1).ok()?;
            }
            track
                .points
                .try_push(Checkpoint {
                    ordinal: track.entries,
                    timestamp,
                    previous_sync: track.previous_sync,
                    slot: slot.clone(),
                })
                .ok()?;
            self.stored += 1;
        }
        track.previous = Some(timestamp);
        track.entries += 1;
        self.total += 1;
        Some(())
    }
}

impl<'a> PacketCursor<'a> {
    /// Collect optional seek restarts as the complete range is checked.
    /// The cursor must be exhausted and rewound before using the restarts.
    pub fn enable_range_seek_checkpoints(&mut self) {
        if matches!(self.phase, Phase::Media) && self.complete_source && !self.front_source {
            if self.projection.is_all() {
                self.initialize_range_seek_checkpoints();
            } else {
                // Allocate checkpoint slots after validating and projecting the tracks.
                self.recording_seek_checkpoints = true;
            }
        }
    }

    pub(super) fn initialize_range_seek_checkpoints(&mut self) {
        self.recording_seek_checkpoints = false;
        if self.limits.max_occurrences <= i32::MAX as u64
            && self.slots.as_slice().iter().all(|slot| {
                slot.resume_remaining == 0
                    && (slot.track.fragments.is_empty()
                        || (slot.track.table.sample_count() == 0 && slot.track.edits.is_some()))
            })
        {
            self.seek_checkpoints = Checkpoints::new(self.slots.budget(), self.slots.as_slice())
                .ok()
                .map(Arc::new);
            self.recording_seek_checkpoints = self.seek_checkpoints.is_some();
        }
    }

    /// Release optional seek state without moving the active packet cursor.
    pub fn discard_seek_checkpoints(&mut self) -> bool {
        self.recording_seek_checkpoints = false;
        self.seek_checkpoints.take().is_some()
    }

    pub(super) fn record_seek_checkpoint(&mut self, at: usize) {
        if self
            .seek_checkpoints
            .as_mut()
            .and_then(Arc::get_mut)
            .and_then(|index| index.record(at, &self.slots.as_slice()[at]))
            .is_none()
        {
            self.seek_checkpoints = None;
            self.recording_seek_checkpoints = false;
        }
    }

    /// Plan a seek using source-bound restarts if they cover this request.
    pub fn prepare_checked_range_seek(
        &self,
        video: u32,
        timestamp: i64,
        limits: SeekLimits,
    ) -> Result<Option<RangeSeekJob<'a>>> {
        let Some(index) = &self.seek_checkpoints else {
            return Ok(None);
        };
        let packets = limits.index.packets;
        if self.recording_seek_checkpoints
            || self.failed.is_some()
            || index.total > limits.index.max_entries
            || index.total > packets.max_occurrences
            || self.validated > packets.max_source_samples
            || self.source_tracks > packets.max_tracks
            || self.media.len() > packets.max_media_extents
            || self.limits.edit.max_occurrences > packets.edit.max_occurrences
            || self.limits.edit.max_scanned_samples > packets.edit.max_scanned_samples
            || index
                .tracks
                .as_slice()
                .iter()
                .any(|track| track.points.is_empty())
        {
            return Ok(None);
        }
        if timestamp == i64::MIN {
            return Err(Error::Unsupported("native seek null timestamp argument"));
        }
        let reference = self
            .slots
            .as_slice()
            .iter()
            .position(|slot| slot.track.track_id == video)
            .ok_or(Error::Invalid("native seek reference track absent"))?;
        let mut slots = BudgetVec::new(self.slots.budget());
        slots.try_reserve_exact(self.slots.len())?;
        Ok(Some(RangeSeekJob {
            index: Arc::clone(index),
            slots,
            media: self.media,
            packet_limits: self.limits,
            source_tracks: self.source_tracks,
            validated: self.validated,
            reference,
            timestamp,
            landing: None,
            at: 0,
            scan: None,
            opus_track: None,
            opus_recovery: None,
            skipped: 0,
            maximum_skipped: limits.max_replayed_occurrences,
            failed: None,
        }))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Search {
    Forward,
    PreviousSync,
    Ready,
}

#[derive(Debug)]
struct TrackSeek<'a> {
    target: i64,
    next: u64,
    best: Option<Checkpoint<'a>>,
    cursor: Slot<'a>,
    previous_sync: u64,
    fallback: bool,
    search: Search,
}

impl<'a> TrackSeek<'a> {
    fn new(points: &[Checkpoint<'a>], target: i64) -> Self {
        let at = points.partition_point(|point| point.timestamp <= target);
        let point = &points[at.saturating_sub(1)];
        Self {
            target,
            next: point.ordinal,
            best: (at == 0).then(|| point.clone()),
            cursor: point.slot.clone(),
            previous_sync: point.previous_sync,
            fallback: at == 0,
            search: if at == 0 {
                Search::Ready
            } else {
                Search::Forward
            },
        }
    }

    fn finish_forward(&mut self, points: &[Checkpoint<'a>]) {
        if self.best.is_some() {
            self.search = Search::Ready;
        } else {
            // The target falls within a long non-key interval. The recorded
            // preceding sync may be in an earlier interval; restart there.
            self.restart_at(points, self.previous_sync);
        }
    }

    fn restart_at(&mut self, points: &[Checkpoint<'a>], ordinal: u64) {
        let at = points.partition_point(|point| point.ordinal <= ordinal);
        let point = &points[at - 1];
        self.cursor = point.slot.clone();
        self.next = point.ordinal;
        self.previous_sync = ordinal;
        self.best = None;
        self.search = Search::PreviousSync;
    }

    fn poll(
        &mut self,
        points: &[Checkpoint<'a>],
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        while self.search != Search::Ready {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if self.cursor.head.is_none() {
                match self.cursor.fill(work, true, false, false)? {
                    CursorStatus::Yield => return Ok(false),
                    CursorStatus::End if self.search == Search::Forward => {
                        self.finish_forward(points);
                        continue;
                    }
                    CursorStatus::End => return Err(Error::Invalid("checked seek lost sync")),
                    CursorStatus::Ready => {}
                }
            }
            if !work.try_charge(1) {
                return Ok(false);
            }
            let head = self
                .cursor
                .head
                .as_ref()
                .ok_or(Error::Invalid("missing seek head"))?;
            let timestamp = head
                .demux
                .dts
                .ok_or(Error::Invalid("checked seek clock missing"))?;
            let key = head.demux.flags & 1 != 0;
            let previous_sync =
                self.search == Search::PreviousSync && self.next == self.previous_sync;
            if self.search == Search::Forward && timestamp > self.target {
                self.finish_forward(points);
                continue;
            }
            if previous_sync && !key {
                return Err(Error::Invalid("checked seek lost sync"));
            }
            if previous_sync || (self.search == Search::Forward && key) {
                self.best = Some(Checkpoint {
                    ordinal: self.next,
                    timestamp,
                    previous_sync: self.next,
                    slot: self.cursor.clone(),
                });
                if previous_sync || timestamp == self.target {
                    self.search = Search::Ready;
                    continue;
                }
            }
            self.next += 1;
            self.cursor.head = None;
        }
        Ok(true)
    }
}

/// Source-bound seek over the sparse track restarts from range open.
#[derive(Debug)]
pub struct RangeSeekJob<'a> {
    index: Arc<Checkpoints<'a>>,
    slots: BudgetVec<Slot<'a>>,
    media: &'a [Range<u64>],
    packet_limits: super::PacketLimits,
    source_tracks: usize,
    validated: u64,
    reference: usize,
    timestamp: i64,
    landing: Option<SeekLanding>,
    at: usize,
    scan: Option<TrackSeek<'a>>,
    opus_track: Option<usize>,
    opus_recovery: Option<Box<OpusRecoverySeek<'a>>>,
    skipped: u64,
    maximum_skipped: u64,
    failed: Option<Error>,
}

impl<'a> RangeSeekJob<'a> {
    /// Use a secondary regular Opus track's source roll groups for decoder
    /// recovery. Select this before polling the seek.
    pub fn with_opus_recovery(mut self, track_id: u32) -> Result<Self> {
        if self.scan.is_some()
            || self.landing.is_some()
            || self.opus_track.is_some()
            || self.failed.is_some()
        {
            return Err(Error::Invalid(
                "Opus recovery must be selected before the seek",
            ));
        }
        let at = self
            .index
            .tracks
            .as_slice()
            .iter()
            .position(|track| track.points.as_slice()[0].slot.track.track_id == track_id)
            .filter(|&at| at != self.reference)
            .ok_or(Error::Invalid("missing secondary Opus track"))?;
        self.index.tracks.as_slice()[at].points.as_slice()[0]
            .slot
            .encoded_edit
            .ok_or(Error::Unsupported("Opus recovery needs an Opus edit"))?
            .opus_roll_scope()?;
        self.opus_track = Some(at);
        Ok(self)
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
            if self.at == self.index.tracks.len() {
                return Ok(BuildStatus::Ready);
            }
            let at = if self.landing.is_none() {
                self.reference
            } else {
                self.at
            };
            let points = self.index.tracks.as_slice()[at].points.as_slice();
            if let Some(recovery) = &mut self.opus_recovery {
                let Some(landing) = recovery.poll(work, cancel)? else {
                    return Ok(BuildStatus::Yield);
                };
                let edit = points[0]
                    .slot
                    .encoded_edit
                    .ok_or(Error::Invalid("missing Opus seek edit"))?;
                self.scan
                    .as_mut()
                    .expect("Opus recovery follows the audio search")
                    .restart_at(points, edit.opus_occurrence(landing.first_sample)?);
                self.landing
                    .as_mut()
                    .expect("Opus recovery follows the reference seek")
                    .opus_recovery = Some(landing);
                self.opus_recovery = None;
                self.opus_track = None;
                continue;
            }
            if let Some(scan) = self.scan.as_mut() {
                if !scan.poll(points, work, cancel)? || !work.try_charge(1) {
                    return Ok(BuildStatus::Yield);
                }
                if self.opus_track == Some(at) {
                    let best = scan
                        .best
                        .as_ref()
                        .ok_or(Error::Invalid("missing Opus seek sample"))?;
                    let sample = &best
                        .slot
                        .head
                        .as_ref()
                        .ok_or(Error::Invalid("missing Opus seek sample"))?
                        .source;
                    let edit = best
                        .slot
                        .encoded_edit
                        .ok_or(Error::Unsupported("Opus recovery needs an Opus edit"))?;
                    if best.ordinal != edit.opus_occurrence(sample.ordinal)? {
                        return Err(Error::Invalid("Opus seek occurrence order"));
                    }
                    self.opus_recovery = Some(OpusRecoverySeek::new(
                        self.slots.budget(),
                        best.slot.track,
                        edit.opus_roll_scope()?,
                        sample,
                    )?);
                    continue;
                }
                let scan = self.scan.take().expect("polled track seek");
                self.finish_track(at, scan)?;
                continue;
            }
            if !work.try_charge(1) {
                return Ok(BuildStatus::Yield);
            }
            let target = if let Some(landing) = &self.landing {
                if at == self.reference {
                    self.at += 1;
                    continue;
                }
                let reference_base = self.slots.as_slice()[0].clock()?.demux_base;
                let target_base = points[0].slot.clock()?.demux_base;
                if self.opus_track == Some(at) {
                    let pts = landing
                        .selected
                        .occurrence
                        .demux
                        .pts
                        .ok_or(Error::Invalid("missing video landing PTS"))?;
                    let edit = points[0]
                        .slot
                        .encoded_edit
                        .ok_or(Error::Invalid("missing Opus seek edit"))?;
                    edit.opus_seek_timestamp(pts, reference_base, target_base)?
                } else {
                    let timestamp = reference_base.rescale(
                        landing.selected.timestamp,
                        target_base,
                        Rounding::NearestAway,
                    )?;
                    if let Some(edit) = points[0].slot.encoded_edit {
                        edit.align_seek_timestamp(timestamp, target_base)?
                    } else {
                        timestamp
                    }
                }
            } else {
                self.timestamp
            };
            if target == i64::MIN {
                return Err(Error::Unsupported("native seek null timestamp argument"));
            }
            // A query below the signed index range still lands at its first
            // key. Every checkpoint has a defined DTS above the null value.
            let target = target.saturating_sub(points[0].slot.fragment_media_start.unwrap_or(0));
            self.scan = Some(TrackSeek::new(points, target));
        }
    }

    fn finish_track(&mut self, at: usize, scan: TrackSeek<'a>) -> Result<()> {
        let Checkpoint {
            ordinal,
            timestamp,
            mut slot,
            ..
        } = scan
            .best
            .ok_or(Error::Invalid("missing checked seek key"))?;
        let head = slot
            .head
            .as_mut()
            .ok_or(Error::Invalid("missing checked seek head"))?;
        if self.landing.is_none() {
            let mut occurrence = head.clone();
            occurrence.scheduling_clock = SchedulingClock::SingleTrack;
            let discarded = occurrence.demux.flags & 4 != 0;
            let range = &occurrence.source.range;
            self.landing = Some(SeekLanding {
                requested_timestamp: self.timestamp,
                selected: IndexEntry {
                    ordinal,
                    track_id: occurrence.track_id,
                    position: range.start as i64,
                    timestamp,
                    flags: if discarded { -1 } else { 1 },
                    raw_flag_bits: if discarded { 3 } else { 1 },
                    size: (range.end - range.start) as u32,
                    min_distance: 0,
                    occurrence,
                },
                scanned_entries: self.index.tracks.as_slice()[at].entries,
                used_first_fallback: scan.fallback,
                opus_recovery: None,
            });
        } else {
            self.at += 1;
        }
        head.scheduling_clock = if self.index.tracks.len() > 1 {
            SchedulingClock::from_demux_clock(head.demux.dts_field, head.media_clock.demux_base)?
        } else {
            SchedulingClock::SingleTrack
        };
        self.slots.try_push(slot).map_err(|failure| failure.error)?;
        self.skipped = self.skipped.checked_add(ordinal).ok_or(Error::Overflow)?;
        Ok(())
    }

    pub fn finish(self) -> Result<(PacketCursor<'a>, SeekLanding)> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if self.at != self.index.tracks.len() || self.slots.len() != self.index.tracks.len() {
            return Err(Error::Invalid("checked range seek still pending"));
        }
        count(self.skipped, self.maximum_skipped)?;
        // The reference was searched first. Restore catalogue order before merging.
        let mut slots = self.slots;
        slots.as_mut_slice()[..=self.reference].rotate_left(1);
        let mut cursor = PacketCursor::from_slots(slots, self.media, self.packet_limits);
        cursor.source_tracks = self.source_tracks;
        cursor.complete_source = true;
        cursor.phase = Phase::Fill;
        cursor.validated = self.validated;
        cursor.generated = self.skipped;
        cursor.seek_checkpoints = Some(self.index);
        Ok((
            cursor,
            self.landing
                .ok_or(Error::Invalid("missing checked seek receipt"))?,
        ))
    }
}

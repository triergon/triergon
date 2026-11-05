//! Sparse restart states for regular tracks that passed the native range scan.
use super::{Mov9PacketCursor, Phase, Slot, count};
use crate::{
    BuildStatus, Mov9CursorStatus, Mov9NativeIndexEntry, Mov9NativeSeekLimits,
    Mov9NativeSeekReceipt, Mov9SchedulingClock,
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
const MAX_CHECKPOINTS: usize = 256;

#[derive(Clone, Debug)]
struct Key<'a> {
    ordinal: u64,
    timestamp: i64,
    slot: Slot<'a>,
}

#[derive(Debug)]
struct Track<'a> {
    keys: BudgetVec<Key<'a>>,
    previous: Option<i64>,
    entries: u64,
}

#[derive(Debug)]
pub(super) struct Checkpoints<'a> {
    tracks: BudgetVec<Track<'a>>,
    total: u64,
    stored: usize,
    _charge: Reservation,
}

impl<'a> Checkpoints<'a> {
    fn new(budget: &Budget, tracks: usize) -> Result<Self> {
        let charge = budget.reserve(size_of::<Self>() + 2 * size_of::<usize>())?;
        let mut slots = BudgetVec::new(budget);
        slots.try_reserve_exact(tracks)?;
        for _ in 0..tracks {
            slots
                .try_push(Track {
                    keys: BudgetVec::new(budget),
                    previous: None,
                    entries: 0,
                })
                .map_err(|e| e.error)?;
        }
        Ok(Self {
            tracks: slots,
            total: 0,
            stored: 0,
            _charge: charge,
        })
    }

    fn record(&mut self, at: usize, slot: &Slot<'a>) -> Option<()> {
        let track = &mut self.tracks.as_mut_slice()[at];
        let row = slot.head.as_ref()?;
        let timestamp = row.native.dts?;
        let flags = row.native.flags;
        let key = flags & 1 != 0;
        let admitted = 5 | if slot
            .encoded_edit
            .is_some_and(|edit| edit.has_dependency_view_v4())
        {
            16
        } else {
            0
        };
        // The regular native index accepts this flag/size/clock profile.
        // Anything ambiguous stays on the original full seek planner.
        if track.previous.is_some_and(|before| timestamp <= before)
            || (track.entries == 0 && !key)
            || flags & !admitted != 0
            || row.source.range.start > i64::MAX as u64
            || row.source.range.end.checked_sub(row.source.range.start)? > 0x1fff_ffff
        {
            return None;
        }
        if key
            && track
                .keys
                .as_slice()
                .last()
                .is_none_or(|last| track.entries - last.ordinal >= CHECKPOINT_INTERVAL)
        {
            if self.stored == MAX_CHECKPOINTS {
                return None;
            }
            track
                .keys
                .try_push(Key {
                    ordinal: track.entries,
                    timestamp,
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

impl<'a> Mov9PacketCursor<'a> {
    /// Collect optional seek restarts as the complete regular range is checked.
    /// The cursor must be exhausted and rewound before using the restarts.
    pub fn enable_range_seek_checkpoints(&mut self) {
        if matches!(self.phase, Phase::Media)
            && self.complete_source
            && !self.front_source
            && self.limits.max_occurrences <= i32::MAX as u64
            && self
                .slots
                .as_slice()
                .iter()
                .all(|slot| slot.track.fragments.is_empty())
        {
            self.seek_checkpoints = Checkpoints::new(self.slots.budget(), self.slots.len())
                .ok()
                .map(Arc::new);
            self.recording_seek_checkpoints = self.seek_checkpoints.is_some();
        }
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
        limits: Mov9NativeSeekLimits,
    ) -> Result<Option<Mov9RangeSeekJob<'a>>> {
        let Some(index) = &self.seek_checkpoints else {
            return Ok(None);
        };
        let packets = limits.index.packets;
        if self.recording_seek_checkpoints
            || self.failed.is_some()
            || index.total > limits.index.max_entries
            || index.total > packets.max_occurrences
            || self.validated > packets.max_source_samples
            || self.slots.len() > packets.max_tracks
            || self.media.len() > packets.max_media_extents
            || self.limits.edit.max_occurrences > packets.edit.max_occurrences
            || self.limits.edit.max_scanned_samples > packets.edit.max_scanned_samples
            || index
                .tracks
                .as_slice()
                .iter()
                .any(|track| track.keys.is_empty())
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
        for track in index.tracks.as_slice() {
            slots
                .try_push(track.keys.as_slice()[0].slot.clone())
                .map_err(|e| e.error)?;
        }
        Ok(Some(Mov9RangeSeekJob {
            index: Arc::clone(index),
            slots,
            media: self.media,
            packet_limits: self.limits,
            validated: self.validated,
            reference,
            timestamp,
            receipt: None,
            at: 0,
            scan: None,
            skipped: 0,
            maximum_skipped: limits.max_replayed_occurrences,
            failed: None,
        }))
    }
}

struct TrackSeek<'a> {
    target: i64,
    next: u64,
    best: Key<'a>,
    cursor: Slot<'a>,
    fallback: bool,
    done: bool,
}

impl<'a> TrackSeek<'a> {
    fn new(keys: &[Key<'a>], target: i64) -> Self {
        let at = keys.partition_point(|key| key.timestamp <= target);
        let best = keys[at.saturating_sub(1)].clone();
        let mut cursor = best.slot.clone();
        cursor.head = None;
        Self {
            target,
            next: best.ordinal + 1,
            best,
            cursor,
            fallback: at == 0,
            done: false,
        }
    }

    fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<bool> {
        if self.done || self.fallback || self.best.timestamp == self.target {
            return Ok(true);
        }
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if self.cursor.head.is_none() {
                match self.cursor.fill(work, true, false, false)? {
                    Mov9CursorStatus::Yield => return Ok(false),
                    Mov9CursorStatus::End => {
                        self.done = true;
                        return Ok(true);
                    }
                    Mov9CursorStatus::Ready => {}
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
                .native
                .dts
                .ok_or(Error::Invalid("checked seek clock missing"))?;
            if timestamp > self.target {
                self.done = true;
                return Ok(true);
            }
            if head.native.flags & 1 != 0 {
                self.best = Key {
                    ordinal: self.next,
                    timestamp,
                    slot: self.cursor.clone(),
                };
            }
            self.next += 1;
            self.cursor.head = None;
        }
    }
}

/// Source-bound seek over the sparse regular-track restarts from range open.
pub struct Mov9RangeSeekJob<'a> {
    index: Arc<Checkpoints<'a>>,
    slots: BudgetVec<Slot<'a>>,
    media: &'a [Range<u64>],
    packet_limits: super::Mov9PacketLimits,
    validated: u64,
    reference: usize,
    timestamp: i64,
    receipt: Option<Mov9NativeSeekReceipt>,
    at: usize,
    scan: Option<TrackSeek<'a>>,
    skipped: u64,
    maximum_skipped: u64,
    failed: Option<Error>,
}

impl<'a> Mov9RangeSeekJob<'a> {
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
            if self.at == self.slots.len() {
                return Ok(BuildStatus::Ready);
            }
            let at = if self.receipt.is_none() {
                self.reference
            } else {
                self.at
            };
            if let Some(scan) = self.scan.as_mut() {
                if !scan.poll(work, cancel)? || !work.try_charge(1) {
                    return Ok(BuildStatus::Yield);
                }
                let scan = self.scan.take().expect("polled track seek");
                self.finish_track(at, scan)?;
                continue;
            }
            if !work.try_charge(1) {
                return Ok(BuildStatus::Yield);
            }
            let target = if let Some(receipt) = &self.receipt {
                if at == self.reference {
                    self.at += 1;
                    continue;
                }
                let reference_base = self.slots.as_slice()[self.reference].clock.native_base;
                let target_base = self.slots.as_slice()[at].clock.native_base;
                reference_base.rescale(
                    receipt.selected.timestamp,
                    target_base,
                    Rounding::NearestAway,
                )?
            } else {
                self.timestamp
            };
            if target == i64::MIN {
                return Err(Error::Unsupported("native seek null timestamp argument"));
            }
            self.scan = Some(TrackSeek::new(
                self.index.tracks.as_slice()[at].keys.as_slice(),
                target,
            ));
        }
    }

    fn finish_track(&mut self, at: usize, scan: TrackSeek<'a>) -> Result<()> {
        let Key {
            ordinal,
            timestamp,
            mut slot,
        } = scan.best;
        let head = slot
            .head
            .as_mut()
            .ok_or(Error::Invalid("missing checked seek head"))?;
        if self.receipt.is_none() {
            let mut occurrence = head.clone();
            occurrence.scheduling_clock = Mov9SchedulingClock::SingleTrack;
            let discarded = occurrence.native.flags & 4 != 0;
            let range = &occurrence.source.range;
            self.receipt = Some(Mov9NativeSeekReceipt {
                requested_timestamp: self.timestamp,
                selected: Mov9NativeIndexEntry {
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
            });
        } else {
            self.at += 1;
        }
        head.scheduling_clock = if self.slots.len() > 1 {
            Mov9SchedulingClock::from_native(head.native.dts_field, slot.clock.native_base)?
        } else {
            Mov9SchedulingClock::SingleTrack
        };
        self.slots.as_mut_slice()[at] = slot;
        self.skipped = self.skipped.checked_add(ordinal).ok_or(Error::Overflow)?;
        Ok(())
    }

    pub fn finish(self) -> Result<(Mov9PacketCursor<'a>, Mov9NativeSeekReceipt)> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if self.at != self.slots.len() {
            return Err(Error::Invalid("checked range seek still pending"));
        }
        count(self.skipped, self.maximum_skipped)?;
        let mut cursor = Mov9PacketCursor::from_slots(self.slots, self.media, self.packet_limits);
        cursor.complete_source = true;
        cursor.phase = Phase::Fill;
        cursor.validated = self.validated;
        cursor.generated = self.skipped;
        cursor.seek_checkpoints = Some(self.index);
        Ok((
            cursor,
            self.receipt
                .ok_or(Error::Invalid("missing checked seek receipt"))?,
        ))
    }
}

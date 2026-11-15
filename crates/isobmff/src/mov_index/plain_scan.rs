//! A backward seek over a plain regular track, read from its sample table.
//!
//! A track with no edits, fragments, partial sync or dependency samples has one
//! index entry per sample, in sample order. An entry's timestamp is the sample's
//! decode time plus the track's composition shift, and it is a key where the
//! sample is a sync sample, or the first sample under an explicitly empty sync
//! table. [`PlainScan`] walks the table for exactly these facts, without building
//! an occurrence, and makes the choices [`SeekJob`](super::SeekJob) makes from
//! the entries. Wherever an entry would fail one of the index's or the seek's
//! checks, it gives up, and the seek builds the entries one by one instead, which
//! reports that failure.
use crate::{ClockField, SampleCursor, SampleTable};
use triergon_core::WorkBudget;

/// What the scan needs beyond the table: the facts the packet cursor and the
/// index established before their first entry.
#[derive(Clone, Copy, Debug)]
pub(super) struct PlainTrack<'a> {
    pub(super) table: &'a SampleTable,
    /// The demux decode-time shift of the track's composition offsets.
    pub(super) shift: i64,
    /// Whether the first entry may be a non-key entry.
    pub(super) allow_initial_nonkey: bool,
    /// The index's entry limit.
    pub(super) max_entries: u64,
}

/// The entry a scan selected: the cursor that produces its sample, its index
/// ordinal, whether it is the first key rather than one at or before the target,
/// and how many entries the index has.
#[derive(Clone, Copy, Debug)]
pub(super) struct PlainChoice<'a> {
    pub(super) cursor: SampleCursor<'a>,
    pub(super) ordinal: u64,
    pub(super) first_fallback: bool,
    pub(super) scanned: u64,
}

#[derive(Debug)]
pub(super) enum PlainStep<'a> {
    Yield,
    /// An entry fails a check, or the seek fails: build the entries instead.
    GiveUp,
    Done(PlainChoice<'a>),
}

#[derive(Debug)]
pub(super) struct PlainScan<'a> {
    cursor: SampleCursor<'a>,
    shift: i64,
    explicit_empty_sync: bool,
    allow_initial_nonkey: bool,
    target: i128,
    first: Option<(SampleCursor<'a>, u64)>,
    best: Option<(SampleCursor<'a>, u64)>,
    previous: Option<i64>,
    unordered_minimum: Option<i64>,
}

impl<'a> PlainScan<'a> {
    /// A scan for the last key at or before `target`, in the index's clock.
    pub(super) fn new(track: PlainTrack<'a>, target: i128) -> Option<Self> {
        let count = track.table.sample_count();
        if count > track.max_entries || count > i32::MAX as u64 {
            return None;
        }
        Some(Self {
            cursor: track.table.cursor(),
            shift: track.shift,
            explicit_empty_sync: track.table.sync_entry_count() == Some(0),
            allow_initial_nonkey: track.allow_initial_nonkey,
            target,
            first: None,
            best: None,
            previous: None,
            unordered_minimum: None,
        })
    }

    pub(super) fn poll(&mut self, work: &mut WorkBudget) -> PlainStep<'a> {
        let count = self.cursor.table().sample_count();
        loop {
            let ordinal = self.cursor.ordinal();
            if ordinal == count {
                return self.finish(count);
            }
            if !work.try_charge(1) {
                return PlainStep::Yield;
            }
            let before = self.cursor;
            let Ok(sample) = self.cursor.skip() else {
                return PlainStep::GiveUp;
            };
            // The index's size and position representations.
            if sample.size > 0x1fff_ffff || i64::try_from(sample.start).is_err() {
                return PlainStep::GiveUp;
            }
            let key = sample.sync || (self.explicit_empty_sync && sample.ordinal == 1);
            if !key && sample.ordinal == 1 && !self.allow_initial_nonkey {
                return PlainStep::GiveUp;
            }
            let Some(timestamp) = sample
                .dts
                .checked_add(i128::from(self.shift))
                .and_then(|ticks| ClockField::from_exact_ticks(ticks).ok())
                .map(ClockField::signed_ticks)
                .filter(|&timestamp| timestamp != i64::MIN)
            else {
                return PlainStep::GiveUp;
            };
            if self.previous.is_some_and(|prior| timestamp <= prior)
                || self.unordered_minimum.is_some()
            {
                self.unordered_minimum = Some(
                    self.unordered_minimum
                        .map_or(timestamp, |least| least.min(timestamp)),
                );
            }
            self.previous = Some(timestamp);
            if key {
                if self.first.is_none() {
                    self.first = Some((before, ordinal));
                }
                if i128::from(timestamp) <= self.target {
                    self.best = Some((before, ordinal));
                }
            }
        }
    }

    fn finish(&self, scanned: u64) -> PlainStep<'a> {
        if self
            .unordered_minimum
            .is_some_and(|minimum| self.target >= i128::from(minimum))
        {
            return PlainStep::GiveUp;
        }
        let (choice, first_fallback) = match (self.best, self.first) {
            (Some(best), _) => (best, false),
            (None, Some(first)) => (first, true),
            (None, None) => return PlainStep::GiveUp,
        };
        PlainStep::Done(PlainChoice {
            cursor: choice.0,
            ordinal: choice.1,
            first_fallback,
            scanned,
        })
    }
}

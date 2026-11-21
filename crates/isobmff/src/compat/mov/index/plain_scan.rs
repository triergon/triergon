//! A backward seek over a track whose index entries are its samples, read from
//! its tables.
//!
//! A track with no edit list, partial sync or dependency samples, or a track of
//! fragments alone, has one index entry per sample, in sample order. An entry's
//! timestamp is the sample's decode time, with a regular track's composition
//! shift added or a fragment edit's offset taken away, and it is a key where the
//! sample is a sync sample, or the first sample under an explicitly empty sync
//! table. [`PlainScan`] walks the tables for exactly these facts, without building
//! an occurrence, and makes the choices [`SeekJob`](super::SeekJob) makes from the
//! entries. Wherever an entry would fail one of the index's or the seek's checks,
//! or would need a rule the scan does not follow (a fragment sample without a
//! decode time, a discarded sample, a run that continues another without a key),
//! it gives up, and the seek builds the entries one by one instead.
use crate::compat::mov::cursor::{PlainSource, PlainStart, PreviousClock, PreviousSample};
use crate::compat::mov::edit_list::EmitStart;
use crate::fragment::RunSkip;
use crate::{
    fragment::{FragmentCursor, FragmentIndex},
    table::SampleCursor,
};
use triergon_core::WorkBudget;

/// What the scan needs beyond the track: the facts the seek and its index
/// established before their first entry.
#[derive(Clone, Copy, Debug)]
pub(super) struct PlainTrack<'a> {
    pub(super) source: PlainSource<'a>,
    /// Whether the index admits a non-key first entry.
    pub(super) allow_initial_nonkey: bool,
    /// Whether the seek may land on a non-key first entry.
    pub(super) allow_initial_prefix: bool,
    /// The index's entry limit.
    pub(super) max_entries: u64,
    /// The seek target in the index's clock.
    pub(super) target: i128,
}

/// The entry a scan selected: where its track continues from, its index ordinal,
/// whether it is the first entry rather than a key at or before the target, and
/// how many entries the index has.
#[derive(Clone, Copy, Debug)]
pub(super) struct PlainChoice<'a> {
    pub(super) start: PlainStart<'a>,
    pub(super) ordinal: u64,
    pub(super) first_fallback: bool,
    pub(super) scanned: u64,
}

// One per poll of a scan.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub(super) enum PlainStep<'a> {
    Yield,
    /// An entry fails a check or needs another rule, or the seek fails.
    GiveUp,
    Done(PlainChoice<'a>),
}

/// The walk over the track's samples.
#[derive(Clone, Copy, Debug)]
enum Walk<'a> {
    Regular {
        cursor: SampleCursor<'a>,
        shift: i64,
        explicit_empty_sync: bool,
    },
    Fragments {
        track_id: u32,
        fragments: &'a [FragmentIndex],
        next_fragment: usize,
        current: Option<FragmentCursor<'a>>,
        edit_offset: i64,
    },
    /// The emitted samples of an edit cursor, with the counts it keeps.
    Edited {
        cursor: SampleCursor<'a>,
        edit: EmitStart<'a>,
        generated: u64,
        ended: bool,
    },
}

/// The index timestamp of decode time `dts` moved by `offset`: the exact ticks,
/// which a demux clock field reads back unchanged where they fit its signed
/// 64 bits. `None` where they do not, or are the index's null clock.
fn timestamp(dts: i128, offset: i128) -> Option<i64> {
    let ticks = dts.checked_add(offset)?;
    i64::try_from(ticks)
        .ok()
        .filter(|&timestamp| timestamp != i64::MIN)
}

/// Whether a sample's bytes fit the index's size and position fields.
fn representable(start: u64, size: u32) -> bool {
    size <= 0x1fff_ffff && i64::try_from(start).is_ok()
}

#[derive(Debug)]
pub(super) struct PlainScan<'a> {
    walk: Walk<'a>,
    state: State<'a>,
}

/// What a scan has learnt from the entries so far.
#[derive(Debug)]
struct State<'a> {
    track: PlainTrack<'a>,
    entries: u64,
    first: Option<(PlainStart<'a>, u64, bool)>,
    best: Option<(PlainStart<'a>, u64)>,
    previous: Option<PreviousSample>,
    previous_timestamp: Option<i64>,
    discard_boundary: Option<i128>,
    unordered_minimum: Option<i64>,
}

impl<'a> PlainScan<'a> {
    pub(super) fn new(track: PlainTrack<'a>) -> Self {
        let walk = match track.source {
            PlainSource::Regular { table, shift } => Walk::Regular {
                cursor: table.cursor(),
                shift,
                explicit_empty_sync: table.sync_entry_count() == Some(0),
            },
            PlainSource::Fragments {
                track_id,
                fragments,
                edit_offset,
            } => Walk::Fragments {
                track_id,
                fragments,
                next_fragment: 0,
                current: None,
                edit_offset,
            },
            PlainSource::Edited(edit) => Walk::Edited {
                cursor: edit.table.cursor(),
                edit,
                generated: 0,
                ended: false,
            },
        };
        Self {
            walk,
            state: State {
                track,
                entries: 0,
                first: None,
                best: None,
                previous: None,
                previous_timestamp: None,
                discard_boundary: None,
                unordered_minimum: None,
            },
        }
    }

    pub(super) fn poll(&mut self, work: &mut WorkBudget) -> PlainStep<'a> {
        let state = &mut self.state;
        match &mut self.walk {
            Walk::Regular {
                cursor,
                shift,
                explicit_empty_sync,
            } => state.poll_regular(work, cursor, *shift, *explicit_empty_sync),
            Walk::Fragments {
                track_id,
                fragments,
                next_fragment,
                current,
                edit_offset,
            } => state.poll_fragments(
                work,
                (*track_id, fragments),
                next_fragment,
                current,
                *edit_offset,
            ),
            Walk::Edited {
                cursor,
                edit,
                generated,
                ended,
            } => state.poll_edited(work, cursor, edit, generated, ended),
        }
    }
}

impl<'a> State<'a> {
    fn poll_regular(
        &mut self,
        work: &mut WorkBudget,
        cursor: &mut SampleCursor<'a>,
        shift: i64,
        explicit_empty_sync: bool,
    ) -> PlainStep<'a> {
        let count = cursor.table().sample_count();
        loop {
            if cursor.ordinal() == count {
                return self.finish();
            }
            if !work.try_charge(1) {
                return PlainStep::Yield;
            }
            let before = *cursor;
            let Ok(sample) = cursor.skip() else {
                return PlainStep::GiveUp;
            };
            let Some(timestamp) = timestamp(sample.dts, i128::from(shift))
                .filter(|_| representable(sample.start, sample.size))
            else {
                return PlainStep::GiveUp;
            };
            let key = sample.sync || (explicit_empty_sync && sample.ordinal == 1);
            if !self.record(timestamp, key, false) {
                return PlainStep::GiveUp;
            }
            if key {
                self.keep(PlainStart::Regular(before), timestamp);
            }
        }
    }

    fn poll_fragments(
        &mut self,
        work: &mut WorkBudget,
        (track_id, fragments): (u32, &'a [FragmentIndex]),
        next_fragment: &mut usize,
        current: &mut Option<FragmentCursor<'a>>,
        edit_offset: i64,
    ) -> PlainStep<'a> {
        loop {
            let Some(fragment) = current else {
                let Some(index) = fragments.get(*next_fragment) else {
                    return self.finish();
                };
                if !work.try_charge(1) {
                    return PlainStep::Yield;
                }
                *next_fragment += 1;
                if index.track_id() == track_id {
                    *current = Some(index.cursor());
                }
                continue;
            };
            let before = *fragment;
            let sample = match fragment.skip(work) {
                Ok(RunSkip::Sample(sample)) => sample,
                Ok(RunSkip::Yield) => return PlainStep::Yield,
                Ok(RunSkip::End) => {
                    *current = None;
                    continue;
                }
                Err(_) => return PlainStep::GiveUp,
            };
            let run = (*next_fragment - 1, fragment.run_index());
            let Some((dts, timestamp)) = sample
                .dts
                .and_then(|dts| Some((dts, timestamp(dts, -i128::from(edit_offset))?)))
                .filter(|_| representable(sample.start, sample.size))
            else {
                return PlainStep::GiveUp;
            };
            let start = PlainStart::Fragment {
                next_fragment: *next_fragment,
                cursor: before,
                previous: self.previous,
                discard_boundary: self.discard_boundary,
            };
            let previous = self.previous;
            if previous.is_none_or(|previous| previous.run != run) {
                // A run start: its samples are discarded at or below the
                // timestamp before it.
                let boundary = self.previous_timestamp.unwrap_or(i64::MIN);
                if timestamp <= boundary {
                    return PlainStep::GiveUp;
                }
                self.discard_boundary = Some(i128::from(boundary));
                // A run without a key restarts the distance only where it
                // starts the next fragment.
                if let Some(previous) = previous
                    && !sample.sync
                    && !(run.0 > previous.run.0
                        && run.1 == 0
                        && previous.clock.sample.checked_add(1) == Some(sample.ordinal))
                {
                    return PlainStep::GiveUp;
                }
            }
            self.previous = Some(PreviousSample {
                clock: PreviousClock {
                    sample: sample.ordinal,
                    dts,
                    signed_dts: timestamp,
                    duration: sample.duration,
                    source_known: true,
                },
                run,
            });
            // The first fragment sample may start the index without a key.
            let first = self.entries == 0;
            if !self.record(timestamp, sample.sync, sample.ordinal == 1) {
                return PlainStep::GiveUp;
            }
            // A fragment track's first entry stands in for a key.
            if first {
                self.first = Some((start, 0, sample.sync));
            }
            if sample.sync {
                self.keep(start, timestamp);
            }
        }
    }

    /// The occurrences an [`EditCursor`](crate::compat::mov::edit_list::EditCursor) emits for one media
    /// edit: its samples from the first it emits through the first sync sample
    /// that reaches the edit's end, or through the last sample. Each scan, decode
    /// delta and occurrence counts toward the edit cursor's limits as it does
    /// there.
    fn poll_edited(
        &mut self,
        work: &mut WorkBudget,
        cursor: &mut SampleCursor<'a>,
        edit: &mut EmitStart<'a>,
        generated: &mut u64,
        ended: &mut bool,
    ) -> PlainStep<'a> {
        let table = edit.table;
        if table.has_composition_offsets() || table.has_dependency_table() {
            return PlainStep::GiveUp;
        }
        loop {
            if *ended || cursor.ordinal() == table.sample_count() {
                return self.finish();
            }
            if !work.try_charge(1) {
                return PlainStep::Yield;
            }
            let Ok(sample) = cursor.skip() else {
                return PlainStep::GiveUp;
            };
            edit.scanned += 1;
            if edit.scanned > edit.limits.max_scanned_samples {
                return PlainStep::GiveUp;
            }
            if sample.ordinal < edit.first {
                continue;
            }
            if sample.delta == 0 || sample.partial {
                return PlainStep::GiveUp;
            }
            *generated += 1;
            // The decode delta of the occurrence's own ordinal.
            if *generated <= table.sample_count() {
                edit.scanned += 1;
            }
            if *generated > edit.limits.max_occurrences
                || edit.scanned > edit.limits.max_scanned_samples
            {
                return PlainStep::GiveUp;
            }
            // The edit projects decode times that fit signed 64 bits in ticks.
            let Some(timestamp) = timestamp(sample.dts, i128::from(edit.shift)).filter(|_| {
                i64::try_from(sample.dts).is_ok() && representable(sample.start, sample.size)
            }) else {
                return PlainStep::GiveUp;
            };
            *ended = sample.sync
                && sample
                    .dts
                    .checked_add(i128::from(sample.delta))
                    .is_some_and(|end| end >= i128::from(edit.end));
            if !self.record(timestamp, sample.sync, false) {
                return PlainStep::GiveUp;
            }
            if sample.sync {
                self.keep(PlainStart::Edited(sample.ordinal), timestamp);
            }
        }
    }

    /// Counts an entry and follows its timestamp; `false` where the index would
    /// refuse it. `starts_index` is whether a non-key entry may still start the
    /// index.
    fn record(&mut self, timestamp: i64, key: bool, starts_index: bool) -> bool {
        let first = self.entries == 0;
        self.entries += 1;
        if self.entries > self.track.max_entries
            || self.entries > i32::MAX as u64
            || (first && !key && !starts_index && !self.track.allow_initial_nonkey)
        {
            return false;
        }
        if self
            .previous_timestamp
            .is_some_and(|prior| timestamp <= prior)
            || self.unordered_minimum.is_some()
        {
            self.unordered_minimum = Some(
                self.unordered_minimum
                    .map_or(timestamp, |least| least.min(timestamp)),
            );
        }
        self.previous_timestamp = Some(timestamp);
        true
    }

    /// Keeps a key entry, the last recorded, as the first and best it may be.
    fn keep(&mut self, start: PlainStart<'a>, timestamp: i64) {
        let ordinal = self.entries - 1;
        if self.first.is_none() {
            self.first = Some((start, ordinal, true));
        }
        if i128::from(timestamp) <= self.track.target {
            self.best = Some((start, ordinal));
        }
    }

    fn finish(&self) -> PlainStep<'a> {
        if self
            .unordered_minimum
            .is_some_and(|minimum| self.track.target >= i128::from(minimum))
        {
            return PlainStep::GiveUp;
        }
        let ((start, ordinal), first_fallback) = match (self.best, self.first) {
            (Some(best), _) => (best, false),
            (None, Some((start, ordinal, key))) if key || self.track.allow_initial_prefix => {
                ((start, ordinal), true)
            }
            _ => return PlainStep::GiveUp,
        };
        PlainStep::Done(PlainChoice {
            start,
            ordinal,
            first_fallback,
            scanned: self.entries,
        })
    }
}

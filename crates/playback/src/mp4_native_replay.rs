//! Replay scheduling over the complete-source native MP4 occurrence catalogue.

use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Error, Result, WorkBudget};
use triergon_isobmff::compat::mov::{clock::seekable_head_precedes, packets::PacketOccurrence};

const MAX_TRACKS: usize = 64;

#[derive(Clone, Copy, Debug)]
struct Slot {
    track_id: u32,
    head: Option<usize>,
}

#[derive(Clone, Copy, Debug)]
enum Phase {
    Ready { slot: usize },
    Search { slot: usize, next: usize },
    Select { next: usize, best: Option<usize> },
    End,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NativeMp4ReplayStep {
    Ready(usize),
    Yield,
    End,
}

/// One head per track in movie catalogue order. The caller keeps the same
/// immutable occurrence table for the lifetime of this replay and its clones.
#[derive(Clone, Debug)]
pub(crate) struct NativeMp4Replay {
    slots: [Slot; MAX_TRACKS],
    track_count: usize,
    row_count: usize,
    first_row: usize,
    phase: Phase,
}

impl NativeMp4Replay {
    pub(crate) fn new(
        track_ids: &[u32],
        starts: &[usize],
        rows: &[PacketOccurrence],
    ) -> Result<Self> {
        if track_ids.len() > MAX_TRACKS {
            return Err(Error::Limit {
                requested: track_ids.len(),
                maximum: MAX_TRACKS,
            });
        }
        if track_ids.is_empty() || track_ids.len() != starts.len() {
            return Err(Error::Invalid("native MP4 replay track starts"));
        }
        let mut slots = [Slot {
            track_id: 0,
            head: None,
        }; MAX_TRACKS];
        let mut first = 0;
        for (slot, (&track_id, &start)) in track_ids.iter().zip(starts).enumerate() {
            let occurrence = rows
                .get(start)
                .filter(|occurrence| occurrence.track_id == track_id)
                .ok_or(Error::Invalid("native MP4 replay start row"))?;
            if track_ids[..slot].contains(&track_id) {
                return Err(Error::Invalid("duplicate native MP4 replay track"));
            }
            slots[slot] = Slot {
                track_id,
                head: Some(start),
            };
            if slot != 0 && seekable_head_precedes(occurrence, &rows[starts[first]]) {
                first = slot;
            }
        }
        Ok(Self {
            slots,
            track_count: track_ids.len(),
            row_count: rows.len(),
            first_row: starts[first],
            phase: Phase::Ready { slot: first },
        })
    }

    pub(crate) fn first_row(&self) -> usize {
        self.first_row
    }

    pub(crate) fn poll(
        &mut self,
        rows: &[PacketOccurrence],
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<NativeMp4ReplayStep> {
        if rows.len() != self.row_count {
            return Err(Error::Invalid("native MP4 replay row count changed"));
        }
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            match self.phase {
                Phase::Ready { slot } => {
                    return Ok(NativeMp4ReplayStep::Ready(self.head(slot)?));
                }
                Phase::Search { slot, next } => {
                    if let Some(row) = rows.get(next) {
                        if !work.try_charge(1) {
                            return Ok(NativeMp4ReplayStep::Yield);
                        }
                        if row.track_id != self.slots[slot].track_id {
                            self.phase = Phase::Search {
                                slot,
                                next: next + 1,
                            };
                            continue;
                        }
                        self.slots[slot].head = Some(next);
                    } else {
                        self.slots[slot].head = None;
                    }
                    self.phase = Phase::Select {
                        next: 0,
                        best: None,
                    };
                }
                Phase::Select { next, mut best } => {
                    if next == self.track_count {
                        self.phase = best.map_or(Phase::End, |slot| Phase::Ready { slot });
                        continue;
                    }
                    if !work.try_charge(1) {
                        return Ok(NativeMp4ReplayStep::Yield);
                    }
                    if let Some(row) = self.slots[next].head {
                        let prefer = match best {
                            Some(slot) => {
                                seekable_head_precedes(&rows[row], &rows[self.head(slot)?])
                            }
                            None => true,
                        };
                        if prefer {
                            best = Some(next);
                        }
                    }
                    self.phase = Phase::Select {
                        next: next + 1,
                        best,
                    };
                }
                Phase::End => return Ok(NativeMp4ReplayStep::End),
            }
        }
    }

    pub(crate) fn delivered(&mut self, row: usize) -> Result<()> {
        if let Phase::Ready { slot } = self.phase
            && self.head(slot)? == row
        {
            self.phase = Phase::Search {
                slot,
                next: row + 1,
            };
            return Ok(());
        }
        Err(Error::Invalid("native MP4 replay delivery row"))
    }

    fn head(&self, slot: usize) -> Result<usize> {
        self.slots[slot]
            .head
            .ok_or(Error::Invalid("native MP4 replay head absent"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use triergon_core::TimeBase;
    use triergon_isobmff::{
        Access, Sample,
        compat::mov::{
            clock::{ClockOrigin, MediaClock, SchedulingClock},
            packets::PacketProjector,
        },
    };

    static RUN: AtomicBool = AtomicBool::new(false);

    fn row(track_id: u32, clock: i64, position: u64) -> PacketOccurrence {
        let base = TimeBase::new(1, 1_000_000).unwrap();
        let source = Sample {
            ordinal: position,
            description_index: 1,
            range: position..position + 1,
            dts_ticks: Some(i128::from(clock)),
            cts_ticks: Some(i128::from(clock)),
            duration_ticks: 1,
            composition_offset: None,
            access: Access::ContainerSync,
            partial_sync: false,
            dependency: None,
            fragment_flags: None,
        };
        let native = PacketProjector::new(base, None)
            .project(&source, None)
            .unwrap();
        PacketOccurrence {
            track_id,
            scheduling_clock: SchedulingClock::from_demux_clock(native.dts_field, base).unwrap(),
            source,
            demux: native,
            edit: None,
            clock_origin: ClockOrigin::SourceClock,
            media_clock: MediaClock::from_source(base).unwrap(),
        }
    }

    #[test]
    fn replay_skips_each_tracks_prefix_and_holds_the_selected_row() {
        let rows = [
            row(1, 2_000_000, 10),
            row(2, 3_000_000, 20),
            row(2, 0, 30),
            row(1, 4_000_000, 40),
            row(2, 4_000_000, 50),
        ];
        let mut replay = NativeMp4Replay::new(&[1, 2], &[0, 2], &rows).unwrap();
        assert_eq!(replay.first_row(), 2);
        let mut no_work = WorkBudget::new(0);
        assert_eq!(
            replay.poll(&rows, &mut no_work, &AtomicBool::new(true)),
            Err(Error::Cancelled)
        );
        assert!(replay.delivered(0).is_err());
        for _ in 0..2 {
            assert_eq!(
                replay.poll(&rows, &mut no_work, &RUN).unwrap(),
                NativeMp4ReplayStep::Ready(2)
            );
        }
        replay.delivered(2).unwrap();
        assert!(replay.delivered(2).is_err());
        let mut one = WorkBudget::new(1);
        assert_eq!(
            replay.poll(&rows, &mut one, &RUN).unwrap(),
            NativeMp4ReplayStep::Yield
        );
        assert_eq!(one.spent(), 1);
        assert_eq!(
            replay.poll(&rows, &mut no_work, &RUN).unwrap(),
            NativeMp4ReplayStep::Yield
        );
        let mut clone = replay.clone();
        for replay in [&mut replay, &mut clone] {
            let mut delivered = Vec::new();
            for _ in 0..32 {
                match replay.poll(&rows, &mut WorkBudget::new(1), &RUN).unwrap() {
                    NativeMp4ReplayStep::Yield => {}
                    NativeMp4ReplayStep::Ready(row) => {
                        delivered.push(row);
                        replay.delivered(row).unwrap();
                    }
                    NativeMp4ReplayStep::End => break,
                }
            }
            assert_eq!(delivered, [0, 3, 4]);
            assert_eq!(
                replay.poll(&rows, &mut no_work, &RUN).unwrap(),
                NativeMp4ReplayStep::End
            );
        }
    }

    #[test]
    fn replay_uses_seekable_threshold_and_sticky_catalogue_order() {
        let mut rows = [row(1, 2_000_000, 50), row(2, 1_000_000, 80)];
        let first = |rows: &[PacketOccurrence]| {
            NativeMp4Replay::new(&[1, 2], &[0, 1], rows)
                .unwrap()
                .first_row()
        };
        assert_eq!(first(&rows), 0);
        rows[1].scheduling_clock = SchedulingClock::ExactMicroseconds(999_999);
        assert_eq!(first(&rows), 1);
        rows[1].scheduling_clock = SchedulingClock::ApiSentinel;
        assert_eq!(first(&rows), 0);
        assert_eq!(
            NativeMp4Replay::new(&[2, 1], &[1, 0], &rows)
                .unwrap()
                .first_row(),
            1
        );
    }
}

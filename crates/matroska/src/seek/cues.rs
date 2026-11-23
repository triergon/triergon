//! Real resumable CuePoint parsing/transaction assembly. Output is explicitly
//! source-ordered; index normalization/tie policy is NOT declared complete here.
use super::Cue;
use crate::ebml::{self, ChildCursor};
use crate::step::{ParseStep, Required, check};
use crate::storage::{SharedBytes, copy_vec::CopyVec};
use crate::{Error, Result};
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, BudgetVec, WorkBudget};
#[derive(Clone, Copy, Debug)]
struct Position {
    track: u64,
    cluster: u64,
    relative: Option<u64>,
    block: Option<u64>,
    codec_state: u64,
}
#[derive(Debug)]
pub(crate) struct SourceOrderedCues {
    pub source: SharedBytes,
    pub entries: BudgetVec<Cue>,
}
/// Where the reading of Cues stands. A CuePoint's cursor lives in the phases that
/// read it; from the end of the CuePoint its CueTime does.
#[derive(Debug)]
enum Phase {
    /// The children of Cues.
    Root,
    /// The children of a CuePoint.
    Point(ChildCursor),
    /// The children of a CueTrackPositions of the CuePoint read by `point`.
    Position {
        point: ChildCursor,
        cursor: ChildCursor,
    },
    /// Adding the position just read; the CuePoint goes on after it.
    PositionAppend(ChildCursor),
    /// Making room in the output for the positions of a CuePoint at `ticks`.
    ReserveMerge {
        ticks: u64,
    },
    WaitReserve {
        ticks: u64,
    },
    /// Appending position `at` of the CuePoint at `ticks`.
    Merge {
        ticks: u64,
        at: usize,
    },
    WaitMerge {
        ticks: u64,
        at: usize,
    },
    Done,
}
#[derive(Debug)]
pub(crate) struct CuesJob {
    source: SharedBytes,
    segment: u64,
    segment_end: Option<u64>,
    limit: usize,
    root: ChildCursor,
    // The first CueTime and the first of each position field read in the current
    // CuePoint and CueTrackPositions.
    ticks: Option<u64>,
    fields: [Option<u64>; 5],
    positions: CopyVec<Position>,
    output: CopyVec<Cue>,
    phase: Phase,
    failed: bool,
}
impl CuesJob {
    pub(crate) fn new(
        source: SharedBytes,
        segment: u64,
        segment_end: Option<u64>,
        limit: usize,
        pool: &Budget,
    ) -> Result<Self> {
        if !source.belongs_to(pool) {
            return Err(Error::ForeignPool);
        }
        let len = source.len();
        Ok(Self {
            source,
            segment,
            segment_end,
            limit,
            root: ChildCursor::new(0..len),
            ticks: None,
            fields: [None; 5],
            positions: CopyVec::new(pool),
            output: CopyVec::new(pool),
            phase: Phase::Root,
            failed: false,
        })
    }
    pub(crate) fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<SourceOrderedCues>> {
        if self.failed {
            return Err(Error::Invalid("Cues job requires replacement after error"));
        }
        let result = self.advance(work, cancel);
        match result {
            Err(Error::Backpressure(triergon_core::Backpressure {
                requested,
                available,
            })) => Ok(ParseStep::Backpressure {
                requested,
                available,
            }),
            Err(error) => {
                self.failed = true;
                Err(error)
            }
            other => other,
        }
    }
    fn advance(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<SourceOrderedCues>> {
        loop {
            check(cancel)?;
            match &mut self.phase {
                Phase::Root => match self.root.step(&self.source, work, cancel)? {
                    ParseStep::Yield => return Ok(ParseStep::Yield),
                    ParseStep::Event(None) => {
                        let entries = self.output.take()?;
                        self.phase = Phase::Done;
                        return Ok(ParseStep::Event(SourceOrderedCues {
                            source: self.source.clone(),
                            entries,
                        }));
                    }
                    ParseStep::Event(Some(child)) => {
                        if child.id == 0xbb {
                            self.positions.reset();
                            self.ticks = None;
                            self.phase = Phase::Point(ChildCursor::new(child.payload));
                        }
                    }
                    _ => return Err(Error::Invalid("invalid Cues root transition")),
                },
                Phase::Point(point) => match point.step(&self.source, work, cancel)? {
                    ParseStep::Yield => return Ok(ParseStep::Yield),
                    ParseStep::Event(None) => {
                        let ticks = self.ticks.required("missing CueTime")?;
                        if self.positions.len() == 0 {
                            return Err(Error::Invalid("missing CueTrackPositions"));
                        }
                        self.phase = Phase::ReserveMerge { ticks };
                    }
                    ParseStep::Event(Some(child)) => match child.id {
                        // A repeated child is passed over (RFC 8794 §12).
                        0xb3 => {
                            let ticks = ebml::uint(&self.source[child.payload])?;
                            self.ticks.get_or_insert(ticks);
                        }
                        0xb7 => {
                            if self
                                .output
                                .len()
                                .checked_add(self.positions.len())
                                .ok_or(Error::Overflow)?
                                >= self.limit
                            {
                                return Err(Error::Policy("cue entries"));
                            }
                            self.fields = [None; 5];
                            self.phase = Phase::Position {
                                point: *point,
                                cursor: ChildCursor::new(child.payload),
                            };
                        }
                        _ => (),
                    },
                    _ => return Err(Error::Invalid("invalid CuePoint transition")),
                },
                Phase::Position { point, cursor } => {
                    match cursor.step(&self.source, work, cancel)? {
                        ParseStep::Yield => return Ok(ParseStep::Yield),
                        ParseStep::Event(Some(child)) => {
                            let index = match child.id {
                                0xf7 => 0,
                                0xf1 => 1,
                                0xf0 => 2,
                                0x5378 => 3,
                                0xea => 4,
                                _ => continue,
                            };
                            let value = ebml::uint(&self.source[child.payload])?;
                            self.fields[index].get_or_insert(value);
                        }
                        ParseStep::Event(None) => {
                            let track = self.fields[0]
                                .filter(|n| *n != 0)
                                .required("missing CueTrack")?;
                            let cluster = self
                                .segment
                                .checked_add(self.fields[1].required("missing CueClusterPosition")?)
                                .ok_or(Error::Overflow)?;
                            if self.segment_end.is_some_and(|end| cluster >= end) {
                                return Err(Error::Invalid("cue outside Segment"));
                            }
                            if self.fields[3] == Some(0) {
                                return Err(Error::Invalid("zero CueBlockNumber"));
                            }
                            self.positions.push(Position {
                                track,
                                cluster,
                                relative: self.fields[2],
                                block: self.fields[3],
                                codec_state: self.fields[4].unwrap_or(0),
                            })?;
                            self.phase = Phase::PositionAppend(*point);
                        }
                        _ => return Err(Error::Invalid("invalid cue position transition")),
                    }
                }
                Phase::PositionAppend(point) => match self.positions.step(work, cancel)? {
                    ParseStep::Event(()) => self.phase = Phase::Point(*point),
                    other => return crate::step::pending(other),
                },
                Phase::ReserveMerge { ticks } => {
                    self.output.reserve(self.positions.len())?;
                    self.phase = Phase::WaitReserve { ticks: *ticks };
                }
                Phase::WaitReserve { ticks } => match self.output.step(work, cancel)? {
                    ParseStep::Event(()) => {
                        self.phase = Phase::Merge {
                            ticks: *ticks,
                            at: 0,
                        };
                    }
                    other => return crate::step::pending(other),
                },
                Phase::Merge { ticks, at } => {
                    if *at == self.positions.len() {
                        self.positions.reset();
                        self.phase = Phase::Root;
                        continue;
                    }
                    if !work.try_charge(1) {
                        return Ok(ParseStep::Yield);
                    }
                    let p = self.positions.as_slice()[*at];
                    self.output.push(Cue {
                        ticks: *ticks,
                        track: p.track,
                        cluster: p.cluster,
                        relative: p.relative,
                        block: p.block,
                        codec_state: p.codec_state,
                    })?;
                    self.phase = Phase::WaitMerge {
                        ticks: *ticks,
                        at: *at,
                    };
                }
                Phase::WaitMerge { ticks, at } => match self.output.step(work, cancel)? {
                    ParseStep::Event(()) => {
                        self.phase = Phase::Merge {
                            ticks: *ticks,
                            at: *at + 1,
                        };
                    }
                    other => return crate::step::pending(other),
                },
                Phase::Done => return Ok(ParseStep::Finished),
            }
        }
    }
}

crate::storage::usage::storage!(CuesJob, |s| [s.source, s.positions, s.output]);

#[cfg(test)]
mod tests;

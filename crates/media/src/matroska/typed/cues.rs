//! Real resumable CuePoint parsing/transaction assembly. Output is explicitly
//! source-ordered; index normalization/tie policy is NOT declared complete here.
use super::{
    copy_vec::CopyVec,
    syntax::{ChildCursor, check},
};
use crate::matroska::typed::syntax::Required;
use crate::{Error, Result, cues::Cue, ebml, incremental::ParseStep, storage::SharedBytes};
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
#[derive(Debug)]
enum Phase {
    Root,
    Point,
    Position,
    PositionAppend,
    ReserveMerge,
    WaitReserve,
    Merge,
    WaitMerge,
    Done,
}
#[derive(Debug)]
pub(crate) struct CuesJob {
    source: SharedBytes,
    segment: u64,
    segment_end: Option<u64>,
    limit: usize,
    root: ChildCursor,
    point: Option<ChildCursor>,
    position: Option<ChildCursor>,
    ticks: Option<u64>,
    fields: [Option<u64>; 5],
    positions: CopyVec<Position>,
    output: Option<CopyVec<Cue>>,
    merge: usize,
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
            point: None,
            position: None,
            ticks: None,
            fields: [None; 5],
            positions: CopyVec::new(pool),
            output: Some(CopyVec::new(pool)),
            merge: 0,
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
            Err(Error::Backpressure {
                requested,
                available,
            }) => Ok(ParseStep::Backpressure {
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
            match self.phase {
                Phase::Root => match self.root.step(&self.source, work, cancel)? {
                    ParseStep::Yield => return Ok(ParseStep::Yield),
                    ParseStep::Event(None) => {
                        let output = self.output.take().required("missing Cues arena")?;
                        self.phase = Phase::Done;
                        return Ok(ParseStep::Event(SourceOrderedCues {
                            source: self.source.clone(),
                            entries: output.finish()?,
                        }));
                    }
                    ParseStep::Event(Some(child)) => {
                        if child.id == 0xbb {
                            self.point = Some(ChildCursor::new(child.payload));
                            self.positions.reset();
                            self.ticks = None;
                            self.phase = Phase::Point;
                        }
                    }
                    _ => return Err(Error::Invalid("invalid Cues root transition")),
                },
                Phase::Point => match self
                    .point
                    .as_mut()
                    .required("missing CuePoint cursor")?
                    .step(&self.source, work, cancel)?
                {
                    ParseStep::Yield => return Ok(ParseStep::Yield),
                    ParseStep::Event(None) => {
                        if self.ticks.is_none() {
                            return Err(Error::Invalid("missing CueTime"));
                        }
                        if self.positions.len() == 0 {
                            return Err(Error::Invalid("missing CueTrackPositions"));
                        }
                        self.phase = Phase::ReserveMerge;
                    }
                    ParseStep::Event(Some(child)) => match child.id {
                        0xb3 => {
                            if self
                                .ticks
                                .replace(ebml::uint(&self.source[child.payload])?)
                                .is_some()
                            {
                                return Err(Error::Invalid("duplicate CueTime"));
                            }
                        }
                        0xb7 => {
                            if self
                                .output
                                .as_ref()
                                .required("missing Cues arena")?
                                .len()
                                .checked_add(self.positions.len())
                                .ok_or(Error::Overflow)?
                                >= self.limit
                            {
                                return Err(Error::Policy("cue entries"));
                            }
                            self.fields = [None; 5];
                            self.position = Some(ChildCursor::new(child.payload));
                            self.phase = Phase::Position;
                        }
                        _ => (),
                    },
                    _ => return Err(Error::Invalid("invalid CuePoint transition")),
                },
                Phase::Position => match self
                    .position
                    .as_mut()
                    .required("missing cue position cursor")?
                    .step(&self.source, work, cancel)?
                {
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
                        if self.fields[index]
                            .replace(ebml::uint(&self.source[child.payload])?)
                            .is_some()
                        {
                            return Err(Error::Invalid("duplicate cue position field"));
                        }
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
                        self.phase = Phase::PositionAppend;
                    }
                    _ => return Err(Error::Invalid("invalid cue position transition")),
                },
                Phase::PositionAppend => match self.positions.step(work, cancel)? {
                    ParseStep::Event(()) => self.phase = Phase::Point,
                    other => return super::pending(other),
                },
                Phase::ReserveMerge => {
                    self.output
                        .as_mut()
                        .required("missing Cues arena")?
                        .reserve(self.positions.len())?;
                    self.phase = Phase::WaitReserve;
                }
                Phase::WaitReserve => match self
                    .output
                    .as_mut()
                    .required("missing Cues arena")?
                    .step(work, cancel)?
                {
                    ParseStep::Event(()) => {
                        self.merge = 0;
                        self.phase = Phase::Merge;
                    }
                    other => return super::pending(other),
                },
                Phase::Merge => {
                    if self.merge == self.positions.len() {
                        self.positions.reset();
                        self.point = None;
                        self.phase = Phase::Root;
                        continue;
                    }
                    if !work.try_charge(1) {
                        return Ok(ParseStep::Yield);
                    }
                    let p = self.positions.as_slice()[self.merge];
                    self.output
                        .as_mut()
                        .required("missing Cues arena")?
                        .push(Cue {
                            ticks: self.ticks.required("missing CueTime")?,
                            track: p.track,
                            cluster: p.cluster,
                            relative: p.relative,
                            block: p.block,
                            codec_state: p.codec_state,
                        })?;
                    self.phase = Phase::WaitMerge;
                }
                Phase::WaitMerge => match self
                    .output
                    .as_mut()
                    .required("missing Cues arena")?
                    .step(work, cancel)?
                {
                    ParseStep::Event(()) => {
                        self.merge += 1;
                        self.phase = Phase::Merge;
                    }
                    other => return super::pending(other),
                },
                Phase::Done => return Ok(ParseStep::Finished),
            }
        }
    }
}

super::usage::storage!(CuesJob, |s| [s.source, s.positions, s.output]);

#[cfg(test)]
mod cue_arena_regressions;

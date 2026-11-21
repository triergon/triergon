//! Bounded retention of a selected track's already validated seek suffix.
use super::{Packet, ReaderEvent, TraceEvent};
use crate::reader::{SourceEvent, Track};
use triergon_core::container::{Event, SeekReceipt};
use triergon_core::{Budget, BudgetVec, BudgetVecIntoIter};

type Output = Event<Packet, ReaderEvent>;

pub(super) struct Retain {
    tables: BudgetVec<SourceEvent>,
    tail: BudgetVec<Output>,
    table_end: usize,
    map_epoch: Option<u64>,
    started: bool,
    previous_picture: bool,
    pub(super) skipped: bool,
}

pub(super) struct RetainedOutput {
    tables: BudgetVecIntoIter<SourceEvent>,
    tail: BudgetVecIntoIter<Output>,
    landing: Option<SeekReceipt>,
    tracks: Option<BudgetVec<Track>>,
}

impl Retain {
    pub(super) fn new(pool: &Budget) -> Self {
        Self {
            tables: BudgetVec::new(pool),
            tail: BudgetVec::new(pool),
            table_end: 0,
            map_epoch: None,
            started: false,
            previous_picture: false,
            skipped: false,
        }
    }

    pub(super) fn packet(&mut self, packet: Packet, better: bool) -> bool {
        if better {
            self.tail.clear();
            self.table_end = self.tables.len();
            self.started = true;
            self.skipped = self.previous_picture;
        }
        self.previous_picture = true;
        self.append(Event::Packet(packet))
    }

    pub(super) fn other(&mut self, event: ReaderEvent, tracks_present: bool) -> bool {
        if let ReaderEvent::Source(source) = &event {
            let table = match source.event() {
                TraceEvent::ProgramMap {
                    active: true,
                    configuration_epoch,
                    ..
                } => {
                    if *self.map_epoch.get_or_insert(*configuration_epoch) != *configuration_epoch {
                        return false;
                    }
                    true
                }
                TraceEvent::Pat(_) if self.map_epoch.is_some() && !tracks_present => return false,
                TraceEvent::Pat(_) | TraceEvent::ProgramMap { .. } => true,
                _ => false,
            };
            if table && (self.tables.len() == 32 || self.tables.try_push(source.clone()).is_err()) {
                return false;
            }
        }
        self.append(Event::Other(event))
    }

    fn append(&mut self, event: Output) -> bool {
        !self.started || (self.tail.len() < 2048 && self.tail.try_push(event).is_ok())
    }

    pub(super) fn finish(
        mut self,
        landing: SeekReceipt,
        tracks: BudgetVec<Track>,
    ) -> RetainedOutput {
        self.tables.truncate(self.table_end);
        RetainedOutput {
            tables: self.tables.into_iter(),
            tail: self.tail.into_iter(),
            landing: Some(landing),
            tracks: Some(tracks),
        }
    }
}

impl RetainedOutput {
    pub(super) fn next(&mut self, tracks: &mut BudgetVec<Track>) -> Option<Output> {
        if let Some(table) = self.tables.next() {
            if matches!(table.event(), TraceEvent::ProgramMap { active: true, .. })
                && let Some(snapshot) = self.tracks.take()
            {
                *tracks = snapshot;
            }
            return Some(Event::Other(ReaderEvent::Source(table)));
        }
        self.landing
            .take()
            .map(Event::Seek)
            .or_else(|| self.tail.next())
    }
}

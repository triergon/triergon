//! A bounded seek hint derived from one complete sequential parse of immutable bytes.
use super::*;
use triergon_core::{Bytes, Reservation};

const MAX_ENTRIES: usize = 4096;

#[derive(Clone, Copy, Debug)]
pub(super) struct IndexedKeyframe {
    pub timestamp_ns: i64,
    pub cluster: u64,
}

#[derive(Debug)]
pub(super) struct ImmutableSeekIndex {
    root: Bytes,
    entries: BudgetVec<IndexedKeyframe>,
    track: Option<usize>,
    limit: usize,
    complete: bool,
    charge: Reservation,
}

impl ImmutableSeekIndex {
    pub(super) fn new(root: Bytes, pool: &Budget, limit: usize) -> Result<Box<Self>> {
        let charge = pool.reserve(size_of::<Self>())?;
        Ok(Box::new(Self {
            root,
            entries: BudgetVec::new(pool),
            track: None,
            limit: limit.min(MAX_ENTRIES),
            complete: false,
            charge,
        }))
    }

    pub(super) fn verify(&self, window: &InputWindow) -> Result<()> {
        let start = usize::try_from(window.offset).map_err(|_| Error::Overflow)?;
        let end = start
            .checked_add(window.bytes().len())
            .ok_or(Error::Overflow)?;
        if self.root.shares_storage_with(window.bytes())
            && self
                .root
                .as_ref()
                .get(start..end)
                .is_some_and(|span| std::ptr::eq(span, window.bytes().as_ref()))
        {
            return Ok(());
        }
        Err(Error::SourceMismatch)
    }

    pub(super) fn reset(&mut self, limit: usize) {
        self.entries = BudgetVec::new(self.entries.budget());
        self.track = None;
        self.limit = limit.min(MAX_ENTRIES);
        self.complete = false;
    }

    pub(super) fn discard(&mut self) {
        self.reset(0);
    }

    pub(super) fn is_collecting(&self) -> bool {
        self.limit != 0 && !self.complete
    }

    pub(super) fn is_complete(&self) -> bool {
        self.complete
    }

    pub(super) fn seal(&mut self, position: u64) {
        if u64::try_from(self.root.len()) == Ok(position) && !self.entries.is_empty() {
            self.complete = true;
        }
    }

    pub(super) fn observe(
        &mut self,
        packet: &super::super::Packet,
        cluster: u64,
        work: &mut WorkBudget,
    ) {
        let packet = &packet.packet;
        let Some(timestamp_ns) = packet.pts_ns else {
            return;
        };
        if !packet.keyframe || packet.timing.frame != 0 {
            return;
        }
        if self.track.is_some_and(|track| track != packet.track) {
            self.discard();
            return;
        }
        if let Some(last) = self.entries.as_slice().last() {
            // Equal timestamps select the first physical Block in the scan.
            if last.timestamp_ns == timestamp_ns {
                return;
            }
            if last.timestamp_ns > timestamp_ns {
                self.discard();
                return;
            }
        }
        let len = self.entries.len();
        if len == self.limit {
            self.discard();
            return;
        }
        if len == self.entries.capacity() {
            let capacity = (len * 2).max(4).min(self.limit);
            // A cache may use spare work, but may never hold up packet delivery.
            // Account for both the allocation and the relocation of old records.
            if !work.try_charge(len as u64 + 1)
                || self.entries.try_reserve_exact(capacity - len).is_err()
            {
                self.discard();
                return;
            }
        }
        if self
            .entries
            .try_push(IndexedKeyframe {
                timestamp_ns,
                cluster,
            })
            .is_err()
        {
            self.discard();
            return;
        }
        self.track = Some(packet.track);
    }

    pub(super) fn landing(
        &self,
        track: usize,
        policy: crate::matroska::typed::seek::ScanPlan,
    ) -> Option<IndexedKeyframe> {
        if !self.complete || self.track != Some(track) {
            return None;
        }
        let entries = self.entries.as_slice();
        let previous = |target| {
            entries
                .partition_point(|entry| entry.timestamp_ns <= target)
                .checked_sub(1)
                .map(|at| entries[at])
        };
        previous(policy.target_ns).or_else(|| {
            if policy.allow_first_after_negative_preroll {
                previous(0)
            } else {
                None
            }
        })
    }
}

crate::matroska::typed::usage::storage!(ImmutableSeekIndex, |s| [s.root, s.entries, s.charge]);

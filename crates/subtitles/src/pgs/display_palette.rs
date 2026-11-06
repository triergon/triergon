use super::display_model::{
    ColourProvenance, NativeColourOrigin, NativeMatrix, PaletteProvenanceOwner, PaletteSnapshot,
    PaletteStorage, SegmentStamp, StoredSourceColour,
};
use super::display_work::DisplayWork;
use std::num::NonZeroU16;
use std::sync::Arc;
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result};

#[cfg(test)]
#[path = "palette_compact_controls.rs"]
mod compact_storage_controls;

#[cfg(test)]
#[path = "palette_init_bulk_controls.rs"]
mod bulk_initialisation_controls;

#[derive(Clone, Copy, Debug)]
enum Phase {
    ReserveColours,
    AllocateColours,
    ReserveWords,
    AllocateWords,
    ReserveOrigins,
    AllocateOrigins,
    ReserveOwners,
    AllocateOwners,
    Initialise,
    Provenance,
    Entries,
    Owner,
    Ready,
}

#[derive(Debug)]
pub(crate) struct PaletteBuild {
    budget: Budget,
    payload: Bytes,
    id: u8,
    version: u8,
    epoch: u64,
    stamp: SegmentStamp,
    matrix: NativeMatrix,
    previous: Option<PaletteSnapshot>,
    preserve_source: bool,
    colours: BudgetVec<StoredSourceColour>,
    words: BudgetVec<u32>,
    origins: BudgetVec<NativeColourOrigin>,
    has_source: bool,
    first_owner: Option<PaletteProvenanceOwner>,
    other_owners: BudgetVec<Option<PaletteProvenanceOwner>>,
    free_owner: Option<NonZeroU16>,
    incoming_owner: Option<NonZeroU16>,
    phase: Phase,
    credit: usize,
    entry: usize,
    result: Option<PaletteSnapshot>,
    provenance: Option<Arc<ColourProvenance>>,
}

impl PaletteBuild {
    pub fn new(
        budget: &Budget,
        payload: Bytes,
        epoch: u64,
        stamp: SegmentStamp,
        matrix: NativeMatrix,
        previous: Option<PaletteSnapshot>,
        preserve_source: bool,
    ) -> Result<Self> {
        if payload.len() < 2 {
            return Err(Error::Truncated);
        }
        if !(payload.len() - 2).is_multiple_of(5) {
            return Err(Error::Invalid("PGS partial palette entry"));
        }
        if !payload.belongs_to(budget) || previous.as_ref().is_some_and(|p| !p.belongs_to(budget)) {
            return Err(Error::ForeignPool);
        }
        let has_source = payload.len() > 2
            || (preserve_source && previous.as_ref().is_some_and(|p| !p.0.colours.is_empty()));
        Ok(Self {
            budget: budget.clone(),
            id: payload[0],
            version: payload[1],
            payload,
            epoch,
            stamp,
            matrix,
            previous,
            preserve_source,
            colours: BudgetVec::new(budget),
            words: BudgetVec::new(budget),
            origins: BudgetVec::new(budget),
            has_source,
            first_owner: None,
            other_owners: BudgetVec::new(budget),
            free_owner: None,
            incoming_owner: None,
            phase: Phase::ReserveColours,
            credit: 0,
            entry: 0,
            result: None,
            provenance: None,
        })
    }

    pub fn step(&mut self, work: &mut DisplayWork<'_>) -> Result<bool> {
        if work.cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        // A pressure retry with an already funded allocation still cannot
        // allocate or mutate private storage when this call offers no work.
        if work.caller.remaining() == 0 {
            return Ok(false);
        }
        loop {
            match self.phase {
                Phase::ReserveColours => {
                    if !work.pay(&mut self.credit, 1)? {
                        return Ok(false);
                    }
                    self.phase = Phase::AllocateColours;
                }
                Phase::AllocateColours => {
                    if self.has_source {
                        self.colours.try_reserve_exact(256)?;
                    }
                    self.credit = 0;
                    self.phase = Phase::ReserveWords;
                }
                Phase::ReserveWords => {
                    if !work.pay(&mut self.credit, 1)? {
                        return Ok(false);
                    }
                    self.phase = Phase::AllocateWords;
                }
                Phase::AllocateWords => {
                    self.words.try_reserve_exact(256)?;
                    self.credit = 0;
                    self.phase = Phase::ReserveOrigins;
                }
                Phase::ReserveOrigins => {
                    if !work.pay(&mut self.credit, 1)? {
                        return Ok(false);
                    }
                    self.phase = Phase::AllocateOrigins;
                }
                Phase::AllocateOrigins => {
                    self.origins.try_reserve_exact(256)?;
                    self.credit = 0;
                    self.phase = Phase::ReserveOwners;
                }
                Phase::ReserveOwners => {
                    if !work.pay(&mut self.credit, 1)? {
                        return Ok(false);
                    }
                    self.phase = Phase::AllocateOwners;
                }
                Phase::AllocateOwners => {
                    let count = self
                        .previous
                        .as_ref()
                        .filter(|_| self.preserve_source)
                        .map_or(0, |p| p.0.other_owners.len());
                    self.other_owners.try_reserve_exact(count)?;
                    self.credit = 0;
                    self.phase = Phase::Initialise;
                }
                Phase::Initialise => {
                    // The allocation phases reserved every present column.
                    // Accumulate the same256 cell units before touching any
                    // private cells or cloning their distinct provenance owners.
                    // Copy/default each complete column once; these Copy APIs
                    // cannot grow an allocation within the reserved capacity.
                    if !work.pay(&mut self.credit, 256)? {
                        return Ok(false);
                    }
                    let previous = self.previous.as_ref();
                    if self.has_source {
                        if let Some(p) =
                            previous.filter(|p| self.preserve_source && !p.0.colours.is_empty())
                        {
                            self.colours
                                .try_extend_from_slice(&p.0.colours.as_slice()[..256])?;
                        } else {
                            self.colours
                                .try_resize(256, StoredSourceColour::default())?;
                        }
                    }
                    if let Some(p) = previous {
                        self.words
                            .try_extend_from_slice(&p.0.words.as_slice()[..256])?;
                        if self.preserve_source {
                            self.origins
                                .try_extend_from_slice(&p.0.origins.as_slice()[..256])?;
                        } else {
                            let mut origins = [NativeColourOrigin::ReceiverZeroInitialization; 256];
                            for (new, old) in origins.iter_mut().zip(&p.0.origins.as_slice()[..256])
                            {
                                if *old != NativeColourOrigin::ReceiverZeroInitialization {
                                    *new = NativeColourOrigin::PreviousEpochSlot;
                                }
                            }
                            self.origins.try_extend_from_slice(&origins)?;
                        }
                    } else {
                        self.words.try_resize(256, 0)?;
                        self.origins
                            .try_resize(256, NativeColourOrigin::ReceiverZeroInitialization)?;
                    }
                    // The same funded cell range carries each live provenance
                    // owner once. Colour cells themselves contain no Arc/drop.
                    let carried = previous.filter(|_| self.preserve_source);
                    self.first_owner = carried.and_then(|p| p.0.first_owner.as_ref()).cloned();
                    if self.first_owner.is_none() {
                        self.free_owner = NonZeroU16::new(1);
                    }
                    if let Some(p) = carried {
                        // There are at most255 actual secondary slots. Empty
                        // colour positions have no provenance owner to clone
                        // or drop. The full256cell writes above remain funded.
                        for slot in 1..p.0.other_owners.len() + 1 {
                            let owner = p.0.other_owners.as_slice()[slot - 1].clone();
                            if owner.is_none() && self.free_owner.is_none() {
                                self.free_owner = NonZeroU16::new((slot + 1) as u16);
                            }
                            self.other_owners.try_push(owner).map_err(|e| e.error)?;
                        }
                    }
                    self.credit = 0;
                    self.phase = Phase::Provenance;
                }
                Phase::Provenance => {
                    if self.payload.len() == 2 {
                        self.phase = Phase::Entries;
                        continue;
                    }
                    if !work.pay(&mut self.credit, 8)? {
                        return Ok(false);
                    }
                    let bytes = std::mem::size_of::<ColourProvenance>()
                        .checked_add(2 * std::mem::size_of::<usize>())
                        .ok_or(Error::Overflow)?;
                    let charge = self.budget.reserve(bytes)?;
                    self.provenance = Some(Arc::new(ColourProvenance {
                        palette_version: self.version,
                        epoch: self.epoch,
                        stamp: self.stamp,
                        receiver_matrix: self.matrix,
                        _owner_charge: charge,
                    }));
                    // The first entry's existing eight units cover its shared
                    // provenance allocation; Entries consumes the paid credit.
                    self.phase = Phase::Entries;
                }
                Phase::Entries => {
                    let at = 2 + self.entry * 5;
                    if at == self.payload.len() {
                        self.phase = Phase::Owner;
                        continue;
                    }
                    let b = &self.payload[at..at + 5];
                    let index = usize::from(b[0]);
                    let old = self.colours.as_slice()[index].provenance;
                    let reusable = old.filter(|id| self.owner(*id).entries == 1);
                    let selected = self.incoming_owner.or(self.free_owner).or(reusable);
                    let grows = selected.is_none();
                    let move_cost =
                        if grows && self.other_owners.len() == self.other_owners.capacity() {
                            self.other_owners.len() + 1
                        } else {
                            0
                        };
                    if !work.pay(&mut self.credit, 8 + move_cost)? {
                        return Ok(false);
                    }
                    // Reserve before releasing an old entry. A failed reserve
                    // preserves the exact entry/counts and paid retry state.
                    if grows {
                        if self.other_owners.len() == 255 {
                            return Err(Error::Invalid("PGS palette live owner bound"));
                        }
                        self.other_owners.try_reserve_exact(1)?;
                    }
                    if self.incoming_owner.is_none() || old != self.incoming_owner {
                        if let Some(id) = old {
                            let owner = self
                                .owner_mut(id)
                                .as_mut()
                                .ok_or(Error::Invalid("PGS palette old owner absent"))?;
                            owner.entries = owner.entries.checked_sub(1).ok_or(Error::Overflow)?;
                            if owner.entries == 0 {
                                *self.owner_mut(id) = None;
                                if self.free_owner.is_none() {
                                    self.free_owner = Some(id);
                                }
                            }
                        }
                        if self.incoming_owner.is_none() {
                            let id = selected.unwrap_or_else(|| {
                                NonZeroU16::new((self.other_owners.len() + 2) as u16).unwrap()
                            });
                            let owner = PaletteProvenanceOwner {
                                provenance: self
                                    .provenance
                                    .as_ref()
                                    .cloned()
                                    .ok_or(Error::Invalid("PGS palette provenance"))?,
                                entries: 0,
                            };
                            if grows {
                                self.other_owners
                                    .try_push(Some(owner))
                                    .map_err(|e| e.error)?;
                            } else {
                                *self.owner_mut(id) = Some(owner);
                            }
                            if self.free_owner == Some(id) {
                                self.free_owner = None;
                            }
                            self.incoming_owner = Some(id);
                        }
                        let id = self
                            .incoming_owner
                            .ok_or(Error::Invalid("PGS palette incoming owner absent"))?;
                        let owner = self
                            .owner_mut(id)
                            .as_mut()
                            .ok_or(Error::Invalid("PGS palette owner absent"))?;
                        owner.entries = owner.entries.checked_add(1).ok_or(Error::Overflow)?;
                    }
                    let b = &self.payload[at..at + 5];
                    self.colours.as_mut_slice()[index] = StoredSourceColour {
                        components: [b[1], b[2], b[3], b[4]],
                        provenance: self.incoming_owner,
                    };
                    self.words.as_mut_slice()[index] = self.matrix.argb(b[1], b[2], b[3], b[4]);
                    self.origins.as_mut_slice()[index] = NativeColourOrigin::ExplicitSource;
                    self.entry += 1;
                    self.credit = 0;
                }
                Phase::Owner => {
                    // Fund the bounded prior snapshot retirement as well as
                    // the new owner. Caller-held immutable snapshots survive.
                    let retired = self.previous.as_ref().map_or(0, |p| {
                        p.0.other_owners.len() + usize::from(p.0.first_owner.is_some())
                    });
                    if !work.pay(&mut self.credit, 1 + retired)? {
                        return Ok(false);
                    }
                    let bytes = std::mem::size_of::<PaletteStorage>()
                        .checked_add(2 * std::mem::size_of::<usize>())
                        .ok_or(Error::Overflow)?;
                    let charge = self.budget.reserve(bytes)?;
                    let storage = PaletteStorage {
                        budget: self.budget.clone(),
                        id: self.id,
                        version: self.version,
                        epoch: self.epoch,
                        colours: std::mem::replace(&mut self.colours, BudgetVec::new(&self.budget)),
                        words: std::mem::replace(&mut self.words, BudgetVec::new(&self.budget)),
                        origins: std::mem::replace(&mut self.origins, BudgetVec::new(&self.budget)),
                        first_owner: self.first_owner.take(),
                        other_owners: std::mem::replace(
                            &mut self.other_owners,
                            BudgetVec::new(&self.budget),
                        ),
                        _owner_charge: charge,
                    };
                    self.result = Some(PaletteSnapshot(Arc::new(storage)));
                    self.previous = None;
                    self.provenance = None;
                    self.phase = Phase::Ready;
                }
                Phase::Ready => return Ok(true),
            }
        }
    }

    pub fn take(&mut self) -> Result<PaletteSnapshot> {
        self.result
            .take()
            .ok_or(Error::Invalid("PGS palette not ready"))
    }

    fn owner(&self, id: NonZeroU16) -> &PaletteProvenanceOwner {
        if id.get() == 1 {
            self.first_owner.as_ref()
        } else {
            self.other_owners.as_slice()[usize::from(id.get() - 2)].as_ref()
        }
        .expect("PGS palette live owner invariant")
    }

    fn owner_mut(&mut self, id: NonZeroU16) -> &mut Option<PaletteProvenanceOwner> {
        if id.get() == 1 {
            &mut self.first_owner
        } else {
            &mut self.other_owners.as_mut_slice()[usize::from(id.get() - 2)]
        }
    }
}

#[cfg(test)]
mod storage_tests {
    use super::*;
    use crate::pgs::{Envelope, PacketClock};
    use std::sync::atomic::{AtomicBool, Ordering};
    use triergon_core::{TimeBase, WorkBudget};

    fn stamp(sequence: u64) -> SegmentStamp {
        SegmentStamp {
            packet_sequence: sequence,
            packet_clock: PacketClock {
                pts: (sequence != 0).then_some(sequence as i64 * 900),
                dts: Some(-17),
                timebase: TimeBase::new(1, 90_000).unwrap(),
            },
            sup_clock: None,
            packet_offset: 13,
            envelope: Envelope::Packet,
        }
    }

    fn payload(version: u8, entries: &[[u8; 5]]) -> Vec<u8> {
        let mut bytes = vec![3, version];
        for entry in entries {
            bytes.extend_from_slice(entry);
        }
        bytes
    }

    fn build(
        budget: &Budget,
        version: u8,
        sequence: u64,
        entries: &[[u8; 5]],
        previous: Option<PaletteSnapshot>,
        preserve: bool,
        quantum: u64,
    ) -> PaletteSnapshot {
        let mut job = PaletteBuild::new(
            budget,
            Bytes::copy_from_slice(budget, &payload(version, entries)).unwrap(),
            sequence + 7,
            stamp(sequence),
            NativeMatrix::for_canvas(if sequence == 0 { 480 } else { 1080 }),
            previous,
            preserve,
        )
        .unwrap();
        let cancel = AtomicBool::new(false);
        let mut spent = 0;
        for _ in 0..20_000 {
            let mut caller = WorkBudget::new(quantum);
            if job
                .step(&mut DisplayWork {
                    caller: &mut caller,
                    spent: &mut spent,
                    maximum: 1_000_000,
                    cancel: &cancel,
                })
                .unwrap()
            {
                return job.take().unwrap();
            }
        }
        panic!("bounded palette storage job did not finish");
    }

    #[test]
    fn shared_provenance_keeps_every_entry_and_all_work_quanta() {
        for quantum in [1, 2, 3, 7, 4096] {
            let budget = Budget::new(1 << 20).unwrap();
            let baseline = budget.used();
            let p = build(
                &budget,
                9,
                0,
                &[[7, 16, 128, 128, 255], [8, 32, 1, 254, 0]],
                None,
                false,
                quantum,
            );
            assert!(Arc::ptr_eq(
                p.0.source_provenance(7).unwrap(),
                p.0.source_provenance(8).unwrap(),
            ));
            let source = p.source(7).unwrap();
            assert_eq!(
                (source.y, source.cr, source.cb, source.alpha),
                (16, 128, 128, 255)
            );
            assert_eq!(
                (source.palette_version, source.epoch, source.stamp),
                (9, 7, stamp(0))
            );
            assert_eq!(source.receiver_matrix, NativeMatrix::for_canvas(480));
            assert_eq!(p.source(0), None);
            assert_eq!(
                p.native_origin(0),
                NativeColourOrigin::ReceiverZeroInitialization
            );
            drop(p);
            assert_eq!(budget.used(), baseline);
        }
    }

    #[test]
    fn partial_update_shares_old_owner_and_keeps_original_clock_and_matrix() {
        let budget = Budget::new(1 << 20).unwrap();
        let a = build(
            &budget,
            4,
            0,
            &[[7, 16, 128, 128, 255], [8, 32, 1, 254, 0]],
            None,
            false,
            1,
        );
        let old = a.source(8);
        let b = build(
            &budget,
            5,
            11,
            &[[7, 40, 90, 170, 63]],
            Some(a.clone()),
            true,
            1,
        );
        assert_eq!(b.source(8), old);
        assert_eq!(b.native_argb(8), a.native_argb(8));
        assert!(Arc::ptr_eq(
            a.0.source_provenance(8).unwrap(),
            b.0.source_provenance(8).unwrap(),
        ));
        assert_eq!(b.source(7).unwrap().stamp, stamp(11));
        assert_eq!(
            b.source(7).unwrap().receiver_matrix,
            NativeMatrix::for_canvas(1080)
        );
        assert_eq!(a.source(7).unwrap().stamp, stamp(0));
        let mut copy = b.source(8).unwrap();
        copy.stamp.packet_sequence = u64::MAX;
        assert_ne!(Some(copy), b.source(8));
    }

    #[test]
    fn epoch_receiver_carry_never_invents_absent_source() {
        let budget = Budget::new(1 << 20).unwrap();
        let a = build(&budget, 4, 0, &[[7, 16, 128, 128, 255]], None, false, 1);
        let b = build(&budget, 5, 1, &[], Some(a.clone()), false, 1);
        assert_eq!(b.source(7), None);
        assert_eq!(b.native_argb(7), a.native_argb(7));
        assert_eq!(b.native_origin(7), NativeColourOrigin::PreviousEpochSlot);
        assert_eq!(
            b.native_origin(8),
            NativeColourOrigin::ReceiverZeroInitialization
        );
    }

    #[test]
    fn retained_generations_release_all_metadata_back_to_original_baseline() {
        let budget = Budget::new(1 << 20).unwrap();
        let baseline = budget.used();
        let a = build(&budget, 4, 0, &[[7, 16, 128, 128, 255]], None, false, 4096);
        let b = build(
            &budget,
            5,
            1,
            &[[8, 40, 90, 170, 63]],
            Some(a.clone()),
            true,
            4096,
        );
        let clone = b.clone();
        drop(a);
        drop(b);
        assert!(budget.used() > baseline);
        assert_eq!(clone.source(7).unwrap().stamp, stamp(0));
        assert_eq!(clone.source(8).unwrap().stamp, stamp(1));
        drop(clone);
        assert_eq!(budget.used(), baseline);
    }

    #[test]
    fn zero_work_and_cancel_allocate_no_shared_metadata() {
        let budget = Budget::new(1 << 20).unwrap();
        let mut job = PaletteBuild::new(
            &budget,
            Bytes::copy_from_slice(&budget, &payload(1, &[[7, 16, 128, 128, 255]])).unwrap(),
            1,
            stamp(0),
            NativeMatrix::for_canvas(480),
            None,
            false,
        )
        .unwrap();
        let before = budget.used();
        let cancel = AtomicBool::new(false);
        let mut spent = 0;
        assert!(
            !job.step(&mut DisplayWork {
                caller: &mut WorkBudget::new(0),
                spent: &mut spent,
                maximum: 10000,
                cancel: &cancel
            })
            .unwrap()
        );
        assert_eq!(budget.used(), before);
        assert!(job.provenance.is_none());
        cancel.store(true, Ordering::Relaxed);
        assert_eq!(
            job.step(&mut DisplayWork {
                caller: &mut WorkBudget::new(1),
                spent: &mut spent,
                maximum: 10000,
                cancel: &cancel
            }),
            Err(Error::Cancelled)
        );
        assert_eq!(budget.used(), before);
    }

    #[test]
    fn provenance_pressure_retries_without_mutating_published_colours() {
        let budget = Budget::new(65536).unwrap();
        let baseline = budget.used();
        let mut job = PaletteBuild::new(
            &budget,
            Bytes::copy_from_slice(&budget, &payload(1, &[[7, 16, 128, 128, 255]])).unwrap(),
            1,
            stamp(0),
            NativeMatrix::for_canvas(480),
            None,
            false,
        )
        .unwrap();
        let cancel = AtomicBool::new(false);
        let mut spent = 0;
        for _ in 0..1000 {
            if matches!(job.phase, Phase::Provenance) {
                break;
            }
            assert!(
                !job.step(&mut DisplayWork {
                    caller: &mut WorkBudget::new(1),
                    spent: &mut spent,
                    maximum: 10000,
                    cancel: &cancel
                })
                .unwrap()
            );
        }
        assert!(matches!(job.phase, Phase::Provenance));
        let pressure = budget.reserve(65536 - budget.used()).unwrap();
        let before = budget.used();
        let result = job.step(&mut DisplayWork {
            caller: &mut WorkBudget::new(8),
            spent: &mut spent,
            maximum: 10000,
            cancel: &cancel,
        });
        assert!(matches!(result, Err(Error::Backpressure { .. })));
        assert!(job.provenance.is_none());
        assert!(
            job.colours
                .as_slice()
                .iter()
                .all(|colour| colour.provenance.is_none())
        );
        assert_eq!(budget.used(), before);
        drop(pressure);
        for _ in 0..100 {
            if job
                .step(&mut DisplayWork {
                    caller: &mut WorkBudget::new(1),
                    spent: &mut spent,
                    maximum: 10000,
                    cancel: &cancel,
                })
                .unwrap()
            {
                break;
            }
        }
        let p = job.take().unwrap();
        assert_eq!(p.source(7).unwrap().stamp, stamp(0));
        drop(p);
        drop(job);
        assert_eq!(budget.used(), baseline);
    }
}

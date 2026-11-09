//! The memory a packet may allocate, worked out before any of its segments is
//! applied, so that a packet the budget cannot hold is refused whole.
//!
//! The plan follows the packet's segments over what the decoder holds, with
//! the regions they change listed apart: region and CLUT slots, each
//! region's pixels and object placements, the page's region list, the output
//! slots and the automatic palette's census. A vector that grows holds its old and new allocation while it
//! copies, and a new epoch frees the regions of the old one. Where a segment's
//! effect depends on more than its own fields (an object's regions, which
//! regions are drawn), the plan takes the most it could allocate.

use super::epoch::{
    CLUT_BYTES, Epoch, Holdings, MAX_REGION_PIXELS, OBJECT_SET_BYTES, PLACEMENT_BYTES,
    REGION_BYTES, RegionHoldings,
};
use super::segment::{PageComposition, Placement, RegionComposition, Segments};
use super::{AutomaticPalette, Parsed, ReceiverDecoder, Slot, push};
use triergon_core::{BudgetVec, Result};

/// Bytes held beyond the start, and the most at any one time.
#[derive(Debug, Default)]
struct Tally {
    now: i128,
    peak: i128,
}

impl Tally {
    /// An allocation of `bytes` while everything counted so far is held.
    fn allocate(&mut self, bytes: usize) {
        self.peak = self.peak.max(self.now + bytes as i128);
        self.now += bytes as i128;
    }

    fn free(&mut self, bytes: usize) {
        self.now -= bytes as i128;
    }

    /// Grow a vector of `size`-byte elements, `(len, capacity)`, to hold
    /// `target`: the new allocation, then the old one freed.
    fn reserve(&mut self, slots: &mut (usize, usize), target: usize, size: usize) {
        if target > slots.1 {
            self.allocate(target * size);
            self.free(slots.1 * size);
            slots.1 = target;
        }
    }
}

/// The decoder's holdings as the packet's segments change them.
struct Plan<'a> {
    tally: Tally,
    /// Units of work spent planning: one per byte read and per region
    /// visited.
    work: u64,
    epoch: &'a Epoch,
    /// The number of regions the epoch holds.
    epoch_regions: usize,
    /// The epoch's vectors.
    held: Holdings,
    /// A new epoch replaced the regions and CLUTs the epoch holds.
    cleared: bool,
    /// The regions the packet composed, in the decoder's reused vector:
    /// these replace the epoch's of the same id.
    changed: &'a mut BudgetVec<RegionHoldings>,
    /// By region id: one more than its place in `changed`, or 0.
    index: [u16; 256],
    /// The CLUTs the packet defined, one bit per id.
    defined: [u64; 4],
    version: Option<u8>,
    /// The page's region list: its length and capacity, and its region ids,
    /// one bit each.
    listed: (usize, usize),
    listed_ids: [u64; 4],
    slots: (usize, usize),
    census: usize,
    automatic: bool,
    /// Pixels of a presentation not yet copied out of its regions.
    presented: Option<usize>,
    /// The packet presented already.
    shown: bool,
}

/// Whether bit `id` of a 256-bit set is set.
fn has(set: &[u64; 4], id: u8) -> bool {
    set[usize::from(id / 64)] & 1 << (id % 64) != 0
}

fn insert(set: &mut [u64; 4], id: u8) {
    set[usize::from(id / 64)] |= 1 << (id % 64);
}

impl Plan<'_> {
    /// The regions held at this point of the packet.
    fn regions(&self) -> impl Iterator<Item = RegionHoldings> + '_ {
        let kept = (!self.cleared).then(|| self.epoch.region_holdings());
        kept.into_iter()
            .flatten()
            .filter(|held| self.index[usize::from(held.id)] == 0)
            .chain(self.changed.iter().copied())
    }

    /// The work of visiting every held region.
    fn visit(&mut self) {
        let kept = if self.cleared { 0 } else { self.epoch_regions };
        self.work += (kept + self.changed.len()) as u64;
    }

    fn present(&mut self) {
        if self.version.is_none() || self.shown {
            return;
        }
        self.visit();
        // Only the regions the page lists are presented.
        let (pixels, values) = self
            .regions()
            .filter(|r| has(&self.listed_ids, r.id))
            .fold((0, 0), |(pixels, values), r| {
                (pixels + r.pixels.0, values.max(r.values))
            });
        // The census grows for each automatic palette between the slots
        // added: hold its largest size, and its old one, while they grow.
        // One count per pixel value and neighbour class.
        let words = (values + 1) * values;
        let census = self.automatic && words > self.census;
        if census {
            self.tally.allocate(words * size_of::<u32>());
        }
        // One slot per listed region at most, each added on its own.
        while self.slots.0 < self.listed.0 {
            let target = self.slots.0 + 1;
            self.tally
                .reserve(&mut self.slots, target, size_of::<Slot>());
            self.slots.0 = target;
        }
        if census {
            self.tally.free(self.census * size_of::<u32>());
            self.census = words;
        }
        self.presented = Some(pixels);
        self.shown = true;
    }

    /// The presented pixels copied out before the epoch changes.
    fn detach(&mut self) {
        if let Some(pixels) = self.presented.take() {
            self.tally.allocate(pixels);
        }
    }

    /// A new page version replaces the region list with `page`'s.
    fn list(&mut self, page: &PageComposition<'_>) {
        let mut ids = [0; 4];
        let mut count = 0;
        for placement in page.regions().take(256) {
            insert(&mut ids, placement.region);
            count += 1;
        }
        self.work += count as u64;
        let distinct = ids.iter().map(|w| w.count_ones() as usize).sum();
        let target = count.max(self.listed.0);
        self.tally
            .reserve(&mut self.listed, target, size_of::<Placement>());
        self.listed.0 = distinct;
        self.listed_ids = ids;
    }

    fn start_epoch(&mut self) {
        self.visit();
        let freed: usize = self
            .regions()
            .map(|r| r.pixels.1 + r.objects.1 * PLACEMENT_BYTES)
            .sum();
        self.tally.free(freed);
        self.cleared = true;
        self.changed.clear();
        self.index = [0; 256];
        self.held.region_slots.0 = 0;
        self.defined = [0; 4];
        self.held.clut_slots.0 = 0;
    }

    /// Where region `id` is in `changed`, copied there from the epoch on its
    /// first change; `None` for a region not held.
    fn change(&mut self, id: u8) -> Result<Option<usize>> {
        if let Some(at) = usize::from(self.index[usize::from(id)]).checked_sub(1) {
            return Ok(Some(at));
        }
        if self.cleared {
            return Ok(None);
        }
        self.work += self.epoch_regions as u64;
        let Some(kept) = self.epoch.region_holdings().find(|r| r.id == id) else {
            return Ok(None);
        };
        Ok(Some(self.add(kept)?))
    }

    /// Add a region to `changed`: its place there.
    fn add(&mut self, region: RegionHoldings) -> Result<usize> {
        push(self.changed, region)?;
        let at = self.changed.len() - 1;
        // At most 256 regions, one per id, so the place fits.
        self.index[usize::from(region.id)] = (at + 1) as u16;
        Ok(at)
    }

    /// `false` where the composition stops the packet.
    fn compose(&mut self, region: &RegionComposition<'_>) -> Result<bool> {
        let size = usize::from(region.width) * usize::from(region.height);
        if size == 0 || size > MAX_REGION_PIXELS {
            return Ok(false);
        }
        let placements = region.objects().count();
        let values = usize::from(region.depth.maximum()) + 1;
        if placements != 0 && !self.held.object_set {
            self.tally.allocate(OBJECT_SET_BYTES);
            self.held.object_set = true;
        }
        let index = self.change(region.id)?;
        if let Some(index) = index {
            let held = &mut self.changed[index];
            let target = placements.max(held.objects.0);
            self.tally
                .reserve(&mut held.objects, target, PLACEMENT_BYTES);
            held.objects.0 = placements;
            if size > held.pixels.1 {
                // A new allocation, filled before the old one is freed.
                self.tally.allocate(size);
                self.tally.free(held.pixels.1);
                held.pixels.1 = size;
            }
            held.pixels.0 = size;
            held.values = held.values.max(values);
        } else {
            let slots = &mut self.held.region_slots;
            self.tally.reserve(slots, slots.0 + 1, REGION_BYTES);
            slots.0 += 1;
            self.tally.allocate(placements * PLACEMENT_BYTES + size);
            self.add(RegionHoldings {
                id: region.id,
                pixels: (size, size),
                objects: (placements, placements),
                values,
            })?;
        }
        Ok(true)
    }

    fn define_clut(&mut self, id: u8) {
        let (word, bit) = (usize::from(id / 64), 1 << (id % 64));
        let defined = self.defined[word] & bit != 0 || !self.cleared && self.epoch.has_clut(id);
        if !defined {
            self.defined[word] |= bit;
            let slots = &mut self.held.clut_slots;
            self.tally.reserve(slots, slots.0 + 1, CLUT_BYTES);
            slots.0 += 1;
        }
    }
}

/// What a packet may allocate, and the work of working it out.
#[derive(Clone, Copy, Debug)]
pub(super) struct PacketPlan {
    pub(super) bytes: usize,
    pub(super) work: u64,
}

impl ReceiverDecoder {
    /// The most bytes decoding `packet` from its start may hold beyond what
    /// the decoder holds now. The plan's list of regions is charged to the
    /// budget and kept for the next packet.
    pub(super) fn packet_memory(&mut self, packet: &[u8]) -> Result<PacketPlan> {
        self.planned.clear();
        let mut listed_ids = [0; 4];
        for placement in self.page.placements.as_slice() {
            insert(&mut listed_ids, placement.region);
        }
        let held = self.epoch.holdings();
        let mut plan = Plan {
            tally: Tally::default(),
            work: self.page.placements.len() as u64,
            epoch: &self.epoch,
            epoch_regions: held.region_slots.0,
            held,
            cleared: false,
            changed: &mut self.planned,
            index: [0; 256],
            defined: [0; 4],
            version: self.page.version,
            listed: (self.page.placements.len(), self.page.placements.capacity()),
            listed_ids,
            slots: (self.slots.len(), self.slots.capacity()),
            census: self.census.capacity(),
            automatic: self.options.automatic_palette != AutomaticPalette::Never,
            presented: None,
            shown: false,
        };
        for segment in Segments::new(packet) {
            let Ok(segment) = segment else {
                break;
            };
            plan.work += 6 + segment.body.len() as u64;
            if !self.pages.admit(segment.page) {
                continue;
            }
            let parsed = Parsed::of(&segment);
            if parsed.changes_epoch() {
                plan.detach();
            }
            match parsed {
                Parsed::Page(page) if plan.version != Some(page.version) => {
                    if self.options.end_from_next {
                        plan.present();
                    }
                    let starts_epoch = page.state.starts_epoch();
                    if starts_epoch {
                        plan.detach();
                    }
                    plan.list(&page);
                    plan.version = Some(page.version);
                    if starts_epoch {
                        plan.start_epoch();
                    }
                }
                Parsed::Region(Some(region)) => {
                    if !plan.compose(&region)? {
                        break;
                    }
                }
                Parsed::Region(None) | Parsed::Object(None) | Parsed::Display(None) => break,
                Parsed::Clut(Some(clut)) => plan.define_clut(clut.id),
                Parsed::End if !self.options.end_from_next => plan.present(),
                _ => {}
            }
        }
        // The presentation at the end of the packet, at most.
        plan.present();
        Ok(PacketPlan {
            bytes: usize::try_from(plan.tally.peak).unwrap_or(usize::MAX),
            work: plan.work,
        })
    }
}

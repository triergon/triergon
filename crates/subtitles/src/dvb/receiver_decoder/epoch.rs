//! Regions, CLUTs and objects of the current epoch, and how the region,
//! CLUT and object segments change them.
use super::palette::{self, Census, DEFAULT, Tables};
use super::pixels::{self, Canvas, Pen};
use super::segment::{
    ClutDefinition, ObjectCoding, ObjectData, ObjectPlacement, RegionComposition,
};
use super::{Flow, push, push_exact};
use crate::dvb::Depth;
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, BudgetVec, Error, Result};

/// The largest region the receiver allocates.
pub(super) const MAX_REGION_PIXELS: usize = 1280 * 1024;

#[derive(Debug)]
pub(super) struct Region {
    id: u8,
    pub(super) width: u16,
    pub(super) height: u16,
    pub(super) depth: Depth,
    pub(super) clut: u8,
    /// Set once an object is drawn; only drawn regions are presented.
    drawn: bool,
    objects: BudgetVec<ObjectPlacement>,
    pixels: BudgetVec<u8>,
    /// The automatic palette as last computed, and whether it still holds.
    automatic: [u32; 256],
    automatic_current: bool,
    /// Every pixel value is below this: the widest depth composed since the
    /// pixels were last cleared.
    values: usize,
}

impl Region {
    fn new(id: u8, budget: &Budget) -> Self {
        Self {
            id,
            width: 0,
            height: 0,
            depth: Depth::Two,
            clut: 0,
            drawn: false,
            objects: BudgetVec::new(budget),
            pixels: BudgetVec::new(budget),
            automatic: [0; 256],
            automatic_current: false,
            values: 1,
        }
    }

    /// `width * height` palette indices, row by row.
    pub(super) fn pixels(&self) -> &[u8] {
        self.pixels.as_slice()
    }

    /// The work of presenting the automatic palette: one unit per pixel,
    /// unless the palette as last computed still holds.
    pub(super) fn automatic_palette_work(&self) -> u64 {
        if self.automatic_current {
            0
        } else {
            self.pixels.len() as u64
        }
    }

    /// Give the region a new size: zero pixels, and nothing drawn. A size
    /// that cannot be allocated changes nothing.
    fn resize(&mut self, width: u16, height: u16) -> Result<()> {
        let size = usize::from(width) * usize::from(height);
        if size > self.pixels.capacity() {
            self.pixels = BudgetVec::try_filled(self.pixels.budget(), 0, size)?;
        } else {
            self.pixels.try_resize(size, 0)?;
            self.pixels.as_mut_slice().fill(0);
        }
        self.width = width;
        self.height = height;
        self.drawn = false;
        self.values = 1;
        Ok(())
    }
}

/// The vectors an epoch holds, for [`memory`](super::memory) to plan a
/// packet's allocations against: each one's length and capacity.
#[derive(Clone, Copy, Debug)]
pub(super) struct Holdings {
    pub(super) region_slots: (usize, usize),
    pub(super) clut_slots: (usize, usize),
    pub(super) object_set: bool,
}

/// One region's allocations: its pixel count and capacity, and its object
/// placements' count and capacity, and the bound of its pixel values.
#[derive(Clone, Copy, Debug)]
pub(super) struct RegionHoldings {
    pub(super) id: u8,
    pub(super) pixels: (usize, usize),
    pub(super) objects: (usize, usize),
    /// Every pixel value is below this.
    pub(super) values: usize,
}

/// Bytes of one element of the epoch's vectors.
pub(super) const REGION_BYTES: usize = size_of::<Region>();
pub(super) const CLUT_BYTES: usize = size_of::<Clut>();
pub(super) const PLACEMENT_BYTES: usize = size_of::<ObjectPlacement>();
pub(super) const OBJECT_SET_BYTES: usize = ObjectSet::WORDS * size_of::<u64>();

#[derive(Debug)]
struct Clut {
    id: u8,
    version: u8,
    tables: Tables,
}

/// A set of object ids, one bit each, allocated on first use.
#[derive(Debug)]
struct ObjectSet {
    words: BudgetVec<u64>,
}

impl ObjectSet {
    const WORDS: usize = (u16::MAX as usize + 1) / 64;

    fn new(budget: &Budget) -> Self {
        Self {
            words: BudgetVec::new(budget),
        }
    }

    fn contains(&self, id: u16) -> bool {
        self.words
            .as_slice()
            .get(usize::from(id / 64))
            .is_some_and(|word| word >> (id % 64) & 1 == 1)
    }

    /// Allocate the set, so that [`insert`](Self::insert) cannot fail.
    fn reserve(&mut self) -> Result<()> {
        if self.words.is_empty() {
            self.words.try_resize(Self::WORDS, 0)?;
        }
        Ok(())
    }

    /// Insert `id` into a [reserved](Self::reserve) set.
    fn insert(&mut self, id: u16) {
        if let Some(word) = self.words.as_mut_slice().get_mut(usize::from(id / 64)) {
            *word |= 1 << (id % 64);
        }
    }

    fn clear(&mut self) {
        self.words.as_mut_slice().fill(0);
    }
}

#[derive(Debug)]
pub(super) struct Epoch {
    regions: BudgetVec<Region>,
    cluts: BudgetVec<Clut>,
    /// Objects a region composition has referenced.
    objects: ObjectSet,
    budget: Budget,
}

impl Epoch {
    pub(super) fn new(budget: &Budget) -> Self {
        Self {
            regions: BudgetVec::new(budget),
            cluts: BudgetVec::new(budget),
            objects: ObjectSet::new(budget),
            budget: budget.clone(),
        }
    }

    pub(super) fn clear(&mut self) {
        self.regions.clear();
        self.cluts.clear();
        self.objects.clear();
    }

    /// Region `id` and its index, when an object has been drawn into it.
    pub(super) fn drawn_region(&self, id: u8) -> Option<(usize, &Region)> {
        self.regions
            .as_slice()
            .iter()
            .enumerate()
            .find(|(_, r)| r.id == id)
            .filter(|(_, r)| r.drawn)
    }

    /// The bytes the epoch holds.
    pub(super) fn retained(&self) -> usize {
        let regions: usize = self
            .regions
            .iter()
            .map(|r| r.pixels.capacity() + r.objects.capacity() * PLACEMENT_BYTES)
            .sum();
        regions
            + self.regions.capacity() * REGION_BYTES
            + self.cluts.capacity() * CLUT_BYTES
            + self.objects.words.capacity() * size_of::<u64>()
    }

    pub(super) fn holdings(&self) -> Holdings {
        Holdings {
            region_slots: (self.regions.len(), self.regions.capacity()),
            clut_slots: (self.cluts.len(), self.cluts.capacity()),
            object_set: !self.objects.words.is_empty(),
        }
    }

    pub(super) fn region_holdings(&self) -> impl Iterator<Item = RegionHoldings> + '_ {
        self.regions.iter().map(|region| RegionHoldings {
            id: region.id,
            pixels: (region.pixels.len(), region.pixels.capacity()),
            objects: (region.objects.len(), region.objects.capacity()),
            values: region.values,
        })
    }

    pub(super) fn has_clut(&self, id: u8) -> bool {
        self.cluts.iter().any(|clut| clut.id == id)
    }

    pub(super) fn region_at(&self, index: usize) -> &Region {
        &self.regions.as_slice()[index]
    }

    /// Region `index`'s automatic palette, recomputed when an object was
    /// drawn into the region since the last computation.
    pub(super) fn automatic_palette(
        &mut self,
        index: usize,
        census: &mut Census,
    ) -> Result<&[u32; 256]> {
        let region = &mut self.regions.as_mut_slice()[index];
        if !region.automatic_current {
            let width = usize::from(region.width);
            palette::automatic(
                region.pixels.as_slice(),
                width,
                census,
                &mut region.automatic,
            )?;
            region.automatic_current = true;
        }
        Ok(&region.automatic)
    }

    /// The CLUT `id`, if one was defined in this epoch.
    pub(super) fn tables(&self, id: u8) -> Option<&Tables> {
        self.cluts
            .as_slice()
            .iter()
            .find(|c| c.id == id)
            .map(|c| &c.tables)
    }

    /// The work of [`compose_region`](Self::compose_region): one unit per
    /// pixel cleared for a new size and per pixel filled.
    pub(super) fn composition_work(&self, composition: &RegionComposition<'_>) -> u64 {
        let size = u64::from(composition.width) * u64::from(composition.height);
        if size == 0 || size > MAX_REGION_PIXELS as u64 {
            return 0;
        }
        let resized = self
            .regions
            .as_slice()
            .iter()
            .find(|r| r.id == composition.id)
            .is_none_or(|r| (r.width, r.height) != (composition.width, composition.height));
        size * (u64::from(resized) + u64::from(composition.fill))
    }

    /// Create or update a region. A new size clears its pixels and makes it
    /// undrawn; the fill flag paints them with the background code. The
    /// object list replaces the previous one. A region without area, or above
    /// the receiver's pixel limit, stops the packet; one that cannot be
    /// allocated is refused and changes nothing.
    pub(super) fn compose_region(&mut self, composition: &RegionComposition<'_>) -> Result<Flow> {
        let size = usize::from(composition.width) * usize::from(composition.height);
        if size == 0 || size > MAX_REGION_PIXELS {
            return Ok(Flow::Stop);
        }
        let placements = composition.objects().count();
        // A composition that cannot be allocated is refused before it changes
        // anything.
        if placements != 0 {
            self.objects.reserve()?;
        }
        let mut created = None;
        let existing = self
            .regions
            .as_slice()
            .iter()
            .position(|r| r.id == composition.id);
        let region = if let Some(index) = existing {
            &mut self.regions.as_mut_slice()[index]
        } else {
            self.regions.try_reserve_exact(1)?;
            created.insert(Region::new(composition.id, &self.budget))
        };
        region
            .objects
            .try_reserve_exact(placements.saturating_sub(region.objects.len()))?;
        if (region.width, region.height) != (composition.width, composition.height) {
            region.resize(composition.width, composition.height)?;
        }
        region.depth = composition.depth;
        region.values = region
            .values
            .max(usize::from(composition.depth.maximum()) + 1);
        region.clut = composition.clut;
        if composition.fill {
            region.pixels.as_mut_slice().fill(composition.background);
        }
        region.objects.clear();
        for placement in composition.objects() {
            push(&mut region.objects, placement)?;
            self.objects.insert(placement.object);
        }
        if let Some(region) = created {
            push(&mut self.regions, region)?;
        }
        Ok(Flow::Continue)
    }

    /// Create or update a CLUT; a repeated version changes nothing.
    pub(super) fn define_clut(&mut self, definition: &ClutDefinition<'_>) -> Result<()> {
        let index = match self
            .cluts
            .as_slice()
            .iter()
            .position(|c| c.id == definition.id)
        {
            Some(index) if self.cluts.as_slice()[index].version == definition.version => {
                return Ok(());
            }
            Some(index) => index,
            None => {
                let clut = Clut {
                    id: definition.id,
                    version: definition.version,
                    tables: DEFAULT.clone(),
                };
                push_exact(&mut self.cluts, clut)?;
                self.cluts.len() - 1
            }
        };
        let clut = &mut self.cluts.as_mut_slice()[index];
        clut.version = definition.version;
        for entry in definition.entries() {
            let word = palette::ycrcbt(entry.y, entry.cr, entry.cb, entry.t);
            clut.tables.set(entry.id, entry.depths, word);
        }
        Ok(())
    }

    /// The work of [`draw_object`](Self::draw_object): one unit per region
    /// placement it visits, and per field byte and pixel a placement may draw.
    pub(super) fn drawing_work(&self, object: &ObjectData<'_>) -> u64 {
        let ObjectCoding::Pixels { top, bottom } = object.coding else {
            return 0;
        };
        if !self.objects.contains(object.id) {
            return 0;
        }
        let bottom = if bottom.is_empty() { top } else { bottom };
        let mut work = 0;
        for region in self.regions.as_slice() {
            let (width, height) = (u64::from(region.width), u64::from(region.height));
            work += region.objects.len() as u64;
            for placement in region.objects.as_slice() {
                if placement.object != object.id {
                    continue;
                }
                let (x, y) = (u64::from(placement.x), u64::from(placement.y));
                if x >= width || y >= height {
                    return work;
                }
                for (field, line) in [(top, 0), (bottom, 1)] {
                    let lines = height.saturating_sub(y + line).div_ceil(2);
                    work += field.len() as u64 + lines * (width - x);
                }
            }
        }
        work
    }

    /// Draw an object into every region placement of it. An object no region
    /// referenced, one not coded as pixels, or a placement outside its region
    /// stops the packet. Cancellation is checked before each placement.
    pub(super) fn draw_object(
        &mut self,
        object: &ObjectData<'_>,
        cancel: &AtomicBool,
    ) -> Result<Flow> {
        if !self.objects.contains(object.id) {
            return Ok(Flow::Stop);
        }
        let ObjectCoding::Pixels { top, bottom } = object.coding else {
            return Ok(Flow::Stop);
        };
        let bottom = if bottom.is_empty() { top } else { bottom };
        for region in self.regions.as_mut_slice() {
            let width = usize::from(region.width);
            let height = usize::from(region.height);
            for placement in region.objects.as_slice() {
                if placement.object != object.id {
                    continue;
                }
                if cancel.load(Ordering::Relaxed) {
                    return Err(Error::Cancelled);
                }
                let (x, y) = (usize::from(placement.x), usize::from(placement.y));
                if x >= width || y >= height {
                    return Ok(Flow::Stop);
                }
                let mut canvas = Canvas {
                    pixels: region.pixels.as_mut_slice(),
                    width,
                    depth: region.depth,
                };
                for (field, line) in [(top, 0), (bottom, 1)] {
                    let pen = Pen {
                        x,
                        y: y + line,
                        non_modifying: object.non_modifying,
                    };
                    // A field the receiver abandons leaves the palette as computed.
                    if pixels::draw_field(&mut canvas, field, pen) {
                        region.automatic_current = false;
                    }
                }
                region.drawn = true;
            }
        }
        Ok(Flow::Continue)
    }
}

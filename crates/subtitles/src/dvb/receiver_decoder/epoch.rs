//! Regions, CLUTs and objects of the current epoch, and how the region,
//! CLUT and object segments change them.
use super::palette::{self, Census, DEFAULT, Tables};
use super::pixels::{self, Canvas, Pen};
use super::segment::{
    ClutDefinition, ObjectCoding, ObjectData, ObjectPlacement, RegionComposition,
};
use super::{Flow, charge, push, push_exact};
use crate::dvb::Depth;
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, BudgetVec, Error, Result, WorkBudget};

/// The largest region the receiver allocates.
const MAX_REGION_PIXELS: usize = 1280 * 1024;

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
    /// The automatic palette as last computed. The receiver recomputes it only
    /// after an object is drawn into the region, and a recomputation rewrites
    /// only the colours it ranks; the rest keep earlier values.
    automatic: [u32; 256],
    automatic_current: bool,
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
        }
    }

    /// `width * height` palette indices, row by row.
    pub(super) fn pixels(&self) -> &[u8] {
        self.pixels.as_slice()
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
        Ok(())
    }
}

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

    pub(super) fn region_at(&self, index: usize) -> &Region {
        &self.regions.as_slice()[index]
    }

    /// Region `index`'s automatic palette at its depth, recomputed when an
    /// object was drawn into the region since the last computation.
    pub(super) fn automatic_palette(
        &mut self,
        index: usize,
        census: &mut Census,
        work: &mut WorkBudget,
    ) -> Result<&[u32]> {
        let region = &mut self.regions.as_mut_slice()[index];
        let colours = 1 << region.depth.bits();
        if !region.automatic_current {
            charge(work, region.pixels.len());
            let width = usize::from(region.width);
            palette::automatic(
                region.pixels.as_slice(),
                width,
                census,
                &mut region.automatic[..colours],
            )?;
            region.automatic_current = true;
        }
        Ok(&region.automatic[..colours])
    }

    /// The CLUT `id`, if one was defined in this epoch.
    pub(super) fn tables(&self, id: u8) -> Option<&Tables> {
        self.cluts
            .as_slice()
            .iter()
            .find(|c| c.id == id)
            .map(|c| &c.tables)
    }

    /// Create or update a region. A new size clears its pixels and makes it
    /// undrawn; the fill flag paints them with the background code. The
    /// object list replaces the previous one. A region without area, or above
    /// the receiver's pixel limit, stops the packet; one that cannot be
    /// allocated is refused and changes nothing.
    pub(super) fn compose_region(
        &mut self,
        composition: &RegionComposition<'_>,
        work: &mut WorkBudget,
    ) -> Result<Flow> {
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
            charge(work, size);
            region.resize(composition.width, composition.height)?;
        }
        region.depth = composition.depth;
        region.clut = composition.clut;
        if composition.fill {
            charge(work, size);
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

    /// Draw an object into every region placement of it. An object no region
    /// referenced, one not coded as pixels, or a placement outside its region
    /// stops the packet. Cancellation is checked before each placement.
    pub(super) fn draw_object(
        &mut self,
        object: &ObjectData<'_>,
        work: &mut WorkBudget,
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
            charge(work, region.objects.len());
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
                    let lines = height.saturating_sub(y + line).div_ceil(2);
                    charge(work, field.len() + lines * (width - x));
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

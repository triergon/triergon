//! Regions, CLUTs and objects of the current epoch, and how the region,
//! CLUT and object segments change them.
use super::palette::{self, Census, DEFAULT, Tables};
use super::pixels::{self, Canvas, Pen};
use super::segment::{
    ClutDefinition, ObjectCoding, ObjectData, ObjectPlacement, RegionComposition,
};
use super::{Flow, charge, push, push_exact};
use crate::dvb::Depth;
use triergon_core::{Budget, BudgetVec, Result, WorkBudget};

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
    /// `width * height` palette indices, row by row.
    pub(super) fn pixels(&self) -> &[u8] {
        self.pixels.as_slice()
    }
}

#[derive(Debug)]
struct Clut {
    id: u8,
    version: u8,
    tables: Tables,
}

#[derive(Debug)]
pub(super) struct Epoch {
    regions: BudgetVec<Region>,
    cluts: BudgetVec<Clut>,
    /// Objects a region composition has referenced.
    objects: BudgetVec<u16>,
    budget: Budget,
}

impl Epoch {
    pub(super) fn new(budget: &Budget) -> Self {
        Self {
            regions: BudgetVec::new(budget),
            cluts: BudgetVec::new(budget),
            objects: BudgetVec::new(budget),
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
    /// the receiver's pixel limit, stops the packet.
    pub(super) fn compose_region(
        &mut self,
        composition: &RegionComposition<'_>,
        work: &mut WorkBudget,
    ) -> Result<Flow> {
        let size = usize::from(composition.width) * usize::from(composition.height);
        if size == 0 || size > MAX_REGION_PIXELS {
            return Ok(Flow::Stop);
        }
        let index = if let Some(index) = self
            .regions
            .as_slice()
            .iter()
            .position(|r| r.id == composition.id)
        {
            index
        } else {
            let region = Region {
                id: composition.id,
                width: 0,
                height: 0,
                depth: composition.depth,
                clut: 0,
                drawn: false,
                objects: BudgetVec::new(&self.budget),
                pixels: BudgetVec::new(&self.budget),
                automatic: [0; 256],
                automatic_current: false,
            };
            push_exact(&mut self.regions, region)?;
            self.regions.len() - 1
        };
        let region = &mut self.regions.as_mut_slice()[index];
        if (region.width, region.height) != (composition.width, composition.height) {
            charge(work, size);
            region.pixels.clear();
            region.pixels.try_resize(size, 0)?;
            region.width = composition.width;
            region.height = composition.height;
            region.drawn = false;
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
            if !self.objects.as_slice().contains(&placement.object) {
                push(&mut self.objects, placement.object)?;
            }
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
    /// stops the packet.
    pub(super) fn draw_object(
        &mut self,
        object: &ObjectData<'_>,
        work: &mut WorkBudget,
    ) -> Result<Flow> {
        if !self.objects.as_slice().contains(&object.id) {
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

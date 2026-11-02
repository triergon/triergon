//! Atomic display-set rendering over the independent raster seam.
use super::colour::{ColourProjection, palette_offset, palette_size};
use super::display_model::*;
use super::display_state::{DecodeWork, Object, State};
use super::native_palette::NativePalettePlan;
use super::{
    FieldParity, FieldSpec, InitialPlane, PaletteFacts, Point, Raster, RasterLimits, RasterSpec,
    SourceFacts, Status,
};
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result, WorkBudget};

#[derive(Debug)]
pub(crate) struct Renderer {
    region: usize,
    entered: bool,
    family: Option<usize>,
    initialized: bool,
    object_ref: usize,
    lookup: usize,
    found: Option<usize>,
    raster: Option<Raster>,
    active_object: Option<Object>,
    drew_object: bool,
    parity: FieldParity,
    offset: usize,
    credit: usize,
    snapshot: usize,
    overlap_at: usize,
    palette_at: usize,
    palette_entries: Option<BudgetVec<super::colour::PaletteEntry>>,
    palette_rgba: Option<BudgetVec<u8>>,
    palette_unrepaired_rgba: Option<BudgetVec<u8>>,
    native_palette: Option<NativePalettePlan>,
    all_entries_explicit: bool,
    frozen_rgba: Option<Bytes>,
    frozen_unrepaired_rgba: Option<Bytes>,
    pending_region: Option<DisplayRegion>,
    pub output: BudgetVec<DisplayRegion>,
    rendered: bool,
    cleanup_region: usize,
    cleanup_family: usize,
    cleanup_object: usize,
}

impl Renderer {
    pub fn new(budget: &Budget) -> Self {
        Self {
            region: 0,
            entered: false,
            family: None,
            initialized: false,
            object_ref: 0,
            lookup: 0,
            found: None,
            raster: None,
            active_object: None,
            drew_object: false,
            parity: FieldParity::Top,
            offset: 0,
            credit: 0,
            snapshot: 0,
            overlap_at: 0,
            palette_at: 0,
            palette_entries: None,
            palette_rgba: None,
            palette_unrepaired_rgba: None,
            native_palette: None,
            all_entries_explicit: true,
            frozen_rgba: None,
            frozen_unrepaired_rgba: None,
            pending_region: None,
            output: BudgetVec::new(budget),
            rendered: false,
            cleanup_region: 0,
            cleanup_family: 0,
            cleanup_object: 0,
        }
    }

    fn region_position(state: &State, address: Point, size: super::Size) -> Result<Point> {
        let window = state.definition.window.unwrap_or(DisplayWindow {
            x: 0,
            y: 0,
            width: state.definition.size.width,
            height: state.definition.size.height,
        });
        let right = address.x.checked_add(size.width).ok_or(Error::Overflow)?;
        let bottom = address.y.checked_add(size.height).ok_or(Error::Overflow)?;
        if right > window.width || bottom > window.height {
            return Err(Error::Invalid("DVB region outside display window"));
        }
        Ok(Point {
            x: window.x.checked_add(address.x).ok_or(Error::Overflow)?,
            y: window.y.checked_add(address.y).ok_or(Error::Overflow)?,
        })
    }

    fn source_facts(
        state: &State,
        family: usize,
        object: Option<Bytes>,
        depth: super::Depth,
    ) -> SourceFacts {
        let family = &state.families.as_slice()[family];
        SourceFacts {
            object,
            palette: match &family.latest {
                Some(original) => PaletteFacts::Encoded {
                    depth,
                    page_id: family.page,
                    clut_id: family.id,
                    version: family.version,
                    original: original.clone(),
                },
                None => PaletteFacts::Default { depth },
            },
        }
    }

    fn choose_family(
        &mut self,
        state: &mut State,
        clut: u8,
        work: &mut DecodeWork<'_>,
    ) -> Result<bool> {
        let index = match self.family {
            Some(index) => index,
            None => {
                let index = match state.family_index(clut)? {
                    Some(index) => index,
                    None => state.ensure_family(state.service.composition_page, clut)?,
                };
                self.family = Some(index);
                index
            }
        };
        state.init_family(index, &mut self.credit, work)
    }

    /// Give the raster an exclusive checked sub-allowance, then debit only its
    /// actual spend. No unavailable parent units are used; unused units are not
    /// burned and no refund API is needed. Absolute display work is capped too.
    fn raster_step(&mut self, bytes: &[u8], work: &mut DecodeWork<'_>) -> Result<Option<Status>> {
        let remaining = work.limit.checked_sub(*work.total).ok_or(Error::Overflow)?;
        if remaining == 0 {
            return Err(Error::Limit {
                requested: work.total.checked_add(1).ok_or(Error::Overflow)?,
                maximum: work.limit,
            });
        }
        let allowance = work
            .work
            .remaining()
            .min(u64::try_from(remaining).map_err(|_| Error::Overflow)?);
        if allowance == 0 {
            return Ok(None);
        }
        let mut local = WorkBudget::new(allowance);
        let result = self
            .raster
            .as_mut()
            .ok_or(Error::Invalid("DVB raster absent"))?
            .step(bytes, true, &mut local, work.cancel);
        let spent = local.spent();
        if !work.work.try_charge(spent) {
            return Err(Error::Invalid("DVB work allowance"));
        }
        *work.total = work
            .total
            .checked_add(usize::try_from(spent).map_err(|_| Error::Overflow)?)
            .ok_or(Error::Overflow)?;
        let progress = result.map_err(|failure| failure.error)?;
        self.offset = self
            .offset
            .checked_add(progress.consumed)
            .ok_or(Error::Overflow)?;
        match progress.status {
            Status::NeedInput => Err(Error::Truncated),
            Status::Yield => Ok(None),
            Status::FieldComplete => Ok(Some(Status::FieldComplete)),
        }
    }

    fn fill_or_copy(&mut self, state: &State, work: &mut DecodeWork<'_>) -> Result<bool> {
        let region = &state.regions.as_slice()[self.region];
        if self.raster.is_none() {
            let seed = if region.fill || region.plane.is_none() {
                InitialPlane::Fill(region.background)
            } else {
                InitialPlane::Copy(
                    region
                        .plane
                        .as_ref()
                        .ok_or(Error::Invalid("DVB cached plane absent"))?
                        .pixels
                        .clone(),
                )
            };
            let family = self.family.ok_or(Error::Invalid("DVB render family"))?;
            let source = Self::source_facts(state, family, None, region.depth);
            let spec = RasterSpec {
                region: region.size,
                object_origin: Point { x: 0, y: 0 },
                depth: region.depth,
                non_modifying: false,
                clip: None,
            };
            let mut raster = Raster::new_with_receiver_view(
                &state.budget,
                spec,
                RasterLimits::default(),
                seed,
                source,
                state.receiver_view,
                work.cancel,
            )?;
            if region.refs.len() > 1 {
                raster.require_disjoint_objects()?;
            }
            // Internal initialization uses empty fields; it invents no source ODS.
            raster.start_field(FieldSpec::new(FieldParity::Top, 0))?;
            self.raster = Some(raster);
            self.parity = FieldParity::Top;
            self.offset = 0;
        }
        loop {
            if self.raster_step(&[], work)?.is_none() {
                return Ok(false);
            }
            if self.parity == FieldParity::Top {
                self.parity = FieldParity::Bottom;
                self.raster
                    .as_mut()
                    .ok_or(Error::Invalid("DVB raster absent"))?
                    .start_field(FieldSpec::new(FieldParity::Bottom, 0))?;
            } else {
                self.initialized = true;
                self.offset = 0;
                return Ok(true);
            }
        }
    }

    fn render_regions(&mut self, state: &mut State, work: &mut DecodeWork<'_>) -> Result<bool> {
        while self.region < state.regions.len() {
            if !self.entered {
                if !work.pay(&mut self.credit, 1)? {
                    return Ok(false);
                }
                self.entered = true;
                self.credit = 0;
            }
            let clut = state.regions.as_slice()[self.region].clut;
            if !self.choose_family(state, clut, work)? {
                return Ok(false);
            }
            let family = self.family.ok_or(Error::Invalid("DVB render family"))?;
            let region = &state.regions.as_slice()[self.region];
            if state.requires_full_regions && !region.seen {
                return Err(Error::Invalid("DVB acquisition missing RCS"));
            }
            if state.families.as_slice()[family].modified && (!region.seen || !region.changed) {
                return Err(Error::Invalid("DVB palette update missing RCS"));
            }
            if !region.dirty && region.plane.is_some() {
                self.next_region();
                continue;
            }
            if !self.initialized && !self.fill_or_copy(state, work)? {
                return Ok(false);
            }
            while self.object_ref < state.regions.as_slice()[self.region].refs.len() {
                let reference =
                    state.regions.as_slice()[self.region].refs.as_slice()[self.object_ref];
                if self.active_object.is_none() {
                    while self.lookup < state.objects.len() {
                        if !work.pay(&mut self.credit, 1)? {
                            return Ok(false);
                        }
                        let object = &state.objects.as_slice()[self.lookup];
                        if object.id == reference.id && state.page_slot(object.page).is_some() {
                            if self.found.is_some() {
                                return Err(Error::Unsupported(
                                    "DVB ambiguous composition/ancillary object identity",
                                ));
                            }
                            self.found = Some(self.lookup);
                        }
                        self.lookup += 1;
                        self.credit = 0;
                    }
                    if self.found.is_none() && state.receiver_view == super::ReceiverView::NativeD32
                    {
                        // Frozen source controls establish that an unavailable
                        // ODS is not an instruction to replay old compressed
                        // data or to reject the native receiver transaction.
                        // Pay even when the object table is empty, and preserve
                        // this reference cursor across yield/backpressure.
                        if !work.pay(&mut self.credit, 1)? {
                            return Ok(false);
                        }
                        self.object_ref += 1;
                        self.lookup = 0;
                        self.credit = 0;
                        continue;
                    }
                    let object = state
                        .objects
                        .as_slice()
                        .get(
                            self.found
                                .ok_or(Error::Invalid("DVB missing object data"))?,
                        )
                        .ok_or(Error::Invalid("DVB object cache index"))?
                        .clone();
                    let depth = state.regions.as_slice()[self.region].depth;
                    let source =
                        Self::source_facts(state, family, Some(object.original.clone()), depth);
                    let raster = self
                        .raster
                        .as_mut()
                        .ok_or(Error::Invalid("DVB raster absent"))?;
                    raster.start_object(
                        reference.position,
                        object.nonmodifying,
                        source,
                        work.cancel,
                    )?;
                    raster.start_field(FieldSpec::new(FieldParity::Top, object.top.len()))?;
                    self.active_object = Some(object);
                    self.parity = FieldParity::Top;
                    self.offset = 0;
                }
                let object = self
                    .active_object
                    .as_ref()
                    .ok_or(Error::Invalid("DVB active object"))?;
                let bytes = if self.parity == FieldParity::Top {
                    object.top.clone()
                } else {
                    object.bottom.clone()
                };
                let data = bytes
                    .as_ref()
                    .get(self.offset..)
                    .ok_or(Error::Invalid("DVB field offset"))?;
                if self.raster_step(data, work)?.is_none() {
                    return Ok(false);
                }
                self.offset = 0;
                if self.parity == FieldParity::Top {
                    self.parity = FieldParity::Bottom;
                    let length = self
                        .active_object
                        .as_ref()
                        .ok_or(Error::Invalid("DVB active object"))?
                        .bottom
                        .len();
                    self.raster
                        .as_mut()
                        .ok_or(Error::Invalid("DVB raster absent"))?
                        .start_field(FieldSpec::new(FieldParity::Bottom, length))?;
                } else {
                    self.drew_object = true;
                    self.active_object = None;
                    self.object_ref += 1;
                    self.lookup = 0;
                    self.found = None;
                }
            }
            if state.receiver_view == super::ReceiverView::NativeD32
                && state.regions.as_slice()[self.region].plane.is_none()
                && !self.drew_object
            {
                // Native fresh-fill/no-ODS and missing-object controls emit an
                // empty rectangle list. Retain RCS/PCS/CDS and source packets;
                // no invented object or valid-looking filled plane is emitted.
                self.raster = None;
                self.next_region();
                continue;
            }
            let plane = self
                .raster
                .as_mut()
                .ok_or(Error::Invalid("DVB raster absent"))?
                .try_finish(work.cancel)?;
            state.regions.as_mut_slice()[self.region].plane = Some(plane);
            self.raster = None;
            self.next_region();
        }
        self.rendered = true;
        Ok(true)
    }

    fn next_region(&mut self) {
        self.region += 1;
        self.entered = false;
        self.family = None;
        self.initialized = false;
        self.object_ref = 0;
        self.lookup = 0;
        self.found = None;
        self.drew_object = false;
        self.credit = 0;
        self.offset = 0;
    }

    pub fn step(&mut self, state: &mut State, work: &mut DecodeWork<'_>) -> Result<bool> {
        if !self.rendered && !self.render_regions(state, work)? {
            return Ok(false);
        }
        while self.snapshot < state.visible.len() {
            if let Some(region) = self.pending_region.take() {
                match self.output.try_push(region) {
                    Ok(()) => {
                        self.snapshot += 1;
                        self.overlap_at = 0;
                        self.family = None;
                        self.palette_at = 0;
                        self.all_entries_explicit = true;
                        self.native_palette = None;
                        self.credit = 0;
                    }
                    Err(error) => {
                        self.pending_region = Some(error.value);
                        return Err(error.error);
                    }
                }
                continue;
            }
            // Actual four default public-API controls establish reverse PCS,
            // independent of RCS arrival, region IDs or geometry sorting.
            // Construct in that order; never reorder a completed output.
            let snapshot_index = match state.receiver_view {
                super::ReceiverView::Source => self.snapshot,
                super::ReceiverView::NativeD32 => state.visible.len() - 1 - self.snapshot,
            };
            let visible = state.visible.as_slice()[snapshot_index];
            let index = state.slots[usize::from(visible.id)]
                .ok_or(Error::Invalid("DVB missing visible region"))?;
            let region = &state.regions.as_slice()[index];
            if state.receiver_view == super::ReceiverView::NativeD32 && region.plane.is_none() {
                Self::region_position(state, visible.address, region.size)?;
                if !work.pay(&mut self.credit, 1)? {
                    return Ok(false);
                }
                self.snapshot += 1;
                self.overlap_at = 0;
                self.credit = 0;
                continue;
            }
            let clut = region.clut;
            if !self.choose_family(state, clut, work)? {
                return Ok(false);
            }
            let region = &state.regions.as_slice()[index];
            let position = Self::region_position(state, visible.address, region.size)?;
            if state.receiver_view == super::ReceiverView::NativeD32 {
                // Native source PCS order may be nonascending. Check actual
                // vertical intersections without assuming emission order. The
                // cursor pays for each pair once and survives yield/pressure.
                let lower_edge = position
                    .y
                    .checked_add(region.size.height)
                    .ok_or(Error::Overflow)?;
                while self.overlap_at < self.output.len() {
                    if !work.pay(&mut self.credit, 1)? {
                        return Ok(false);
                    }
                    let prior = &self.output.as_slice()[self.overlap_at];
                    let prior_lower_edge = prior
                        .position
                        .y
                        .checked_add(prior.size.height)
                        .ok_or(Error::Overflow)?;
                    if prior.position.y < lower_edge && position.y < prior_lower_edge {
                        return Err(Error::Invalid("DVB visible regions share scan lines"));
                    }
                    self.overlap_at += 1;
                    self.credit = 0;
                }
            } else if self
                .output
                .as_slice()
                .last()
                .is_some_and(|prior| prior.position.y + prior.size.height > position.y)
            {
                return Err(Error::Invalid("DVB visible regions share scan lines"));
            }
            let count = palette_size(region.depth);
            if self.palette_entries.is_none() {
                let mut entries = BudgetVec::new(&state.budget);
                entries.try_reserve_exact(count)?;
                self.palette_entries = Some(entries);
            }
            if self.palette_rgba.is_none() && self.frozen_rgba.is_none() {
                let mut rgba = BudgetVec::new(&state.budget);
                rgba.try_reserve_exact(count.checked_mul(4).ok_or(Error::Overflow)?)?;
                self.palette_rgba = Some(rgba);
            }
            if self.palette_unrepaired_rgba.is_none() && self.frozen_unrepaired_rgba.is_none() {
                let mut rgba = BudgetVec::new(&state.budget);
                rgba.try_reserve_exact(count.checked_mul(4).ok_or(Error::Overflow)?)?;
                self.palette_unrepaired_rgba = Some(rgba);
            }
            let family = self.family.ok_or(Error::Invalid("DVB snapshot family"))?;
            if state.receiver_view == super::ReceiverView::NativeD32 {
                let source_family = &state.families.as_slice()[family];
                if self.native_palette.is_none() {
                    let pixels = region
                        .plane
                        .as_ref()
                        .ok_or(Error::Invalid("DVB native region plane absent"))?
                        .pixels
                        .clone();
                    self.native_palette = Some(NativePalettePlan::new(
                        &state.budget,
                        region.depth,
                        source_family.latest.is_none(),
                        pixels,
                    )?);
                }
                let offset = palette_offset(region.depth);
                let effective = source_family
                    .native_entries
                    .as_ref()
                    .ok_or(Error::Invalid("DVB native CLUT state absent"))?;
                if !self
                    .native_palette
                    .as_mut()
                    .ok_or(Error::Invalid("DVB native palette plan absent"))?
                    .prepare(
                        &source_family.entries.as_slice()[offset..offset + count],
                        &effective.as_slice()[offset..offset + count],
                        work,
                    )?
                {
                    return Ok(false);
                }
            }
            while self.palette_at < count {
                let entry_work = if self.native_palette.is_some() { 12 } else { 8 };
                if !work.pay(&mut self.credit, entry_work)? {
                    return Ok(false);
                }
                let cell = state.families.as_slice()[family].entries.as_slice()
                    [palette_offset(region.depth) + self.palette_at]
                    .clone();
                let rgba = cell.source.rgba_source_nearest()?;
                let unrepaired = cell.source.rgba(ColourProjection::StudioUnrepaired8)?;
                if let Some(native) = self.native_palette.as_mut() {
                    let source = state.families.as_slice()[family]
                        .native_entries
                        .as_ref()
                        .ok_or(Error::Invalid("DVB native CLUT state absent"))?
                        .as_slice()[palette_offset(region.depth) + self.palette_at];
                    native.push_entry(self.palette_at, source, unrepaired)?;
                }
                self.all_entries_explicit &= cell.original_segment.is_some();
                self.palette_entries
                    .as_mut()
                    .ok_or(Error::Invalid("DVB palette builder"))?
                    .try_push(cell)
                    .map_err(|e| e.error)?;
                self.palette_rgba
                    .as_mut()
                    .ok_or(Error::Invalid("DVB RGBA builder"))?
                    .try_extend_from_slice(&rgba)?;
                self.palette_unrepaired_rgba
                    .as_mut()
                    .ok_or(Error::Invalid("DVB unrepaired RGBA builder"))?
                    .try_extend_from_slice(&unrepaired)?;
                self.palette_at += 1;
                self.credit = 0;
            }
            if self.frozen_rgba.is_none() {
                let rgba = self
                    .palette_rgba
                    .take()
                    .ok_or(Error::Invalid("DVB RGBA builder"))?;
                match rgba.freeze() {
                    Ok(bytes) => self.frozen_rgba = Some(bytes),
                    Err(error) => {
                        self.palette_rgba = Some(error.buffer);
                        return Err(error.error);
                    }
                }
            }
            if self.frozen_unrepaired_rgba.is_none() {
                let rgba = self
                    .palette_unrepaired_rgba
                    .take()
                    .ok_or(Error::Invalid("DVB unrepaired RGBA builder"))?;
                match rgba.freeze() {
                    Ok(bytes) => self.frozen_unrepaired_rgba = Some(bytes),
                    Err(error) => {
                        self.palette_unrepaired_rgba = Some(error.buffer);
                        return Err(error.error);
                    }
                }
            }
            let native_d32 = if let Some(native) = self.native_palette.as_mut() {
                Some(
                    native.try_finish(
                        self.frozen_unrepaired_rgba
                            .as_ref()
                            .ok_or(Error::Invalid("DVB native studio RGBA absent"))?,
                        self.all_entries_explicit,
                    )?,
                )
            } else {
                None
            };
            let palette = PaletteSnapshot {
                native_d32,
                depth: region.depth,
                projection: PaletteSnapshot::projection(),
                rgba: self
                    .frozen_rgba
                    .take()
                    .ok_or(Error::Invalid("DVB RGBA absent"))?,
                studio_unrepaired_rgba: self
                    .frozen_unrepaired_rgba
                    .take()
                    .ok_or(Error::Invalid("DVB unrepaired RGBA absent"))?,
                studio_unrepaired_projection: PaletteSnapshot::studio_projection(),
                entries: self
                    .palette_entries
                    .take()
                    .ok_or(Error::Invalid("DVB palette entries absent"))?,
            };
            self.pending_region = Some(DisplayRegion {
                id: region.id,
                position,
                source_address: visible.address,
                size: region.size,
                plane: region
                    .plane
                    .clone()
                    .ok_or(Error::Invalid("DVB region plane absent"))?,
                palette,
                original_composition: region.original.clone(),
            });
        }
        // Reset mutable display flags only after all snapshots are complete.
        // This work yields too; no output event is visible while cleanup is partial.
        while self.cleanup_region < state.regions.len() {
            if !work.pay(&mut self.credit, 1)? {
                return Ok(false);
            }
            let region = &mut state.regions.as_mut_slice()[self.cleanup_region];
            region.seen = false;
            region.changed = false;
            region.dirty = false;
            region.fill = false;
            self.cleanup_region += 1;
            self.credit = 0;
        }
        while self.cleanup_family < state.families.len() {
            if !work.pay(&mut self.credit, 1)? {
                return Ok(false);
            }
            state.families.as_mut_slice()[self.cleanup_family].modified = false;
            self.cleanup_family += 1;
            self.credit = 0;
        }
        if state.object_retention == ObjectRetention::CurrentDisplay {
            while self.cleanup_object < state.objects.len() {
                if !work.pay(&mut self.credit, 1)? {
                    return Ok(false);
                }
                self.cleanup_object += 1;
                self.credit = 0;
            }
        }
        Ok(true)
    }
}

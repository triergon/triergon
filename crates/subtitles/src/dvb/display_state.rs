//! Primary-derived, charged DVB service state and small-entry segment parser.
use super::bits::{cancelled, take_work};
use super::colour::{ColourSource, PaletteEntry, palette_offset};
use super::display_model::*;
use super::{Depth, IndexedPlane, Point, Size};
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result, WorkBudget};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct VisibleRegion {
    pub id: u8,
    pub address: Point,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ObjectRef {
    pub id: u16,
    pub position: Point,
}

#[derive(Debug)]
pub(crate) struct Region {
    pub id: u8,
    pub version: u8,
    pub size: Size,
    pub depth: Depth,
    pub compatibility: u8,
    pub clut: u8,
    pub background: u8,
    pub fill: bool,
    pub refs: BudgetVec<ObjectRef>,
    pub original: Bytes,
    pub plane: Option<IndexedPlane>,
    pub dirty: bool,
    pub seen: bool,
    pub changed: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct Object {
    pub page: u16,
    pub id: u16,
    pub version: u8,
    pub nonmodifying: bool,
    pub top: Bytes,
    pub bottom: Bytes,
    pub original: Bytes,
}

#[derive(Debug)]
pub(crate) struct Family {
    pub page: u16,
    pub id: u8,
    pub version: u8,
    pub entries: BudgetVec<PaletteEntry>,
    // Native-only effective encoded/default source values. The original CDS
    // facts above remain authoritative and independent. Required by observed
    // final-reduced-entry update behavior, not a rendered palette cache.
    pub native_entries: Option<BudgetVec<ColourSource>>,
    pub latest: Option<Bytes>,
    pub modified: bool,
}

pub(crate) struct DecodeWork<'a> {
    pub work: &'a mut WorkBudget,
    pub total: &'a mut usize,
    pub limit: usize,
    pub cancel: &'a AtomicBool,
}

impl DecodeWork<'_> {
    /// Incremental credit for one small semantic entry. Retry after allocation
    /// backpressure preserves already-paid credit until the entry commits.
    pub(super) fn pay(&mut self, credit: &mut usize, amount: usize) -> Result<bool> {
        if *credit < amount {
            let count = take_work(
                self.work,
                self.total,
                self.limit,
                amount - *credit,
                self.cancel,
            )?;
            *credit += count;
        }
        Ok(*credit == amount)
    }
}

#[derive(Debug)]
pub(crate) struct State {
    pub budget: Budget,
    pub service: Service,
    pub limits: DisplayLimits,
    pub object_retention: ObjectRetention,
    pub receiver_view: super::ReceiverView,
    pub geometry_policy: super::GeometryPolicy,
    pub definition: DisplayDefinition,
    pub native_canvas: Option<Size>,
    pub regions: BudgetVec<Region>,
    pub slots: [Option<usize>; 256],
    pub objects: BudgetVec<Object>,
    pub families: BudgetVec<Family>,
    pub clut_slots: [[Option<usize>; 256]; 2],
    pub visible: BudgetVec<VisibleRegion>,
    pub timeout: u8,
    pub page_composition: Option<Bytes>,
    pub page_version: Option<u8>,
    pub page_state: Option<u8>,
    pub requires_full_regions: bool,
    pub acquired: bool,
    pub allow_new: bool,
    pub epoch: u64,
    pub had_visible: bool,
    pub total_pixels: usize,
    pub total_refs: usize,
    pub opaque: BudgetVec<OpaqueSegment>,
}

impl State {
    pub(super) fn new(
        budget: &Budget,
        service: Service,
        limits: DisplayLimits,
        object_retention: ObjectRetention,
    ) -> Self {
        Self {
            budget: budget.clone(),
            service,
            limits,
            object_retention,
            receiver_view: super::ReceiverView::Source,
            geometry_policy: super::GeometryPolicy::ValidateSource,
            definition: DisplayDefinition::default(),
            native_canvas: None,
            regions: BudgetVec::new(budget),
            slots: [None; 256],
            objects: BudgetVec::new(budget),
            families: BudgetVec::new(budget),
            clut_slots: [[None; 256]; 2],
            visible: BudgetVec::new(budget),
            timeout: 0,
            page_composition: None,
            page_version: None,
            page_state: None,
            requires_full_regions: false,
            acquired: false,
            allow_new: false,
            epoch: 0,
            had_visible: false,
            total_pixels: 0,
            total_refs: 0,
            opaque: BudgetVec::new(budget),
        }
    }

    pub(super) fn page_slot(&self, page: u16) -> Option<usize> {
        if page == self.service.composition_page {
            Some(0)
        } else if page == self.service.ancillary_page {
            Some(1)
        } else {
            None
        }
    }

    pub(super) fn new_epoch(&mut self) -> Result<()> {
        self.epoch = self.epoch.checked_add(1).ok_or(Error::Overflow)?;
        self.regions = BudgetVec::new(&self.budget);
        self.slots = [None; 256];
        self.objects = BudgetVec::new(&self.budget);
        self.families = BudgetVec::new(&self.budget);
        self.clut_slots = [[None; 256]; 2];
        self.total_pixels = 0;
        self.total_refs = 0;
        self.acquired = true;
        self.allow_new = true;
        self.had_visible = false;
        Ok(())
    }

    pub(super) fn family_index(&self, clut: u8) -> Result<Option<usize>> {
        let a = self.clut_slots[0][usize::from(clut)];
        let b = self.clut_slots[1][usize::from(clut)];
        if a.is_some() && b.is_some() {
            return Err(Error::Unsupported(
                "DVB ambiguous composition/ancillary CLUT identity",
            ));
        }
        Ok(a.or(b))
    }

    pub(super) fn ensure_family(&mut self, page: u16, id: u8) -> Result<usize> {
        let slot = self
            .page_slot(page)
            .ok_or(Error::Invalid("DVB family page"))?;
        if let Some(index) = self.clut_slots[slot][usize::from(id)] {
            return Ok(index);
        }
        if !self.allow_new {
            return Err(Error::Invalid("DVB new CLUT family within epoch"));
        }
        limit(
            self.families.len().checked_add(1).ok_or(Error::Overflow)?,
            self.limits.clut_families,
        )?;
        self.families.try_reserve_exact(1)?;
        let mut entries = BudgetVec::new(&self.budget);
        entries.try_reserve_exact(276)?;
        let native_entries = if self.receiver_view == super::ReceiverView::Receiver {
            let mut values = BudgetVec::new(&self.budget);
            values.try_reserve_exact(276)?;
            Some(values)
        } else {
            None
        };
        let index = self.families.len();
        self.families
            .try_push(Family {
                page,
                id,
                version: 0,
                entries,
                native_entries,
                latest: None,
                modified: false,
            })
            .map_err(|e| e.error)?;
        self.clut_slots[slot][usize::from(id)] = Some(index);
        Ok(index)
    }

    pub(super) fn init_family(
        &mut self,
        index: usize,
        credit: &mut usize,
        work: &mut DecodeWork<'_>,
    ) -> Result<bool> {
        let family = self
            .families
            .as_mut_slice()
            .get_mut(index)
            .ok_or(Error::Invalid("DVB family index"))?;
        while family.entries.len() < 276 {
            let amount = if family.native_entries.is_some() {
                2
            } else {
                1
            };
            if !work.pay(credit, amount)? {
                return Ok(false);
            }
            let n = family.entries.len();
            let (depth, value) = if n < 4 {
                (Depth::Two, n)
            } else if n < 20 {
                (Depth::Four, n - 4)
            } else {
                (Depth::Eight, n - 20)
            };
            let value = u8::try_from(value).map_err(|_| Error::Overflow)?;
            let source = ColourSource::Default {
                depth,
                index: value,
            };
            if let Some(values) = family.native_entries.as_mut() {
                if values.len() != n || values.capacity() < 276 {
                    return Err(Error::Invalid("DVB native CLUT initialization"));
                }
                values.try_push(source).map_err(|error| error.error)?;
            }
            family
                .entries
                .try_push(PaletteEntry {
                    source,
                    original_segment: None,
                })
                .map_err(|e| e.error)?;
            *credit = 0;
        }
        Ok(true)
    }

    pub(super) fn finish_display(&mut self) {
        self.allow_new = false;
        self.requires_full_regions = false;
        if self.object_retention == ObjectRetention::CurrentDisplay {
            // Renderer already paid for this bounded cleanup before publication.
            self.objects = BudgetVec::new(&self.budget);
        }
    }
}

pub(crate) fn limit(requested: usize, maximum: usize) -> Result<()> {
    if requested > maximum {
        Err(Error::Limit { requested, maximum })
    } else {
        Ok(())
    }
}

fn u16_at(bytes: &[u8], at: usize) -> Result<u16> {
    let end = at.checked_add(2).ok_or(Error::Overflow)?;
    let b = bytes.get(at..end).ok_or(Error::Truncated)?;
    Ok(u16::from_be_bytes([b[0], b[1]]))
}

fn depth(value: u8) -> Result<Depth> {
    match value {
        1 => Ok(Depth::Two),
        2 => Ok(Depth::Four),
        3 => Ok(Depth::Eight),
        _ => Err(Error::Unsupported("DVB region depth")),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SegmentResult {
    Yield,
    Complete,
    EndDisplay,
}

#[derive(Debug)]
pub(crate) struct Segment {
    pub kind: u8,
    pub page: u16,
    pub original: Bytes,
    body: Bytes,
    phase: u8,
    at: usize,
    credit: usize,
    draft: Option<Region>,
    family: Option<usize>,
    object: Option<Object>,
    lookup: usize,
    duplicate: bool,
    visible_ids: [bool; 256],
}

impl Segment {
    pub(super) fn new(kind: u8, page: u16, original: Bytes) -> Result<Self> {
        let body = original.slice(6..original.len())?;
        Ok(Self {
            kind,
            page,
            original,
            body,
            phase: 0,
            at: 0,
            credit: 0,
            draft: None,
            family: None,
            object: None,
            lookup: 0,
            duplicate: false,
            visible_ids: [false; 256],
        })
    }

    pub(super) fn step(
        &mut self,
        state: &mut State,
        work: &mut DecodeWork<'_>,
    ) -> Result<SegmentResult> {
        cancelled(work.cancel)?;
        let bytes = self.body.clone();
        let b = bytes.as_ref();
        if state.page_slot(self.page).is_none() {
            return Ok(SegmentResult::Complete);
        }
        match self.kind {
            0x14 => {
                if self.page != state.service.composition_page {
                    return Err(Error::Invalid("DVB ancillary DDS"));
                }
                let flags = *b.first().ok_or(Error::Truncated)?;
                let needed = if flags & 8 != 0 { 13 } else { 5 };
                if b.len() < needed {
                    return Err(Error::Truncated);
                }
                if b.len() != needed {
                    return Err(Error::Invalid("DVB DDS length"));
                }
                if !work.pay(&mut self.credit, needed)? {
                    return Ok(SegmentResult::Yield);
                }
                let width = u32::from(u16_at(b, 1)?) + 1;
                let height = u32::from(u16_at(b, 3)?) + 1;
                limit(width as usize, 4096)?;
                limit(height as usize, 4096)?;
                let window = if flags & 8 != 0 {
                    let x = u32::from(u16_at(b, 5)?);
                    let right = u32::from(u16_at(b, 7)?);
                    let y = u32::from(u16_at(b, 9)?);
                    let bottom = u32::from(u16_at(b, 11)?);
                    if right < x || bottom < y || right >= width || bottom >= height {
                        return Err(Error::Invalid("DVB display window bounds"));
                    }
                    Some(DisplayWindow {
                        x,
                        y,
                        width: right - x + 1,
                        height: bottom - y + 1,
                    })
                } else {
                    None
                };
                state.definition = DisplayDefinition {
                    size: Size { width, height },
                    window,
                    version: Some(flags >> 4),
                    original: Some(self.original.clone()),
                };
                if state.receiver_view == super::ReceiverView::Receiver {
                    state.native_canvas.get_or_insert(Size { width, height });
                }
                Ok(SegmentResult::Complete)
            }
            0x10 => self.page_composition(state, work, b),
            0x11 => self.region_composition(state, work, b),
            0x12 => self.clut(state, work, b),
            0x13 => self.object(state, work, b),
            0x80 => {
                if !b.is_empty() && state.receiver_view == super::ReceiverView::Source {
                    return Err(Error::Invalid("DVB EOD payload"));
                }
                Ok(SegmentResult::EndDisplay)
            }
            _ => {
                state
                    .opaque
                    .try_push(OpaqueSegment {
                        kind: self.kind,
                        page_id: self.page,
                        original: self.original.clone(),
                        unsupported_feature: matches!(self.kind, 0x15 | 0x16),
                    })
                    .map_err(|e| e.error)?;
                Ok(SegmentResult::Complete)
            }
        }
    }

    fn page_composition(
        &mut self,
        state: &mut State,
        work: &mut DecodeWork<'_>,
        b: &[u8],
    ) -> Result<SegmentResult> {
        if self.page != state.service.composition_page {
            return Err(Error::Invalid("DVB ancillary PCS"));
        }
        if b.len() < 2 || !(b.len() - 2).is_multiple_of(6) {
            return Err(Error::Invalid("DVB PCS length"));
        }
        if self.phase == 0 {
            let mode = (b[1] >> 2) & 3;
            if mode == 3 {
                return Err(Error::Unsupported("DVB reserved page state"));
            }
            let resets_epoch = mode == 2 || (mode == 1 && !state.acquired);
            // Precharge bounded cache teardown as well as the PCS header. The
            // old state remains intact while this credit yields/cancels; dropping
            // a cache cannot become unaccounted work at a cheap two-byte header.
            let reset_cost = if resets_epoch {
                state
                    .regions
                    .len()
                    .checked_mul(5)
                    .and_then(|n| {
                        state
                            .objects
                            .len()
                            .checked_mul(4)
                            .and_then(|v| n.checked_add(v))
                    })
                    .and_then(|n| {
                        state
                            .families
                            .len()
                            .checked_mul(277)
                            .and_then(|v| n.checked_add(v))
                    })
                    .ok_or(Error::Overflow)?
            } else {
                0
            };
            if !work.pay(
                &mut self.credit,
                reset_cost.checked_add(2).ok_or(Error::Overflow)?,
            )? {
                return Ok(SegmentResult::Yield);
            }
            if resets_epoch {
                state.new_epoch()?;
            }
            if !state.acquired {
                return Err(Error::Unsupported("DVB acquisition required"));
            }
            state.timeout = b[0];
            state.visible.clear();
            state.page_composition = Some(self.original.clone());
            state.page_version = Some(b[1] >> 4);
            state.page_state = Some(mode);
            state.requires_full_regions = mode != 0;
            self.at = 2;
            self.phase = 1;
            self.credit = 0;
        }
        while self.at < b.len() {
            if !work.pay(&mut self.credit, 6)? {
                return Ok(SegmentResult::Yield);
            }
            limit(state.visible.len() + 1, state.limits.regions)?;
            let item = VisibleRegion {
                id: b[self.at],
                address: Point {
                    x: u32::from(u16_at(b, self.at + 2)?),
                    y: u32::from(u16_at(b, self.at + 4)?),
                },
            };
            if self.visible_ids[usize::from(item.id)] {
                return Err(Error::Invalid("DVB duplicate visible region"));
            }
            if state.receiver_view == super::ReceiverView::Source
                && state
                    .visible
                    .as_slice()
                    .last()
                    .is_some_and(|r| r.address.y >= item.address.y)
            {
                return Err(Error::Invalid("DVB visible region order"));
            }
            state.visible.try_push(item).map_err(|e| e.error)?;
            self.visible_ids[usize::from(item.id)] = true;
            self.at += 6;
            self.credit = 0;
        }
        Ok(SegmentResult::Complete)
    }

    fn region_composition(
        &mut self,
        state: &mut State,
        work: &mut DecodeWork<'_>,
        b: &[u8],
    ) -> Result<SegmentResult> {
        if self.page != state.service.composition_page {
            return Err(Error::Invalid("DVB ancillary RCS"));
        }
        if b.len() < 10 {
            return Err(Error::Truncated);
        }
        if !state.acquired {
            return Err(Error::Unsupported("DVB acquisition required"));
        }
        if self.phase == 0 {
            if !work.pay(&mut self.credit, 10)? {
                return Ok(SegmentResult::Yield);
            }
            let id = b[0];
            let version = b[1] >> 4;
            let fill = b[1] & 8 != 0;
            let size = Size {
                width: u32::from(u16_at(b, 2)?),
                height: u32::from(u16_at(b, 4)?),
            };
            if size.width == 0 || size.height == 0 {
                return Err(Error::Invalid("DVB zero region size"));
            }
            if size.width > state.definition.size.width
                || size.height > state.definition.size.height
            {
                return Err(Error::Invalid("DVB region exceeds display dimensions"));
            }
            let depth = depth((b[6] >> 2) & 7)?;
            let compatibility = b[6] >> 5;
            if !(1..=3).contains(&compatibility) {
                return Err(Error::Unsupported("DVB region compatibility"));
            }
            let background = match depth {
                Depth::Two => (b[9] >> 2) & 3,
                Depth::Four => b[9] >> 4,
                Depth::Eight => b[8],
            };
            let old = state.slots[usize::from(id)].map(|i| &state.regions.as_slice()[i]);
            if let Some(old) = old {
                if (old.size != size && state.receiver_view == super::ReceiverView::Source)
                    || old.depth != depth
                    || old.compatibility != compatibility
                    || old.clut != b[7]
                {
                    return Err(Error::Invalid("DVB region shape changed within epoch"));
                }
                // Both distinguished width-change controls discard the old
                // native plane, including the unchanged-version control.
                // Other footprint changes retain their original rejection.
                self.duplicate = old.version == version && old.size == size;
            } else if !state.allow_new {
                return Err(Error::Invalid("DVB new region within epoch"));
            }
            let area = usize::try_from(u64::from(size.width) * u64::from(size.height))
                .map_err(|_| Error::Overflow)?;
            let old_area = old.map_or(Ok(0), |region| {
                usize::try_from(u64::from(region.size.width) * u64::from(region.size.height))
                    .map_err(|_| Error::Overflow)
            })?;
            let pixels = state
                .total_pixels
                .checked_sub(old_area)
                .and_then(|n| n.checked_add(area))
                .ok_or(Error::Overflow)?;
            limit(pixels, state.limits.aggregate_region_pixels)?;
            let mut refs = BudgetVec::new(&state.budget);
            let count = (b.len() - 10) / 6;
            limit(count, state.limits.object_references)?;
            refs.try_reserve_exact(count)?;
            self.draft = Some(Region {
                id,
                version,
                size,
                depth,
                compatibility,
                clut: b[7],
                background,
                fill,
                refs,
                original: self.original.clone(),
                plane: old.filter(|r| r.size == size).and_then(|r| r.plane.clone()),
                dirty: true,
                seen: true,
                changed: true,
            });
            self.at = 10;
            self.phase = 1;
            self.credit = 0;
        }
        while self.at < b.len() {
            if self.at.checked_add(6).ok_or(Error::Overflow)? > b.len() {
                return Err(Error::Truncated);
            }
            if !work.pay(&mut self.credit, 6)? {
                return Ok(SegmentResult::Yield);
            }
            let x = u16_at(b, self.at + 2)?;
            let y = u16_at(b, self.at + 4)?;
            if x >> 12 != 0 {
                return Err(Error::Unsupported("DVB character/ROM object reference"));
            }
            let item = ObjectRef {
                id: u16_at(b, self.at)?,
                position: Point {
                    x: u32::from(x & 4095),
                    y: u32::from(y & 4095),
                },
            };
            let draft = self.draft.as_mut().ok_or(Error::Invalid("DVB RCS draft"))?;
            if item.position.x >= draft.size.width || item.position.y >= draft.size.height {
                return Err(Error::Invalid("DVB object origin outside region"));
            }
            draft.refs.try_push(item).map_err(|e| e.error)?;
            self.at += 6;
            self.credit = 0;
        }
        let draft = self.draft.as_ref().ok_or(Error::Invalid("DVB RCS draft"))?;
        if let Some(index) = state.slots[usize::from(draft.id)] {
            if self.duplicate {
                state.regions.as_mut_slice()[index].seen = true;
                return Ok(SegmentResult::Complete);
            }
            let old_refs = state.regions.as_slice()[index].refs.len();
            let total = state
                .total_refs
                .checked_sub(old_refs)
                .and_then(|n| n.checked_add(draft.refs.len()))
                .ok_or(Error::Overflow)?;
            limit(total, state.limits.object_references)?;
            let old = &state.regions.as_slice()[index];
            let old_area = usize::try_from(u64::from(old.size.width) * u64::from(old.size.height))
                .map_err(|_| Error::Overflow)?;
            let new_area =
                usize::try_from(u64::from(draft.size.width) * u64::from(draft.size.height))
                    .map_err(|_| Error::Overflow)?;
            let pixels = state
                .total_pixels
                .checked_sub(old_area)
                .and_then(|n| n.checked_add(new_area))
                .ok_or(Error::Overflow)?;
            limit(pixels, state.limits.aggregate_region_pixels)?;
            state.regions.as_mut_slice()[index] =
                self.draft.take().ok_or(Error::Invalid("DVB RCS draft"))?;
            state.total_refs = total;
            state.total_pixels = pixels;
        } else {
            limit(state.regions.len() + 1, state.limits.regions)?;
            let total = state
                .total_refs
                .checked_add(draft.refs.len())
                .ok_or(Error::Overflow)?;
            limit(total, state.limits.object_references)?;
            state.regions.try_reserve_exact(1)?;
            let draft = self.draft.take().ok_or(Error::Invalid("DVB RCS draft"))?;
            let id = draft.id;
            state.total_pixels +=
                usize::try_from(u64::from(draft.size.width) * u64::from(draft.size.height))
                    .map_err(|_| Error::Overflow)?;
            state.slots[usize::from(id)] = Some(state.regions.len());
            state.regions.try_push(draft).map_err(|e| e.error)?;
            state.total_refs = total;
        }
        Ok(SegmentResult::Complete)
    }

    fn clut(
        &mut self,
        state: &mut State,
        work: &mut DecodeWork<'_>,
        b: &[u8],
    ) -> Result<SegmentResult> {
        if b.len() < 2 {
            return Err(Error::Truncated);
        }
        if self.phase == 0 {
            if !work.pay(&mut self.credit, 2)? {
                return Ok(SegmentResult::Yield);
            }
            let index = state.ensure_family(self.page, b[0])?;
            let old = &state.families.as_slice()[index];
            self.duplicate = old.latest.is_some() && old.version == b[1] >> 4;
            self.family = Some(index);
            self.at = 2;
            self.phase = 1;
            self.credit = 0;
        }
        let index = self.family.ok_or(Error::Invalid("DVB CDS family"))?;
        if self.phase == 1 {
            if !state.init_family(index, &mut self.credit, work)? {
                return Ok(SegmentResult::Yield);
            }
            self.phase = 2;
        }
        while self.at < b.len() {
            if self.at + 2 > b.len() {
                return Err(Error::Truncated);
            }
            let flags = b[self.at + 1];
            let full = flags & 1 != 0;
            let count = if full { 6 } else { 4 };
            if self.at + count > b.len() {
                return Err(Error::Truncated);
            }
            if !work.pay(&mut self.credit, count)? {
                return Ok(SegmentResult::Yield);
            }
            let depth = match flags >> 5 {
                4 => Depth::Two,
                2 => Depth::Four,
                1 => Depth::Eight,
                _ => return Err(Error::Invalid("DVB CDS depth flags")),
            };
            let entry = b[self.at];
            if entry > depth.maximum() {
                return Err(Error::Invalid("DVB CDS entry index"));
            }
            let (y, cr, cb, transparency) = if full {
                (
                    b[self.at + 2],
                    b[self.at + 3],
                    b[self.at + 4],
                    b[self.at + 5],
                )
            } else {
                let raw = u16_at(b, self.at + 2)?;
                (
                    (raw >> 10) as u8,
                    ((raw >> 6) & 15) as u8,
                    ((raw >> 2) & 15) as u8,
                    (raw & 3) as u8,
                )
            };
            if state.receiver_view == super::ReceiverView::Source
                && y == 0
                && (cr != 0 || cb != 0 || transparency != 0)
            {
                return Err(Error::Invalid("DVB transparent source components"));
            }
            let family = &mut state.families.as_mut_slice()[index];
            if !self.duplicate {
                let position = palette_offset(depth) + usize::from(entry);
                let source = ColourSource::YCrCbT {
                    y,
                    cr,
                    cb,
                    transparency,
                    full_resolution: full,
                };
                if let Some(values) = family.native_entries.as_mut() {
                    // Public-API controls: a reduced entry occupying the
                    // final four CDS bytes leaves the previous native value.
                    // Source still records every complete legal entry. Moving
                    // this same entry before a full entry applies it natively.
                    if full || self.at + count < b.len() {
                        values.as_mut_slice()[position] = source;
                    }
                }
                family.entries.as_mut_slice()[position] = PaletteEntry {
                    source,
                    original_segment: Some(self.original.clone()),
                };
                family.modified = true;
            }
            self.at += count;
            self.credit = 0;
        }
        if !self.duplicate {
            let family = &mut state.families.as_mut_slice()[index];
            family.version = b[1] >> 4;
            family.latest = Some(self.original.clone());
        }
        Ok(SegmentResult::Complete)
    }

    fn object(
        &mut self,
        state: &mut State,
        work: &mut DecodeWork<'_>,
        b: &[u8],
    ) -> Result<SegmentResult> {
        if b.len() < 3 {
            return Err(Error::Truncated);
        }
        if (b[2] >> 2) & 3 != 0 {
            return Err(Error::Unsupported(
                "DVB non-interlaced-bitmap object method",
            ));
        }
        if b.len() < 7 {
            return Err(Error::Truncated);
        }
        if self.phase == 0 {
            if !work.pay(&mut self.credit, 7)? {
                return Ok(SegmentResult::Yield);
            }
            let a = usize::from(u16_at(b, 3)?);
            let c = usize::from(u16_at(b, 5)?);
            let end = 7_usize
                .checked_add(a)
                .and_then(|n| n.checked_add(c))
                .ok_or(Error::Overflow)?;
            if end > b.len() {
                return Err(Error::Truncated);
            }
            if b.len() - end > 1 || (b.len() != end && b[end] != 0) {
                return Err(Error::Invalid("DVB object stuffing"));
            }
            let top = self.body.slice(7..7 + a)?;
            let bottom = if c == 0 {
                top.clone()
            } else {
                self.body.slice(7 + a..end)?
            };
            self.object = Some(Object {
                page: self.page,
                id: u16_at(b, 0)?,
                version: b[2] >> 4,
                nonmodifying: b[2] & 2 != 0,
                top,
                bottom,
                original: self.original.clone(),
            });
            self.phase = 1;
            self.credit = 0;
        }
        let object = self
            .object
            .as_ref()
            .ok_or(Error::Invalid("DVB ODS draft"))?;
        while self.lookup < state.objects.len() {
            if !work.pay(&mut self.credit, 1)? {
                return Ok(SegmentResult::Yield);
            }
            let old = &state.objects.as_slice()[self.lookup];
            if old.page == object.page && old.id == object.id {
                if old.version != object.version {
                    state.objects.as_mut_slice()[self.lookup] =
                        self.object.take().ok_or(Error::Invalid("DVB ODS draft"))?;
                }
                return Ok(SegmentResult::Complete);
            }
            self.lookup += 1;
            self.credit = 0;
        }
        limit(state.objects.len() + 1, state.limits.objects)?;
        state.objects.try_reserve_exact(1)?;
        state
            .objects
            .try_push(self.object.take().ok_or(Error::Invalid("DVB ODS draft"))?)
            .map_err(|e| e.error)?;
        Ok(SegmentResult::Complete)
    }
}

//! The display-set state machine: one method per [`Phase`], driven by
//! [`StoredDisplayDecoder::inner`]. Each method is one step of the original
//! loop; `Step::Continue` re-enters the loop and `Step::Break` returns.
use super::{
    Current, DisplayStatus, Frame, ObjectSlot, PaletteSlot, Phase, PreparedObject,
    StoredDisplayDecoder, stamp_known, stamp_time,
};
use crate::pgs::display_model::*;
use crate::pgs::display_palette::PaletteBuild;
use crate::pgs::display_render::{CachedObject, StoredRenderBuild};
use crate::pgs::display_work::DisplayWork;
use crate::pgs::indexed_argb::ArgbStorage;
use crate::pgs::{
    AssemblyStatus, CompositionObject, Crop, ObjectFragment, RasterJob, RasterLimits, RasterStatus,
    SegmentIter, Window,
};
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{BudgetVec, Error, Result, WorkBudget};

/// Outcome of one phase step.
type Step = ControlFlow<DisplayStatus>;
const NEXT: Step = Step::Continue(());
const YIELD: Step = Step::Break(DisplayStatus::Yield);

impl<A: ArgbStorage> StoredDisplayDecoder<A> {
    pub(super) fn inner(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<DisplayStatus> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if self.output.is_some() {
                return Ok(DisplayStatus::OutputReady);
            }
            if work.remaining() == 0 {
                return Ok(DisplayStatus::Yield);
            }
            let step = match self.phase {
                Phase::ReserveObjects => self.reserve(work, cancel, 1, Phase::AllocateObjects)?,
                Phase::AllocateObjects => self.allocate_objects()?,
                Phase::InitObjects => self.init_objects(work, cancel)?,
                Phase::ReservePalettes => self.reserve(work, cancel, 1, Phase::AllocatePalettes)?,
                Phase::AllocatePalettes => self.allocate_palettes()?,
                Phase::InitPalettes => self.init_palettes(work, cancel)?,
                Phase::ReserveWindows => self.reserve(work, cancel, 1, Phase::AllocateWindows)?,
                Phase::AllocateWindows => self.allocate_windows()?,
                Phase::InitWindows => self.init_windows(work, cancel)?,
                Phase::Next => self.next_segment(work, cancel)?,
                Phase::Trace => self.trace_segment(work, cancel)?,
                Phase::Dispatch => self.dispatch_segment()?,
                Phase::PcsHeader => self.presentation_header(work, cancel)?,
                Phase::ClearObjects => self.clear_objects(work, cancel)?,
                Phase::ClearPalettes => self.clear_palettes(work, cancel)?,
                Phase::ClearWindows => self.clear_windows(work, cancel)?,
                Phase::PcsReserve => self.reserve(work, cancel, 1, Phase::PcsAllocate)?,
                Phase::PcsAllocate => self.presentation_allocate()?,
                Phase::PcsReferences => self.presentation_reference(work, cancel)?,
                Phase::PaletteStart => self.palette_start(work, cancel)?,
                Phase::PaletteGrow => self.palette_grow(work, cancel)?,
                Phase::PaletteCreate => self.palette_create()?,
                Phase::PaletteStep => self.palette_step(work, cancel)?,
                Phase::WindowStart => self.window_start(work, cancel)?,
                Phase::WindowClear => self.window_clear(work, cancel)?,
                Phase::WindowEntries => self.window_entry(work, cancel)?,
                Phase::WindowGrowReserve => self.reserve(
                    work,
                    cancel,
                    self.windows.len().max(1),
                    Phase::WindowGrowAllocate,
                )?,
                Phase::WindowGrowAllocate => self.window_grow_allocate()?,
                Phase::WindowGrowInitialise => self.window_grow_initialise(work, cancel)?,
                Phase::ObjectStart => self.object_start(work, cancel)?,
                Phase::ObjectAllocate => self.object_allocate(cancel)?,
                Phase::ObjectStep => self.object_step(work, cancel)?,
                Phase::ObjectFinish => self.object_finish(work, cancel)?,
                Phase::RasterStep => self.raster_step(work, cancel)?,
                Phase::RasterFinish => self.raster_finish(work, cancel)?,
                Phase::EndCheck => self.end_check(work, cancel)?,
                Phase::EndReserve => self.reserve(work, cancel, 1, Phase::EndAllocate)?,
                Phase::EndAllocate => self.end_allocate()?,
                Phase::EndLookup => self.end_lookup(work, cancel)?,
                Phase::EndRender => self.end_render(work, cancel)?,
                Phase::EndPublish => self.end_publish(work, cancel)?,
                Phase::EofCheck => self.eof_check(work, cancel)?,
                Phase::Finished => Step::Break(DisplayStatus::Ended),
            };
            if let Step::Break(status) = step {
                return Ok(status);
            }
        }
    }

    /// Pays for the allocation phase that follows. The paid credit stays
    /// until that phase completes and clears it.
    #[inline]
    fn reserve(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        cost: usize,
        next: Phase,
    ) -> Result<Step> {
        if !self.pay(work, cancel, cost)? {
            return Ok(YIELD);
        }
        self.phase = next;
        Ok(NEXT)
    }

    #[inline]
    fn allocate_objects(&mut self) -> Result<Step> {
        self.objects
            .try_reserve_exact(self.limits.max_objects.min(1))?;
        self.done(Phase::InitObjects);
        Ok(NEXT)
    }

    #[inline]
    fn init_objects(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Step> {
        if self.objects.len() == self.limits.max_objects.min(1) {
            self.done(Phase::ReservePalettes);
            return Ok(NEXT);
        }
        if !self.pay(work, cancel, 1)? {
            return Ok(YIELD);
        }
        self.objects
            .try_push(ObjectSlot::default())
            .map_err(|e| e.error)?;
        self.credit = 0;
        Ok(NEXT)
    }

    #[inline]
    fn allocate_palettes(&mut self) -> Result<Step> {
        self.palettes
            .try_reserve_exact(self.limits.max_palettes.min(1))?;
        self.done(Phase::InitPalettes);
        Ok(NEXT)
    }

    #[inline]
    fn init_palettes(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Step> {
        if self.palettes.len() == self.limits.max_palettes.min(1) {
            self.done(Phase::ReserveWindows);
            return Ok(NEXT);
        }
        if !self.pay(work, cancel, 1)? {
            return Ok(YIELD);
        }
        self.palettes
            .try_push(PaletteSlot::default())
            .map_err(|e| e.error)?;
        self.credit = 0;
        Ok(NEXT)
    }

    #[inline]
    fn allocate_windows(&mut self) -> Result<Step> {
        self.windows.try_reserve_exact(1)?;
        self.done(Phase::InitWindows);
        Ok(NEXT)
    }

    #[inline]
    fn init_windows(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Step> {
        if self.windows.len() == 1 {
            self.done(Phase::Next);
            return Ok(NEXT);
        }
        if !self.pay(work, cancel, 1)? {
            return Ok(YIELD);
        }
        self.windows.try_push(None).map_err(|e| e.error)?;
        self.credit = 0;
        Ok(NEXT)
    }

    #[inline]
    fn next_segment(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Step> {
        let Some(packet) = self.packet.as_ref() else {
            if self.ended {
                if self.frame.is_some() {
                    return Err(Error::Truncated);
                }
                self.index = 0;
                self.done(Phase::EofCheck);
                return Ok(NEXT);
            }
            return Ok(Step::Break(DisplayStatus::NeedPacket));
        };
        if packet.at == packet.bytes.len() {
            self.packet = None;
            return Ok(NEXT);
        }
        if !self.pay(work, cancel, 1)? {
            return Ok(YIELD);
        }
        let packet = self
            .packet
            .as_ref()
            .ok_or(Error::Invalid("PGS packet absent"))?;
        let segment = SegmentIter::new(&packet.bytes[packet.at..], packet.envelope)
            .next()
            .ok_or(Error::Truncated)??;
        let payload = packet.at + segment.raw.len() - segment.payload.len();
        let end = packet.at + segment.raw.len();
        self.current = Some(Current {
            kind: segment.kind,
            payload,
            end,
            stamp: SegmentStamp {
                packet_sequence: packet.sequence,
                packet_clock: packet.clock,
                sup_clock: segment.clock,
                packet_offset: packet.at,
                envelope: packet.envelope,
            },
        });
        self.done(if segment.kind == 0x16 {
            Phase::PcsHeader
        } else {
            Phase::Trace
        });
        Ok(NEXT)
    }

    #[inline]
    fn trace_segment(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Step> {
        let cost = match &self.frame {
            Some(frame) if frame.pending_segment.is_some() => 2,
            Some(frame) if frame.segments.len() == frame.segments.capacity() => {
                frame.segments.len().checked_add(1).ok_or(Error::Overflow)?
            }
            _ => 1,
        };
        if !self.pay(work, cancel, cost)? {
            return Ok(YIELD);
        }
        self.trace()?;
        self.done(Phase::Dispatch);
        Ok(NEXT)
    }

    #[inline]
    fn dispatch_segment(&mut self) -> Result<Step> {
        self.done(match self.info()?.kind {
            0x14 => Phase::PaletteStart,
            0x15 => Phase::ObjectStart,
            0x17 => Phase::WindowStart,
            0x80 => Phase::EndCheck,
            _ => {
                self.advance()?;
                return Ok(NEXT);
            }
        });
        self.index = 0;
        Ok(NEXT)
    }

    #[inline]
    fn presentation_header(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Step> {
        if !self.pay(work, cancel, 1)? {
            return Ok(YIELD);
        }
        if self.frame.is_some() {
            return Err(Error::Invalid("PGS presentation before previous END"));
        }
        let p = self.payload()?;
        if p.len() < 11 {
            return Err(Error::Truncated);
        }
        let width = u16::from_be_bytes([p[0], p[1]]);
        let height = u16::from_be_bytes([p[2], p[3]]);
        if width == 0 || height == 0 {
            return Err(Error::Invalid("PGS zero canvas"));
        }
        for axis in [width, height] {
            if axis > self.limits.max_axis {
                return Err(Error::Limit {
                    requested: usize::from(axis),
                    maximum: usize::from(self.limits.max_axis),
                });
            }
        }
        let area = usize::from(width)
            .checked_mul(usize::from(height))
            .ok_or(Error::Overflow)?;
        if area > self.limits.max_canvas_pixels {
            return Err(Error::Limit {
                requested: area,
                maximum: self.limits.max_canvas_pixels,
            });
        }
        let count = usize::from(p[10]);
        if count > self.limits.max_references {
            return Err(Error::Limit {
                requested: count,
                maximum: self.limits.max_references,
            });
        }
        let state = CompositionState::parse(p[7])?;
        if p[8] & 0x7f != 0 {
            return Err(Error::Unsupported("PGS reserved palette update bits"));
        }
        let facts = PresentationFacts {
            width,
            height,
            frame_rate: p[4],
            number: u16::from_be_bytes([p[5], p[6]]),
            raw_state: p[7],
            state,
            palette_update_flags: p[8],
            palette_id: p[9],
            stamp: self.info()?.stamp,
        };
        self.frame = Some(Frame {
            presentation: facts,
            declared_references: count,
            references: BudgetVec::new(&self.budget),
            objects: BudgetVec::new(&self.budget),
            segments: BudgetVec::new(&self.budget),
            pending_segment: None,
            rendered_pixels: 0,
        });
        self.cursor = 11;
        self.index = 0;
        if state != CompositionState::Normal {
            self.epoch = self.epoch.checked_add(1).ok_or(Error::Overflow)?;
            self.done(Phase::ClearObjects);
        } else {
            self.done(Phase::PcsReserve);
        }
        Ok(NEXT)
    }

    #[inline]
    fn clear_objects(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Step> {
        if self.index == self.objects.len() {
            self.index = 0;
            self.done(Phase::ClearPalettes);
            return Ok(NEXT);
        }
        if !self.pay(work, cancel, 1)? {
            return Ok(YIELD);
        }
        self.objects.as_mut_slice()[self.index] = ObjectSlot::default();
        self.index += 1;
        self.credit = 0;
        Ok(NEXT)
    }

    #[inline]
    fn clear_palettes(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Step> {
        if self.index == self.palettes.len() {
            self.index = 0;
            self.done(Phase::ClearWindows);
            return Ok(NEXT);
        }
        if !self.pay(work, cancel, 1)? {
            return Ok(YIELD);
        }
        self.palettes.as_mut_slice()[self.index].id = None;
        self.index += 1;
        self.credit = 0;
        Ok(NEXT)
    }

    #[inline]
    fn clear_windows(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Step> {
        if self.index == self.windows.len() {
            self.index = 0;
            self.done(Phase::PcsReserve);
            return Ok(NEXT);
        }
        if !self.pay(work, cancel, 1)? {
            return Ok(YIELD);
        }
        self.windows.as_mut_slice()[self.index] = None;
        self.index += 1;
        self.credit = 0;
        Ok(NEXT)
    }

    #[inline]
    fn presentation_allocate(&mut self) -> Result<Step> {
        let frame = self
            .frame
            .as_mut()
            .ok_or(Error::Invalid("PGS frame absent"))?;
        frame
            .references
            .try_reserve_exact(frame.declared_references)?;
        self.trace()?;
        self.done(Phase::PcsReferences);
        Ok(NEXT)
    }

    #[inline]
    fn presentation_reference(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step> {
        let frame = self
            .frame
            .as_ref()
            .ok_or(Error::Invalid("PGS frame absent"))?;
        if frame.references.len() == frame.declared_references {
            if self.cursor != self.payload()?.len() {
                return Err(Error::Invalid("PGS trailing presentation fields"));
            }
            self.advance()?;
            return Ok(NEXT);
        }
        if !self.pay(work, cancel, 1)? {
            return Ok(YIELD);
        }
        let p = self.payload()?;
        let b = p
            .get(self.cursor..self.cursor + 8)
            .ok_or(Error::Truncated)?;
        let flags = b[3];
        if flags & 0x3f != 0 {
            return Err(Error::Unsupported("PGS reserved object composition bits"));
        }
        let mut reference = CompositionObject {
            id: u16::from_be_bytes([b[0], b[1]]),
            window: b[2],
            flags,
            x: u16::from_be_bytes([b[4], b[5]]),
            y: u16::from_be_bytes([b[6], b[7]]),
            crop: None,
        };
        let length = if flags & 0x80 != 0 {
            let c = p
                .get(self.cursor + 8..self.cursor + 16)
                .ok_or(Error::Truncated)?;
            reference.crop = Some(Crop {
                x: u16::from_be_bytes([c[0], c[1]]),
                y: u16::from_be_bytes([c[2], c[3]]),
                width: u16::from_be_bytes([c[4], c[5]]),
                height: u16::from_be_bytes([c[6], c[7]]),
            });
            16
        } else {
            8
        };
        self.frame
            .as_mut()
            .ok_or(Error::Invalid("PGS frame absent"))?
            .references
            .try_push(reference)
            .map_err(|e| e.error)?;
        self.cursor += length;
        self.credit = 0;
        Ok(NEXT)
    }

    #[inline]
    fn palette_start(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Step> {
        if !self.pay(work, cancel, self.palettes.len().max(1))? {
            return Ok(YIELD);
        }
        let payload = self.owned_payload()?;
        if payload.len() < 2 {
            return Err(Error::Truncated);
        }
        let id = payload[0];
        let found = self
            .palettes
            .as_slice()
            .iter()
            .position(|s| s.id == Some(id));
        if let Some(slot) =
            found.or_else(|| self.palettes.as_slice().iter().position(|s| s.id.is_none()))
        {
            self.slot = slot;
            self.done(Phase::PaletteCreate);
        } else {
            if self.palettes.len() == self.limits.max_palettes {
                return Err(Error::Limit {
                    requested: self.palettes.len() + 1,
                    maximum: self.limits.max_palettes,
                });
            }
            // Preserve the original syntax refusal before a new
            // slot allocation can report resource pressure.
            if (payload.len() - 2) % 5 != 0 {
                return Err(Error::Invalid("PGS partial palette entry"));
            }
            self.slot = self.palettes.len();
            self.done(Phase::PaletteGrow);
        }
        Ok(NEXT)
    }

    #[inline]
    fn palette_grow(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Step> {
        let length = self.palettes.len();
        let grows = length == self.palettes.capacity();
        let cost = if grows {
            length.checked_add(1).ok_or(Error::Overflow)?
        } else {
            1
        };
        if !self.pay(work, cancel, cost)? {
            return Ok(YIELD);
        }
        if grows {
            let capacity = self
                .palettes
                .capacity()
                .checked_mul(2)
                .ok_or(Error::Overflow)?
                .max(1)
                .min(self.limits.max_palettes);
            self.palettes.try_reserve_exact(capacity - length)?;
        }
        self.palettes
            .try_push(PaletteSlot::default())
            .map_err(|e| e.error)?;
        self.done(Phase::PaletteCreate);
        Ok(NEXT)
    }

    #[inline]
    fn palette_create(&mut self) -> Result<Step> {
        let payload = self.owned_payload()?;
        let slot = self.slot;
        let found = self.palettes.as_slice()[slot].id == Some(payload[0]);
        let previous = self.palettes.as_slice()[slot].image.clone();
        let matrix = NativeMatrix::for_canvas(
            self.frame
                .as_ref()
                .ok_or(Error::Invalid("PGS frame absent"))?
                .presentation
                .height,
        );
        self.palette_job = Some(PaletteBuild::new(
            &self.budget,
            payload,
            self.epoch,
            self.info()?.stamp,
            matrix,
            previous,
            found,
        )?);
        self.slot = slot;
        self.done(Phase::PaletteStep);
        Ok(NEXT)
    }

    #[inline]
    fn palette_step(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Step> {
        let mut ledger = DisplayWork {
            caller: work,
            spent: &mut self.work,
            maximum: self.limits.max_work,
            cancel,
        };
        if !self
            .palette_job
            .as_mut()
            .ok_or(Error::Invalid("PGS palette job absent"))?
            .step(&mut ledger)?
        {
            return Ok(YIELD);
        }
        let image = self
            .palette_job
            .as_mut()
            .ok_or(Error::Invalid("PGS palette job absent"))?
            .take()?;
        self.palettes.as_mut_slice()[self.slot] = PaletteSlot {
            id: Some(image.id()),
            image: Some(image),
        };
        self.palette_job = None;
        self.advance()?;
        Ok(NEXT)
    }

    #[inline]
    fn window_start(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Step> {
        if !self.pay(work, cancel, 1)? {
            return Ok(YIELD);
        }
        let payload = self.payload()?;
        let count = usize::from(*payload.first().ok_or(Error::Truncated)?);
        if count > self.limits.max_windows {
            return Err(Error::Limit {
                requested: count,
                maximum: self.limits.max_windows,
            });
        }
        if payload.len() != 1 + 9 * count {
            return Err(Error::Invalid("PGS window segment length"));
        }
        self.window_count = count;
        self.index = 0;
        self.done(Phase::WindowClear);
        Ok(NEXT)
    }

    #[inline]
    fn window_clear(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Step> {
        if self.index == self.windows.len() {
            self.index = 0;
            self.done(Phase::WindowEntries);
            return Ok(NEXT);
        }
        if !self.pay(work, cancel, 1)? {
            return Ok(YIELD);
        }
        self.windows.as_mut_slice()[self.index] = None;
        self.index += 1;
        self.credit = 0;
        Ok(NEXT)
    }

    #[inline]
    fn window_entry(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Step> {
        if self.index == self.window_count {
            self.advance()?;
            return Ok(NEXT);
        }
        if !self.pay(work, cancel, 1)? {
            return Ok(YIELD);
        }
        let p = self.payload()?;
        let at = 1 + 9 * self.index;
        let b = &p[at..at + 9];
        let w = Window {
            id: b[0],
            x: u16::from_be_bytes([b[1], b[2]]),
            y: u16::from_be_bytes([b[3], b[4]]),
            width: u16::from_be_bytes([b[5], b[6]]),
            height: u16::from_be_bytes([b[7], b[8]]),
        };
        let needed = usize::from(w.id) + 1;
        if self.windows.len() < needed {
            self.slot = needed;
            self.done(Phase::WindowGrowReserve);
            return Ok(NEXT);
        }
        self.windows.as_mut_slice()[usize::from(w.id)] = Some(w);
        self.index += 1;
        self.credit = 0;
        Ok(NEXT)
    }

    #[inline]
    fn window_grow_allocate(&mut self) -> Result<Step> {
        self.windows
            .try_reserve_exact(self.slot - self.windows.len())?;
        self.done(Phase::WindowGrowInitialise);
        Ok(NEXT)
    }

    #[inline]
    fn window_grow_initialise(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step> {
        if self.windows.len() == self.slot {
            self.done(Phase::WindowEntries);
            return Ok(NEXT);
        }
        if !self.pay(work, cancel, 1)? {
            return Ok(YIELD);
        }
        self.windows.try_push(None).map_err(|e| e.error)?;
        self.credit = 0;
        Ok(NEXT)
    }

    #[inline]
    fn object_start(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Step> {
        if !self.pay(work, cancel, self.objects.len().max(1))? {
            return Ok(YIELD);
        }
        let payload = self.owned_payload()?;
        let fragment = ObjectFragment::parse_payload(&payload)?;
        let found = self
            .objects
            .as_slice()
            .iter()
            .position(|s| s.id == Some(fragment.id));
        let available =
            found.or_else(|| self.objects.as_slice().iter().position(|s| s.id.is_none()));
        if let Some(slot) = available {
            let prepared = PreparedObject {
                id: fragment.id,
                version: fragment.version,
                first: fragment.first,
                payload,
            };
            self.commit_object(slot, prepared, cancel)?;
        } else {
            if self.objects.len() == self.limits.max_objects {
                return Err(Error::Limit {
                    requested: self.objects.len() + 1,
                    maximum: self.limits.max_objects,
                });
            }
            // Preserve the full-cache limit above, but a known
            // orphan must not depend on an unnecessary allocation.
            if fragment.first.is_none() {
                return Err(Error::Invalid("PGS object continuation without assembly"));
            }
            self.slot = self.objects.len();
            self.pending_object = Some(PreparedObject {
                id: fragment.id,
                version: fragment.version,
                first: fragment.first,
                payload,
            });
            // Parsing and both lookups have been paid and cached.
            // Retries only reserve/move slots under that prior
            // charge; they cannot repeat source parsing or scans.
            self.done(Phase::ObjectAllocate);
        }
        Ok(NEXT)
    }

    #[inline]
    fn object_allocate(&mut self, cancel: &AtomicBool) -> Result<Step> {
        if self.pending_object.is_none() || self.slot != self.objects.len() {
            return Err(Error::Invalid("PGS pending object growth absent"));
        }
        self.objects.try_reserve_exact(1)?;
        self.objects
            .try_push(ObjectSlot::default())
            .map_err(|e| e.error)?;
        let prepared = self
            .pending_object
            .take()
            .ok_or(Error::Invalid("PGS prepared object absent"))?;
        self.commit_object(self.slot, prepared, cancel)?;
        Ok(NEXT)
    }

    #[inline]
    fn object_step(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Step> {
        let item = &mut self.objects.as_mut_slice()[self.slot];
        let mut ledger = DisplayWork {
            caller: work,
            spent: &mut self.work,
            maximum: self.limits.max_work,
            cancel,
        };
        match ledger.child(|child| {
            item.assembly
                .as_mut()
                .ok_or(Error::Invalid("PGS object assembly absent"))?
                .poll(child, cancel)
        })? {
            AssemblyStatus::Yield => return Ok(YIELD),
            AssemblyStatus::NeedFragment => self.advance()?,
            AssemblyStatus::Ready => self.done(Phase::ObjectFinish),
        }
        Ok(NEXT)
    }

    #[inline]
    fn object_finish(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Step> {
        if !self.pay(work, cancel, 1)? {
            return Ok(YIELD);
        }
        let item = &mut self.objects.as_mut_slice()[self.slot];
        let encoded = item
            .assembly
            .as_mut()
            .ok_or(Error::Invalid("PGS object assembly absent"))?
            .try_finish(cancel)?;
        item.raster = Some(RasterJob::new(
            encoded.into_encoded(),
            item.meta
                .ok_or(Error::Invalid("PGS object metadata absent"))?
                .width,
            item.meta
                .ok_or(Error::Invalid("PGS object metadata absent"))?
                .height,
            &self.budget,
            RasterLimits {
                max_axis: self.limits.max_axis,
                max_pixels: self.limits.max_object_pixels,
                max_encoded: self.limits.max_encoded,
                max_work: self.limits.max_work,
            },
        )?);
        item.assembly = None;
        self.done(Phase::RasterStep);
        Ok(NEXT)
    }

    #[inline]
    fn raster_step(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Step> {
        let item = &mut self.objects.as_mut_slice()[self.slot];
        let mut ledger = DisplayWork {
            caller: work,
            spent: &mut self.work,
            maximum: self.limits.max_work,
            cancel,
        };
        match ledger.child(|child| {
            item.raster
                .as_mut()
                .ok_or(Error::Invalid("PGS raster job absent"))?
                .poll(child, cancel)
        })? {
            RasterStatus::Yield => return Ok(YIELD),
            RasterStatus::Ready => self.done(Phase::RasterFinish),
        }
        Ok(NEXT)
    }

    #[inline]
    fn raster_finish(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Step> {
        if !self.pay(work, cancel, 1)? {
            return Ok(YIELD);
        }
        let item = &mut self.objects.as_mut_slice()[self.slot];
        let validated_rle = if A::MATERIALIZE {
            None
        } else {
            item.raster
                .as_ref()
                .and_then(RasterJob::strict_validated_encoded)
                .cloned()
        };
        let image = item
            .raster
            .as_mut()
            .ok_or(Error::Invalid("PGS raster job absent"))?
            .try_finish(cancel)?;
        let meta = item
            .meta
            .ok_or(Error::Invalid("PGS object metadata absent"))?;
        item.image = Some(CachedObject {
            version: meta.version,
            width: image.width,
            height: image.height,
            pixels: image.pixels().clone(),
            // A small slice may retain a much larger physical
            // packet. Bound the real backing, not the slice length.
            // Larger inputs keep the original receiver fallback.
            validated_rle: validated_rle
                .filter(|rle| rle.retained_capacity() < image.pixels().len()),
            first: meta.first,
            last: meta.last,
        });
        item.raster = None;
        self.advance()?;
        Ok(NEXT)
    }

    #[inline]
    fn end_check(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Step> {
        if !self.payload()?.is_empty() {
            return Err(Error::Invalid("PGS END payload"));
        }
        if !self.pay(work, cancel, 1)? {
            return Ok(YIELD);
        }
        // END closes this presentation, not the entire object
        // preload/cache stream. Referenced objects are checked in
        // EndLookup; unused partial objects may continue later.
        self.index = 0;
        self.done(Phase::EndReserve);
        Ok(NEXT)
    }

    #[inline]
    fn end_allocate(&mut self) -> Result<Step> {
        let f = self
            .frame
            .as_mut()
            .ok_or(Error::Invalid("PGS frame absent"))?;
        f.objects.try_reserve_exact(f.references.len())?;
        self.index = 0;
        self.done(Phase::EndLookup);
        Ok(NEXT)
    }

    #[inline]
    fn end_lookup(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Step> {
        let frame = self
            .frame
            .as_ref()
            .ok_or(Error::Invalid("PGS frame absent"))?;
        if self.index == frame.references.len() {
            self.done(Phase::EndPublish);
            return Ok(NEXT);
        }
        if !self.pay(work, cancel, self.objects.len() + self.palettes.len() + 1)? {
            return Ok(YIELD);
        }
        let frame = self
            .frame
            .as_ref()
            .ok_or(Error::Invalid("PGS frame absent"))?;
        let reference = frame.references.as_slice()[self.index];
        let slot = self
            .objects
            .as_slice()
            .iter()
            .find(|s| s.id == Some(reference.id))
            .ok_or(Error::Invalid("PGS missing cached object"))?;
        if slot.assembly.is_some() || slot.raster.is_some() {
            return Err(Error::Truncated);
        }
        let object = slot
            .image
            .clone()
            .ok_or(Error::Invalid("PGS missing cached object"))?;
        let palette = self
            .palettes
            .as_slice()
            .iter()
            .find(|s| s.id == Some(frame.presentation.palette_id))
            .and_then(|s| s.image.clone())
            .ok_or(Error::Invalid("PGS missing cached palette"))?;
        let window = self
            .windows
            .as_slice()
            .get(usize::from(reference.window))
            .copied()
            .flatten();
        let canvas = (frame.presentation.width, frame.presentation.height);
        let area = usize::from(object.width)
            .checked_mul(usize::from(object.height))
            .ok_or(Error::Overflow)?;
        let total = frame
            .rendered_pixels
            .checked_add(area)
            .ok_or(Error::Overflow)?;
        if total > self.limits.max_display_pixels {
            return Err(Error::Limit {
                requested: total,
                maximum: self.limits.max_display_pixels,
            });
        }
        self.render_job = Some(StoredRenderBuild::<A>::new(
            &self.budget,
            object,
            reference,
            window,
            palette,
            canvas,
            self.config.native_forced_only,
        )?);
        self.frame
            .as_mut()
            .ok_or(Error::Invalid("PGS frame absent"))?
            .rendered_pixels = total;
        self.done(Phase::EndRender);
        Ok(NEXT)
    }

    #[inline]
    fn end_render(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Step> {
        let mut ledger = DisplayWork {
            caller: work,
            spent: &mut self.work,
            maximum: self.limits.max_work,
            cancel,
        };
        if !self
            .render_job
            .as_mut()
            .ok_or(Error::Invalid("PGS render job absent"))?
            .step(&mut ledger)?
        {
            return Ok(YIELD);
        }
        let object = self
            .render_job
            .as_mut()
            .ok_or(Error::Invalid("PGS render job absent"))?
            .take()?;
        self.frame
            .as_mut()
            .ok_or(Error::Invalid("PGS frame absent"))?
            .objects
            .try_push(object)
            .map_err(|e| e.error)?;
        self.render_job = None;
        self.index += 1;
        self.done(Phase::EndLookup);
        Ok(NEXT)
    }

    #[inline]
    fn end_publish(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Step> {
        if !self.pay(work, cancel, 1)? {
            return Ok(YIELD);
        }
        if self
            .frame
            .as_ref()
            .is_some_and(|f| f.pending_segment.is_some())
        {
            return Err(Error::Invalid("PGS unpublished first trace fact"));
        }
        let end_stamp = self.info()?.stamp;
        let frame = self
            .frame
            .take()
            .ok_or(Error::Invalid("PGS frame absent"))?;
        let (native_time, native_clock_source) = if stamp_known(frame.presentation.stamp) {
            (
                stamp_time(frame.presentation.stamp),
                NativeClockSource::Presentation,
            )
        } else if stamp_known(end_stamp) {
            (stamp_time(end_stamp), NativeClockSource::EndFallback)
        } else {
            (Ok(None), NativeClockSource::Unknown)
        };
        let (native_pts_microseconds, native_time_error) = match native_time {
            Ok(value) => (value, None),
            Err(error) => (None, Some(error)),
        };
        self.output = Some(StoredDisplaySet {
            generation: self.generation,
            epoch: self.epoch,
            presentation: frame.presentation,
            end_stamp,
            native_pts_microseconds,
            native_time_error,
            native_clock_source,
            source_end: None,
            objects: frame.objects,
            segments: frame.segments,
        });
        self.advance()?;
        Ok(NEXT)
    }

    #[inline]
    fn eof_check(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Step> {
        if self.index == self.objects.len() {
            self.done(Phase::Finished);
            return Ok(NEXT);
        }
        if !self.pay(work, cancel, 1)? {
            return Ok(YIELD);
        }
        let slot = &mut self.objects.as_mut_slice()[self.index];
        if let Some(assembly) = slot.assembly.as_mut() {
            assembly.finish_input(cancel)?;
        }
        if slot.raster.is_some() {
            return Err(Error::Truncated);
        }
        self.index += 1;
        self.credit = 0;
        Ok(NEXT)
    }
}

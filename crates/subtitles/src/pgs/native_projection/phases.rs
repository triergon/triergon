//! The receiver projection state machine: one method per [`Phase`], driven
//! by [`NativeProjector::inner`]. Each method is one step of the original
//! loop; `Step::Continue` re-enters the loop and `Step::Break` returns.
use super::{
    Image, NativeDisplay, NativeProjector, NativeRasterOrigin, NativeRect, Phase, ProjectionStatus,
    Slot,
};
use crate::pgs::display_work::DisplayWork;
use crate::pgs::{
    AssemblyLimits, AssemblyStatus, Envelope, ObjectAssembly, ObjectFragment, RasterJob,
    RasterLimits, RasterStatus, SegmentIter,
};
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{BudgetVec, Error, Result, WorkBudget};

/// Outcome of one phase step.
type Step = ControlFlow<ProjectionStatus>;
const NEXT: Step = Step::Continue(());
const YIELD: Step = Step::Break(ProjectionStatus::Yield);

impl NativeProjector {
    pub(super) fn inner(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ProjectionStatus> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if self.output.is_some() {
                return Ok(ProjectionStatus::Ready);
            }
            if work.remaining() == 0 {
                return Ok(ProjectionStatus::Yield);
            }
            let step = match self.phase {
                Phase::ReserveSlots => self.reserve(work, cancel, 1, Phase::AllocateSlots)?,
                Phase::AllocateSlots => self.allocate_slots()?,
                Phase::InitSlots => self.init_slots(work, cancel)?,
                Phase::SourceCheck => self.source_check(work, cancel)?,
                Phase::ResetSlots => self.reset_slots(work, cancel)?,
                Phase::Trace => self.trace_segment(work, cancel)?,
                Phase::TraceLookup => self.trace_lookup(work, cancel)?,
                Phase::AssemblyStep => self.assembly_step(work, cancel)?,
                Phase::AssemblyFinish => self.assembly_finish(work, cancel)?,
                Phase::RasterStep => self.raster_step(work, cancel)?,
                Phase::RasterFinish => self.raster_finish(work, cancel)?,
                Phase::ReserveRects => self.reserve_rects(work, cancel)?,
                Phase::AllocateRects => self.allocate_rects()?,
                Phase::Rect => self.rectangle(work, cancel)?,
                Phase::ReserveArgb => self.reserve(work, cancel, 1, Phase::AllocateArgb)?,
                Phase::AllocateArgb => self.allocate_argb()?,
                Phase::Argb => self.argb_byte(work, cancel)?,
                Phase::FreezeArgb => self.freeze_argb(work, cancel)?,
                Phase::Append => self.append_rectangle(work, cancel)?,
                Phase::Ready => self.publish(work, cancel)?,
                Phase::Idle => Step::Break(ProjectionStatus::NeedSource),
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
    fn allocate_slots(&mut self) -> Result<Step> {
        self.slots.try_reserve_exact(64)?;
        self.done(Phase::InitSlots);
        Ok(NEXT)
    }

    #[inline]
    fn init_slots(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Step> {
        if self.slots.len() == 64 {
            self.done(if self.source.is_some() {
                Phase::SourceCheck
            } else {
                Phase::Idle
            });
            return Ok(NEXT);
        }
        if !self.pay(work, cancel, 1)? {
            return Ok(YIELD);
        }
        self.slots.try_push(Slot::default()).map_err(|e| e.error)?;
        self.credit = 0;
        Ok(NEXT)
    }

    #[inline]
    fn source_check(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Step> {
        let source = self
            .source
            .as_ref()
            .ok_or(Error::Invalid("PGS receiver source absent"))?;
        if self.index < source.objects().len() {
            if !self.pay(work, cancel, 1)? {
                return Ok(YIELD);
            }
            let source = self
                .source
                .as_ref()
                .ok_or(Error::Invalid("PGS receiver source absent"))?;
            let object = &source.objects()[self.index];
            if !object.pixels().belongs_to(&self.budget)
                || !object.palette().belongs_to(&self.budget)
            {
                return Err(Error::ForeignPool);
            }
            self.index += 1;
            self.credit = 0;
            return Ok(NEXT);
        }
        let reset =
            self.generation != Some(source.generation) || source.presentation.raw_state != 0;
        self.generation = Some(source.generation);
        self.index = 0;
        self.done(if reset {
            Phase::ResetSlots
        } else {
            Phase::Trace
        });
        Ok(NEXT)
    }

    #[inline]
    fn reset_slots(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Step> {
        if self.index == self.slots.len() {
            self.index = 0;
            self.done(Phase::Trace);
            return Ok(NEXT);
        }
        if !self.pay(work, cancel, 1)? {
            return Ok(YIELD);
        }
        self.slots.as_mut_slice()[self.index] = Slot::default();
        self.index += 1;
        self.credit = 0;
        Ok(NEXT)
    }

    #[inline]
    fn trace_segment(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Step> {
        let source = self
            .source
            .as_ref()
            .ok_or(Error::Invalid("PGS receiver source absent"))?;
        if self.trace_index == source.segments().len() {
            self.done(Phase::ReserveRects);
            return Ok(NEXT);
        }
        if !self.pay(work, cancel, 1)? {
            return Ok(YIELD);
        }
        let source = self
            .source
            .as_ref()
            .ok_or(Error::Invalid("PGS receiver source absent"))?;
        let fact = &source.segments()[self.trace_index];
        if fact.kind == 0x15 {
            let segment = SegmentIter::new(&fact.raw, fact.stamp.envelope)
                .next()
                .ok_or(Error::Truncated)??;
            let fragment = ObjectFragment::parse(segment)?;
            let accepted = fragment.first.is_some_and(|first| {
                !fragment.data.is_empty()
                    && first.width != 0
                    && first.height != 0
                    && first.width <= source.presentation.width
                    && first.height <= source.presentation.height
            });
            self.pending_fragment = Some((
                fragment.id,
                if accepted { Some(fact.stamp) } else { None },
                fragment.first.is_some(),
            ));
            self.done(Phase::TraceLookup);
            return Ok(NEXT);
        }
        self.trace_index += 1;
        self.credit = 0;
        Ok(NEXT)
    }

    #[inline]
    fn trace_lookup(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Step> {
        let source = self
            .source
            .as_ref()
            .ok_or(Error::Invalid("PGS receiver source absent"))?;
        let cost = self
            .slots
            .len()
            .checked_add(source.objects().len())
            .ok_or(Error::Overflow)?;
        if !self.pay(work, cancel, cost)? {
            return Ok(YIELD);
        }
        let (id, stamp, first) = self
            .pending_fragment
            .take()
            .ok_or(Error::Invalid("PGS receiver fragment absent"))?;
        let index = self
            .slots
            .as_slice()
            .iter()
            .position(|s| s.id == Some(id))
            .or_else(|| self.slots.as_slice().iter().position(|s| s.id.is_none()))
            .ok_or(Error::Limit {
                requested: 65,
                maximum: 64,
            })?;
        let source = self
            .source
            .as_ref()
            .ok_or(Error::Invalid("PGS receiver source absent"))?;
        let slot = &mut self.slots.as_mut_slice()[index];
        slot.id = Some(id);
        if let Some(stamp) = stamp {
            slot.accepted_first = Some(stamp);
            // Reuse a strict source raster for every referenced object,
            // including the third and later references hidden by this API.
            if let Some(object) = source
                .objects()
                .iter()
                .find(|o| o.composition.id == id && o.object_first_stamp == stamp)
            {
                slot.image = Some(Image {
                    width: object.object_width,
                    height: object.object_height,
                    pixels: object.pixels().clone(),
                });
                slot.assembly = None;
            } else {
                // Unreferenced preloads have no DisplayObject owner. Decode
                // their actual owned ODS payloads with the proven bounded
                // primitives, so later rejected firsts can retain this image.
                slot.assembly = Some(ObjectAssembly::new(
                    &self.budget,
                    AssemblyLimits {
                        max_axis: u16::MAX,
                        max_work: self.max_work,
                        ..AssemblyLimits::default()
                    },
                ));
            }
        }
        if slot.assembly.is_some() && (!first || stamp.is_some()) {
            let fact = &source.segments()[self.trace_index];
            let header = match fact.stamp.envelope {
                Envelope::Packet => 3,
                Envelope::Sup => 13,
            };
            let payload = fact.raw.slice(header..fact.raw.len())?;
            slot.assembly
                .as_mut()
                .ok_or(Error::Invalid("PGS receiver assembly absent"))?
                .push(payload, cancel)
                .map_err(|e| e.error)?;
            self.build_slot = index;
            self.done(Phase::AssemblyStep);
        } else {
            self.trace_index += 1;
            self.done(Phase::Trace);
        }
        Ok(NEXT)
    }

    #[inline]
    fn assembly_step(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Step> {
        let slot = &mut self.slots.as_mut_slice()[self.build_slot];
        let mut ledger = DisplayWork {
            caller: work,
            spent: &mut self.spent,
            maximum: self.max_work,
            cancel,
        };
        match ledger.child(|w| {
            slot.assembly
                .as_mut()
                .ok_or(Error::Invalid("PGS receiver assembly absent"))?
                .poll(w, cancel)
        })? {
            AssemblyStatus::Yield => return Ok(YIELD),
            AssemblyStatus::NeedFragment => {
                self.trace_index += 1;
                self.done(Phase::Trace);
            }
            AssemblyStatus::Ready => self.done(Phase::AssemblyFinish),
        }
        Ok(NEXT)
    }

    #[inline]
    fn assembly_finish(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Step> {
        if !self.pay(work, cancel, 1)? {
            return Ok(YIELD);
        }
        let slot = &mut self.slots.as_mut_slice()[self.build_slot];
        let encoded = slot
            .assembly
            .as_mut()
            .ok_or(Error::Invalid("PGS receiver assembly absent"))?
            .try_finish(cancel)?;
        let (width, height) = (encoded.width, encoded.height);
        self.raster = Some(RasterJob::new(
            encoded.into_encoded(),
            width,
            height,
            &self.budget,
            RasterLimits {
                max_axis: u16::MAX,
                max_work: self.max_work,
                ..RasterLimits::default()
            },
        )?);
        slot.assembly = None;
        self.done(Phase::RasterStep);
        Ok(NEXT)
    }

    #[inline]
    fn raster_step(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Step> {
        let mut ledger = DisplayWork {
            caller: work,
            spent: &mut self.spent,
            maximum: self.max_work,
            cancel,
        };
        match ledger.child(|w| {
            self.raster
                .as_mut()
                .ok_or(Error::Invalid("PGS receiver raster absent"))?
                .poll(w, cancel)
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
        let image = self
            .raster
            .as_mut()
            .ok_or(Error::Invalid("PGS receiver raster absent"))?
            .try_finish(cancel)?;
        self.slots.as_mut_slice()[self.build_slot].image = Some(Image {
            width: image.width,
            height: image.height,
            pixels: image.pixels().clone(),
        });
        self.raster = None;
        self.trace_index += 1;
        self.done(Phase::Trace);
        Ok(NEXT)
    }

    #[inline]
    fn reserve_rects(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Step> {
        if !self.pay(work, cancel, 1)? {
            return Ok(YIELD);
        }
        self.rectangles = Some(BudgetVec::new(&self.budget));
        self.phase = Phase::AllocateRects;
        Ok(NEXT)
    }

    #[inline]
    fn allocate_rects(&mut self) -> Result<Step> {
        let count = self
            .source
            .as_ref()
            .ok_or(Error::Invalid("PGS receiver source absent"))?
            .objects()
            .len()
            .min(2);
        self.rectangles
            .as_mut()
            .ok_or(Error::Invalid("PGS receiver rectangles absent"))?
            .try_reserve_exact(count)?;
        self.done(Phase::Rect);
        Ok(NEXT)
    }

    #[inline]
    fn rectangle(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Step> {
        let source = self
            .source
            .as_ref()
            .ok_or(Error::Invalid("PGS receiver source absent"))?;
        if self.object_index == source.objects().len().min(2) {
            self.done(Phase::Ready);
            return Ok(NEXT);
        }
        if !self.pay(work, cancel, self.slots.len())? {
            return Ok(YIELD);
        }
        let source = self
            .source
            .as_ref()
            .ok_or(Error::Invalid("PGS receiver source absent"))?;
        let object = &source.objects()[self.object_index];
        let slot = self
            .slots
            .as_mut_slice()
            .iter_mut()
            .find(|s| s.id == Some(object.composition.id))
            .ok_or(Error::Invalid("PGS receiver object history absent"))?;
        if slot.accepted_first == Some(object.object_first_stamp) {
            slot.image = Some(Image {
                width: object.object_width,
                height: object.object_height,
                pixels: object.pixels().clone(),
            });
            self.origin = NativeRasterOrigin::SourceRaster;
        } else {
            self.origin = if slot.image.is_some() {
                NativeRasterOrigin::RetainedBeforeRejectedFirst
            } else {
                NativeRasterOrigin::EmptyAfterRejectedFirst
            };
        }
        self.image = Some(slot.image.clone().unwrap_or(Image {
            width: 0,
            height: 0,
            pixels: object.pixels().slice(0..0)?,
        }));
        self.argb = Some(BudgetVec::new(&self.budget));
        self.at = 0;
        self.done(Phase::ReserveArgb);
        Ok(NEXT)
    }

    #[inline]
    fn allocate_argb(&mut self) -> Result<Step> {
        let length = self
            .image
            .as_ref()
            .ok_or(Error::Invalid("PGS receiver image absent"))?
            .pixels
            .len()
            .checked_mul(4)
            .ok_or(Error::Overflow)?;
        self.argb
            .as_mut()
            .ok_or(Error::Invalid("PGS receiver ARGB absent"))?
            .try_reserve_exact(length)?;
        self.done(Phase::Argb);
        Ok(NEXT)
    }

    #[inline]
    fn argb_byte(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Step> {
        let image = self
            .image
            .as_ref()
            .ok_or(Error::Invalid("PGS receiver image absent"))?;
        if self.at == image.pixels.len() * 4 {
            self.done(Phase::FreezeArgb);
            return Ok(NEXT);
        }
        if !self.pay(work, cancel, 1)? {
            return Ok(YIELD);
        }
        let image = self
            .image
            .as_ref()
            .ok_or(Error::Invalid("PGS receiver image absent"))?;
        let index = image.pixels[self.at / 4];
        let source = self
            .source
            .as_ref()
            .ok_or(Error::Invalid("PGS receiver source absent"))?;
        let word = source.objects()[self.object_index].native_argb(index);
        self.argb
            .as_mut()
            .ok_or(Error::Invalid("PGS receiver ARGB absent"))?
            .try_push(word.to_be_bytes()[self.at % 4])
            .map_err(|e| e.error)?;
        self.at += 1;
        self.credit = 0;
        Ok(NEXT)
    }

    #[inline]
    fn freeze_argb(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Step> {
        if !self.pay(work, cancel, 1)? {
            return Ok(YIELD);
        }
        let buffer = self
            .argb
            .take()
            .ok_or(Error::Invalid("PGS receiver ARGB absent"))?;
        match buffer.freeze() {
            Ok(bytes) => {
                self.frozen_argb = Some(bytes);
                self.done(Phase::Append);
            }
            Err(failure) => {
                self.argb = Some(failure.buffer);
                return Err(failure.error);
            }
        }
        Ok(NEXT)
    }

    #[inline]
    fn append_rectangle(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Step> {
        if !self.pay(work, cancel, 1)? {
            return Ok(YIELD);
        }
        let source = self
            .source
            .as_ref()
            .ok_or(Error::Invalid("PGS receiver source absent"))?;
        let object = &source.objects()[self.object_index];
        let image = self
            .image
            .take()
            .ok_or(Error::Invalid("PGS receiver image absent"))?;
        let rectangle = NativeRect {
            source_object_index: self.object_index,
            x: object.native_x,
            y: object.native_y,
            width: image.width,
            height: image.height,
            stride: image.width,
            forced: object.composition.forced(),
            origin: self.origin,
            pixels: image.pixels,
            argb: self
                .frozen_argb
                .take()
                .ok_or(Error::Invalid("PGS receiver ARGB not frozen"))?,
            palette: object.palette().clone(),
            suppressed: object.native_forced_only_suppressed,
        };
        self.rectangles
            .as_mut()
            .ok_or(Error::Invalid("PGS receiver rectangles absent"))?
            .try_push(rectangle)
            .map_err(|e| e.error)?;
        self.object_index += 1;
        self.done(Phase::Rect);
        Ok(NEXT)
    }

    #[inline]
    fn publish(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Step> {
        if !self.pay(work, cancel, 1)? {
            return Ok(YIELD);
        }
        let source = self
            .source
            .take()
            .ok_or(Error::Invalid("PGS receiver source absent"))?;
        let pts = source.native_pts_microseconds;
        let omitted = source.objects().len().saturating_sub(2);
        self.output = Some(NativeDisplay {
            source,
            rectangles: self
                .rectangles
                .take()
                .ok_or(Error::Invalid("PGS receiver rectangles absent"))?,
            pts_microseconds: pts,
            omitted_source_references: omitted,
        });
        Ok(NEXT)
    }
}

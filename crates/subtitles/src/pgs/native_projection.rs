//! Explicit pinned-receiver projection. Source sets remain complete and
//! unchanged, including objects/sets the receiver API cannot emit together.
use super::display_work::DisplayWork;
use super::{
    AssemblyLimits, AssemblyStatus, Composition, DisplaySet, Envelope, ObjectAssembly,
    ObjectFragment, PaletteSnapshot, RasterJob, RasterLimits, RasterStatus, Segment, SegmentIter,
    SegmentStamp,
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result, WorkBudget};

pub const NATIVE_PROJECTION: &str = "NativeD32-PGS-public-API-projection-v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeRasterOrigin {
    SourceRaster,
    RetainedBeforeRejectedFirst,
    EmptyAfterRejectedFirst,
}

#[derive(Debug)]
pub struct NativeRect {
    pub source_object_index: usize,
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
    pub stride: u16,
    pub forced: bool,
    pub origin: NativeRasterOrigin,
    pub(crate) pixels: Bytes,
    pub(crate) argb: Bytes,
    pub(crate) palette: PaletteSnapshot,
    pub(crate) suppressed: bool,
}
impl NativeRect {
    pub fn pixels(&self) -> &Bytes {
        &self.pixels
    }
    pub fn argb_pixels(&self) -> &Bytes {
        &self.argb
    }
    pub fn native_argb(&self, index: u8) -> u32 {
        if self.suppressed {
            0
        } else {
            self.palette.native_argb(index)
        }
    }
    pub fn palette(&self) -> &PaletteSnapshot {
        &self.palette
    }
}

#[derive(Debug)]
pub struct NativeDisplay {
    source: DisplaySet,
    rectangles: BudgetVec<NativeRect>,
    /// Source native-time diagnostics remain on `source()`. The receiver
    /// representation uses its no-timestamp marker, exposed here as None.
    pub pts_microseconds: Option<i64>,
    pub omitted_source_references: usize,
}
impl NativeDisplay {
    pub fn source(&self) -> &DisplaySet {
        &self.source
    }
    pub fn rectangles(&self) -> &[NativeRect] {
        self.rectangles.as_slice()
    }
}

/// Literal context dimensions from an actual PCS, including a trailing PCS
/// that has not yet produced an END. Its source stamp remains available.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeCanvas {
    pub width: u16,
    pub height: u16,
    pub stamp: SegmentStamp,
}
impl NativeCanvas {
    pub fn parse(segment: Segment<'_>, stamp: SegmentStamp) -> Result<Self> {
        let composition = Composition::parse(segment)?;
        Ok(Self {
            width: composition.width,
            height: composition.height,
            stamp,
        })
    }
}

/// One native API call can report one subtitle. All strict source sets remain
/// owned here. The final PCS context is distinct from the first subtitle.
#[derive(Debug)]
pub struct NativePacket {
    sets: BudgetVec<NativeDisplay>,
    context: Option<NativeCanvas>,
}
impl NativePacket {
    pub fn from_sets(sets: BudgetVec<NativeDisplay>, context: Option<NativeCanvas>) -> Self {
        Self { sets, context }
    }
    pub fn source_sets(&self) -> &[NativeDisplay] {
        self.sets.as_slice()
    }
    pub fn receiver_output(&self) -> Option<&NativeDisplay> {
        self.sets.as_slice().first()
    }
    pub fn context(&self) -> Option<NativeCanvas> {
        self.context
    }
    pub fn context_canvas(&self) -> Option<[u16; 2]> {
        self.context.map(|c| [c.width, c.height])
    }
    pub fn omitted_receiver_events(&self) -> usize {
        self.sets.len().saturating_sub(1)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeEofBehavior {
    NoDelayedSubtitle,
    UnsupportedSourceFailure,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeEof {
    pub source_error: Option<Error>,
    pub receiver_error: Option<Error>,
    pub behavior: NativeEofBehavior,
}
impl NativeEof {
    /// Applies only to finalization after all packets, never packet parsing.
    /// Missing END/unused partial source objects remain source truncations;
    /// the pinned receiver's drain emits no fabricated clear or extra bitmap.
    pub fn from_source_finalization(source_error: Option<Error>) -> Self {
        match source_error {
            None | Some(Error::Truncated) => Self {
                source_error,
                receiver_error: None,
                behavior: NativeEofBehavior::NoDelayedSubtitle,
            },
            Some(error) => Self {
                source_error,
                receiver_error: Some(error),
                behavior: NativeEofBehavior::UnsupportedSourceFailure,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProjectionStatus {
    NeedSource,
    Yield,
    Ready,
}

#[derive(Clone, Debug)]
struct Image {
    width: u16,
    height: u16,
    pixels: Bytes,
}
#[derive(Debug, Default)]
struct Slot {
    id: Option<u16>,
    accepted_first: Option<SegmentStamp>,
    image: Option<Image>,
    assembly: Option<ObjectAssembly>,
}
#[derive(Clone, Copy, Debug)]
enum Phase {
    ReserveSlots,
    AllocateSlots,
    InitSlots,
    SourceCheck,
    ResetSlots,
    Trace,
    TraceLookup,
    AssemblyStep,
    AssemblyFinish,
    RasterStep,
    RasterFinish,
    ReserveRects,
    AllocateRects,
    Rect,
    ReserveArgb,
    AllocateArgb,
    Argb,
    FreezeArgb,
    Append,
    Ready,
    Idle,
}

#[derive(Debug)]
pub struct NativeProjector {
    budget: Budget,
    slots: BudgetVec<Slot>,
    source: Option<DisplaySet>,
    rectangles: Option<BudgetVec<NativeRect>>,
    output: Option<NativeDisplay>,
    phase: Phase,
    credit: usize,
    index: usize,
    trace_index: usize,
    object_index: usize,
    pending_fragment: Option<(u16, Option<SegmentStamp>, bool)>,
    build_slot: usize,
    raster: Option<RasterJob>,
    generation: Option<u64>,
    image: Option<Image>,
    origin: NativeRasterOrigin,
    argb: Option<BudgetVec<u8>>,
    frozen_argb: Option<Bytes>,
    at: usize,
    spent: usize,
    max_work: usize,
    failed: Option<Error>,
}

impl NativeProjector {
    pub fn new(budget: &Budget, max_work: usize) -> Self {
        Self {
            budget: budget.clone(),
            slots: BudgetVec::new(budget),
            source: None,
            rectangles: None,
            output: None,
            phase: Phase::ReserveSlots,
            credit: 0,
            index: 0,
            trace_index: 0,
            object_index: 0,
            pending_fragment: None,
            build_slot: 0,
            raster: None,
            generation: None,
            image: None,
            origin: NativeRasterOrigin::SourceRaster,
            argb: None,
            frozen_argb: None,
            at: 0,
            spent: 0,
            max_work,
            failed: None,
        }
    }
    fn poison(&mut self, error: Error) -> Error {
        self.source = None;
        self.rectangles = None;
        self.output = None;
        self.image = None;
        self.argb = None;
        self.frozen_argb = None;
        self.raster = None;
        self.pending_fragment = None;
        self.slots = BudgetVec::new(&self.budget);
        self.failed = Some(error);
        error
    }
    pub fn work_spent(&self) -> usize {
        self.spent
    }

    /// The caller's option is consumed only on acceptance. Busy/error paths
    /// retain its exact source owner without another allocation.
    pub fn push(&mut self, source: &mut Option<DisplaySet>, cancel: &AtomicBool) -> Result<()> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(self.poison(Error::Cancelled));
        }
        if self.source.is_some() || self.output.is_some() {
            return Err(Error::Backpressure {
                requested: 1,
                available: 0,
            });
        }
        let Some(view) = source.as_ref() else {
            return Err(self.poison(Error::Invalid("PGS receiver source absent")));
        };
        if !view
            .segments()
            .first()
            .is_some_and(|s| s.raw.belongs_to(&self.budget))
        {
            return Err(self.poison(Error::ForeignPool));
        }
        self.source = source.take();
        self.index = 0;
        self.trace_index = 0;
        self.object_index = 0;
        if matches!(self.phase, Phase::Idle) {
            self.done(Phase::SourceCheck);
        }
        Ok(())
    }
    pub fn take_output(&mut self, cancel: &AtomicBool) -> Result<NativeDisplay> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(self.poison(Error::Cancelled));
        }
        match self.output.take() {
            Some(value) => {
                self.done(Phase::Idle);
                Ok(value)
            }
            None => Err(self.poison(Error::Invalid("PGS receiver output not ready"))),
        }
    }
    pub fn reset(&mut self, cancel: &AtomicBool) -> Result<()> {
        if cancel.load(Ordering::Relaxed) {
            return Err(self.poison(Error::Cancelled));
        }
        self.source = None;
        self.rectangles = None;
        self.output = None;
        self.image = None;
        self.argb = None;
        self.frozen_argb = None;
        self.raster = None;
        self.pending_fragment = None;
        self.slots = BudgetVec::new(&self.budget);
        self.phase = Phase::ReserveSlots;
        self.credit = 0;
        self.index = 0;
        self.generation = None;
        self.spent = 0;
        self.failed = None;
        Ok(())
    }
    fn done(&mut self, next: Phase) {
        self.phase = next;
        self.credit = 0;
    }
    fn pay(&mut self, work: &mut WorkBudget, cancel: &AtomicBool, cost: usize) -> Result<bool> {
        DisplayWork {
            caller: work,
            spent: &mut self.spent,
            maximum: self.max_work,
            cancel,
        }
        .pay(&mut self.credit, cost)
    }
    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<ProjectionStatus> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(self.poison(Error::Cancelled));
        }
        if self.output.is_some() {
            return Ok(ProjectionStatus::Ready);
        }
        if work.remaining() == 0 {
            return Ok(ProjectionStatus::Yield);
        }
        match self.inner(work, cancel) {
            Ok(status) => Ok(status),
            Err(error @ Error::Backpressure { .. }) => Err(error),
            Err(error) => Err(self.poison(error)),
        }
    }
    fn inner(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<ProjectionStatus> {
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
            match self.phase {
                Phase::ReserveSlots => {
                    if !self.pay(work, cancel, 1)? {
                        return Ok(ProjectionStatus::Yield);
                    }
                    self.phase = Phase::AllocateSlots;
                }
                Phase::AllocateSlots => {
                    self.slots.try_reserve_exact(64)?;
                    self.done(Phase::InitSlots);
                }
                Phase::InitSlots => {
                    if self.slots.len() == 64 {
                        self.done(if self.source.is_some() {
                            Phase::SourceCheck
                        } else {
                            Phase::Idle
                        });
                        continue;
                    }
                    if !self.pay(work, cancel, 1)? {
                        return Ok(ProjectionStatus::Yield);
                    }
                    self.slots.try_push(Slot::default()).map_err(|e| e.error)?;
                    self.credit = 0;
                }
                Phase::SourceCheck => {
                    let source = self
                        .source
                        .as_ref()
                        .ok_or(Error::Invalid("PGS receiver source absent"))?;
                    if self.index < source.objects().len() {
                        if !self.pay(work, cancel, 1)? {
                            return Ok(ProjectionStatus::Yield);
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
                        continue;
                    }
                    let reset = self.generation != Some(source.generation)
                        || source.presentation.raw_state != 0;
                    self.generation = Some(source.generation);
                    self.index = 0;
                    self.done(if reset {
                        Phase::ResetSlots
                    } else {
                        Phase::Trace
                    });
                }
                Phase::ResetSlots => {
                    if self.index == self.slots.len() {
                        self.index = 0;
                        self.done(Phase::Trace);
                        continue;
                    }
                    if !self.pay(work, cancel, 1)? {
                        return Ok(ProjectionStatus::Yield);
                    }
                    self.slots.as_mut_slice()[self.index] = Slot::default();
                    self.index += 1;
                    self.credit = 0;
                }
                Phase::Trace => {
                    let source = self
                        .source
                        .as_ref()
                        .ok_or(Error::Invalid("PGS receiver source absent"))?;
                    if self.trace_index == source.segments().len() {
                        self.done(Phase::ReserveRects);
                        continue;
                    }
                    if !self.pay(work, cancel, 1)? {
                        return Ok(ProjectionStatus::Yield);
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
                        continue;
                    }
                    self.trace_index += 1;
                    self.credit = 0;
                }
                Phase::TraceLookup => {
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
                        return Ok(ProjectionStatus::Yield);
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
                }
                Phase::AssemblyStep => {
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
                        AssemblyStatus::Yield => return Ok(ProjectionStatus::Yield),
                        AssemblyStatus::NeedFragment => {
                            self.trace_index += 1;
                            self.done(Phase::Trace);
                        }
                        AssemblyStatus::Ready => self.done(Phase::AssemblyFinish),
                    }
                }
                Phase::AssemblyFinish => {
                    if !self.pay(work, cancel, 1)? {
                        return Ok(ProjectionStatus::Yield);
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
                }
                Phase::RasterStep => {
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
                        RasterStatus::Yield => return Ok(ProjectionStatus::Yield),
                        RasterStatus::Ready => self.done(Phase::RasterFinish),
                    }
                }
                Phase::RasterFinish => {
                    if !self.pay(work, cancel, 1)? {
                        return Ok(ProjectionStatus::Yield);
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
                }
                Phase::ReserveRects => {
                    if !self.pay(work, cancel, 1)? {
                        return Ok(ProjectionStatus::Yield);
                    }
                    self.rectangles = Some(BudgetVec::new(&self.budget));
                    self.phase = Phase::AllocateRects;
                }
                Phase::AllocateRects => {
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
                }
                Phase::Rect => {
                    let source = self
                        .source
                        .as_ref()
                        .ok_or(Error::Invalid("PGS receiver source absent"))?;
                    if self.object_index == source.objects().len().min(2) {
                        self.done(Phase::Ready);
                        continue;
                    }
                    if !self.pay(work, cancel, self.slots.len())? {
                        return Ok(ProjectionStatus::Yield);
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
                }
                Phase::ReserveArgb => {
                    if !self.pay(work, cancel, 1)? {
                        return Ok(ProjectionStatus::Yield);
                    }
                    self.phase = Phase::AllocateArgb;
                }
                Phase::AllocateArgb => {
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
                }
                Phase::Argb => {
                    let image = self
                        .image
                        .as_ref()
                        .ok_or(Error::Invalid("PGS receiver image absent"))?;
                    if self.at == image.pixels.len() * 4 {
                        self.done(Phase::FreezeArgb);
                        continue;
                    }
                    if !self.pay(work, cancel, 1)? {
                        return Ok(ProjectionStatus::Yield);
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
                }
                Phase::FreezeArgb => {
                    if !self.pay(work, cancel, 1)? {
                        return Ok(ProjectionStatus::Yield);
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
                }
                Phase::Append => {
                    if !self.pay(work, cancel, 1)? {
                        return Ok(ProjectionStatus::Yield);
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
                }
                Phase::Ready => {
                    if !self.pay(work, cancel, 1)? {
                        return Ok(ProjectionStatus::Yield);
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
                }
                Phase::Idle => return Ok(ProjectionStatus::NeedSource),
            }
        }
    }
}

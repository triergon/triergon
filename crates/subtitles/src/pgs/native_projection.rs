//! Explicit pinned-receiver projection. Source sets remain complete and
//! unchanged, including objects/sets the receiver API cannot emit together.
mod phases;

use super::display_work::DisplayWork;
use super::{
    Composition, DisplaySet, ObjectAssembly, PaletteSnapshot, RasterJob, Segment, SegmentStamp,
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
}

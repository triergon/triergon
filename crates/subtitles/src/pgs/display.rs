//! Original bounded display-set interpreter. Each accepted segment remains an
//! owned source fact; cache/source state and native receiver projections differ
//! explicitly where the pinned receiver ignores crop/window metadata.
use super::display_model::*;
use super::display_palette::PaletteBuild;
use super::display_render::{CachedObject, StoredRenderBuild};
use super::display_work::DisplayWork;
use super::indexed_argb::{ArgbStorage, IndexedArgb};
use super::{
    AssemblyLimits, AssemblyStatus, CompositionObject, Crop, Envelope, ObjectAssembly,
    ObjectFragment, ObjectStart, RasterJob, RasterLimits, RasterStatus, SegmentIter, Window,
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result, WorkBudget};

#[derive(Clone, Copy, Debug)]
pub struct DisplayLimits {
    pub max_axis: u16,
    pub max_canvas_pixels: usize,
    pub max_object_pixels: usize,
    pub max_display_pixels: usize,
    pub max_encoded: usize,
    pub max_fragments: usize,
    pub max_objects: usize,
    pub max_palettes: usize,
    pub max_references: usize,
    pub max_windows: usize,
    pub max_segments: usize,
    pub max_packet_bytes: usize,
    /// Cumulative deterministic work until explicit reset, not wall time.
    pub max_work: usize,
}

impl Default for DisplayLimits {
    fn default() -> Self {
        Self {
            max_axis: u16::MAX,
            max_canvas_pixels: 64 * 1024 * 1024,
            max_object_pixels: 8 * 1024 * 1024,
            max_display_pixels: 16 * 1024 * 1024,
            max_encoded: 4 * 1024 * 1024,
            max_fragments: 1024,
            max_objects: 64,
            max_palettes: 8,
            max_references: 255,
            max_windows: 255,
            max_segments: 256,
            max_packet_bytes: 16 * 1024 * 1024,
            max_work: 256 * 1024 * 1024,
        }
    }
}

impl DisplayLimits {
    /// Shared policy bounds for strict and explicit native packet entry points.
    pub(super) fn validate(self) -> Result<()> {
        for (requested, maximum) in [
            (self.max_objects, 64),
            (self.max_palettes, 8),
            (self.max_references, 255),
            (self.max_windows, 255),
            (self.max_segments, 4096),
        ] {
            if requested > maximum {
                return Err(Error::Limit { requested, maximum });
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DisplayConfig {
    pub native_forced_only: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisplayStatus {
    NeedPacket,
    Yield,
    OutputReady,
    Ended,
}

#[derive(Debug)]
pub struct DisplayPushError {
    pub error: Error,
    pub packet: Bytes,
    pub clock: PacketClock,
    pub envelope: Envelope,
}

#[derive(Debug)]
struct Packet {
    bytes: Bytes,
    clock: PacketClock,
    envelope: Envelope,
    sequence: u64,
    at: usize,
}

#[derive(Clone, Copy, Debug)]
struct Current {
    kind: u8,
    payload: usize,
    end: usize,
    stamp: SegmentStamp,
}

#[derive(Clone, Copy, Debug)]
struct ObjectMeta {
    version: u8,
    width: u16,
    height: u16,
    first: SegmentStamp,
    last: SegmentStamp,
}

#[derive(Debug, Default)]
struct ObjectSlot {
    id: Option<u16>,
    meta: Option<ObjectMeta>,
    assembly: Option<ObjectAssembly>,
    raster: Option<RasterJob>,
    image: Option<CachedObject>,
}

#[derive(Debug)]
struct PreparedObject {
    payload: Bytes,
    id: u16,
    version: u8,
    first: Option<ObjectStart>,
}

#[derive(Debug, Default)]
struct PaletteSlot {
    id: Option<u8>,
    image: Option<PaletteSnapshot>,
}

#[derive(Debug)]
struct Frame<A: ArgbStorage> {
    presentation: PresentationFacts,
    declared_references: usize,
    references: BudgetVec<CompositionObject>,
    objects: BudgetVec<StoredDisplayObject<A>>,
    segments: BudgetVec<SegmentFact>,
    // The first physical fact is retained verbatim before any trace allocation.
    // A second segment moves it into the ordinary ordered published vector.
    pending_segment: Option<SegmentFact>,
    rendered_pixels: usize,
}

#[derive(Clone, Copy, Debug)]
enum Phase {
    ReserveObjects,
    AllocateObjects,
    InitObjects,
    ReservePalettes,
    AllocatePalettes,
    InitPalettes,
    ReserveWindows,
    AllocateWindows,
    InitWindows,
    Next,
    Trace,
    Dispatch,
    PcsHeader,
    ClearObjects,
    ClearPalettes,
    ClearWindows,
    PcsReserve,
    PcsAllocate,
    PcsReferences,
    PaletteStart,
    PaletteGrow,
    PaletteCreate,
    PaletteStep,
    WindowStart,
    WindowClear,
    WindowEntries,
    WindowGrowReserve,
    WindowGrowAllocate,
    WindowGrowInitialise,
    ObjectStart,
    ObjectAllocate,
    ObjectStep,
    ObjectFinish,
    RasterStep,
    RasterFinish,
    EndCheck,
    EndReserve,
    EndAllocate,
    EndLookup,
    EndRender,
    EndPublish,
    EofCheck,
    Finished,
}

pub type DisplayDecoder = StoredDisplayDecoder<Bytes>;
/// Additive strict decoder with owned indexed ARGB output. Parsing, source
/// metadata, visibility, cache rules and refusal grammar use the same machine.
pub type IndexedDisplayDecoder = StoredDisplayDecoder<IndexedArgb>;

#[derive(Debug)]
pub struct StoredDisplayDecoder<A: ArgbStorage> {
    budget: Budget,
    limits: DisplayLimits,
    config: DisplayConfig,
    objects: BudgetVec<ObjectSlot>,
    palettes: BudgetVec<PaletteSlot>,
    windows: BudgetVec<Option<Window>>,
    packet: Option<Packet>,
    current: Option<Current>,
    frame: Option<Frame<A>>,
    output: Option<StoredDisplaySet<A>>,
    palette_job: Option<PaletteBuild>,
    render_job: Option<StoredRenderBuild<A>>,
    pending_object: Option<PreparedObject>,
    slot: usize,
    phase: Phase,
    credit: usize,
    index: usize,
    cursor: usize,
    window_count: usize,
    work: usize,
    generation: u64,
    epoch: u64,
    next_packet: u64,
    ended: bool,
    failed: Option<Error>,
}

impl<A: ArgbStorage> StoredDisplayDecoder<A> {
    pub fn new(budget: &Budget, limits: DisplayLimits, config: DisplayConfig) -> Result<Self> {
        limits.validate()?;
        Ok(Self {
            budget: budget.clone(),
            limits,
            config,
            objects: BudgetVec::new(budget),
            palettes: BudgetVec::new(budget),
            windows: BudgetVec::new(budget),
            packet: None,
            current: None,
            frame: None,
            output: None,
            palette_job: None,
            render_job: None,
            pending_object: None,
            slot: 0,
            phase: Phase::ReserveObjects,
            credit: 0,
            index: 0,
            cursor: 0,
            window_count: 0,
            work: 0,
            generation: 0,
            epoch: 0,
            next_packet: 0,
            ended: false,
            failed: None,
        })
    }

    fn clear_graph(&mut self) {
        self.packet = None;
        self.current = None;
        self.frame = None;
        self.output = None;
        self.palette_job = None;
        self.render_job = None;
        self.pending_object = None;
        self.objects = BudgetVec::new(&self.budget);
        self.palettes = BudgetVec::new(&self.budget);
        self.windows = BudgetVec::new(&self.budget);
    }
    // Sibling receiver policies may release this graph without discarding the
    // checked generation and packet identity counters. This is not a reset.
    pub(super) fn poison(&mut self, error: Error) -> Error {
        self.clear_graph();
        self.failed = Some(error);
        error
    }
    pub fn work_spent(&self) -> usize {
        self.work
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn epoch(&self) -> u64 {
        self.epoch
    }
    pub(super) fn evidence_slots(&self) -> usize {
        self.objects.len()
    }

    /// Caller funds the bounded slot search. No source facts are changed by
    /// sharing immutable decoded bytes with an independently named receiver.
    pub(super) fn validated_indices(
        &self,
        id: u16,
        width: u16,
        height: u16,
    ) -> Option<(Bytes, Bytes)> {
        let image = self
            .objects
            .as_slice()
            .iter()
            .find(|slot| slot.id == Some(id))?
            .image
            .as_ref()?;
        if image.width != width || image.height != height {
            return None;
        }
        let encoded = image.validated_rle.as_ref()?;
        Some((encoded.clone(), image.pixels.clone()))
    }

    pub fn cached_objects(&self) -> usize {
        self.objects
            .as_slice()
            .iter()
            .filter(|s| s.image.is_some())
            .count()
    }
    pub fn cached_palettes(&self) -> usize {
        self.palettes
            .as_slice()
            .iter()
            .filter(|s| s.id.is_some())
            .count()
    }

    pub fn push(
        &mut self,
        packet: Bytes,
        clock: PacketClock,
        envelope: Envelope,
        cancel: &AtomicBool,
    ) -> std::result::Result<(), DisplayPushError> {
        let error = if let Some(error) = self.failed {
            Some(error)
        } else if cancel.load(Ordering::Relaxed) {
            Some(self.poison(Error::Cancelled))
        } else if self.ended {
            Some(self.poison(Error::Invalid("PGS input already closed")))
        } else if !packet.belongs_to(&self.budget) {
            Some(self.poison(Error::ForeignPool))
        } else if packet.len() > self.limits.max_packet_bytes {
            Some(self.poison(Error::Limit {
                requested: packet.len(),
                maximum: self.limits.max_packet_bytes,
            }))
        } else if self.packet.is_some() || self.output.is_some() {
            Some(Error::Backpressure {
                requested: 1,
                available: 0,
            })
        } else if self.next_packet == u64::MAX {
            Some(self.poison(Error::Overflow))
        } else {
            None
        };
        if let Some(error) = error {
            return Err(DisplayPushError {
                error,
                packet,
                clock,
                envelope,
            });
        }
        let sequence = self.next_packet;
        self.next_packet += 1;
        self.packet = Some(Packet {
            bytes: packet,
            clock,
            envelope,
            sequence,
            at: 0,
        });
        Ok(())
    }

    pub fn finish_input(&mut self, cancel: &AtomicBool) -> Result<()> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(self.poison(Error::Cancelled));
        }
        self.ended = true;
        if self.packet.is_none() && self.frame.is_some() {
            return Err(self.poison(Error::Truncated));
        }
        Ok(())
    }

    /// Explicit hard reset: invalidates unpublished state and reinitializes a
    /// fresh receiver generation. Caller-held outputs remain charged and valid.
    pub fn reset(&mut self, cancel: &AtomicBool) -> Result<()> {
        if cancel.load(Ordering::Relaxed) {
            return Err(self.poison(Error::Cancelled));
        }
        let generation = self.generation.checked_add(1).ok_or(Error::Overflow)?;
        self.clear_graph();
        self.failed = None;
        self.ended = false;
        self.work = 0;
        self.epoch = 0;
        self.generation = generation;
        self.phase = Phase::ReserveObjects;
        self.credit = 0;
        self.index = 0;
        self.cursor = 0;
        self.window_count = 0;
        Ok(())
    }

    pub fn take_output(&mut self, cancel: &AtomicBool) -> Result<StoredDisplaySet<A>> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(self.poison(Error::Cancelled));
        }
        match self.output.take() {
            Some(output) => Ok(output),
            None => Err(self.poison(Error::Invalid("PGS output not ready"))),
        }
    }

    fn pay(&mut self, work: &mut WorkBudget, cancel: &AtomicBool, cost: usize) -> Result<bool> {
        DisplayWork {
            caller: work,
            spent: &mut self.work,
            maximum: self.limits.max_work,
            cancel,
        }
        .pay(&mut self.credit, cost)
    }
    fn done(&mut self, next: Phase) {
        self.phase = next;
        self.credit = 0;
    }
    fn info(&self) -> Result<Current> {
        self.current.ok_or(Error::Invalid("PGS segment absent"))
    }
    fn payload(&self) -> Result<&[u8]> {
        let info = self.info()?;
        self.packet
            .as_ref()
            .and_then(|p| p.bytes.get(info.payload..info.end))
            .ok_or(Error::Invalid("PGS payload bounds"))
    }
    fn owned_payload(&self) -> Result<Bytes> {
        let info = self.info()?;
        self.packet
            .as_ref()
            .ok_or(Error::Invalid("PGS packet absent"))?
            .bytes
            .slice(info.payload..info.end)
    }
    fn advance(&mut self) -> Result<()> {
        let end = self.info()?.end;
        self.packet
            .as_mut()
            .ok_or(Error::Invalid("PGS packet absent"))?
            .at = end;
        self.current = None;
        self.done(Phase::Next);
        Ok(())
    }
    fn trace(&mut self) -> Result<()> {
        let info = self.info()?;
        let packet = self
            .packet
            .as_ref()
            .ok_or(Error::Invalid("PGS packet absent"))?;
        let raw = packet.bytes.slice(info.stamp.packet_offset..info.end)?;
        let frame = self
            .frame
            .as_mut()
            .ok_or(Error::Invalid("PGS segment before presentation"))?;
        let length = frame
            .segments
            .len()
            .checked_add(usize::from(frame.pending_segment.is_some()))
            .ok_or(Error::Overflow)?;
        if length >= self.limits.max_segments {
            return Err(Error::Limit {
                requested: length.checked_add(1).ok_or(Error::Overflow)?,
                maximum: self.limits.max_segments,
            });
        }
        let fact = SegmentFact {
            kind: info.kind,
            stamp: info.stamp,
            raw,
        };
        if length == 0 {
            frame.pending_segment = Some(fact);
            return Ok(());
        }
        if frame.pending_segment.is_some() {
            // Reserve before taking the sole unpublished fact. Pressure retry
            // retains the physical owner, and cannot duplicate it on success.
            frame
                .segments
                .try_reserve_exact(self.limits.max_segments.min(4))?;
            let first = frame
                .pending_segment
                .take()
                .ok_or(Error::Invalid("PGS first trace fact absent"))?;
            if let Err(failure) = frame.segments.try_push(first) {
                frame.pending_segment = Some(failure.value);
                return Err(failure.error);
            }
        }
        frame.segments.try_push(fact).map_err(|e| e.error)
    }

    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<DisplayStatus> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(self.poison(Error::Cancelled));
        }
        if self.output.is_some() {
            return Ok(DisplayStatus::OutputReady);
        }
        if work.remaining() == 0 {
            return Ok(DisplayStatus::Yield);
        }
        match self.inner(work, cancel) {
            Ok(status) => Ok(status),
            Err(error @ Error::Backpressure { .. }) => Err(error),
            Err(error) => Err(self.poison(error)),
        }
    }

    fn commit_object(
        &mut self,
        slot: usize,
        prepared: PreparedObject,
        cancel: &AtomicBool,
    ) -> Result<()> {
        let stamp = self.info()?.stamp;
        let item = self
            .objects
            .as_mut_slice()
            .get_mut(slot)
            .ok_or(Error::Invalid("PGS object slot absent"))?;
        if let Some(first) = prepared.first {
            if item.assembly.is_some() {
                return Err(Error::Invalid("PGS repeated first object fragment"));
            }
            item.id = Some(prepared.id);
            item.meta = Some(ObjectMeta {
                version: prepared.version,
                width: first.width,
                height: first.height,
                first: stamp,
                last: stamp,
            });
            item.assembly = Some(ObjectAssembly::new(
                &self.budget,
                AssemblyLimits {
                    max_axis: self.limits.max_axis,
                    max_pixels: self.limits.max_object_pixels,
                    max_encoded: self.limits.max_encoded,
                    max_fragments: self.limits.max_fragments,
                    max_work: self.limits.max_work,
                },
            ));
        } else if item.assembly.is_none() {
            return Err(Error::Invalid("PGS object continuation without assembly"));
        }
        if let Some(meta) = item.meta.as_mut() {
            meta.last = stamp;
        }
        item.assembly
            .as_mut()
            .ok_or(Error::Invalid("PGS object assembly absent"))?
            .push(prepared.payload, cancel)
            .map_err(|e| e.error)?;
        self.slot = slot;
        self.done(Phase::ObjectStep);
        Ok(())
    }

    fn inner(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<DisplayStatus> {
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
            match self.phase {
                Phase::ReserveObjects => {
                    if !self.pay(work, cancel, 1)? {
                        return Ok(DisplayStatus::Yield);
                    }
                    self.phase = Phase::AllocateObjects;
                }
                Phase::AllocateObjects => {
                    self.objects
                        .try_reserve_exact(self.limits.max_objects.min(1))?;
                    self.done(Phase::InitObjects);
                }
                Phase::InitObjects => {
                    if self.objects.len() == self.limits.max_objects.min(1) {
                        self.done(Phase::ReservePalettes);
                        continue;
                    }
                    if !self.pay(work, cancel, 1)? {
                        return Ok(DisplayStatus::Yield);
                    }
                    self.objects
                        .try_push(ObjectSlot::default())
                        .map_err(|e| e.error)?;
                    self.credit = 0;
                }
                Phase::ReservePalettes => {
                    if !self.pay(work, cancel, 1)? {
                        return Ok(DisplayStatus::Yield);
                    }
                    self.phase = Phase::AllocatePalettes;
                }
                Phase::AllocatePalettes => {
                    self.palettes
                        .try_reserve_exact(self.limits.max_palettes.min(1))?;
                    self.done(Phase::InitPalettes);
                }
                Phase::InitPalettes => {
                    if self.palettes.len() == self.limits.max_palettes.min(1) {
                        self.done(Phase::ReserveWindows);
                        continue;
                    }
                    if !self.pay(work, cancel, 1)? {
                        return Ok(DisplayStatus::Yield);
                    }
                    self.palettes
                        .try_push(PaletteSlot::default())
                        .map_err(|e| e.error)?;
                    self.credit = 0;
                }
                Phase::ReserveWindows => {
                    if !self.pay(work, cancel, 1)? {
                        return Ok(DisplayStatus::Yield);
                    }
                    self.phase = Phase::AllocateWindows;
                }
                Phase::AllocateWindows => {
                    self.windows.try_reserve_exact(1)?;
                    self.done(Phase::InitWindows);
                }
                Phase::InitWindows => {
                    if self.windows.len() == 1 {
                        self.done(Phase::Next);
                        continue;
                    }
                    if !self.pay(work, cancel, 1)? {
                        return Ok(DisplayStatus::Yield);
                    }
                    self.windows.try_push(None).map_err(|e| e.error)?;
                    self.credit = 0;
                }
                Phase::Next => {
                    let Some(packet) = self.packet.as_ref() else {
                        if self.ended {
                            if self.frame.is_some() {
                                return Err(Error::Truncated);
                            }
                            self.index = 0;
                            self.done(Phase::EofCheck);
                            continue;
                        }
                        return Ok(DisplayStatus::NeedPacket);
                    };
                    if packet.at == packet.bytes.len() {
                        self.packet = None;
                        continue;
                    }
                    if !self.pay(work, cancel, 1)? {
                        return Ok(DisplayStatus::Yield);
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
                }
                Phase::Trace => {
                    let cost = match &self.frame {
                        Some(frame) if frame.pending_segment.is_some() => 2,
                        Some(frame) if frame.segments.len() == frame.segments.capacity() => {
                            frame.segments.len().checked_add(1).ok_or(Error::Overflow)?
                        }
                        _ => 1,
                    };
                    if !self.pay(work, cancel, cost)? {
                        return Ok(DisplayStatus::Yield);
                    }
                    self.trace()?;
                    self.done(Phase::Dispatch);
                }
                Phase::Dispatch => {
                    self.done(match self.info()?.kind {
                        0x14 => Phase::PaletteStart,
                        0x15 => Phase::ObjectStart,
                        0x17 => Phase::WindowStart,
                        0x80 => Phase::EndCheck,
                        _ => {
                            self.advance()?;
                            continue;
                        }
                    });
                    self.index = 0;
                }
                Phase::PcsHeader => {
                    if !self.pay(work, cancel, 1)? {
                        return Ok(DisplayStatus::Yield);
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
                }
                Phase::ClearObjects => {
                    if self.index == self.objects.len() {
                        self.index = 0;
                        self.done(Phase::ClearPalettes);
                        continue;
                    }
                    if !self.pay(work, cancel, 1)? {
                        return Ok(DisplayStatus::Yield);
                    }
                    self.objects.as_mut_slice()[self.index] = ObjectSlot::default();
                    self.index += 1;
                    self.credit = 0;
                }
                Phase::ClearPalettes => {
                    if self.index == self.palettes.len() {
                        self.index = 0;
                        self.done(Phase::ClearWindows);
                        continue;
                    }
                    if !self.pay(work, cancel, 1)? {
                        return Ok(DisplayStatus::Yield);
                    }
                    self.palettes.as_mut_slice()[self.index].id = None;
                    self.index += 1;
                    self.credit = 0;
                }
                Phase::ClearWindows => {
                    if self.index == self.windows.len() {
                        self.index = 0;
                        self.done(Phase::PcsReserve);
                        continue;
                    }
                    if !self.pay(work, cancel, 1)? {
                        return Ok(DisplayStatus::Yield);
                    }
                    self.windows.as_mut_slice()[self.index] = None;
                    self.index += 1;
                    self.credit = 0;
                }
                Phase::PcsReserve => {
                    if !self.pay(work, cancel, 1)? {
                        return Ok(DisplayStatus::Yield);
                    }
                    self.phase = Phase::PcsAllocate;
                }
                Phase::PcsAllocate => {
                    let frame = self
                        .frame
                        .as_mut()
                        .ok_or(Error::Invalid("PGS frame absent"))?;
                    frame
                        .references
                        .try_reserve_exact(frame.declared_references)?;
                    self.trace()?;
                    self.done(Phase::PcsReferences);
                }
                Phase::PcsReferences => {
                    let frame = self
                        .frame
                        .as_ref()
                        .ok_or(Error::Invalid("PGS frame absent"))?;
                    if frame.references.len() == frame.declared_references {
                        if self.cursor != self.payload()?.len() {
                            return Err(Error::Invalid("PGS trailing presentation fields"));
                        }
                        self.advance()?;
                        continue;
                    }
                    if !self.pay(work, cancel, 1)? {
                        return Ok(DisplayStatus::Yield);
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
                }
                Phase::PaletteStart => {
                    if !self.pay(work, cancel, self.palettes.len().max(1))? {
                        return Ok(DisplayStatus::Yield);
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
                    if let Some(slot) = found
                        .or_else(|| self.palettes.as_slice().iter().position(|s| s.id.is_none()))
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
                }
                Phase::PaletteGrow => {
                    let length = self.palettes.len();
                    let grows = length == self.palettes.capacity();
                    let cost = if grows {
                        length.checked_add(1).ok_or(Error::Overflow)?
                    } else {
                        1
                    };
                    if !self.pay(work, cancel, cost)? {
                        return Ok(DisplayStatus::Yield);
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
                }
                Phase::PaletteCreate => {
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
                }
                Phase::PaletteStep => {
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
                        return Ok(DisplayStatus::Yield);
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
                }
                Phase::WindowStart => {
                    if !self.pay(work, cancel, 1)? {
                        return Ok(DisplayStatus::Yield);
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
                }
                Phase::WindowClear => {
                    if self.index == self.windows.len() {
                        self.index = 0;
                        self.done(Phase::WindowEntries);
                        continue;
                    }
                    if !self.pay(work, cancel, 1)? {
                        return Ok(DisplayStatus::Yield);
                    }
                    self.windows.as_mut_slice()[self.index] = None;
                    self.index += 1;
                    self.credit = 0;
                }
                Phase::WindowEntries => {
                    if self.index == self.window_count {
                        self.advance()?;
                        continue;
                    }
                    if !self.pay(work, cancel, 1)? {
                        return Ok(DisplayStatus::Yield);
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
                        continue;
                    }
                    self.windows.as_mut_slice()[usize::from(w.id)] = Some(w);
                    self.index += 1;
                    self.credit = 0;
                }
                Phase::WindowGrowReserve => {
                    if !self.pay(work, cancel, self.windows.len().max(1))? {
                        return Ok(DisplayStatus::Yield);
                    }
                    self.phase = Phase::WindowGrowAllocate;
                }
                Phase::WindowGrowAllocate => {
                    self.windows
                        .try_reserve_exact(self.slot - self.windows.len())?;
                    self.done(Phase::WindowGrowInitialise);
                }
                Phase::WindowGrowInitialise => {
                    if self.windows.len() == self.slot {
                        self.done(Phase::WindowEntries);
                        continue;
                    }
                    if !self.pay(work, cancel, 1)? {
                        return Ok(DisplayStatus::Yield);
                    }
                    self.windows.try_push(None).map_err(|e| e.error)?;
                    self.credit = 0;
                }
                Phase::ObjectStart => {
                    if !self.pay(work, cancel, self.objects.len().max(1))? {
                        return Ok(DisplayStatus::Yield);
                    }
                    let payload = self.owned_payload()?;
                    let fragment = ObjectFragment::parse_payload(&payload)?;
                    let found = self
                        .objects
                        .as_slice()
                        .iter()
                        .position(|s| s.id == Some(fragment.id));
                    let available = found
                        .or_else(|| self.objects.as_slice().iter().position(|s| s.id.is_none()));
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
                }
                Phase::ObjectAllocate => {
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
                }
                Phase::ObjectStep => {
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
                        AssemblyStatus::Yield => return Ok(DisplayStatus::Yield),
                        AssemblyStatus::NeedFragment => self.advance()?,
                        AssemblyStatus::Ready => self.done(Phase::ObjectFinish),
                    }
                }
                Phase::ObjectFinish => {
                    if !self.pay(work, cancel, 1)? {
                        return Ok(DisplayStatus::Yield);
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
                }
                Phase::RasterStep => {
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
                        RasterStatus::Yield => return Ok(DisplayStatus::Yield),
                        RasterStatus::Ready => self.done(Phase::RasterFinish),
                    }
                }
                Phase::RasterFinish => {
                    if !self.pay(work, cancel, 1)? {
                        return Ok(DisplayStatus::Yield);
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
                }
                Phase::EndCheck => {
                    if !self.payload()?.is_empty() {
                        return Err(Error::Invalid("PGS END payload"));
                    }
                    if !self.pay(work, cancel, 1)? {
                        return Ok(DisplayStatus::Yield);
                    }
                    // END closes this presentation, not the entire object
                    // preload/cache stream. Referenced objects are checked in
                    // EndLookup; unused partial objects may continue later.
                    self.index = 0;
                    self.done(Phase::EndReserve);
                }
                Phase::EndReserve => {
                    if !self.pay(work, cancel, 1)? {
                        return Ok(DisplayStatus::Yield);
                    }
                    self.phase = Phase::EndAllocate;
                }
                Phase::EndAllocate => {
                    let f = self
                        .frame
                        .as_mut()
                        .ok_or(Error::Invalid("PGS frame absent"))?;
                    f.objects.try_reserve_exact(f.references.len())?;
                    self.index = 0;
                    self.done(Phase::EndLookup);
                }
                Phase::EndLookup => {
                    let frame = self
                        .frame
                        .as_ref()
                        .ok_or(Error::Invalid("PGS frame absent"))?;
                    if self.index == frame.references.len() {
                        self.done(Phase::EndPublish);
                        continue;
                    }
                    if !self.pay(work, cancel, self.objects.len() + self.palettes.len() + 1)? {
                        return Ok(DisplayStatus::Yield);
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
                }
                Phase::EndRender => {
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
                        return Ok(DisplayStatus::Yield);
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
                }
                Phase::EndPublish => {
                    if !self.pay(work, cancel, 1)? {
                        return Ok(DisplayStatus::Yield);
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
                    let (native_time, native_clock_source) =
                        if stamp_known(frame.presentation.stamp) {
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
                }
                Phase::EofCheck => {
                    if self.index == self.objects.len() {
                        self.done(Phase::Finished);
                        continue;
                    }
                    if !self.pay(work, cancel, 1)? {
                        return Ok(DisplayStatus::Yield);
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
                }
                Phase::Finished => return Ok(DisplayStatus::Ended),
            }
        }
    }
}

fn stamp_known(stamp: SegmentStamp) -> bool {
    stamp.sup_clock.is_some() || stamp.packet_clock.pts.is_some()
}
fn stamp_time(stamp: SegmentStamp) -> Result<Option<i64>> {
    if let Some(raw) = stamp.sup_clock {
        PacketClock {
            pts: Some(i64::from(raw.pts)),
            dts: Some(i64::from(raw.dts)),
            timebase: triergon_core::TimeBase::new(1, 90000)?,
        }
        .native_microseconds()
    } else if stamp.packet_clock.pts == Some(i64::MIN) {
        Err(Error::Unsupported(
            "PGS PTS equals native no-timestamp sentinel",
        ))
    } else {
        stamp.packet_clock.native_microseconds()
    }
}

#[cfg(test)]
mod storage_growth_tests {
    use super::*;
    use triergon_core::TimeBase;

    fn segment(packet: &mut Vec<u8>, kind: u8, body: &[u8]) {
        packet.push(kind);
        packet.extend_from_slice(&u16::try_from(body.len()).unwrap().to_be_bytes());
        packet.extend_from_slice(body);
    }

    fn presentation(references: bool, window: u8) -> Vec<u8> {
        let mut body = vec![0, 1, 0, 1, 0x10, 0, 1, 0x80, 0, 3, u8::from(references)];
        if references {
            body.extend_from_slice(&[0, 7, window, 0, 0, 0, 0, 0]);
        }
        let mut packet = Vec::new();
        segment(&mut packet, 0x16, &body);
        packet
    }

    fn object(packet: &mut Vec<u8>, id: u16) {
        let mut body = id.to_be_bytes().to_vec();
        body.extend_from_slice(&[1, 0xc0, 0, 0, 7, 0, 1, 0, 1, 7, 0, 0]);
        segment(packet, 0x15, &body);
    }

    fn decode(
        budget: &Budget,
        packet: &[u8],
        quantum: u64,
        limits: DisplayLimits,
    ) -> (DisplayDecoder, DisplaySet) {
        let cancel = AtomicBool::new(false);
        let mut decoder = DisplayDecoder::new(budget, limits, DisplayConfig::default()).unwrap();
        let mut owned = Some(Bytes::copy_from_slice(budget, packet).unwrap());
        decoder
            .push(
                owned.take().unwrap(),
                PacketClock {
                    pts: Some(900),
                    dts: None,
                    timebase: TimeBase::new(1, 90000).unwrap(),
                },
                Envelope::Packet,
                &cancel,
            )
            .unwrap();
        assert!(owned.is_none());
        for _ in 0..100_000 {
            match decoder
                .poll(&mut WorkBudget::new(quantum), &cancel)
                .unwrap()
            {
                DisplayStatus::Yield => (),
                DisplayStatus::OutputReady => {
                    let output = decoder.take_output(&cancel).unwrap();
                    return (decoder, output);
                }
                status => panic!("unexpected display status {status:?}"),
            }
        }
        panic!("bounded display storage growth did not finish");
    }

    #[test]
    fn empty_setup_initializes_one_slot_and_one_window_without_losing_limits() {
        let budget = Budget::new(1 << 20).unwrap();
        let baseline = budget.used();
        let mut decoder =
            DisplayDecoder::new(&budget, DisplayLimits::default(), DisplayConfig::default())
                .unwrap();
        assert_eq!(
            decoder.poll(&mut WorkBudget::new(0), &AtomicBool::new(false)),
            Ok(DisplayStatus::Yield)
        );
        assert_eq!(budget.used(), baseline);
        assert_eq!(
            decoder.poll(&mut WorkBudget::new(4096), &AtomicBool::new(false)),
            Ok(DisplayStatus::NeedPacket)
        );
        assert_eq!(decoder.objects.len(), 1);
        assert_eq!(decoder.windows.len(), 1);
        assert_eq!(decoder.limits.max_objects, 64);
        drop(decoder);
        assert_eq!(budget.used(), baseline);
    }

    #[test]
    fn high_window_and_trace_growth_preserve_all_facts_and_retained_owners() {
        for quantum in [1, 2, 7, 4096] {
            let budget = Budget::new(1 << 20).unwrap();
            let baseline = budget.used();
            let mut packet = presentation(true, 255);
            for index in 0..20u8 {
                segment(&mut packet, 0x42, &[0xab, index]);
            }
            segment(&mut packet, 0x17, &[1, 255, 0, 0, 0, 0, 0, 1, 0, 1]);
            segment(&mut packet, 0x14, &[3, 0, 7, 16, 128, 128, 255]);
            object(&mut packet, 7);
            segment(&mut packet, 0x80, &[]);
            let (decoder, output) = decode(&budget, &packet, quantum, DisplayLimits::default());
            assert_eq!(decoder.windows.len(), 256);
            assert_eq!(decoder.objects.len(), 1);
            assert_eq!(output.segments().len(), 25);
            assert_eq!(
                output.segments().iter().filter(|s| s.kind == 0x42).count(),
                20
            );
            assert_eq!(output.objects()[0].window.unwrap().id, 255);
            assert_eq!(output.objects()[0].pixels().as_ref(), &[7]);
            assert_eq!(output.objects()[0].palette().source(7).unwrap().y, 16);
            drop(decoder);
            assert!(budget.used() > baseline);
            assert_eq!(
                output.objects()[0]
                    .palette()
                    .source(7)
                    .unwrap()
                    .stamp
                    .packet_clock
                    .pts,
                Some(900)
            );
            drop(output);
            assert_eq!(budget.used(), baseline);
        }
    }

    #[test]
    fn object_storage_grows_through_the_original_configured_id_bound() {
        let budget = Budget::new(1 << 22).unwrap();
        let mut packet = presentation(false, 0);
        for id in 0..40 {
            object(&mut packet, id);
        }
        segment(&mut packet, 0x80, &[]);
        let limits = DisplayLimits {
            max_objects: 40,
            ..DisplayLimits::default()
        };
        let (decoder, output) = decode(&budget, &packet, 1, limits);
        assert_eq!(decoder.objects.len(), 40);
        assert!(
            decoder
                .objects
                .as_slice()
                .iter()
                .all(|slot| slot.id.is_some())
        );
        assert_eq!(output.segments().len(), 42);
        assert!(output.is_clear());
    }
}

#[cfg(test)]
mod generation_poison_controls {
    use super::*;

    #[test]
    fn poisoned_generation_overflow_is_checked_without_recreating_context() {
        let budget = Budget::new(1024 * 1024).unwrap();
        let mut decoder =
            DisplayDecoder::new(&budget, DisplayLimits::default(), DisplayConfig::default())
                .unwrap();
        decoder.generation = u64::MAX;
        decoder.next_packet = 17;
        decoder.poison(Error::Cancelled);
        let cancel = AtomicBool::new(false);
        for _ in 0..3 {
            assert_eq!(decoder.reset(&cancel), Err(Error::Overflow));
            assert_eq!(decoder.generation(), u64::MAX);
            assert_eq!(decoder.next_packet, 17);
            assert_eq!(decoder.failed, Some(Error::Cancelled));
            assert_eq!(budget.used(), Budget::control_storage_bytes());
        }
    }
}

#[cfg(test)]
mod independent_source6_orphan_regression {
    // Standalone authored regression for inclusion as a child test module of
    // pgs/display.rs in a NEW author workspace. NOT compiled/executed by review.
    use super::*;
    use triergon_core::TimeBase;
    fn segment(bytes: &mut Vec<u8>, kind: u8, body: &[u8]) {
        bytes.push(kind);
        bytes.extend_from_slice(&(body.len() as u16).to_be_bytes());
        bytes.extend_from_slice(body);
    }
    #[test]
    fn orphan_continuation_keeps_source_refusal_before_unneeded_cache_growth() {
        let budget = Budget::new(1 << 20).unwrap();
        let mut packet = Vec::new();
        segment(&mut packet, 0x16, &[0, 1, 0, 1, 0x10, 0, 1, 0x80, 0, 3, 0]);
        // A real complete 1x1 cached object consumes the initial lazy slot.
        segment(
            &mut packet,
            0x15,
            &[0, 1, 1, 0xc0, 0, 0, 7, 0, 1, 0, 1, 7, 0, 0],
        );
        // A valid ODS header, but no first fragment / assembly for object2.
        segment(&mut packet, 0x15, &[0, 2, 1, 0, 7]);
        let cancel = AtomicBool::new(false);
        let mut decoder =
            DisplayDecoder::new(&budget, DisplayLimits::default(), DisplayConfig::default())
                .unwrap();
        decoder
            .push(
                Bytes::copy_from_slice(&budget, &packet).unwrap(),
                PacketClock {
                    pts: None,
                    dts: Some(-17),
                    timebase: TimeBase::new(1, 90000).unwrap(),
                },
                Envelope::Packet,
                &cancel,
            )
            .unwrap();
        let mut reached = false;
        for _ in 0..100000 {
            if matches!(decoder.phase, Phase::Dispatch | Phase::ObjectStart)
                && decoder.info().unwrap().kind == 0x15
                && decoder.payload().unwrap().starts_with(&[0, 2])
            {
                reached = true;
                break;
            }
            assert_eq!(
                decoder.poll(&mut WorkBudget::new(1), &cancel).unwrap(),
                DisplayStatus::Yield
            );
        }
        assert!(reached);
        assert_eq!(decoder.cached_objects(), 1);
        let held = budget.reserve(budget.available()).unwrap();
        assert_eq!(
            decoder.poll(&mut WorkBudget::new(4096), &cancel),
            Err(Error::Invalid("PGS object continuation without assembly"))
        );
        drop(held);
    }

    #[cfg(test)]
    mod storage_object_retry_tests {
        use super::*;
        use triergon_core::TimeBase;

        fn segment(packet: &mut Vec<u8>, kind: u8, body: &[u8]) {
            packet.push(kind);
            packet.extend_from_slice(&u16::try_from(body.len()).unwrap().to_be_bytes());
            packet.extend_from_slice(body);
        }

        fn packet(first: bool, orphan: bool) -> Vec<u8> {
            let mut packet = Vec::new();
            segment(&mut packet, 0x16, &[0, 1, 0, 1, 0x10, 0, 1, 0x80, 0, 3, 0]);
            if first {
                segment(
                    &mut packet,
                    0x15,
                    &[0, 1, 1, 0xc0, 0, 0, 7, 0, 1, 0, 1, 7, 0, 0],
                );
            }
            if orphan {
                segment(&mut packet, 0x15, &[0, 2, 1, 0, 7]);
            } else {
                segment(
                    &mut packet,
                    0x15,
                    &[0, 2, 1, 0xc0, 0, 0, 7, 0, 1, 0, 1, 8, 0, 0],
                );
            }
            segment(&mut packet, 0x80, &[]);
            packet
        }

        fn at_second_object(budget: &Budget, packet: &[u8], max_objects: usize) -> DisplayDecoder {
            let cancel = AtomicBool::new(false);
            let mut decoder = DisplayDecoder::new(
                budget,
                DisplayLimits {
                    max_objects,
                    ..DisplayLimits::default()
                },
                DisplayConfig::default(),
            )
            .unwrap();
            decoder
                .push(
                    Bytes::copy_from_slice(budget, packet).unwrap(),
                    PacketClock {
                        pts: None,
                        dts: Some(-17),
                        timebase: TimeBase::new(1, 90000).unwrap(),
                    },
                    Envelope::Packet,
                    &cancel,
                )
                .unwrap();
            for _ in 0..100_000 {
                if matches!(decoder.phase, Phase::Dispatch | Phase::ObjectStart)
                    && decoder.info().unwrap().kind == 0x15
                    && decoder.payload().unwrap().starts_with(&[0, 2])
                {
                    return decoder;
                }
                assert_eq!(
                    decoder.poll(&mut WorkBudget::new(1), &cancel).unwrap(),
                    DisplayStatus::Yield
                );
            }
            panic!("second object dispatch was not reached");
        }

        #[test]
        fn cached_paid_growth_retries_keep_the_owner_and_all_source_facts() {
            for quantum in [1, 2, 3, 7, 4096] {
                let budget = Budget::new(1 << 20).unwrap();
                let cancel = AtomicBool::new(false);
                let mut decoder = at_second_object(&budget, &packet(true, false), 64);
                let held = budget.reserve(budget.available()).unwrap();
                assert!(matches!(
                    decoder.poll(&mut WorkBudget::new(4096), &cancel),
                    Err(Error::Backpressure { .. })
                ));
                assert!(matches!(decoder.phase, Phase::ObjectAllocate));
                assert_eq!(decoder.objects.len(), 1);
                assert_eq!(decoder.pending_object.as_ref().unwrap().id, 2);
                assert_eq!(decoder.pending_object.as_ref().unwrap().version, 1);
                assert_eq!(
                    decoder.pending_object.as_ref().unwrap().payload.as_ref(),
                    &[0, 2, 1, 0xc0, 0, 0, 7, 0, 1, 0, 1, 8, 0, 0]
                );
                let paid = decoder.work_spent();
                let used = budget.used();
                for _ in 0..32 {
                    assert_eq!(
                        decoder.poll(&mut WorkBudget::new(0), &cancel),
                        Ok(DisplayStatus::Yield)
                    );
                    let mut work = WorkBudget::new(quantum);
                    assert!(matches!(
                        decoder.poll(&mut work, &cancel),
                        Err(Error::Backpressure { .. })
                    ));
                    assert_eq!(work.remaining(), quantum);
                    assert_eq!(decoder.work_spent(), paid);
                    assert_eq!(budget.used(), used);
                    assert!(matches!(decoder.phase, Phase::ObjectAllocate));
                    assert!(decoder.pending_object.is_some());
                }
                drop(held);
                let output = loop {
                    match decoder
                        .poll(&mut WorkBudget::new(quantum), &cancel)
                        .unwrap()
                    {
                        DisplayStatus::Yield => (),
                        DisplayStatus::OutputReady => break decoder.take_output(&cancel).unwrap(),
                        status => panic!("unexpected retry status {status:?}"),
                    }
                };
                assert!(decoder.pending_object.is_none());
                assert_eq!(decoder.cached_objects(), 2);
                assert_eq!(output.segments().len(), 4);
                assert!(
                    output
                        .segments()
                        .iter()
                        .all(|fact| fact.stamp.packet_clock.pts.is_none()
                            && fact.stamp.packet_clock.dts == Some(-17))
                );
                assert_eq!(
                    decoder.objects.as_slice()[0]
                        .image
                        .as_ref()
                        .unwrap()
                        .pixels
                        .as_ref(),
                    &[7]
                );
                assert_eq!(
                    decoder.objects.as_slice()[1]
                        .image
                        .as_ref()
                        .unwrap()
                        .pixels
                        .as_ref(),
                    &[8]
                );
            }
        }

        #[test]
        fn full_object_limit_keeps_precedence_over_orphan_and_pool_pressure() {
            for maximum in [0, 1] {
                let budget = Budget::new(1 << 20).unwrap();
                let cancel = AtomicBool::new(false);
                let mut decoder = at_second_object(&budget, &packet(maximum != 0, true), maximum);
                let held = budget.reserve(budget.available()).unwrap();
                let error = Error::Limit {
                    requested: maximum + 1,
                    maximum,
                };
                assert_eq!(
                    decoder.poll(&mut WorkBudget::new(4096), &cancel),
                    Err(error)
                );
                assert_eq!(decoder.poll(&mut WorkBudget::new(0), &cancel), Err(error));
                drop(held);
            }
        }

        #[test]
        fn cancel_pending_growth_releases_only_private_owners_and_stays_sticky() {
            let budget = Budget::new(1 << 20).unwrap();
            let baseline = budget.used();
            let packet = packet(true, false);
            let cancel = AtomicBool::new(false);
            let mut decoder = at_second_object(&budget, &packet, 64);
            let retained = decoder.packet.as_ref().unwrap().bytes.clone();
            let retained_bytes = retained.retained_storage_bytes();
            let held = budget.reserve(budget.available()).unwrap();
            assert!(matches!(
                decoder.poll(&mut WorkBudget::new(4096), &cancel),
                Err(Error::Backpressure { .. })
            ));
            assert!(decoder.pending_object.is_some());
            cancel.store(true, Ordering::Relaxed);
            assert_eq!(
                decoder.poll(&mut WorkBudget::new(0), &cancel),
                Err(Error::Cancelled)
            );
            assert!(decoder.pending_object.is_none());
            cancel.store(false, Ordering::Relaxed);
            assert_eq!(
                decoder.poll(&mut WorkBudget::new(4096), &cancel),
                Err(Error::Cancelled)
            );
            assert_eq!(retained.as_ref(), packet.as_slice());
            drop(held);
            drop(decoder);
            assert_eq!(budget.used(), baseline + retained_bytes);
            drop(retained);
            assert_eq!(budget.used(), baseline);
        }
    }
}

#[cfg(test)]
mod context_setup_controls;

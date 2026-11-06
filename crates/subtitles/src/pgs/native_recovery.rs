//! Shared bounded PGS receiver operation. Full recovery independently preserves
//! strict Source outputs/errors; native compatibility owns only native output.
//! Receiver recovery never publishes guessed source pixels, colours or clocks.
use super::display_model::{PaletteSnapshot, StoredDisplaySet};
use super::display_work::DisplayWork;
use super::indexed_argb::{ArgbStorage, ArgbWrite, ArgbWriter, IndexedArgb};
use super::recovery_role::{FullRecovery, RecoveryRole};
use super::{
    Composition, CompositionObject, DisplayConfig, DisplayLimits, NativeMatrix, PacketClock,
    RasterJob, RasterLimits, RasterStatus, Segment,
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, BudgetVec, Bytes, Error, Reservation, Result, WorkBudget};

pub const NATIVE_RECOVERY: &str = "native-pgs-bounded-packet-recovery-v1";

#[derive(Debug)]
pub struct RecoveredRect {
    /// Original syntactic PCS reference, independent of receiver placement.
    pub reference: CompositionObject,
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
    /// The receiver keeps the allocated row width even if RLE is rejected.
    pub stride: u16,
    pub forced: bool,
    pixels: Bytes,
    palette: ReceiverPalette,
}

/// Palette values are receiver facts even when an immutable Source allocation
/// happens to contain exactly the same256 words. Sharing grants no Source facts.
#[derive(Debug)]
enum ReceiverPalette {
    Pending,
    /// Literal comparison already finished. A pressure retry only reserves the
    /// fallback's real allocation; it never repeats paid comparison work.
    Fallback,
    Shared(PaletteSnapshot),
    Owned {
        words: Box<[u32; 256]>,
        // Fields drop in declaration order: storage before its live charge.
        _charge: Reservation,
    },
}

impl RecoveredRect {
    pub fn pixels(&self) -> &Bytes {
        &self.pixels
    }
    pub fn palette(&self) -> &[u32; 256] {
        match &self.palette {
            ReceiverPalette::Shared(source) => source
                .native_words()
                .try_into()
                .expect("PGS immutable palette has256 words"),
            ReceiverPalette::Owned { words, .. } => words,
            ReceiverPalette::Pending | ReceiverPalette::Fallback => {
                unreachable!("unpublished PGS receiver palette")
            }
        }
    }
}

#[derive(Debug)]
pub struct RecoveredEvent {
    pub pts_microseconds: Option<i64>,
    rectangles: BudgetVec<RecoveredRect>,
}
impl RecoveredEvent {
    pub fn rectangles(&self) -> &[RecoveredRect] {
        self.rectangles.as_slice()
    }
}

pub type RecoveredPacket = StoredRecoveredPacket<Bytes>;
/// Original packet/receiver outputs plus complete indexed strict Source sets.
pub type IndexedRecoveredPacket = StoredRecoveredPacket<IndexedArgb>;

#[derive(Debug)]
pub struct StoredRecoveredPacket<A: ArgbStorage> {
    pub(super) raw: Bytes,
    pub clock: PacketClock,
    pub(super) source_sets: BudgetVec<StoredDisplaySet<A>>,
    pub(super) source_error: Option<Error>,
    pub(super) events: BudgetVec<RecoveredEvent>,
    pub context_canvas: [u16; 2],
    pub receiver_observations: ReceiverObservations,
}
/// These facts describe only the selected receiver read view. API padding is
/// never included in raw_packet() or declared to be authored source data.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReceiverObservations {
    pub truncated_segment_tail: bool,
    pub palette_cross_segment_bytes: usize,
    pub palette_zero_padding_bytes: usize,
}
impl<A: ArgbStorage> StoredRecoveredPacket<A> {
    pub fn raw_packet(&self) -> &Bytes {
        &self.raw
    }
    pub fn source_sets(&self) -> &[StoredDisplaySet<A>] {
        self.source_sets.as_slice()
    }
    pub fn source_error(&self) -> Option<Error> {
        self.source_error
    }
    pub fn receiver_output(&self) -> Option<&RecoveredEvent> {
        self.events.as_slice().first()
    }
    pub fn receiver_sets(&self) -> &[RecoveredEvent] {
        self.events.as_slice()
    }
    pub fn omitted_receiver_events(&self) -> usize {
        self.events.len().saturating_sub(1)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryStatus {
    NeedPacket,
    Yield,
    OutputReady,
    Ended,
}

#[derive(Debug)]
struct Object {
    id: u16,
    width: u16,
    height: u16,
    declared: usize,
    encoded: BudgetVec<u8>,
    fragments: usize,
}
#[derive(Debug)]
struct PaletteSlot {
    id: Option<u8>,
    words: [u32; 256],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Source,
    Segment,
    Palette,
    CopyReserve,
    Copy,
    End,
    Rectangle,
    ShareLookup,
    ShareCompare,
    PrepareRaster,
    Raster,
    RasterFinish,
    StoreRect,
    StoreEvent,
    Finish,
    Ready,
    Eof,
    Ended,
}

/// An explicitly selected receiver, never a replacement for DisplayDecoder.
/// One physical packet is active at a time. Retained packets/strict sets/receiver
/// outputs keep their actual capacities charged to the supplied shared budget.
pub type NativeRecoveryDecoder = Decoder<FullRecovery<Bytes>>;
/// Explicit indexed storage selection. Receiver recovery is unchanged and
/// never supplies missing strict Source pixels, palette facts or clocks.
pub type IndexedNativeRecoveryDecoder = Decoder<FullRecovery<IndexedArgb>>;

#[derive(Debug)]
pub struct Decoder<S: RecoveryRole> {
    budget: Budget,
    limits: DisplayLimits,
    config: DisplayConfig,
    role: S,
    packet: Option<Bytes>,
    clock: Option<PacketClock>,
    output: Option<S::Packet>,
    objects: BudgetVec<Object>,
    palettes: BudgetVec<PaletteSlot>,
    references: BudgetVec<CompositionObject>,
    canvas: [u16; 2],
    presentation_clock: Option<PacketClock>,
    before_any_pcs: bool,
    palette_id: u8,
    palette_index: usize,
    palette_at: usize,
    palette_end: usize,
    at: usize,
    segments: usize,
    epoch: u64,
    slot: usize,
    copy_at: usize,
    copy_end: usize,
    rectangles: Option<BudgetVec<RecoveredRect>>,
    rect_next: Option<RecoveredRect>,
    rect_index: usize,
    share_candidate: Option<(Bytes, Bytes)>,
    share_at: usize,
    raster: Option<RasterJob>,
    raster_width: u16,
    phase: Phase,
    credit: usize,
    spent: usize,
    failed: Option<Error>,
    final_input: bool,
    eof: Option<S::Eof>,
    display_pixels: usize,
    observations: ReceiverObservations,
}

impl<S: RecoveryRole> Decoder<S> {
    pub fn new(budget: &Budget, limits: DisplayLimits, config: DisplayConfig) -> Result<Self> {
        Ok(Self {
            budget: budget.clone(),
            limits,
            config,
            role: S::new(budget, limits, config)?,
            packet: None,
            clock: None,
            output: None,
            objects: BudgetVec::new(budget),
            palettes: BudgetVec::new(budget),
            references: BudgetVec::new(budget),
            canvas: [0, 0],
            presentation_clock: None,
            before_any_pcs: true,
            palette_id: 0,
            palette_index: 0,
            palette_at: 0,
            palette_end: 0,
            at: 0,
            segments: 0,
            epoch: 0,
            slot: 0,
            copy_at: 0,
            copy_end: 0,
            rectangles: None,
            rect_next: None,
            rect_index: 0,
            share_candidate: None,
            share_at: 0,
            raster: None,
            raster_width: 0,
            phase: Phase::Idle,
            credit: 0,
            spent: 0,
            failed: None,
            final_input: false,
            eof: None,
            display_pixels: 0,
            observations: ReceiverObservations::default(),
        })
    }

    /// Rejection preserves the caller's exact owner, including busy/foreign cases.
    pub fn push(
        &mut self,
        raw: &mut Option<Bytes>,
        clock: PacketClock,
        cancel: &AtomicBool,
    ) -> Result<()> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(self.poison(Error::Cancelled));
        }
        if self.phase != Phase::Idle || self.final_input {
            return Err(Error::Invalid("PGS recovery packet busy or finalized"));
        }
        let packet = raw
            .as_ref()
            .ok_or(Error::Invalid("PGS recovery packet absent"))?;
        if !packet.belongs_to(&self.budget) {
            return Err(Error::ForeignPool);
        }
        if packet.is_empty() {
            return Err(Error::Invalid("PGS recovery empty physical packet"));
        }
        if packet.len() > self.limits.max_packet_bytes {
            return Err(Error::Limit {
                requested: packet.len(),
                maximum: self.limits.max_packet_bytes,
            });
        }
        self.packet = raw.take();
        self.clock = Some(clock);
        self.at = 0;
        self.segments = 0;
        self.role.begin_packet(&self.budget);
        self.observations = ReceiverObservations::default();
        self.done(self.role.first_phase());
        Ok(())
    }

    pub fn work_spent(&self) -> usize {
        self.spent
    }

    fn poison(&mut self, error: Error) -> Error {
        self.packet = None;
        self.role.poison(error);
        self.output = None;
        self.objects = BudgetVec::new(&self.budget);
        self.palettes = BudgetVec::new(&self.budget);
        self.references = BudgetVec::new(&self.budget);
        self.rectangles = None;
        self.rect_next = None;
        self.share_candidate = None;
        self.share_at = 0;
        self.raster = None;
        self.failed = Some(error);
        error
    }
    fn done(&mut self, phase: Phase) {
        self.phase = phase;
        self.credit = 0;
    }
    fn pay(&mut self, work: &mut WorkBudget, cancel: &AtomicBool, cost: usize) -> Result<bool> {
        DisplayWork {
            caller: work,
            spent: &mut self.spent,
            maximum: self.limits.max_work,
            cancel,
        }
        .pay(&mut self.credit, cost)
    }
    fn packet(&self) -> Result<&Bytes> {
        self.packet
            .as_ref()
            .ok_or(Error::Invalid("PGS recovery physical packet absent"))
    }

    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<RecoveryStatus> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(self.poison(Error::Cancelled));
        }
        if self.phase == Phase::Ready {
            return Ok(RecoveryStatus::OutputReady);
        }
        if self.phase == Phase::Idle {
            return Ok(RecoveryStatus::NeedPacket);
        }
        if self.phase == Phase::Ended {
            return Ok(RecoveryStatus::Ended);
        }
        if work.remaining() == 0 {
            return Ok(RecoveryStatus::Yield);
        }
        match self.inner(work, cancel) {
            Ok(status) => Ok(status),
            Err(error @ Error::Backpressure { .. }) => Err(error),
            Err(error) => Err(self.poison(error)),
        }
    }

    fn inner(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<RecoveryStatus> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if work.remaining() == 0 {
                return Ok(RecoveryStatus::Yield);
            }
            match self.phase {
                Phase::Source => {
                    let raw = self
                        .packet
                        .as_ref()
                        .ok_or(Error::Invalid("PGS recovery physical packet absent"))?;
                    let clock = self.clock.ok_or(Error::Invalid("PGS clock absent"))?;
                    if !self.role.poll_source(
                        raw,
                        clock,
                        &self.limits,
                        work,
                        &mut self.spent,
                        cancel,
                    )? {
                        return Ok(RecoveryStatus::Yield);
                    }
                    self.done(Phase::Segment);
                }
                Phase::Segment => {
                    let raw = self.packet()?;
                    if self.at == raw.len() {
                        self.done(Phase::Finish);
                        continue;
                    }
                    if raw.len() - self.at < 3 {
                        return Err(Error::Truncated);
                    }
                    let kind = raw[self.at];
                    let start = self.at + 3;
                    let end = start
                        .checked_add(usize::from(u16::from_be_bytes([
                            raw[self.at + 1],
                            raw[self.at + 2],
                        ])))
                        .ok_or(Error::Overflow)?;
                    if end > raw.len() {
                        if !self.pay(work, cancel, 3)? {
                            return Ok(RecoveryStatus::Yield);
                        }
                        self.observations.truncated_segment_tail = true;
                        self.at = self.packet()?.len();
                        self.done(Phase::Finish);
                        continue;
                    }
                    if self.segments == self.limits.max_segments {
                        return Err(Error::Limit {
                            requested: self.segments + 1,
                            maximum: self.limits.max_segments,
                        });
                    }
                    // Credit complete bounded parsing/lookups before doing them.
                    if !self.pay(
                        work,
                        cancel,
                        end - self.at + self.objects.len() + self.palettes.len() + 256,
                    )? {
                        return Ok(RecoveryStatus::Yield);
                    }
                    // The funded dispatch needs a borrow, not a temporary shared owner.
                    // Borrow the packet field directly so mutations of the other
                    // decoder fields stay disjoint; retained output owners are
                    // still created at their original publication sites.
                    let raw = self
                        .packet
                        .as_ref()
                        .ok_or(Error::Invalid("PGS recovery physical packet absent"))?;
                    let payload = &raw[start..end];
                    match kind {
                        0x16 => {
                            let composition = Composition::parse(Segment {
                                offset: self.at,
                                kind,
                                clock: None,
                                raw: &raw[self.at..end],
                                payload,
                            })?;
                            for axis in [composition.width, composition.height] {
                                if axis > self.limits.max_axis {
                                    return Err(Error::Limit {
                                        requested: usize::from(axis),
                                        maximum: usize::from(self.limits.max_axis),
                                    });
                                }
                            }
                            let area = usize::from(composition.width)
                                .checked_mul(usize::from(composition.height))
                                .ok_or(Error::Overflow)?;
                            if area > self.limits.max_canvas_pixels {
                                return Err(Error::Limit {
                                    requested: area,
                                    maximum: self.limits.max_canvas_pixels,
                                });
                            }
                            if usize::from(composition.object_count) > self.limits.max_references {
                                return Err(Error::Limit {
                                    requested: usize::from(composition.object_count),
                                    maximum: self.limits.max_references,
                                });
                            }
                            let needed = usize::from(composition.object_count).min(2);
                            self.references
                                .try_reserve_exact(needed.saturating_sub(self.references.len()))?;
                            if composition.state != 0 {
                                self.objects.clear();
                                for palette in self.palettes.as_mut_slice() {
                                    palette.id = None;
                                }
                                self.epoch = self.epoch.checked_add(1).ok_or(Error::Overflow)?;
                            }
                            self.references.clear();
                            for reference in composition.objects() {
                                let reference = reference?;
                                if self.references.len() < 2 {
                                    self.references.try_push(reference).map_err(|e| e.error)?;
                                }
                            }
                            self.canvas = [composition.width, composition.height];
                            self.palette_id = composition.palette_id;
                            self.presentation_clock = self.clock;
                            self.before_any_pcs = false;
                        }
                        0x14 if payload.len() >= 2 => {
                            let id = payload[0];
                            let index = self
                                .palettes
                                .as_slice()
                                .iter()
                                .position(|p| p.id == Some(id))
                                .or_else(|| {
                                    self.palettes.as_slice().iter().position(|p| p.id.is_none())
                                });
                            let index = if let Some(i) = index {
                                i
                            } else {
                                if self.palettes.len() == self.limits.max_palettes {
                                    return Err(Error::Limit {
                                        requested: self.palettes.len() + 1,
                                        maximum: self.limits.max_palettes,
                                    });
                                }
                                if self.palettes.is_empty() {
                                    self.palettes.try_reserve_exact(1)?;
                                }
                                self.palettes
                                    .try_push(PaletteSlot {
                                        id: None,
                                        words: [0; 256],
                                    })
                                    .map_err(|e| e.error)?;
                                self.palettes.len() - 1
                            };
                            self.palettes.as_mut_slice()[index].id = Some(id);
                            self.palette_index = index;
                            self.palette_at = start + 2;
                            self.palette_end = end;
                            self.at = end;
                            self.segments += 1;
                            self.done(Phase::Palette);
                            continue;
                        }
                        0x15 if payload.len() >= 4 => {
                            let id = u16::from_be_bytes([payload[0], payload[1]]);
                            let found = self.objects.as_slice().iter().position(|o| o.id == id);
                            let slot = if let Some(i) = found {
                                i
                            } else {
                                if self.objects.len() == self.limits.max_objects {
                                    return Err(Error::Limit {
                                        requested: self.objects.len() + 1,
                                        maximum: self.limits.max_objects,
                                    });
                                }
                                if self.objects.is_empty() {
                                    self.objects.try_reserve_exact(1)?;
                                }
                                self.objects
                                    .try_push(Object {
                                        id,
                                        width: 0,
                                        height: 0,
                                        declared: 0,
                                        encoded: BudgetVec::new(&self.budget),
                                        fragments: 0,
                                    })
                                    .map_err(|e| e.error)?;
                                self.objects.len() - 1
                            };
                            let first = payload[3] & 0x80 != 0;
                            let mut data_at = start + 4;
                            let mut accept = true;
                            if first {
                                if payload.len() < 11 {
                                    accept = false;
                                } else {
                                    let declared = usize::from(payload[4]) * 65536
                                        + usize::from(payload[5]) * 256
                                        + usize::from(payload[6]);
                                    let width = u16::from_be_bytes([payload[7], payload[8]]);
                                    let height = u16::from_be_bytes([payload[9], payload[10]]);
                                    data_at = start + 11;
                                    accept = declared >= 4
                                        && end > data_at
                                        && end - data_at <= declared - 4
                                        && width != 0
                                        && height != 0
                                        && width <= self.canvas[0]
                                        && height <= self.canvas[1];
                                    if accept {
                                        let count = usize::from(width)
                                            .checked_mul(usize::from(height))
                                            .ok_or(Error::Overflow)?;
                                        if count > self.limits.max_object_pixels {
                                            return Err(Error::Limit {
                                                requested: count,
                                                maximum: self.limits.max_object_pixels,
                                            });
                                        }
                                        if declared - 4 > self.limits.max_encoded {
                                            return Err(Error::Limit {
                                                requested: declared - 4,
                                                maximum: self.limits.max_encoded,
                                            });
                                        }
                                        let object = &mut self.objects.as_mut_slice()[slot];
                                        object.width = width;
                                        object.height = height;
                                        object.declared = declared - 4;
                                        object.encoded = BudgetVec::new(&self.budget);
                                        object.fragments = 0;
                                    }
                                }
                            }
                            let object = &self.objects.as_slice()[slot];
                            accept &= object.width != 0
                                && end - data_at
                                    <= object.declared.saturating_sub(object.encoded.len());
                            if accept {
                                if object.fragments == self.limits.max_fragments {
                                    return Err(Error::Limit {
                                        requested: object.fragments + 1,
                                        maximum: self.limits.max_fragments,
                                    });
                                }
                                self.objects.as_mut_slice()[slot].fragments += 1;
                                self.slot = slot;
                                self.copy_at = data_at;
                                self.copy_end = end;
                                self.at = end;
                                self.segments += 1;
                                self.done(Phase::CopyReserve);
                                continue;
                            }
                        }
                        0x80 => {
                            self.at = end;
                            self.segments += 1;
                            self.done(Phase::End);
                            continue;
                        }
                        _ => {}
                    }
                    self.at = end;
                    self.segments += 1;
                    self.done(Phase::Segment);
                }
                Phase::Palette => {
                    if self.palette_at >= self.palette_end {
                        self.done(Phase::Segment);
                        continue;
                    }
                    if !self.pay(work, cancel, 8)? {
                        return Ok(RecoveryStatus::Yield);
                    }
                    let raw = self.packet()?;
                    let mut tail: [u8; 5];
                    let mut crosses = 0;
                    let mut padding = 0;
                    // Segment dispatch already checked palette_end <= raw.len().
                    // A complete entry needs neither API padding nor a copy.
                    let entry: &[u8] = if self.palette_end - self.palette_at >= 5 {
                        &raw[self.palette_at..self.palette_at + 5]
                    } else {
                        tail = [0; 5];
                        for (i, b) in tail.iter_mut().enumerate() {
                            let offset = self.palette_at + i;
                            *b = raw.get(offset).copied().unwrap_or(0);
                            crosses += usize::from(offset >= self.palette_end);
                            padding += usize::from(offset >= raw.len());
                        }
                        &tail
                    };
                    // At most four receiver bytes cross a segment boundary; bytes
                    // outside the physical packet are the public API's zero padding,
                    // never source bytes or a source palette declaration.
                    let index = usize::from(entry[0]);
                    let word = NativeMatrix::for_canvas(self.canvas[1])
                        .argb(entry[1], entry[2], entry[3], entry[4]);
                    self.palettes.as_mut_slice()[self.palette_index].words[index] = word;
                    self.observations.palette_cross_segment_bytes += crosses;
                    self.observations.palette_zero_padding_bytes += padding;
                    self.palette_at += 5;
                    self.credit = 0;
                }
                Phase::CopyReserve => {
                    if !self.pay(work, cancel, 1)? {
                        return Ok(RecoveryStatus::Yield);
                    }
                    self.objects.as_mut_slice()[self.slot]
                        .encoded
                        .try_reserve_exact(self.copy_end - self.copy_at)?;
                    self.done(Phase::Copy);
                }
                Phase::Copy => {
                    if self.copy_at == self.copy_end {
                        self.done(Phase::Segment);
                        continue;
                    }
                    if !self.pay(work, cancel, 1)? {
                        return Ok(RecoveryStatus::Yield);
                    }
                    let byte = self.packet()?[self.copy_at];
                    self.objects.as_mut_slice()[self.slot]
                        .encoded
                        .try_push(byte)
                        .map_err(|e| e.error)?;
                    self.copy_at += 1;
                    self.credit = 0;
                }
                Phase::End => {
                    // The native API suppresses later displays in this packet,
                    // while every following segment still updates receiver state.
                    // Full recovery always constructs all of its typed events.
                    if self.role.display_already_returned() {
                        if !self.pay(work, cancel, 1)? {
                            return Ok(RecoveryStatus::Yield);
                        }
                        self.done(Phase::Segment);
                        continue;
                    }
                    if !self.pay(work, cancel, self.palettes.len() + 1)? {
                        return Ok(RecoveryStatus::Yield);
                    }
                    let palette = self
                        .palettes
                        .as_slice()
                        .iter()
                        .position(|p| p.id == Some(self.palette_id));
                    if !self.references.is_empty() && palette.is_none() {
                        self.done(Phase::Segment);
                        continue;
                    }
                    let event_count = self.role.event_count()?;
                    if event_count == self.limits.max_segments {
                        return Err(Error::Limit {
                            requested: event_count + 1,
                            maximum: self.limits.max_segments,
                        });
                    }
                    let mut rectangles = BudgetVec::new(&self.budget);
                    rectangles.try_reserve_exact(self.references.len())?;
                    self.rectangles = Some(rectangles);
                    self.rect_index = 0;
                    self.palette_index = palette.unwrap_or(0);
                    self.display_pixels = 0;
                    self.done(Phase::Rectangle);
                }
                Phase::Rectangle => {
                    if self.rect_index == self.references.len() {
                        self.done(Phase::StoreEvent);
                        continue;
                    }
                    if !self.pay(work, cancel, self.objects.len() + 1)? {
                        return Ok(RecoveryStatus::Yield);
                    }
                    let reference = self.references.as_slice()[self.rect_index];
                    let object = self
                        .objects
                        .as_slice()
                        .iter()
                        .position(|o| o.id == reference.id);
                    let (width, height, stride) = object.map_or((0, 0, 0), |i| {
                        let o = &self.objects.as_slice()[i];
                        if !o.encoded.is_empty() && o.width != 0 {
                            (o.width, o.height, o.width)
                        } else {
                            (0, 0, o.width)
                        }
                    });
                    self.raster_width = stride;
                    if width != 0 {
                        let slot =
                            object.ok_or(Error::Invalid("PGS recovery object disappeared"))?;
                        let area = usize::from(width)
                            .checked_mul(usize::from(height))
                            .ok_or(Error::Overflow)?;
                        let total = self
                            .display_pixels
                            .checked_add(area)
                            .ok_or(Error::Overflow)?;
                        if total > self.limits.max_display_pixels {
                            return Err(Error::Limit {
                                requested: total,
                                maximum: self.limits.max_display_pixels,
                            });
                        }
                        self.display_pixels = total;
                        self.slot = slot;
                        self.done(Phase::ShareLookup);
                    } else {
                        self.empty_rect(stride)?;
                        if object.is_none() {
                            let rect = self
                                .rect_next
                                .as_mut()
                                .ok_or(Error::Invalid("PGS missing-object rectangle absent"))?;
                            rect.x = 0;
                            rect.y = 0;
                        }
                        self.done(Phase::StoreRect);
                    }
                }
                Phase::ShareLookup => {
                    // The materialized default keeps its original raster path.
                    let Some(slots) = self.role.index_slots() else {
                        self.done(Phase::PrepareRaster);
                        continue;
                    };
                    if !self.pay(work, cancel, slots.checked_add(1).ok_or(Error::Overflow)?)? {
                        return Ok(RecoveryStatus::Yield);
                    }
                    let object = &self.objects.as_slice()[self.slot];
                    let evidence =
                        self.role
                            .validated_indices(object.id, object.width, object.height);
                    match evidence {
                        Some((encoded, pixels))
                            if encoded.len() == object.encoded.len()
                                && pixels.len()
                                    == usize::from(object.width) * usize::from(object.height) =>
                        {
                            self.share_candidate = Some((encoded, pixels));
                            self.share_at = 0;
                            self.done(Phase::ShareCompare);
                        }
                        _ => self.done(Phase::PrepareRaster),
                    }
                }
                Phase::ShareCompare => {
                    let length = self.objects.as_slice()[self.slot].encoded.len();
                    if self.share_at == length {
                        if !self.pay(work, cancel, 1)? {
                            return Ok(RecoveryStatus::Yield);
                        }
                        let (_, pixels) = self
                            .share_candidate
                            .take()
                            .ok_or(Error::Invalid("PGS validated index evidence absent"))?;
                        let object = &self.objects.as_slice()[self.slot];
                        self.rect_next = Some(self.make_rect(
                            object.width,
                            object.height,
                            object.width,
                            pixels,
                        )?);
                        self.done(Phase::StoreRect);
                        continue;
                    }
                    let count = (length - self.share_at)
                        .min(256)
                        .min(usize::try_from(work.remaining()).unwrap_or(usize::MAX))
                        .min(self.limits.max_work.saturating_sub(self.spent))
                        .max(1);
                    if !self.pay(work, cancel, count)? {
                        return Ok(RecoveryStatus::Yield);
                    }
                    let end = self.share_at.checked_add(count).ok_or(Error::Overflow)?;
                    let (encoded, _) = self
                        .share_candidate
                        .as_ref()
                        .ok_or(Error::Invalid("PGS validated index evidence absent"))?;
                    if encoded[self.share_at..end]
                        != self.objects.as_slice()[self.slot].encoded.as_slice()[self.share_at..end]
                    {
                        self.share_candidate = None;
                        self.done(Phase::PrepareRaster);
                    } else {
                        self.share_at = end;
                        self.credit = 0;
                    }
                }
                Phase::PrepareRaster => {
                    let len = self.objects.as_slice()[self.slot].encoded.len();
                    if !self.pay(work, cancel, len + 1)? {
                        return Ok(RecoveryStatus::Yield);
                    }
                    let object = &self.objects.as_slice()[self.slot];
                    // A charged copy is needed only once while the cached mutable
                    // assembly remains live. Source and receiver inputs are separate.
                    let encoded = Bytes::copy_from_slice(&self.budget, object.encoded.as_slice())?;
                    self.raster = Some(RasterJob::native_receiver(
                        encoded,
                        object.width,
                        object.height,
                        &self.budget,
                        RasterLimits {
                            max_axis: self.limits.max_axis,
                            max_pixels: self.limits.max_object_pixels,
                            max_encoded: self.limits.max_encoded,
                            max_work: self.limits.max_work,
                        },
                    )?);
                    self.done(Phase::Raster);
                }
                Phase::Raster => {
                    let raster = self
                        .raster
                        .as_mut()
                        .ok_or(Error::Invalid("PGS receiver raster absent"))?;
                    let result = DisplayWork {
                        caller: work,
                        spent: &mut self.spent,
                        maximum: self.limits.max_work,
                        cancel,
                    }
                    .child(|w| raster.poll(w, cancel));
                    match result {
                        Ok(RasterStatus::Yield) => return Ok(RecoveryStatus::Yield),
                        Ok(RasterStatus::Ready) => self.done(Phase::RasterFinish),
                        Err(Error::Invalid(_) | Error::Truncated) => {
                            self.raster = None;
                            self.empty_rect(self.raster_width)?;
                            self.done(Phase::StoreRect);
                        }
                        Err(error) => return Err(error),
                    }
                }
                Phase::RasterFinish => {
                    if !self.pay(work, cancel, 1)? {
                        return Ok(RecoveryStatus::Yield);
                    }
                    let object = self
                        .raster
                        .as_mut()
                        .ok_or(Error::Invalid("PGS recovery raster absent"))?
                        .try_finish(cancel)?;
                    self.rect_next = Some(self.make_rect(
                        object.width,
                        object.height,
                        object.width,
                        object.pixels().clone(),
                    )?);
                    self.raster = None;
                    self.done(Phase::StoreRect);
                }
                Phase::StoreRect => {
                    if !self.pay(work, cancel, 1)? {
                        return Ok(RecoveryStatus::Yield);
                    }
                    self.finish_rect_palette()?;
                    let rect = self
                        .rect_next
                        .take()
                        .ok_or(Error::Invalid("PGS recovery rectangle absent"))?;
                    if let Err(e) = self
                        .rectangles
                        .as_mut()
                        .ok_or(Error::Invalid("PGS rectangles absent"))?
                        .try_push(rect)
                    {
                        self.rect_next = Some(e.value);
                        return Err(e.error);
                    }
                    self.rect_index += 1;
                    self.done(Phase::Rectangle);
                }
                Phase::StoreEvent => {
                    if !self.pay(work, cancel, 1)? {
                        return Ok(RecoveryStatus::Yield);
                    }
                    self.role.reserve_event()?;
                    let clock = if self.before_any_pcs {
                        None
                    } else {
                        self.presentation_clock
                            .filter(|c| c.pts.is_some())
                            .or(self.clock)
                    };
                    let pts = if self.before_any_pcs {
                        Some(0)
                    } else {
                        clock
                            .and_then(|c| c.native_microseconds().ok().flatten())
                            .filter(|v| *v != i64::MIN)
                    };
                    let rectangles = self
                        .rectangles
                        .take()
                        .ok_or(Error::Invalid("PGS event rectangles absent"))?;
                    self.role.store_event(RecoveredEvent {
                        pts_microseconds: pts,
                        rectangles,
                    })?;
                    self.done(Phase::Segment);
                }
                Phase::Finish => {
                    if !self.pay(work, cancel, 1)? {
                        return Ok(RecoveryStatus::Yield);
                    }
                    self.output = Some(
                        self.role.finish_packet(
                            self.packet
                                .take()
                                .ok_or(Error::Invalid("PGS packet absent"))?,
                            self.clock.ok_or(Error::Invalid("PGS clock absent"))?,
                            self.canvas,
                            self.observations,
                        )?,
                    );
                    self.done(Phase::Ready);
                }
                Phase::Ready => return Ok(RecoveryStatus::OutputReady),
                Phase::Idle => return Ok(RecoveryStatus::NeedPacket),
                Phase::Eof => {
                    match self
                        .role
                        .poll_eof(work, &mut self.spent, self.limits.max_work, cancel)?
                    {
                        Some(eof) => {
                            self.eof = Some(eof);
                            self.done(Phase::Ended);
                        }
                        None => return Ok(RecoveryStatus::Yield),
                    }
                }
                Phase::Ended => return Ok(RecoveryStatus::Ended),
            }
        }
    }

    fn make_rect(
        &self,
        width: u16,
        height: u16,
        stride: u16,
        pixels: Bytes,
    ) -> Result<RecoveredRect> {
        let reference = self.references.as_slice()[self.rect_index];
        let (x, y) = if reference.x > self.canvas[0] || reference.y > self.canvas[1] {
            (0, 0)
        } else {
            (reference.x, reference.y)
        };
        Ok(RecoveredRect {
            reference,
            x,
            y,
            width,
            height,
            stride,
            forced: reference.forced(),
            pixels,
            palette: ReceiverPalette::Pending,
        })
    }

    /// The existing funded fixed-size rectangle-store action now resolves its
    /// palette too. It examines at most one retained Source owner and all256
    /// literal words. No packet/Source-list scan or source-knownness inference
    /// occurs. This replaces the old unconditional256-word inline copy.
    fn finish_rect_palette(&mut self) -> Result<()> {
        let rect = self
            .rect_next
            .as_ref()
            .ok_or(Error::Invalid("PGS recovery rectangle absent"))?;
        if matches!(rect.palette, ReceiverPalette::Pending) {
            let words = self.rect_words(rect);
            let candidate = self.role.palette_candidate();
            let shared = match candidate {
                Some(source) => {
                    if !source.belongs_to(&self.budget) {
                        return Err(Error::ForeignPool);
                    }
                    if source.native_words() == words.as_slice() {
                        Some(source.clone())
                    } else {
                        None
                    }
                }
                None => None,
            };
            self.rect_next
                .as_mut()
                .ok_or(Error::Invalid("PGS recovery rectangle absent"))?
                .palette = shared.map_or(ReceiverPalette::Fallback, ReceiverPalette::Shared);
        }
        let rect = self
            .rect_next
            .as_ref()
            .ok_or(Error::Invalid("PGS recovery rectangle absent"))?;
        if matches!(rect.palette, ReceiverPalette::Fallback) {
            let charge = self.budget.reserve(size_of::<[u32; 256]>())?;
            let words = Box::new(*self.rect_words(rect));
            self.rect_next
                .as_mut()
                .ok_or(Error::Invalid("PGS recovery rectangle absent"))?
                .palette = ReceiverPalette::Owned {
                words,
                _charge: charge,
            };
        }
        Ok(())
    }

    fn rect_words(&self, rect: &RecoveredRect) -> &[u32; 256] {
        if self.config.native_forced_only && !rect.forced {
            &[0; 256]
        } else {
            &self.palettes.as_slice()[self.palette_index].words
        }
    }
    fn empty_rect(&mut self, stride: u16) -> Result<()> {
        self.rect_next = Some(self.make_rect(0, 0, stride, self.packet()?.slice(0..0)?)?);
        Ok(())
    }
    pub fn take_output(&mut self, cancel: &AtomicBool) -> Result<S::Packet> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(self.poison(Error::Cancelled));
        }
        if self.phase != Phase::Ready {
            return Err(Error::Invalid("PGS recovery output not ready"));
        }
        let result = self
            .output
            .take()
            .ok_or(Error::Invalid("PGS recovery output absent"))?;
        self.done(Phase::Idle);
        Ok(result)
    }
    pub fn finish_input(&mut self, cancel: &AtomicBool) -> Result<()> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(self.poison(Error::Cancelled));
        }
        if self.phase != Phase::Idle {
            return Err(Error::Invalid("PGS recovery input busy"));
        }
        self.final_input = true;
        if let Err(error) = self.role.finish_input(cancel) {
            return Err(self.poison(error));
        }
        self.done(Phase::Eof);
        Ok(())
    }
    pub fn eof_report(&self) -> Option<S::Eof> {
        self.eof
    }
    pub fn reset(&mut self, cancel: &AtomicBool) -> Result<()> {
        if cancel.load(Ordering::Relaxed) {
            return Err(self.poison(Error::Cancelled));
        }
        if self.failed.is_none() && self.phase != Phase::Idle && self.phase != Phase::Ended {
            return Err(Error::Invalid("PGS recovery reset busy"));
        }
        self.role.reset(cancel)?;
        self.objects = BudgetVec::new(&self.budget);
        self.palettes = BudgetVec::new(&self.budget);
        self.references = BudgetVec::new(&self.budget);
        self.canvas = [0, 0];
        self.presentation_clock = None;
        self.before_any_pcs = true;
        self.failed = None;
        self.spent = 0;
        self.credit = 0;
        self.final_input = false;
        self.eof = None;
        self.done(Phase::Idle);
        Ok(())
    }
}

impl Decoder<FullRecovery<IndexedArgb>> {
    /// Write a retained Source view while charging this decoder's cumulative
    /// operation limit. No raster allocation, target retention or source
    /// generation change occurs. Foreign writers remain owned by the caller.
    pub fn write_source_argb(
        &mut self,
        writer: &mut ArgbWriter,
        target: &mut [u8],
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ArgbWrite> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(self.poison(Error::Cancelled));
        }
        writer.check_budget(&self.budget)?;
        if target.is_empty() || work.remaining() == 0 || writer.is_complete() {
            return writer.write(target, work, cancel);
        }
        let result = DisplayWork {
            caller: work,
            spent: &mut self.spent,
            maximum: self.limits.max_work,
            cancel,
        }
        .child(|child| writer.write(target, child, cancel));
        match result {
            Ok(step) => Ok(step),
            Err(error) => Err(self.poison(error)),
        }
    }
}

#[cfg(test)]
mod index_sharing_controls;

#[cfg(test)]
mod receiver_palette_sharing_controls;

#[cfg(test)]
mod minimal_work_controls;

#[cfg(test)]
mod palette_entry_read_controls;

//! The PGS receiver: FFmpeg's `pgssub` decoder as one bounded packet loop.
//!
//! One engine serves both roles: [`RecoveryDecoder`] and
//! [`IndexedRecoveryDecoder`] also run the strict source decoder over each
//! packet and keep its sets and error beside the receiver's events;
//! [`ReceiverDecoder`](super::receiver_decoder::ReceiverDecoder) runs only the
//! receiver. The receiver never publishes guessed source pixels, colours or
//! clocks: its output is what the reference presents, named as such.
//!
//! Each packet runs once through [`Decoder::poll`]: segments in order (PCS,
//! PDS and ODS update the [`state::Receiver`], END builds a display), work paid
//! before each step, and every allocation made before the state it feeds
//! changes, so a pressure error can be retried without repeating work.
mod rle;
mod state;

use super::display_model::{PaletteSnapshot, StoredDisplaySet};
use super::display_work::DisplayWork;
use super::indexed_argb::{ArgbStorage, ArgbWrite, ArgbWriter, IndexedArgb};
use super::recovery_role::{FullRecovery, RecoveryRole};
use super::{CompositionObject, DisplayConfig, DisplayLimits, PacketClock};
use state::{PaletteWords, Receiver, Reference};

/// The segment types the receiver acts on; it skips every other type.
const PALETTE: u8 = 0x14;
const OBJECT: u8 = 0x15;
const COMPOSITION: u8 = 0x16;
const END: u8 = 0x80;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result, WorkBudget};

/// Room for `additional` more items in `items`, which never needs more than
/// `bound`: capacity at least doubles, so a vector grown one item at a time
/// moves each item a bounded number of times. Under pool pressure the exact
/// request is tried instead.
pub(super) fn reserve_amortised<T>(
    items: &mut BudgetVec<T>,
    additional: usize,
    bound: usize,
) -> Result<()> {
    let needed = items.len().checked_add(additional).ok_or(Error::Overflow)?;
    if needed <= items.capacity() {
        return Ok(());
    }
    let target = items
        .capacity()
        .saturating_mul(2)
        .max(4)
        .min(bound)
        .max(needed);
    items
        .try_reserve_exact(target - items.len())
        .or_else(|error| match error {
            Error::Backpressure { .. } | Error::Limit { .. } if target > needed => {
                items.try_reserve_exact(additional)
            }
            error => Err(error),
        })
}

/// The end of full recovery's input. The receiver holds no delayed subtitle,
/// so its drain returns nothing; the strict source decoder may still report
/// what its last packets left incomplete (a missing END is a truncation).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecoveryEof {
    pub source_error: Option<Error>,
}

/// Why the receiver refused a packet, as the reference's decode call fails.
/// The receiver keeps the state the packet's earlier segments left.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Shorter than one segment header; nothing in it is read.
    ShortPacket,
    /// An object's first fragment declares fewer data bytes than its own size
    /// fields; the rest of the packet is skipped.
    ObjectLength,
}

/// One presented rectangle.
#[derive(Debug)]
pub struct RecoveredRect {
    /// Original syntactic PCS reference, independent of receiver placement.
    pub reference: CompositionObject,
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
    /// The object's allocated row width, kept even when its RLE is rejected.
    pub stride: u16,
    pub forced: bool,
    pixels: Bytes,
    palette: ReceiverPalette,
}

/// A rectangle's ARGB words: the composition palette shared by the event's
/// rectangles (or a strict source palette with exactly the same words), or
/// zero for a rectangle that forced-only output hides.
#[derive(Clone, Debug)]
enum ReceiverPalette {
    Hidden,
    Shared(PaletteSnapshot),
    Owned(Arc<PaletteWords>),
}

const HIDDEN_PALETTE: [u32; 256] = [0; 256];

impl RecoveredRect {
    pub fn pixels(&self) -> &Bytes {
        &self.pixels
    }
    pub fn palette(&self) -> &[u32; 256] {
        match &self.palette {
            ReceiverPalette::Hidden => &HIDDEN_PALETTE,
            ReceiverPalette::Shared(source) => source
                .receiver_words()
                .try_into()
                .expect("PGS palette snapshot has 256 words"),
            ReceiverPalette::Owned(owned) => &owned.words,
        }
    }
}

/// One subtitle the receiver presents.
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
    pub(super) refusal: Option<Refusal>,
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
    /// The subtitle the reference returns for this packet.
    pub fn receiver_output(&self) -> Option<&RecoveredEvent> {
        self.events.as_slice().first()
    }
    /// That subtitle, then what each later END of the packet would present;
    /// the reference ignores those.
    pub fn receiver_sets(&self) -> &[RecoveredEvent] {
        self.events.as_slice()
    }
    pub fn omitted_receiver_events(&self) -> usize {
        self.events.len().saturating_sub(1)
    }
    pub fn receiver_refusal(&self) -> Option<Refusal> {
        self.refusal
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryStatus {
    NeedPacket,
    Yield,
    OutputReady,
    Ended,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Idle,
    /// The strict source decoder's pass over the packet (full recovery only).
    Source,
    Segment,
    /// After an END segment: the receiver builds its display.
    Display,
    /// The display's next rectangle.
    Rectangle,
    /// Decoding the rectangle's object.
    Raster,
    StoreEvent,
    Finish,
    Ready,
    Eof,
    Ended,
}

/// A rectangle's pixels: a decoded object, or nothing with the row width the
/// receiver allocated.
struct Bitmap {
    width: u16,
    height: u16,
    stride: u16,
    pixels: Bytes,
}

impl Bitmap {
    fn empty(stride: u16, pixels: Bytes) -> Self {
        Self {
            width: 0,
            height: 0,
            stride,
            pixels,
        }
    }
}

/// Whether a rectangle's reference names an object the receiver has.
#[derive(Clone, Copy)]
enum Placement {
    Object,
    Missing,
}

/// A display being built from an END segment.
#[derive(Debug)]
struct Draft {
    pts: Option<i64>,
    rectangles: BudgetVec<RecoveredRect>,
    palette: Option<ReceiverPalette>,
    pixels: usize,
}

/// An explicitly selected receiver, never a replacement for DisplayDecoder.
/// One physical packet is active at a time. Retained packets/strict sets/receiver
/// outputs keep their actual capacities charged to the supplied shared budget.
pub type RecoveryDecoder = Decoder<FullRecovery<Bytes>>;
/// Explicit indexed storage selection. Receiver recovery is unchanged and
/// never supplies missing strict Source pixels, palette facts or clocks.
pub type IndexedRecoveryDecoder = Decoder<FullRecovery<IndexedArgb>>;

#[derive(Debug)]
pub struct Decoder<S: RecoveryRole> {
    budget: Budget,
    limits: DisplayLimits,
    config: DisplayConfig,
    role: S,
    receiver: Receiver,
    phase: Phase,
    credit: usize,
    spent: usize,
    failed: Option<Error>,
    final_input: bool,
    eof: Option<S::Eof>,
    output: Option<S::Packet>,
    // The active packet.
    packet: Option<Bytes>,
    clock: Option<PacketClock>,
    at: usize,
    /// The PTS of the subtitle this packet returns, as the reference fills it:
    /// the packet's own, then a display's presentation PTS, or zero once a
    /// display failed.
    subtitle_pts: Option<i64>,
    displayed: bool,
    refusal: Option<Refusal>,
    observations: ReceiverObservations,
    draft: Option<Draft>,
}

impl<S: RecoveryRole> Decoder<S> {
    pub fn new(budget: &Budget, limits: DisplayLimits, config: DisplayConfig) -> Result<Self> {
        Ok(Self {
            budget: budget.clone(),
            limits,
            config,
            role: S::new(budget, limits, config)?,
            receiver: Receiver::new(budget),
            phase: Phase::Idle,
            credit: 0,
            spent: 0,
            failed: None,
            final_input: false,
            eof: None,
            output: None,
            packet: None,
            clock: None,
            at: 0,
            subtitle_pts: None,
            displayed: false,
            refusal: None,
            observations: ReceiverObservations::default(),
            draft: None,
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
        self.subtitle_pts = state::microseconds(clock);
        self.displayed = false;
        self.refusal = None;
        self.observations = ReceiverObservations::default();
        self.role.begin_packet(&self.budget);
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
        self.receiver = Receiver::new(&self.budget);
        self.draft = None;
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

    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<RecoveryStatus> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(self.poison(Error::Cancelled));
        }
        match self.phase {
            Phase::Ready => return Ok(RecoveryStatus::OutputReady),
            Phase::Idle => return Ok(RecoveryStatus::NeedPacket),
            Phase::Ended => return Ok(RecoveryStatus::Ended),
            _ if work.remaining() == 0 => return Ok(RecoveryStatus::Yield),
            _ => {}
        }
        match self.run(work, cancel) {
            Ok(status) => Ok(status),
            Err(error @ Error::Backpressure { .. }) => Err(error),
            Err(error) => Err(self.poison(error)),
        }
    }

    /// The decode loop: every phase of a packet, until it is ready or the
    /// caller's work runs out.
    fn run(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<RecoveryStatus> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if work.remaining() == 0 {
                return Ok(RecoveryStatus::Yield);
            }
            let phase = self.phase;
            let next = match phase {
                Phase::Source => self.source(work, cancel)?,
                Phase::Segment => self.segment(work, cancel)?,
                Phase::Display => self.display(work, cancel)?,
                Phase::Rectangle => self.rectangle(work, cancel)?,
                Phase::Raster => self.raster(work, cancel)?,
                Phase::StoreEvent => self.store_event(work, cancel)?,
                Phase::Finish => self.finish(work, cancel)?,
                Phase::Eof => self.end_of_stream(work, cancel)?,
                Phase::Idle => return Ok(RecoveryStatus::NeedPacket),
                Phase::Ready => return Ok(RecoveryStatus::OutputReady),
                Phase::Ended => return Ok(RecoveryStatus::Ended),
            };
            match next {
                Some(next) => self.done(next),
                None => return Ok(RecoveryStatus::Yield),
            }
        }
    }

    fn packet(&self) -> Result<&Bytes> {
        self.packet
            .as_ref()
            .ok_or(Error::Invalid("PGS recovery physical packet absent"))
    }

    fn source(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Option<Phase>> {
        let raw = self
            .packet
            .as_ref()
            .ok_or(Error::Invalid("PGS recovery physical packet absent"))?;
        let clock = self.clock.ok_or(Error::Invalid("PGS clock absent"))?;
        let finished =
            self.role
                .poll_source(raw, clock, &self.limits, work, &mut self.spent, cancel)?;
        Ok(finished.then_some(Phase::Segment))
    }

    /// The segment at `at`: its header (the length read past the packet end as
    /// zero), then its effect on the receiver. A segment running past the
    /// packet ends the packet, except END, which has no body to read.
    fn segment(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Option<Phase>> {
        let length = self.packet()?.len();
        if length < 3 {
            self.refusal = Some(Refusal::ShortPacket);
            return Ok(Some(Phase::Finish));
        }
        if self.at >= length {
            return Ok(Some(Phase::Finish));
        }
        let raw = self.packet()?;
        let byte = |at: usize| raw.get(at).copied().unwrap_or(0);
        let kind = byte(self.at);
        let body = self.at + 3
            ..self.at + 3 + usize::from(u16::from_be_bytes([byte(self.at + 1), byte(self.at + 2)]));
        if body.end > length && kind != END {
            self.observations.truncated_segment_tail = true;
            return Ok(Some(Phase::Finish));
        }
        // Fixed when first computed, so a retry after pressure pays nothing
        // twice; the cache lookups a segment makes are bounded by the limits.
        let cost = body.end.min(length) - self.at + 1;
        if !self.pay(work, cancel, cost)? {
            return Ok(None);
        }
        let raw = self
            .packet
            .as_ref()
            .ok_or(Error::Invalid("PGS recovery physical packet absent"))?;
        let (receiver, limits) = (&mut self.receiver, &self.limits);
        match kind {
            COMPOSITION => receiver.composition(raw, body.clone(), self.subtitle_pts, limits)?,
            PALETTE => {
                receiver.palette_definition(
                    raw,
                    body.clone(),
                    limits,
                    &self.budget,
                    &mut self.observations,
                )?;
            }
            OBJECT => {
                self.refusal =
                    receiver.object_fragment(&raw[body.clone()], limits, &self.budget)?;
            }
            _ => {}
        }
        self.at = body.end;
        Ok(Some(match kind {
            _ if self.refusal.is_some() => Phase::Finish,
            END => Phase::Display,
            _ => Phase::Segment,
        }))
    }

    /// END: the reference returns the first display of a packet and ignores
    /// later ones (full recovery still builds them, without the state changes
    /// a returned display makes). A display that needs the composition's
    /// palette and lacks it returns nothing and zeroes the packet's PTS.
    fn display(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Option<Phase>> {
        let duplicate = self.displayed;
        if duplicate && !S::BUILDS_IGNORED_DISPLAYS {
            return Ok(self.pay(work, cancel, 1)?.then_some(Phase::Segment));
        }
        if !self.pay(work, cancel, self.receiver.cache_entries() + 1)? {
            return Ok(None);
        }
        let references = self.receiver.references().count();
        if references != 0 && self.receiver.palette().is_none() {
            if !duplicate {
                self.receiver.presentation = None;
                self.subtitle_pts = Some(0);
            }
            return Ok(Some(Phase::Segment));
        }
        let mut rectangles = BudgetVec::new(&self.budget);
        rectangles.try_reserve_exact(references)?;
        let pts = self.receiver.presentation.or(self.subtitle_pts);
        if !duplicate {
            self.subtitle_pts = pts;
            self.receiver.presentation = None;
            self.displayed = true;
        }
        self.draft = Some(Draft {
            pts,
            rectangles,
            palette: None,
            pixels: 0,
        });
        Ok(Some(Phase::Rectangle))
    }

    fn draft(&mut self) -> Result<&mut Draft> {
        self.draft
            .as_mut()
            .ok_or(Error::Invalid("PGS receiver display absent"))
    }

    /// The next reference: a missing object is an empty rectangle at the
    /// origin, an object without data an empty one at its position; an
    /// object with data is decoded next, within the pixel limits for one
    /// object and for the display.
    fn rectangle(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Option<Phase>> {
        let index = self.draft()?.rectangles.len();
        let Some(reference) = self.receiver.references.get(index).copied().flatten() else {
            return Ok(Some(Phase::StoreEvent));
        };
        if !self.pay(work, cancel, self.receiver.objects.len() + 1)? {
            return Ok(None);
        }
        let Some(object) = self.receiver.object(reference.object.id) else {
            let empty = self.packet()?.slice(0..0)?;
            self.push_rect(reference, Placement::Missing, Bitmap::empty(0, empty))?;
            return Ok(Some(Phase::Rectangle));
        };
        if !object.has_data() {
            let stride = object.width;
            let empty = self.packet()?.slice(0..0)?;
            self.push_rect(reference, Placement::Object, Bitmap::empty(stride, empty))?;
            return Ok(Some(Phase::Rectangle));
        }
        let area = usize::from(object.width) * usize::from(object.height);
        let total = self
            .draft()?
            .pixels
            .checked_add(area)
            .ok_or(Error::Overflow)?;
        for (requested, maximum) in [
            (area, self.limits.max_object_pixels),
            (total, self.limits.max_display_pixels),
        ] {
            if requested > maximum {
                return Err(Error::Limit { requested, maximum });
            }
        }
        Ok(Some(Phase::Raster))
    }

    /// Decodes the current reference's object, or shares the strict source's
    /// pixels when its validated RLE is byte for byte the same.
    fn raster(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Option<Phase>> {
        let index = self.draft()?.rectangles.len();
        let reference = self.receiver.references[index]
            .ok_or(Error::Invalid("PGS receiver reference absent"))?;
        let object = self
            .receiver
            .object(reference.object.id)
            .ok_or(Error::Invalid("PGS receiver object absent"))?;
        let (width, height) = (object.width, object.height);
        let area = usize::from(width) * usize::from(height);
        if !self.pay(work, cancel, object.encoded.len() + area)? {
            return Ok(None);
        }
        let object = self
            .receiver
            .object(reference.object.id)
            .ok_or(Error::Invalid("PGS receiver object absent"))?;
        let shared = self
            .role
            .validated_indices(reference.object.id, width, height)
            .filter(|(encoded, pixels)| {
                pixels.len() == area && encoded.as_ref() == object.encoded.as_slice()
            })
            .map(|(_, pixels)| pixels);
        let pixels = if let Some(pixels) = shared {
            Some(pixels)
        } else {
            object.decode(&self.budget)?
        };
        let bitmap = match pixels {
            Some(pixels) => Bitmap {
                width,
                height,
                stride: width,
                pixels,
            },
            None => Bitmap::empty(width, self.packet()?.slice(0..0)?),
        };
        self.draft()?.pixels += area;
        self.push_rect(reference, Placement::Object, bitmap)?;
        Ok(Some(Phase::Rectangle))
    }

    /// Appends a rectangle. One for a missing object sits at the origin and
    /// carries no forced flag; forced-only output still selects its palette
    /// by the reference's flag.
    fn push_rect(
        &mut self,
        reference: Reference,
        placement: Placement,
        bitmap: Bitmap,
    ) -> Result<()> {
        let ((x, y), forced) = match placement {
            Placement::Object => ((reference.x, reference.y), reference.object.forced()),
            Placement::Missing => ((0, 0), false),
        };
        let palette = if self.config.receiver_forced_only && !reference.object.forced() {
            ReceiverPalette::Hidden
        } else {
            self.composition_palette()?
        };
        self.draft()?
            .rectangles
            .try_push(RecoveredRect {
                reference: reference.object,
                x,
                y,
                width: bitmap.width,
                height: bitmap.height,
                stride: bitmap.stride,
                forced,
                pixels: bitmap.pixels,
                palette,
            })
            .map_err(|e| e.error)
    }

    /// The composition palette for this display, chosen once: the strict
    /// source set's palette when its words are the same, otherwise the
    /// receiver's own.
    fn composition_palette(&mut self) -> Result<ReceiverPalette> {
        if let Some(palette) = &self.draft()?.palette {
            return Ok(palette.clone());
        }
        let words = self
            .receiver
            .palette()
            .ok_or(Error::Invalid("PGS receiver palette absent"))?;
        let palette = match self.role.palette_candidate() {
            Some(source) if !source.belongs_to(&self.budget) => return Err(Error::ForeignPool),
            Some(source) if source.receiver_words() == words.words.as_slice() => {
                ReceiverPalette::Shared(source.clone())
            }
            _ => ReceiverPalette::Owned(Arc::clone(words)),
        };
        self.draft()?.palette = Some(palette.clone());
        Ok(palette)
    }

    fn store_event(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Option<Phase>> {
        let cost = self.role.event_cost()?;
        if !self.pay(work, cancel, cost)? {
            return Ok(None);
        }
        self.role.reserve_event(self.limits.max_segments)?;
        let draft = self
            .draft
            .take()
            .ok_or(Error::Invalid("PGS receiver display absent"))?;
        self.role.store_event(RecoveredEvent {
            pts_microseconds: draft.pts,
            rectangles: draft.rectangles,
        })?;
        Ok(Some(Phase::Segment))
    }

    fn finish(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Option<Phase>> {
        if !self.pay(work, cancel, 1)? {
            return Ok(None);
        }
        let raw = self
            .packet
            .take()
            .ok_or(Error::Invalid("PGS packet absent"))?;
        let clock = self.clock.ok_or(Error::Invalid("PGS clock absent"))?;
        self.output = Some(self.role.finish_packet(
            raw,
            clock,
            self.receiver.canvas,
            self.observations,
            self.refusal,
        )?);
        Ok(Some(Phase::Ready))
    }

    fn end_of_stream(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<Phase>> {
        let eof = self
            .role
            .poll_eof(work, &mut self.spent, self.limits.max_work, cancel)?;
        Ok(eof.map(|eof| {
            self.eof = Some(eof);
            Phase::Ended
        }))
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
        self.receiver = Receiver::new(&self.budget);
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
mod controls;

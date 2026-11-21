//! The PGS receiver: FFmpeg's `pgssub` decoder as one bounded packet loop.
//!
//! Each packet runs once through [`ReceiverDecoder::poll`]: segments in order
//! (PCS, PDS and ODS update the [`state::Receiver`], END builds a display),
//! work paid before each step, and every allocation made before the state it
//! feeds changes, so a pressure error can be retried without repeating work.
//! The output is what the reference presents, named as such: the receiver
//! never publishes guessed pixels, colours or clocks.
mod rle;
mod state;
mod work;

use super::CompositionObject;
use state::{PaletteWords, Receiver, Reference};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, BudgetVec, Bytes, BytesWriter, Error, Result, Rounding, Time, TimeBase, WorkBudget,
};
use work::Work;

/// The segment types the receiver acts on; it skips every other type.
const PALETTE: u8 = 0x14;
const OBJECT: u8 = 0x15;
const COMPOSITION: u8 = 0x16;
const END: u8 = 0x80;

/// A packet's timestamps in its stream's time base.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PacketClock {
    /// Presentation time in `timebase` ticks; `None`, or `i64::MIN`, when
    /// unknown.
    pub pts: Option<i64>,
    /// Decoding time in `timebase` ticks; carried to the output, not read.
    pub dts: Option<i64>,
    /// The seconds per tick of `pts` and `dts`.
    pub timebase: TimeBase,
}

impl PacketClock {
    /// The PTS in microseconds, rounded to nearest with ties away from zero.
    fn receiver_microseconds(self) -> Result<Option<i64>> {
        self.pts
            .map(|ticks| {
                Time::from_ticks(i128::from(ticks), self.timebase)?
                    .to_i64_ticks(TimeBase::new(1, 1_000_000)?, Rounding::NearestAway)
            })
            .transpose()
    }
}

/// Bounds on what one decoder holds and does. Exceeding one fails the
/// decoder with `Error::Limit` until [`ReceiverDecoder::reset`], except where
/// a field says otherwise.
#[derive(Clone, Copy, Debug)]
pub struct DisplayLimits {
    /// Largest area of one object a display decodes, in pixels.
    pub max_object_pixels: usize,
    /// Largest total area of the objects one display decodes, in pixels.
    pub max_display_pixels: usize,
    /// Most RLE bytes one object holds.
    pub max_encoded: usize,
    /// Most fragments one object takes.
    pub max_fragments: usize,
    /// Most objects cached at once, at most 64; an ODS for a new id is then
    /// ignored.
    pub max_objects: usize,
    /// Most palettes cached at once, at most 8; a PDS for a new id is then
    /// ignored.
    pub max_palettes: usize,
    /// Most composition objects a PCS may declare, at most 255. The receiver
    /// presents the first two.
    pub max_references: usize,
    /// Largest packet [`ReceiverDecoder::push`] accepts, in bytes; a larger
    /// one is refused with `Error::Limit` and the decoder carries on.
    pub max_packet_bytes: usize,
    /// Work units the decoder spends in all until reset; work, not wall time.
    pub max_work: usize,
}

impl Default for DisplayLimits {
    fn default() -> Self {
        Self {
            max_object_pixels: 8 * 1024 * 1024,
            max_display_pixels: 16 * 1024 * 1024,
            max_encoded: 4 * 1024 * 1024,
            max_fragments: 1024,
            max_objects: 64,
            max_palettes: 8,
            max_references: 255,
            max_packet_bytes: 16 * 1024 * 1024,
            max_work: 256 * 1024 * 1024,
        }
    }
}

impl DisplayLimits {
    /// The ceilings of the format's identifier spaces.
    fn validate(self) -> Result<()> {
        for (requested, maximum) in [
            (self.max_objects, 64),
            (self.max_palettes, 8),
            (self.max_references, 255),
        ] {
            if requested > maximum {
                return Err(Error::Limit { requested, maximum });
            }
        }
        Ok(())
    }
}

/// The decoder options that change the output.
#[derive(Clone, Copy, Debug, Default)]
pub struct DisplayConfig {
    /// Hide every rectangle without the forced flag (`forced_subs_only`): it
    /// keeps its pixels and takes an all-zero palette.
    pub receiver_forced_only: bool,
}

/// Room for `additional` more items in `items`, which never needs more than
/// `bound`: capacity at least doubles, so a vector grown one item at a time
/// moves each item a bounded number of times. Under pool pressure the exact
/// request is tried instead.
fn reserve_amortised<T>(items: &mut BudgetVec<T>, additional: usize, bound: usize) -> Result<()> {
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
    reserve_room(items, additional, target)
}

/// Room for `additional` more items in `items`, `target` in all (at least what
/// is needed) where the pool allows; under pool pressure the exact request is
/// tried instead.
fn reserve_room<T>(items: &mut BudgetVec<T>, additional: usize, target: usize) -> Result<()> {
    let room = target - items.len();
    items.try_reserve_exact(room).or_else(|error| match error {
        Error::Backpressure { .. } | Error::Limit { .. } if room > additional => {
            items.try_reserve_exact(additional)
        }
        error => Err(error),
    })
}

/// Why the receiver refused a packet, as the reference's decode call fails.
/// The receiver keeps the state the packet's earlier segments left.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Shorter than one segment header; nothing in it is read.
    ShortPacket,
    /// An object's first fragment declares fewer data bytes than its own size
    /// fields; the object keeps the fragment's size without data, and the
    /// rest of the packet is skipped.
    ObjectLength,
}

/// One presented rectangle.
#[derive(Debug)]
pub struct RecoveredRect {
    /// The composition object it presents.
    pub reference: CompositionObject,
    /// Column of the top-left corner on the canvas: the composition
    /// object's, or 0 when its object is missing or either coordinate lies
    /// beyond the canvas of its PCS.
    pub x: u16,
    /// Row of the top-left corner on the canvas, placed as `x` is.
    pub y: u16,
    /// Width in pixels; 0 when the object is missing, has no data or its RLE
    /// does not fill it.
    pub width: u16,
    /// Height in pixels; 0 whenever `width` is.
    pub height: u16,
    /// The object's allocated row width, kept even when its RLE is rejected;
    /// 0 for a missing object or one without data.
    pub stride: u16,
    /// The composition object's forced flag; false for a missing object.
    pub forced: bool,
    pixels: Bytes,
    palette: ReceiverPalette,
}

/// A rectangle's ARGB words: the composition palette shared by the event's
/// rectangles, or zero for a rectangle that forced-only output hides.
#[derive(Clone, Debug)]
enum ReceiverPalette {
    Hidden,
    Owned(Arc<PaletteWords>),
}

const HIDDEN_PALETTE: [u32; 256] = [0; 256];

impl RecoveredRect {
    /// `height` rows of `width` palette indices, row after row; empty when
    /// the rectangle has no pixels.
    pub fn pixels(&self) -> &Bytes {
        &self.pixels
    }
    /// `0xAARRGGBB` colours by pixel value: the palette the composition
    /// selects, or all zero for a rectangle forced-only output hides.
    pub fn palette(&self) -> &[u32; 256] {
        match &self.palette {
            ReceiverPalette::Hidden => &HIDDEN_PALETTE,
            ReceiverPalette::Owned(owned) => &owned.words,
        }
    }
}

/// One subtitle the receiver presents.
#[derive(Debug)]
pub struct RecoveredEvent {
    /// Presentation time in microseconds, ties away from zero: the PTS of the
    /// packet whose PCS this display is the first to present (zero before
    /// any PCS), else this packet's own; `None` when that PTS is unknown.
    pub pts_microseconds: Option<i64>,
    rectangles: BudgetVec<RecoveredRect>,
}
impl RecoveredEvent {
    /// The rectangles in composition-object order, at most two; none for a
    /// display that clears the screen.
    pub fn rectangles(&self) -> &[RecoveredRect] {
        self.rectangles.as_slice()
    }
}

/// How the receiver read a packet where its bytes run short. The zeros it
/// reads past the end of the packet are never part of
/// [`ReceiverPacket::raw_packet`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReceiverObservations {
    /// A segment other than END declared a body running past the end of the
    /// packet, which ended the packet there.
    pub truncated_segment_tail: bool,
    /// Bytes that a PDS's partial last entry read past the end of its
    /// segment, summed over the packet.
    pub palette_cross_segment_bytes: usize,
    /// Of those, the bytes past the end of the packet, read as zero.
    pub palette_zero_padding_bytes: usize,
}

/// One decoded packet. Segments after its first display still update the
/// decoder's caches and `context_canvas`.
#[derive(Debug)]
pub struct ReceiverPacket {
    raw: Bytes,
    /// The clock the packet was pushed with.
    pub clock: PacketClock,
    event: Option<RecoveredEvent>,
    refusal: Option<Refusal>,
    /// Canvas width and height after the packet: the size of the latest PCS
    /// the receiver accepted, `[0, 0]` before one or after a PCS whose size
    /// it rejects.
    pub context_canvas: [u16; 2],
    /// Where the receiver read past a segment or the packet.
    pub receiver_observations: ReceiverObservations,
}

impl ReceiverPacket {
    /// The packet as pushed, unchanged.
    pub fn raw_packet(&self) -> &Bytes {
        &self.raw
    }
    /// The subtitle the reference returns for this packet.
    pub fn receiver_output(&self) -> Option<&RecoveredEvent> {
        self.event.as_ref()
    }
    /// Set when the reference's decode call fails on this packet.
    pub fn refusal(&self) -> Option<Refusal> {
        self.refusal
    }
}

/// What the receiver reports at the end of input
/// ([`ReceiverDecoder::eof_report`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReceiverEof {
    /// The receiver holds no delayed subtitle, so draining it outputs
    /// nothing.
    NoDelayedSubtitle,
}

/// What [`ReceiverDecoder::poll`] reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryStatus {
    /// No packet is active: push the next one, or finish the input.
    NeedPacket,
    /// The work budget ran out; poll again with more.
    Yield,
    /// The packet is decoded; take it with
    /// [`take_output`](ReceiverDecoder::take_output).
    OutputReady,
    /// The input has ended and nothing more is output.
    Ended,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Idle,
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

/// Plane storage. A display writes the last display's buffer again in place
/// once the caller has released its rectangles, so its pages stay resident:
/// each decoded rectangle's pixels in turn, sliced into the rectangles when the
/// display is stored. Otherwise each plane is its own allocation, and the
/// largest becomes the next display's buffer.
#[derive(Debug)]
enum Planes {
    None,
    /// The last display's buffer.
    Kept(Bytes),
    /// The buffer a display writes, and where its next plane starts.
    Writing(BytesWriter, usize),
    /// A display's largest plane so far, each in its own allocation.
    Fresh(Bytes),
}

/// FFmpeg's PGS decoder over one stream's packets.
///
/// One packet is active at a time. Accepted inputs, caches and outputs stay
/// charged to the supplied pool; work is cumulative until reset and every
/// operation receives caller cancellation.
#[derive(Debug)]
pub struct ReceiverDecoder {
    budget: Budget,
    limits: DisplayLimits,
    config: DisplayConfig,
    /// The packet's subtitle, once its first END is displayed.
    event: Option<RecoveredEvent>,
    receiver: Receiver,
    phase: Phase,
    credit: usize,
    spent: usize,
    failed: Option<Error>,
    final_input: bool,
    eof: Option<ReceiverEof>,
    output: Option<ReceiverPacket>,
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
    planes: Planes,
}

impl ReceiverDecoder {
    /// A decoder charging `budget`, with no stream state.
    ///
    /// # Errors
    ///
    /// `Error::Limit` when `limits` allows more than 64 objects, 8 palettes
    /// or 255 composition objects.
    pub fn new(budget: &Budget, limits: DisplayLimits, config: DisplayConfig) -> Result<Self> {
        limits.validate()?;
        Ok(Self {
            budget: budget.clone(),
            limits,
            config,
            event: None,
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
            planes: Planes::None,
        })
    }

    /// Makes the packet in `raw`, taken from it, the active packet, with its
    /// `clock`. A refused packet stays in `raw`.
    ///
    /// # Errors
    ///
    /// `Error::Invalid` for an absent or empty packet, while a packet is
    /// active or its output not taken, and after
    /// [`finish_input`](Self::finish_input); `Error::ForeignPool` for bytes
    /// of another budget; `Error::Limit` beyond
    /// [`DisplayLimits::max_packet_bytes`]. `Error::Cancelled` when `cancel`
    /// is set, which fails the decoder until [`reset`](Self::reset); a failed
    /// decoder returns its failure.
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
        // `finish`, `poison` and `reset` leave no event behind; release
        // builds clear it anyway, out of line.
        debug_assert!(self.event.is_none());
        if self.event.is_some() {
            self.discard_event();
        }
        self.done(Phase::Segment);
        Ok(())
    }

    #[cold]
    fn discard_event(&mut self) {
        self.event = None;
    }

    /// Work units spent since the decoder was made or last reset.
    pub fn work_spent(&self) -> usize {
        self.spent
    }

    fn poison(&mut self, error: Error) -> Error {
        self.packet = None;
        self.event = None;
        self.output = None;
        self.receiver = Receiver::new(&self.budget);
        self.draft = None;
        self.planes = Planes::None;
        self.failed = Some(error);
        error
    }
    fn done(&mut self, phase: Phase) {
        self.phase = phase;
        self.credit = 0;
    }
    fn pay(&mut self, work: &mut WorkBudget, cancel: &AtomicBool, cost: usize) -> Result<bool> {
        Work {
            caller: work,
            spent: &mut self.spent,
            maximum: self.limits.max_work,
            cancel,
        }
        .pay(&mut self.credit, cost)
    }

    /// Decodes the active packet, or the end of input, as far as `work`
    /// allows, and reports what the decoder waits for.
    ///
    /// # Errors
    ///
    /// `Error::Backpressure` when the pool refuses memory: the call can be
    /// repeated, and work already paid is not charged again. Any other error,
    /// `Error::Cancelled` and the `Error::Limit` of [`DisplayLimits`]
    /// included, fails the decoder: every call returns it until
    /// [`reset`](Self::reset).
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
    /// later ones. A display that needs the composition's palette and lacks
    /// it returns nothing and zeroes the packet's PTS.
    fn display(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Option<Phase>> {
        if self.displayed {
            return Ok(self.pay(work, cancel, 1)?.then_some(Phase::Segment));
        }
        if !self.pay(work, cancel, self.receiver.cache_entries() + 1)? {
            return Ok(None);
        }
        let references = self.receiver.references().count();
        if references != 0 && self.receiver.palette().is_none() {
            self.receiver.presentation = None;
            self.subtitle_pts = Some(0);
            return Ok(Some(Phase::Segment));
        }
        let mut rectangles = BudgetVec::new(&self.budget);
        rectangles.try_reserve_exact(references)?;
        let pts = self.receiver.presentation.or(self.subtitle_pts);
        self.subtitle_pts = pts;
        self.receiver.presentation = None;
        self.displayed = true;
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
    /// origin, an object without data an empty one at its position with no
    /// row width; an object with data is decoded next, within the pixel
    /// limits for one object and for the display.
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
            let empty = self.packet()?.slice(0..0)?;
            self.push_rect(reference, Placement::Object, Bitmap::empty(0, empty))?;
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

    /// Decodes the current reference's object.
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
        // A rectangle without its own pixels views none of the packet.
        let packet = self
            .packet
            .as_ref()
            .ok_or(Error::Invalid("PGS recovery physical packet absent"))?;
        let empty = || packet.slice(0..0);
        if let Planes::Kept(_) = self.planes
            && let Planes::Kept(kept) = std::mem::replace(&mut self.planes, Planes::None)
            && let Ok(planes) = kept.try_into_writer()
        {
            self.planes = Planes::Writing(planes, 0);
        }
        let bitmap = if let Planes::Writing(planes, at) = &mut self.planes {
            let filled =
                planes.try_write_at(*at, area, |plane| Ok::<_, Error>(object.fill(plane)))?;
            if filled {
                *at += area;
                // Its pixels are sliced from the buffer when the display is stored.
                Bitmap {
                    width,
                    height,
                    stride: width,
                    pixels: empty()?,
                }
            } else {
                Bitmap::empty(width, empty()?)
            }
        } else {
            let mut plane = BudgetVec::new(&self.budget);
            plane.try_resize(area, 0)?;
            if object.fill(plane.as_mut_slice()) {
                let pixels = plane.freeze().map_err(|failure| failure.error)?;
                if !matches!(&self.planes, Planes::Fresh(largest) if largest.len() >= pixels.len())
                {
                    self.planes = Planes::Fresh(pixels.clone());
                }
                Bitmap {
                    width,
                    height,
                    stride: width,
                    pixels,
                }
            } else {
                Bitmap::empty(width, empty()?)
            }
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

    /// The composition palette for this display, chosen once.
    fn composition_palette(&mut self) -> Result<ReceiverPalette> {
        if let Some(palette) = &self.draft()?.palette {
            return Ok(palette.clone());
        }
        let words = self
            .receiver
            .palette()
            .ok_or(Error::Invalid("PGS receiver palette absent"))?;
        let palette = ReceiverPalette::Owned(Arc::clone(words));
        self.draft()?.palette = Some(palette.clone());
        Ok(palette)
    }

    fn store_event(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Option<Phase>> {
        if !self.pay(work, cancel, 1)? {
            return Ok(None);
        }
        let mut draft = self
            .draft
            .take()
            .ok_or(Error::Invalid("PGS receiver display absent"))?;
        if self.event.is_some() {
            return Err(Error::Invalid("PGS native public event already stored"));
        }
        match std::mem::replace(&mut self.planes, Planes::None) {
            Planes::Writing(planes, _) => {
                // Each decoded rectangle takes the next width * height bytes of
                // the buffer; the others have no pixels.
                let planes = planes.finish();
                let mut at = 0;
                for rect in draft.rectangles.as_mut_slice() {
                    let area = usize::from(rect.width) * usize::from(rect.height);
                    if area != 0 {
                        rect.pixels = planes.slice(at..at + area)?;
                        at += area;
                    }
                }
                self.planes = Planes::Kept(planes);
            }
            Planes::Kept(planes) | Planes::Fresh(planes) => self.planes = Planes::Kept(planes),
            Planes::None => {}
        }
        self.event = Some(RecoveredEvent {
            pts_microseconds: draft.pts,
            rectangles: draft.rectangles,
        });
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
        self.output = Some(ReceiverPacket {
            raw,
            clock,
            event: self.event.take(),
            refusal: self.refusal,
            context_canvas: self.receiver.canvas,
            receiver_observations: self.observations,
        });
        Ok(Some(Phase::Ready))
    }

    fn end_of_stream(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<Phase>> {
        let mut credit = 0;
        let paid = Work {
            caller: work,
            spent: &mut self.spent,
            maximum: self.limits.max_work,
            cancel,
        }
        .pay(&mut credit, 1)?;
        Ok(paid.then(|| {
            self.eof = Some(ReceiverEof::NoDelayedSubtitle);
            Phase::Ended
        }))
    }

    /// The decoded packet, once [`poll`](Self::poll) has returned
    /// `OutputReady`; the decoder then needs the next packet.
    ///
    /// # Errors
    ///
    /// `Error::Invalid` before the output is ready; `Error::Cancelled` when
    /// `cancel` is set, which fails the decoder; a failed decoder returns its
    /// failure.
    pub fn take_output(&mut self, cancel: &AtomicBool) -> Result<ReceiverPacket> {
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

    /// Ends the input; [`poll`](Self::poll) then reaches `Ended`.
    ///
    /// # Errors
    ///
    /// `Error::Invalid` while a packet is active or its output not taken, and
    /// once input has ended; `Error::Cancelled` when `cancel` is set, which
    /// fails the decoder; a failed decoder returns its failure.
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
        self.done(Phase::Eof);
        Ok(())
    }

    /// What the receiver reported at the end of input, once
    /// [`poll`](Self::poll) has reached `Ended`; `None` before.
    pub fn eof_report(&self) -> Option<ReceiverEof> {
        self.eof
    }

    /// Discards the stream state (caches, canvas, failure, work spent and the
    /// end of input), keeping the limits and configuration. Outputs already
    /// taken keep their own charged storage.
    ///
    /// # Errors
    ///
    /// `Error::Invalid` unless the decoder has failed, needs a packet or has
    /// ended; `Error::Cancelled` when `cancel` is set, which fails the
    /// decoder.
    pub fn reset(&mut self, cancel: &AtomicBool) -> Result<()> {
        if cancel.load(Ordering::Relaxed) {
            return Err(self.poison(Error::Cancelled));
        }
        if self.failed.is_none() && self.phase != Phase::Idle && self.phase != Phase::Ended {
            return Err(Error::Invalid("PGS recovery reset busy"));
        }
        self.event = None;
        self.planes = Planes::None;
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

#[cfg(test)]
mod controls;

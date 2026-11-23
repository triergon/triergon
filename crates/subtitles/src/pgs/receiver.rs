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
    Budget, BudgetVec, Bytes, BytesWriter, Error, Refused, Result, Rounding, Time, TimeBase,
    WorkBudget,
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
pub struct Limits {
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

impl Default for Limits {
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

impl Limits {
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
pub struct Options {
    /// Hide every rectangle without the forced flag (`forced_subs_only`): it
    /// keeps its pixels and takes an all-zero palette.
    pub forced_only: bool,
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
pub struct Rect {
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

impl Rect {
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
pub struct Subtitle {
    /// Presentation time in microseconds, ties away from zero: the PTS of the
    /// packet whose PCS this display is the first to present (zero before
    /// any PCS), else this packet's own; `None` when that PTS is unknown.
    pub pts_microseconds: Option<i64>,
    rectangles: BudgetVec<Rect>,
}
impl Subtitle {
    /// The rectangles in composition-object order, at most two; none for a
    /// display that clears the screen.
    pub fn rectangles(&self) -> &[Rect] {
        self.rectangles.as_slice()
    }
}

/// One decoded packet. Segments after its first display still update the
/// decoder's caches and `canvas`.
#[derive(Debug)]
pub struct ReceiverPacket {
    raw: Bytes,
    /// The clock the packet was pushed with.
    pub clock: PacketClock,
    event: Option<Subtitle>,
    refusal: Option<Refusal>,
    /// Canvas width and height after the packet: the size of the latest PCS
    /// the receiver accepted, `[0, 0]` before one or after a PCS whose size
    /// it rejects.
    pub canvas: [u16; 2],
}

impl ReceiverPacket {
    /// The packet as pushed, unchanged.
    pub fn raw_packet(&self) -> &Bytes {
        &self.raw
    }
    /// The subtitle the reference returns for this packet.
    pub fn subtitle(&self) -> Option<&Subtitle> {
        self.event.as_ref()
    }
    /// Set when the reference's decode call fails on this packet.
    pub fn refusal(&self) -> Option<Refusal> {
        self.refusal
    }
}

/// What [`ReceiverDecoder::poll`] reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
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

/// What the decoder is doing.
// The packet is stepped in place; boxing it would allocate outside the pool.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
enum State {
    /// No packet is active.
    Idle,
    /// A packet was pushed and its output is not yet taken.
    Packet(Packet),
    /// The input has ended; `polled` once the end is paid for.
    End { polled: bool },
    /// An error other than pool pressure, returned until reset.
    Failed(Error),
}

impl State {
    /// The decoded packet, leaving the decoder idle; `None`, changing
    /// nothing, unless a packet is decoded.
    fn take_ready(&mut self) -> Option<Packet> {
        if !matches!(
            self,
            State::Packet(Packet {
                step: Step::Ready,
                ..
            })
        ) {
            return None;
        }
        match std::mem::replace(self, State::Idle) {
            State::Packet(packet) => Some(packet),
            state => {
                *self = state;
                None
            }
        }
    }
}

/// The active packet and how far its decoding got.
#[derive(Debug)]
struct Packet {
    raw: Bytes,
    clock: PacketClock,
    /// Where the next segment starts.
    at: usize,
    /// The PTS of the subtitle this packet returns, as the reference fills it:
    /// the packet's own, then the presentation PTS of the display it shows,
    /// or zero once a display failed.
    subtitle_pts: Option<i64>,
    refusal: Option<Refusal>,
    step: Step,
    display: Display,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    /// The segment at `at`.
    Segment,
    /// After an END segment: the packet's display, as far as it got.
    Display,
    /// Every segment the receiver reads is read.
    Finish,
    /// The packet is decoded.
    Ready,
}

/// The first display of a packet, which the reference returns.
#[derive(Debug)]
enum Display {
    /// No END has been displayed.
    Pending,
    /// The packet's subtitle, which an END started; `drawing` while its
    /// rectangles are placed.
    Shown {
        subtitle: Subtitle,
        drawing: Option<Drawing>,
    },
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

/// What placing a display's rectangles needs besides the rectangles.
#[derive(Debug)]
struct Drawing {
    /// The words of the palette the composition selects, which every
    /// rectangle takes that forced-only output does not hide.
    palette: ReceiverPalette,
    /// The pixels the display has decoded.
    pixels: usize,
    /// The rectangle whose object is decoded next, once its reference is
    /// placed.
    raster: Option<Raster>,
}

/// A reference whose object has data, and where the object is in the cache.
#[derive(Clone, Copy, Debug)]
struct Raster {
    reference: Reference,
    object: usize,
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

/// What the decoder keeps from packet to packet: its bounds, the stream state
/// its segments build and the work paid.
#[derive(Debug)]
struct Core {
    budget: Budget,
    limits: Limits,
    options: Options,
    receiver: Receiver,
    /// Work paid towards the current step; cleared when a step is done.
    credit: usize,
    spent: usize,
    planes: Planes,
}

/// FFmpeg's PGS decoder over one stream's packets.
///
/// One packet is active at a time. Accepted inputs, caches and outputs stay
/// charged to the supplied pool; work is cumulative until reset and every
/// operation receives caller cancellation.
#[derive(Debug)]
pub struct ReceiverDecoder {
    core: Core,
    state: State,
}

impl ReceiverDecoder {
    /// A decoder charging `budget`, with no stream state.
    ///
    /// # Errors
    ///
    /// `Error::Limit` when `limits` allows more than 64 objects, 8 palettes
    /// or 255 composition objects.
    pub fn new(budget: &Budget, limits: Limits, options: Options) -> Result<Self> {
        limits.validate()?;
        Ok(Self {
            core: Core {
                budget: budget.clone(),
                limits,
                options,
                receiver: Receiver::new(budget),
                credit: 0,
                spent: 0,
                planes: Planes::None,
            },
            state: State::Idle,
        })
    }

    /// Makes `packet` the active packet with its `clock`. Returns the bytes
    /// unchanged if the decoder cannot accept them.
    ///
    /// # Errors
    ///
    /// `Error::Invalid` for an empty packet, while a packet is
    /// active or its output not taken, and after
    /// [`finish_input`](Self::finish_input); `Error::ForeignPool` for bytes
    /// of another budget; `Error::Limit` beyond
    /// [`Limits::max_packet_bytes`]. `Error::Cancelled` when `cancel`
    /// is set, which fails the decoder until [`reset`](Self::reset); a failed
    /// decoder returns its failure.
    pub fn push(
        &mut self,
        packet: Bytes,
        clock: &PacketClock,
        cancel: &AtomicBool,
    ) -> std::result::Result<(), Refused<Bytes>> {
        if let State::Failed(error) = self.state {
            return Err(Refused {
                error,
                value: packet,
            });
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(Refused {
                error: self.poison(Error::Cancelled),
                value: packet,
            });
        }
        if !matches!(self.state, State::Idle) {
            return Err(Refused {
                error: Error::Invalid(
                    "PGS packet pushed while one is active or after the end of input",
                ),
                value: packet,
            });
        }
        if let Err(error) = self.core.admit(&packet) {
            return Err(Refused {
                error,
                value: packet,
            });
        }
        self.state = State::Packet(Packet {
            raw: packet,
            clock: *clock,
            at: 0,
            subtitle_pts: state::microseconds(*clock),
            refusal: None,
            step: Step::Segment,
            display: Display::Pending,
        });
        self.core.credit = 0;
        Ok(())
    }

    /// Work units spent since the decoder was made or last reset.
    pub fn work_spent(&self) -> usize {
        self.core.spent
    }

    fn poison(&mut self, error: Error) -> Error {
        self.core.receiver = Receiver::new(&self.core.budget);
        self.core.planes = Planes::None;
        self.state = State::Failed(error);
        error
    }

    /// Decodes the active packet, or the end of input, as far as `work`
    /// allows, and reports what the decoder waits for.
    ///
    /// # Errors
    ///
    /// `Error::Backpressure` when the pool refuses memory: the call can be
    /// repeated, and work already paid is not charged again. Any other error,
    /// `Error::Cancelled` and the `Error::Limit` of [`Limits`]
    /// included, fails the decoder: every call returns it until
    /// [`reset`](Self::reset).
    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Status> {
        if let State::Failed(error) = self.state {
            return Err(error);
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(self.poison(Error::Cancelled));
        }
        match &self.state {
            State::Packet(Packet {
                step: Step::Ready, ..
            }) => return Ok(Status::OutputReady),
            State::Idle => return Ok(Status::NeedPacket),
            State::End { polled: true } => return Ok(Status::Ended),
            _ if work.remaining() == 0 => return Ok(Status::Yield),
            _ => {}
        }
        match self.run(work, cancel) {
            Ok(status) => Ok(status),
            Err(error @ Error::Backpressure { .. }) => Err(error),
            Err(error) => Err(self.poison(error)),
        }
    }

    /// The decode loop: every step of a packet, until it is ready or the
    /// caller's work runs out.
    fn run(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Status> {
        let Self { core, state } = self;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if work.remaining() == 0 {
                return Ok(Status::Yield);
            }
            let done = match state {
                State::Packet(packet) => match packet.step {
                    Step::Segment => core.segment(packet, work, cancel)?,
                    Step::Display => core.display(packet, work, cancel)?,
                    Step::Finish => {
                        let done = core.pay(work, cancel, 1)?;
                        if done {
                            packet.step = Step::Ready;
                        }
                        done
                    }
                    Step::Ready => return Ok(Status::OutputReady),
                },
                // The receiver holds no delayed subtitle: the end outputs nothing.
                State::End { polled: true } => return Ok(Status::Ended),
                State::End { polled } => {
                    let done = core.pay(work, cancel, 1)?;
                    if done {
                        *polled = true;
                    }
                    done
                }
                State::Idle => return Ok(Status::NeedPacket),
                State::Failed(error) => return Err(*error),
            };
            if !done {
                return Ok(Status::Yield);
            }
            core.credit = 0;
        }
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
        if let State::Failed(error) = self.state {
            return Err(error);
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(self.poison(Error::Cancelled));
        }
        let Some(packet) = self.state.take_ready() else {
            return Err(Error::Invalid("PGS output not ready"));
        };
        self.core.credit = 0;
        Ok(ReceiverPacket {
            raw: packet.raw,
            clock: packet.clock,
            event: match packet.display {
                Display::Shown { subtitle, .. } => Some(subtitle),
                Display::Pending => None,
            },
            refusal: packet.refusal,
            canvas: self.core.receiver.canvas,
        })
    }

    /// Ends the input; [`poll`](Self::poll) then reaches `Ended`.
    ///
    /// # Errors
    ///
    /// `Error::Invalid` while a packet is active or its output not taken, and
    /// once input has ended; `Error::Cancelled` when `cancel` is set, which
    /// fails the decoder; a failed decoder returns its failure.
    pub fn finish_input(&mut self, cancel: &AtomicBool) -> Result<()> {
        if let State::Failed(error) = self.state {
            return Err(error);
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(self.poison(Error::Cancelled));
        }
        if !matches!(self.state, State::Idle) {
            return Err(Error::Invalid("PGS input ended while a packet is active"));
        }
        self.state = State::End { polled: false };
        self.core.credit = 0;
        Ok(())
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
        if !matches!(
            self.state,
            State::Failed(_) | State::Idle | State::End { polled: true }
        ) {
            return Err(Error::Invalid("PGS decoder reset while a packet is active"));
        }
        let core = &mut self.core;
        core.planes = Planes::None;
        core.receiver = Receiver::new(&core.budget);
        core.spent = 0;
        core.credit = 0;
        self.state = State::Idle;
        Ok(())
    }
}

impl Core {
    /// Checks a packet before it is taken.
    fn admit(&self, packet: &Bytes) -> Result<()> {
        if !packet.belongs_to(&self.budget) {
            return Err(Error::ForeignPool);
        }
        if packet.is_empty() {
            return Err(Error::Invalid("empty PGS packet"));
        }
        if packet.len() > self.limits.max_packet_bytes {
            return Err(Error::Limit {
                requested: packet.len(),
                maximum: self.limits.max_packet_bytes,
            });
        }
        Ok(())
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

    /// The segment at `at`: its header (the length read past the packet end as
    /// zero), then its effect on the receiver. A segment running past the
    /// packet ends the packet, except END, which has no body to read.
    fn segment(
        &mut self,
        packet: &mut Packet,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        let length = packet.raw.len();
        if length < 3 {
            packet.refusal = Some(Refusal::ShortPacket);
            packet.step = Step::Finish;
            return Ok(true);
        }
        if packet.at >= length {
            packet.step = Step::Finish;
            return Ok(true);
        }
        let raw = &packet.raw;
        let at = packet.at;
        let byte = |at: usize| raw.get(at).copied().unwrap_or(0);
        let kind = byte(at);
        let body = at + 3..at + 3 + usize::from(u16::from_be_bytes([byte(at + 1), byte(at + 2)]));
        if body.end > length && kind != END {
            packet.step = Step::Finish;
            return Ok(true);
        }
        // Fixed when first computed, so a retry after pressure pays nothing
        // twice; the cache lookups a segment makes are bounded by the limits.
        let cost = body.end.min(length) - at + 1;
        if !self.pay(work, cancel, cost)? {
            return Ok(false);
        }
        let (receiver, limits) = (&mut self.receiver, &self.limits);
        match kind {
            COMPOSITION => receiver.composition(raw, body.clone(), packet.subtitle_pts, limits)?,
            PALETTE => receiver.palette_definition(raw, body.clone(), limits, &self.budget)?,
            OBJECT => {
                packet.refusal =
                    receiver.object_fragment(&raw[body.clone()], limits, &self.budget)?;
            }
            _ => {}
        }
        packet.at = body.end;
        if packet.refusal.is_some() {
            packet.step = Step::Finish;
        } else if kind == END {
            packet.step = Step::Display;
        }
        Ok(true)
    }

    /// END: the reference returns the first display of a packet and ignores
    /// later ones; the display is drawn one rectangle at a time.
    fn display(
        &mut self,
        packet: &mut Packet,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        let Display::Shown { subtitle, drawing } = &mut packet.display else {
            return self.start_display(packet, work, cancel);
        };
        let Some(draw) = drawing else {
            let done = self.pay(work, cancel, 1)?;
            if done {
                packet.step = Step::Segment;
            }
            return Ok(done);
        };
        if let Some(raster) = draw.raster {
            return self.raster(&packet.raw, subtitle, draw, raster, work, cancel);
        }
        let index = subtitle.rectangles.len();
        let Some(reference) = self.receiver.references.get(index).copied().flatten() else {
            // Every reference is placed: the rectangles' pixels are sliced
            // from the planes buffer.
            if !self.pay(work, cancel, 1)? {
                return Ok(false);
            }
            self.store(subtitle)?;
            *drawing = None;
            packet.step = Step::Segment;
            return Ok(true);
        };
        self.rectangle(&packet.raw, subtitle, draw, reference, work, cancel)
    }

    /// The subtitle an END starts, with room for a rectangle per reference. A
    /// display that needs the composition's palette and lacks it returns
    /// nothing and zeroes the packet's PTS; a later END tries again.
    fn start_display(
        &mut self,
        packet: &mut Packet,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        if !self.pay(work, cancel, self.receiver.cache_entries() + 1)? {
            return Ok(false);
        }
        let references = self.receiver.references().count();
        let palette = if references == 0 {
            ReceiverPalette::Hidden
        } else if let Some(words) = self.receiver.palette() {
            ReceiverPalette::Owned(Arc::clone(words))
        } else {
            self.receiver.presentation = None;
            packet.subtitle_pts = Some(0);
            packet.step = Step::Segment;
            return Ok(true);
        };
        let mut rectangles = BudgetVec::new(&self.budget);
        rectangles.try_reserve_exact(references)?;
        let pts = self.receiver.presentation.or(packet.subtitle_pts);
        packet.subtitle_pts = pts;
        self.receiver.presentation = None;
        packet.display = Display::Shown {
            subtitle: Subtitle {
                pts_microseconds: pts,
                rectangles,
            },
            drawing: Some(Drawing {
                palette,
                pixels: 0,
                raster: None,
            }),
        };
        Ok(true)
    }

    /// Places `reference`: a missing object is an empty rectangle at the
    /// origin, an object without data an empty one at its position with no
    /// row width; an object with data is decoded next, within the pixel
    /// limits for one object and for the display.
    fn rectangle(
        &mut self,
        raw: &Bytes,
        subtitle: &mut Subtitle,
        draw: &mut Drawing,
        reference: Reference,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        if !self.pay(work, cancel, self.receiver.objects.len() + 1)? {
            return Ok(false);
        }
        let Some((object, found)) = self.receiver.object(reference.object.id) else {
            let empty = raw.slice(0..0)?;
            self.push_rect(
                subtitle,
                draw,
                reference,
                Placement::Missing,
                Bitmap::empty(0, empty),
            )?;
            return Ok(true);
        };
        if !found.has_data() {
            let empty = raw.slice(0..0)?;
            self.push_rect(
                subtitle,
                draw,
                reference,
                Placement::Object,
                Bitmap::empty(0, empty),
            )?;
            return Ok(true);
        }
        let area = usize::from(found.width) * usize::from(found.height);
        let total = draw.pixels.checked_add(area).ok_or(Error::Overflow)?;
        for (requested, maximum) in [
            (area, self.limits.max_object_pixels),
            (total, self.limits.max_display_pixels),
        ] {
            if requested > maximum {
                return Err(Error::Limit { requested, maximum });
            }
        }
        draw.raster = Some(Raster { reference, object });
        Ok(true)
    }

    /// Decodes the object of the rectangle being placed.
    fn raster(
        &mut self,
        raw: &Bytes,
        subtitle: &mut Subtitle,
        draw: &mut Drawing,
        Raster { reference, object }: Raster,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        // No segment is read while a display is drawn: the object stays where
        // the rectangle found it.
        let object = &self.receiver.objects[object];
        let (width, height) = (object.width, object.height);
        let area = usize::from(width) * usize::from(height);
        let paid = Work {
            caller: work,
            spent: &mut self.spent,
            maximum: self.limits.max_work,
            cancel,
        }
        .pay(&mut self.credit, object.encoded.len() + area)?;
        if !paid {
            return Ok(false);
        }
        // A rectangle without its own pixels views none of the packet.
        let empty = || raw.slice(0..0);
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
        draw.pixels += area;
        draw.raster = None;
        self.push_rect(subtitle, draw, reference, Placement::Object, bitmap)?;
        Ok(true)
    }

    /// Appends a rectangle. One for a missing object sits at the origin and
    /// carries no forced flag; forced-only output still selects its palette
    /// by the reference's flag.
    fn push_rect(
        &self,
        subtitle: &mut Subtitle,
        draw: &Drawing,
        reference: Reference,
        placement: Placement,
        bitmap: Bitmap,
    ) -> Result<()> {
        let ((x, y), forced) = match placement {
            Placement::Object => ((reference.x, reference.y), reference.object.forced()),
            Placement::Missing => ((0, 0), false),
        };
        let palette = if self.options.forced_only && !reference.object.forced() {
            ReceiverPalette::Hidden
        } else {
            draw.palette.clone()
        };
        subtitle
            .rectangles
            .try_push(Rect {
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

    /// Slices the decoded rectangles' pixels from the planes buffer they
    /// were written to.
    fn store(&mut self, subtitle: &mut Subtitle) -> Result<()> {
        match std::mem::replace(&mut self.planes, Planes::None) {
            Planes::Writing(planes, _) => {
                // Each decoded rectangle takes the next width * height bytes of
                // the buffer; the others have no pixels.
                let planes = planes.finish();
                let mut at = 0;
                for rect in subtitle.rectangles.as_mut_slice() {
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
        Ok(())
    }
}

#[cfg(test)]
mod controls;

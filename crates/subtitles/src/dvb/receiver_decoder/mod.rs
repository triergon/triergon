//! The DVB subtitle decoder as FFmpeg's `dvbsub` decoder presents it: one call
//! per codec packet, at most one subtitle out per call.
//!
//! A display set is presented at its end of display set segment, or at the
//! end of a packet that held a page, a region and an object segment but no
//! end of display set. With [`Options::end_from_next`] it is presented at the
//! next page composition instead, ending where that one starts. Rectangles
//! follow the page's region list in reverse, for regions an object has been
//! drawn into; each carries the region's index plane and the palette of its
//! CLUT, or the [automatic palette](AutomaticPalette) the options select.
//!
//! The receiver decodes the segments of every page unless a service is
//! selected ([`Options::service`]). An object it cannot draw, a short region
//! composition or display definition, a region without area or above
//! 1280 × 1024 pixels, or a segment longer than the packet stops the packet:
//! the rest of it is not decoded, and the display set is not presented at
//! its end.
mod epoch;
mod memory;
mod palette;
mod pixels;
mod segment;

use crate::dvb::{Depth, Size};
use epoch::{Epoch, Region, RegionHoldings};
use palette::{Census, DEFAULT};
use segment::{
    ClutDefinition, DisplayDefinition, Kind, ObjectData, PageComposition, Placement,
    RegionComposition, Segment, Segments,
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, BudgetVec, Error, Result, WorkBudget};

/// When a region's palette is replaced by the automatic one (`compute_clut`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AutomaticPalette {
    /// Never: regions keep their CLUT, or the default CLUT.
    Never,
    /// For regions whose CLUT was never defined (`compute_clut` −1).
    #[default]
    WithoutClut,
    /// For every region.
    Always,
    /// For regions whose CLUT was never defined, computed when such a region
    /// is first presented and kept while the region lasts: drawing, filling,
    /// resizing or changing its depth leaves the palette as it was, and only
    /// a new epoch (an acquisition point or a mode change) starts it afresh
    /// (`compute_clut` −2).
    WithoutClutOnce,
}

/// The decoder options that change its output.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Options {
    /// Present each display set at the next page composition, ending where
    /// that one starts (`compute_edt`).
    pub end_from_next: bool,
    pub automatic_palette: AutomaticPalette,
    /// Decode only the composition and ancillary pages of this service of the
    /// stream configuration, or of its first service when it has fewer
    /// (`dvb_substream`). `None` decodes every page.
    pub service: Option<usize>,
}

/// What a [`ReceiverDecoder::decode`] call did.
#[derive(Clone, Copy, Debug)]
pub enum Status<'a> {
    /// The packet is decoded and presented this subtitle.
    Subtitle(Subtitle<'a>),
    /// The packet is decoded and presented nothing.
    Nothing,
    /// The work budget ran out before the packet's next segment, which is
    /// not decoded yet. The decoder keeps the packet, and the work left over
    /// counts towards its next segment:
    /// [`resume`](ReceiverDecoder::resume) or
    /// [`abandon`](ReceiverDecoder::abandon) it.
    Yield,
    /// The pool refused memory a segment needed after the packet's check
    /// admitted it: another user of the pool took it in between. The segments
    /// before it stay decoded and the decoder keeps the packet from that
    /// segment on, as for [`Yield`](Self::Yield): resume it once the pool has
    /// room, or abandon it.
    Stalled(Error),
}

impl<'a> Status<'a> {
    /// The presented subtitle, if any.
    pub fn subtitle(self) -> Option<Subtitle<'a>> {
        match self {
            Self::Subtitle(subtitle) => Some(subtitle),
            Self::Nothing | Self::Yield | Self::Stalled(_) => None,
        }
    }
}

/// One presented subtitle; valid until the next call.
#[derive(Clone, Copy, Debug)]
pub struct Subtitle<'a> {
    /// Microseconds; `None` when unknown.
    pub pts: Option<i64>,
    /// Milliseconds after `pts`. With [`Options::end_from_next`] the end is
    /// one millisecond before the next display set, `u32::MAX` when that
    /// starts within the same millisecond.
    pub start_display_ms: u32,
    pub end_display_ms: u32,
    /// The display size rectangle positions refer to, as the receiver keeps
    /// it: the first display definition's, or 720 × 576 once a packet held a
    /// page, a region and an object segment without one; `None` before.
    pub display_size: Option<Size>,
    slots: &'a [Slot],
    epoch: &'a Epoch,
}

impl<'a> Subtitle<'a> {
    /// The rectangles, bottom of the page's region list first.
    pub fn rects(self) -> impl ExactSizeIterator<Item = Rect<'a>> {
        let epoch = self.epoch;
        self.slots.iter().map(move |slot| Rect {
            x: slot.x,
            y: slot.y,
            width: slot.width,
            height: slot.height,
            palette: &slot.palette[..1 << slot.depth.bits()],
            colours: &slot.palette,
            pixels: match slot.region {
                Some(index) => epoch.region_at(index).pixels(),
                None => slot.pixels.as_slice(),
            },
        })
    }
}

/// One region's bitmap on the page.
#[derive(Clone, Copy, Debug)]
pub struct Rect<'a> {
    pub x: i32,
    pub y: i32,
    pub width: u16,
    pub height: u16,
    /// ARGB words, alpha in the top byte; `1 << depth` of them.
    pub palette: &'a [u32],
    /// The colour of every pixel value, `palette` first. A region recomposed
    /// at a smaller depth without a fill keeps pixel values past `palette`;
    /// as in the reference, they are transparent, or under the automatic
    /// palette take the levels it ranked them at.
    pub colours: &'a [u32; 256],
    /// `width * height` indices into `colours`, row by row.
    pub pixels: &'a [u8],
}

/// A presented rectangle. Its pixels stay in the region until a later
/// segment of the same packet changes the epoch; they are copied then.
#[derive(Debug)]
struct Slot {
    x: i32,
    y: i32,
    width: u16,
    height: u16,
    depth: Depth,
    palette: [u32; 256],
    /// The epoch region holding the pixels, or `None` once copied.
    region: Option<usize>,
    pixels: BudgetVec<u8>,
}

/// The pages whose segments are decoded.
#[derive(Clone, Copy, Debug)]
enum Pages {
    All,
    Service { composition: u16, ancillary: u16 },
}

impl Pages {
    /// The configuration holds five bytes per service: composition page,
    /// ancillary page, subtitling type.
    fn select(configuration: &[u8], service: Option<usize>) -> Self {
        let Some(service) = service else {
            return Self::All;
        };
        let at = service
            .checked_mul(5)
            .filter(|&at| {
                configuration
                    .len()
                    .checked_sub(5)
                    .is_some_and(|last| at <= last)
            })
            .unwrap_or(0);
        match configuration.get(at..at + 4) {
            Some(&[c0, c1, a0, a1]) => Self::Service {
                composition: u16::from_be_bytes([c0, c1]),
                ancillary: u16::from_be_bytes([a0, a1]),
            },
            _ => Self::All,
        }
    }

    fn admit(self, page: u16) -> bool {
        match self {
            Self::All => true,
            Self::Service {
                composition,
                ancillary,
            } => page == composition || page == ancillary,
        }
    }
}

/// A packet whose decoding stopped before a segment, to continue there. Its
/// bytes are in [`ReceiverDecoder::held_packet`].
#[derive(Clone, Copy, Debug)]
struct Suspended {
    pts: Option<i64>,
    /// Where the next segment starts; the packet length once only the
    /// end-of-packet presentation remains.
    offset: usize,
}

/// How far a call decoded its packet.
#[derive(Clone, Copy, Debug)]
enum Progress {
    Done,
    /// Stopped before the segment at `at` (the packet length for the
    /// end-of-packet presentation). `paid` is the work charged for that
    /// segment, which it pays for again when resumed.
    Halted {
        at: usize,
        why: Halt,
        paid: u64,
    },
}

#[derive(Clone, Copy, Debug)]
enum Halt {
    /// The work ran out.
    Yield,
    Cancelled,
    /// The segment's memory was refused.
    Refused(Error),
}

/// A segment of a selected page as the receiver reads it; `None` where it
/// cannot be read.
#[derive(Clone, Copy, Debug)]
enum Parsed<'a> {
    Page(PageComposition<'a>),
    Region(Option<RegionComposition<'a>>),
    Clut(Option<ClutDefinition<'a>>),
    Object(Option<ObjectData<'a>>),
    Display(Option<DisplayDefinition>),
    End,
    Other,
}

impl<'a> Parsed<'a> {
    fn of(segment: &Segment<'a>) -> Self {
        match segment.kind {
            Kind::PageComposition => Self::Page(PageComposition::parse(segment)),
            Kind::RegionComposition => Self::Region(RegionComposition::parse(segment.body)),
            Kind::ClutDefinition => Self::Clut(ClutDefinition::parse(segment)),
            Kind::ObjectData => Self::Object(ObjectData::parse(segment.body).ok()),
            Kind::DisplayDefinition => Self::Display(DisplayDefinition::parse(segment.body)),
            Kind::EndOfDisplaySet => Self::End,
            Kind::Other => Self::Other,
        }
    }

    /// Whether the segment may change regions a presented subtitle shows.
    fn changes_epoch(&self) -> bool {
        matches!(self, Self::Page(_) | Self::Region(_) | Self::Object(_))
    }
}

/// Whether a packet's remaining segments are decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Flow {
    Continue,
    Stop,
}

/// The page: its version, time-out and region placements.
#[derive(Debug)]
struct Page {
    version: Option<u8>,
    timeout: u8,
    placements: BudgetVec<Placement>,
    /// The last display definition, whose window offsets the rectangles.
    display: Option<DisplayDefinition>,
}

/// The display size before any display definition.
const DEFAULT_DISPLAY: Size = Size {
    width: 720,
    height: 576,
};

/// The subtitle of the current call: its clock and the rectangles in use.
#[derive(Clone, Copy, Debug)]
struct Presented {
    pts: Option<i64>,
    end_display_ms: u32,
    rects: usize,
    /// The subtitle is output; one built only for its automatic palettes is
    /// not.
    output: bool,
}

/// The segment kinds a packet held, for the end-of-packet presentation.
#[derive(Clone, Copy, Debug, Default)]
struct Held {
    page: bool,
    region: bool,
    object: bool,
}

impl Held {
    fn display_set(self) -> bool {
        self.page && self.region && self.object
    }
}

/// The decoder. Memory is charged to the budget it was created with.
#[derive(Debug)]
pub struct ReceiverDecoder {
    options: Options,
    pages: Pages,
    epoch: Epoch,
    page: Page,
    /// The time of the last packet decoded without a stop.
    previous_pts: Option<i64>,
    display_size: Option<Size>,
    /// The segment kinds the current packet held.
    held: Held,
    census: Census,
    /// Output rectangles, reused across calls.
    slots: BudgetVec<Slot>,
    presented: Option<Presented>,
    /// Where the packet that yielded, was cancelled or refused continues.
    suspended: Option<Suspended>,
    /// The suspended packet's bytes. Its capacity is reserved for each packet
    /// before the packet changes anything, so keeping a packet cannot fail.
    held_packet: BudgetVec<u8>,
    /// The regions a packet's memory plan changes.
    planned: BudgetVec<RegionHoldings>,
    /// Work granted to calls on the current packet and left unspent.
    credit: u64,
    /// The work of the current packet's memory plan, charged before its
    /// first segment.
    owed: u64,
    budget: Budget,
}

impl ReceiverDecoder {
    /// `configuration` is the stream's codec configuration (the services'
    /// page ids), read when [`Options::service`] selects one.
    pub fn new(budget: &Budget, configuration: &[u8], options: Options) -> Self {
        Self {
            options,
            pages: Pages::select(configuration, options.service),
            epoch: Epoch::new(
                budget,
                options.automatic_palette == AutomaticPalette::WithoutClutOnce,
            ),
            page: Page {
                version: None,
                timeout: 0,
                placements: BudgetVec::new(budget),
                display: None,
            },
            previous_pts: None,
            display_size: None,
            held: Held::default(),
            census: Census::new(budget),
            slots: BudgetVec::new(budget),
            presented: None,
            suspended: None,
            held_packet: BudgetVec::new(budget),
            planned: BudgetVec::new(budget),
            credit: 0,
            owed: 0,
            budget: budget.clone(),
        }
    }

    /// Decode one codec packet with its presentation time in microseconds.
    /// A packet of a segment header or less, or one that does not start with
    /// a segment, is refused (`Error::Invalid`) and changes nothing.
    ///
    /// The memory the packet's segments may allocate, and room to keep the
    /// packet, are checked against the pool before any segment is decoded: a
    /// packet the pool cannot hold now is refused (`Error::Backpressure`),
    /// one it could never hold beside what the decoder holds
    /// (`Error::Limit`), and either changes nothing but the work of the
    /// check. Each segment's work, one unit per byte, per
    /// object placement it visits and per pixel it may draw, copy or present,
    /// is charged to `work` before the segment is decoded, and the check's
    /// own work before the first. When the budget cannot pay for the next one,
    /// the call yields ([`Status::Yield`]) before it, and the decoder keeps
    /// the rest of the packet: [`resume`](Self::resume) it, and the work left
    /// over counts towards the next segment, or [`abandon`](Self::abandon) it.
    /// Cancellation is checked before each segment and each object placement,
    /// and a cancelled packet is kept the same way, as is one whose memory
    /// another user of the pool took after the check ([`Status::Stalled`]).
    /// `decode` refuses another packet while one is kept.
    ///
    /// The leftover work of a yielded call is banked whole, and a segment is
    /// charged at once from what is banked: a DVB segment's work is bounded
    /// by its region, unlike a DVD control sequence's, which the DVD receiver
    /// pays for in parts.
    pub fn decode(
        &mut self,
        packet: &[u8],
        pts: Option<i64>,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Status<'_>> {
        if self.suspended.is_some() {
            return Err(Error::Invalid(
                "DVB receiver: resume or abandon the kept packet",
            ));
        }
        if packet.len() <= 6 || packet[0] != 0x0f {
            return Err(Error::Invalid("DVB subtitle packet"));
        }
        self.presented = None;
        self.held = Held::default();
        let plan = self.packet_memory(packet)?;
        // Room to keep the packet, while what the copy replaces is held.
        let kept = if packet.len() > self.held_packet.capacity() {
            packet.len()
        } else {
            0
        };
        let requested = plan.bytes.saturating_add(kept);
        let available = self.budget.available();
        let refusal = if requested > available {
            // Beside what the decoder holds, a packet may never fit even
            // with the pool otherwise empty: it is not worth retrying.
            let maximum = (self.budget.limit() - Budget::control_storage_bytes())
                .saturating_sub(self.retained());
            Some(if requested > maximum {
                Error::Limit { requested, maximum }
            } else {
                Error::Backpressure {
                    requested,
                    available,
                }
            })
        } else {
            self.held_packet.clear();
            self.held_packet.try_reserve_exact(packet.len()).err()
        };
        // The check's work is spent whether or not the packet is admitted.
        if let Some(error) = refusal {
            work.take(plan.work);
            return Err(error);
        }
        self.credit = 0;
        self.owed = plan.work;
        self.step(Some(packet), 0, pts, work, cancel)
    }

    /// Continue the kept packet before its next segment.
    pub fn resume(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Status<'_>> {
        let Suspended { pts, offset } = self
            .suspended
            .take()
            .ok_or(Error::Invalid("DVB receiver: no kept packet"))?;
        self.step(None, offset, pts, work, cancel)
    }

    /// Whether a packet is kept for [`resume`](Self::resume).
    pub fn has_kept_packet(&self) -> bool {
        self.suspended.is_some()
    }

    /// Whether the kept packet is `packet` with time `pts`: for a caller that
    /// offers each packet until it is decoded, to resume it rather than
    /// decode another.
    pub fn keeps(&self, packet: &[u8], pts: Option<i64>) -> bool {
        self.suspended
            .is_some_and(|kept| kept.pts == pts && self.held_packet.as_slice() == packet)
    }

    /// Drop the rest of the kept packet. The segments decoded before it
    /// stopped stay decoded.
    pub fn abandon(&mut self) {
        self.suspended = None;
        self.credit = 0;
        self.owed = 0;
    }

    /// Run `packet`, or the kept packet, from `start`, and keep it if it
    /// stops before its end.
    fn step(
        &mut self,
        packet: Option<&[u8]>,
        start: usize,
        pts: Option<i64>,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Status<'_>> {
        let granted = work.remaining();
        let mut banked = WorkBudget::new(granted.saturating_add(self.credit));
        let progress = if let Some(packet) = packet {
            self.run(packet, start, pts, &mut banked, cancel)
        } else {
            let held = std::mem::replace(&mut self.held_packet, BudgetVec::new(&self.budget));
            let progress = self.run(held.as_slice(), start, pts, &mut banked, cancel);
            self.held_packet = held;
            progress
        };
        let Progress::Halted { at, why, paid } = progress else {
            work.take(banked.spent().saturating_sub(self.credit));
            self.credit = 0;
            self.owed = 0;
            return Ok(self.status());
        };
        // The packet is kept, with the work left over and the work of the
        // segment it stopped at, which that segment is charged again.
        work.take(granted);
        self.credit = banked.remaining().saturating_add(paid);
        if let Some(packet) = packet {
            self.held_packet.try_extend_from_slice(packet)?;
        }
        self.suspended = Some(Suspended { pts, offset: at });
        match why {
            Halt::Yield => Ok(Status::Yield),
            Halt::Cancelled => Err(Error::Cancelled),
            Halt::Refused(error) => Ok(Status::Stalled(error)),
        }
    }

    /// The bytes the decoder holds, all of which stay held while a packet
    /// waits to be decoded.
    fn retained(&self) -> usize {
        fn bytes<T>(vec: &BudgetVec<T>) -> usize {
            vec.capacity() * size_of::<T>()
        }
        let slots: usize = self.slots.iter().map(|slot| slot.pixels.capacity()).sum();
        self.epoch.retained()
            + bytes(&self.page.placements)
            + bytes(&self.slots)
            + slots
            + self.census.bytes()
            + bytes(&self.held_packet)
            + bytes(&self.planned)
    }

    /// What the packet presented, once it is decoded.
    fn status(&self) -> Status<'_> {
        match self.presented {
            Some(p) if p.output => Status::Subtitle(Subtitle {
                pts: p.pts,
                start_display_ms: 0,
                end_display_ms: p.end_display_ms,
                display_size: self.display_size,
                slots: &self.slots.as_slice()[..p.rects],
                epoch: &self.epoch,
            }),
            _ => Status::Nothing,
        }
    }

    /// Decode the segments from `start` on, each once `work` pays for it,
    /// then present the packet as the receiver does at its end.
    fn run(
        &mut self,
        packet: &[u8],
        start: usize,
        pts: Option<i64>,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Progress {
        let halt = |at, why, paid| Progress::Halted { at, why, paid };
        if cancel.load(Ordering::Relaxed) {
            return halt(start, Halt::Cancelled, 0);
        }
        if !work.try_charge(self.owed) {
            return halt(start, Halt::Yield, 0);
        }
        self.owed = 0;
        let mut segments = Segments::new(&packet[start..]);
        loop {
            let at = packet.len() - segments.remaining();
            let Some(segment) = segments.next() else {
                break;
            };
            if cancel.load(Ordering::Relaxed) {
                return halt(at, Halt::Cancelled, 0);
            }
            let Ok(segment) = segment else {
                return Progress::Done;
            };
            let parsed = self.pages.admit(segment.page).then(|| Parsed::of(&segment));
            let bytes = 6 + segment.body.len() as u64;
            let cost = bytes + parsed.as_ref().map_or(0, |parsed| self.work(parsed));
            if !work.try_charge(cost) {
                return halt(at, Halt::Yield, 0);
            }
            let Some(parsed) = parsed else {
                continue;
            };
            match self.apply(parsed, pts, cancel) {
                Ok(Flow::Continue) => {}
                Ok(Flow::Stop) => return Progress::Done,
                Err(Error::Cancelled) => return halt(at, Halt::Cancelled, cost),
                Err(error) => return halt(at, Halt::Refused(error), cost),
            }
        }
        let presents = self.held.display_set() && !self.options.end_from_next;
        let cost = if presents { self.present_work().0 } else { 0 };
        if !work.try_charge(cost) {
            return halt(packet.len(), Halt::Yield, 0);
        }
        if self.held.display_set() {
            self.display_size.get_or_insert(DEFAULT_DISPLAY);
        }
        if presents && let Err(error) = self.present(pts, self.timeout_ms(), true) {
            return halt(packet.len(), Halt::Refused(error), cost);
        }
        self.previous_pts = pts;
        Progress::Done
    }

    fn timeout_ms(&self) -> u32 {
        u32::from(self.page.timeout) * 1000
    }

    /// The work of [`apply`](Self::apply) to `segment` in the current state.
    fn work(&self, segment: &Parsed<'_>) -> u64 {
        let detach = if self.presented.is_some() && segment.changes_epoch() {
            self.detach_work()
        } else {
            0
        };
        detach
            + match segment {
                Parsed::Page(page)
                    if self.options.end_from_next && self.page.version != Some(page.version) =>
                {
                    let (palettes, pixels) = self.present_work();
                    palettes + if page.state.starts_epoch() { pixels } else { 0 }
                }
                Parsed::Region(Some(region)) => self.epoch.composition_work(region),
                Parsed::Object(Some(object)) => self.epoch.drawing_work(object),
                Parsed::End if !self.options.end_from_next => self.present_work().0,
                _ => 0,
            }
    }

    fn apply(
        &mut self,
        segment: Parsed<'_>,
        pts: Option<i64>,
        cancel: &AtomicBool,
    ) -> Result<Flow> {
        if self.presented.is_some() && segment.changes_epoch() {
            self.detach()?;
        }
        match segment {
            Parsed::Page(page) => {
                self.held.page = true;
                self.compose_page(&page, pts)?;
            }
            Parsed::Region(region) => {
                let Some(region) = region else {
                    return Ok(Flow::Stop);
                };
                if self.epoch.compose_region(&region)? == Flow::Stop {
                    return Ok(Flow::Stop);
                }
                self.held.region = true;
            }
            Parsed::Clut(clut) => {
                if let Some(clut) = clut {
                    self.epoch.define_clut(&clut)?;
                }
            }
            Parsed::Object(object) => {
                let Some(object) = object else {
                    return Ok(Flow::Stop);
                };
                if self.epoch.draw_object(&object, cancel)? == Flow::Stop {
                    return Ok(Flow::Stop);
                }
                self.held.object = true;
            }
            Parsed::Display(display) => match display {
                // A repeated version changes nothing.
                Some(display)
                    if self
                        .page
                        .display
                        .is_some_and(|d| d.version == display.version) => {}
                Some(display) => {
                    self.display_size.get_or_insert(display.size);
                    self.page.display = Some(display);
                }
                None => return Ok(Flow::Stop),
            },
            Parsed::End if !self.options.end_from_next => {
                self.present(pts, self.timeout_ms(), true)?;
            }
            Parsed::End | Parsed::Other => {}
        }
        Ok(Flow::Continue)
    }

    /// A new page version replaces the region list; at an acquisition point
    /// or mode change it also starts a new epoch. A repeated version, or one
    /// that cannot be allocated, changes nothing.
    fn compose_page(&mut self, page: &PageComposition<'_>, pts: Option<i64>) -> Result<()> {
        if self.page.version == Some(page.version) {
            return Ok(());
        }
        // The reference builds the previous display set, automatic palettes
        // included, and outputs it only when that packet had a time. It
        // subtracts in 64 bits, from its no-time value, `i64::MIN`, when this
        // packet has none.
        if self.options.end_from_next {
            let previous = self.previous_pts;
            let end = previous.map_or(-1, |previous| {
                millis(pts.unwrap_or(i64::MIN).wrapping_sub(previous)) - 1
            });
            // The receiver's display time is 32 bits: a negative end wraps.
            self.present(previous, end as u32, previous.is_some())?;
        }
        let starts_epoch = page.state.starts_epoch();
        if starts_epoch {
            self.detach()?;
        }
        let regions = page.regions().count().min(256);
        self.page
            .placements
            .try_reserve_exact(regions.saturating_sub(self.page.placements.len()))?;
        self.page.version = Some(page.version);
        self.page.timeout = page.timeout;
        if starts_epoch {
            self.epoch.clear();
        }
        self.page.placements.clear();
        // A region listed again keeps its first place.
        let mut listed = [false; 256];
        for placement in page.regions() {
            if !std::mem::replace(&mut listed[usize::from(placement.region)], true) {
                push(&mut self.page.placements, placement)?;
            }
        }
        Ok(())
    }

    /// Whether `region` is presented with the automatic palette.
    fn automatic(&self, region: &Region) -> bool {
        match self.options.automatic_palette {
            AutomaticPalette::Never => false,
            AutomaticPalette::WithoutClut | AutomaticPalette::WithoutClutOnce => {
                self.epoch.tables(region.clut).is_none()
            }
            AutomaticPalette::Always => true,
        }
    }

    /// The work of presenting the page now: the automatic palettes to
    /// compute, and the pixels of its rectangles.
    fn present_work(&self) -> (u64, u64) {
        if self.page.version.is_none() || self.presented.is_some() {
            return (0, 0);
        }
        self.page
            .placements
            .as_slice()
            .iter()
            .filter_map(|placement| self.epoch.drawn_region(placement.region))
            .fold((0, 0), |(palettes, pixels), (_, region)| {
                let palette = if self.automatic(region) {
                    region.automatic_palette_work()
                } else {
                    0
                };
                (palettes + palette, pixels + region.pixels().len() as u64)
            })
    }

    /// Present the page as it stands, unless no page was composed yet or this
    /// packet presented already. Without `output` the subtitle is built, its
    /// automatic palettes computed, but not output.
    fn present(&mut self, pts: Option<i64>, end_display_ms: u32, output: bool) -> Result<()> {
        if self.page.version.is_none() || self.presented.is_some() {
            return Ok(());
        }
        if self.held.display_set() {
            self.display_size.get_or_insert(DEFAULT_DISPLAY);
        }
        let (dx, dy) = self.page.display.map_or((0, 0), |d| d.offset);
        let mut count = 0;
        for placement in self.page.placements.as_slice().iter().rev() {
            let Some((index, region)) = self.epoch.drawn_region(placement.region) else {
                continue;
            };
            if count == self.slots.len() {
                let slot = Slot {
                    x: 0,
                    y: 0,
                    width: 0,
                    height: 0,
                    depth: Depth::Two,
                    palette: [0; 256],
                    region: None,
                    pixels: BudgetVec::new(&self.budget),
                };
                push_exact(&mut self.slots, slot)?;
            }
            let automatic = self.automatic(region);
            let table = self
                .epoch
                .tables(region.clut)
                .unwrap_or(&DEFAULT)
                .get(region.depth);
            let slot = &mut self.slots.as_mut_slice()[count];
            slot.x = i32::from(placement.x) + i32::from(dx);
            slot.y = i32::from(placement.y) + i32::from(dy);
            slot.width = region.width;
            slot.height = region.height;
            slot.depth = region.depth;
            slot.region = Some(index);
            if automatic {
                slot.palette = *self.epoch.automatic_palette(index, &mut self.census)?;
            } else {
                let (defined, rest) = slot.palette.split_at_mut(table.len());
                defined.copy_from_slice(table);
                rest.fill(0);
            }
            count += 1;
        }
        self.presented = Some(Presented {
            pts,
            end_display_ms,
            rects: count,
            output,
        });
        Ok(())
    }

    /// The work of [`detach`](Self::detach): the pixels it copies.
    fn detach_work(&self) -> u64 {
        let count = self.presented.map_or(0, |p| p.rects);
        self.slots.as_slice()[..count]
            .iter()
            .filter_map(|slot| slot.region)
            .map(|index| self.epoch.region_at(index).pixels().len() as u64)
            .sum()
    }

    /// Copy the presented pixels out of the regions before a segment of the
    /// same packet changes them.
    fn detach(&mut self) -> Result<()> {
        let count = self.presented.map_or(0, |p| p.rects);
        for slot in &mut self.slots.as_mut_slice()[..count] {
            if let Some(index) = slot.region {
                slot.pixels.clear();
                slot.pixels
                    .try_extend_from_slice(self.epoch.region_at(index).pixels())?;
                slot.region = None;
            }
        }
        Ok(())
    }
}

fn push<T>(vec: &mut BudgetVec<T>, value: T) -> Result<()> {
    vec.try_push(value).map_err(|e| e.error)
}

/// [`push`] without spare capacity, for the large elements a stream has few
/// of (regions, CLUTs, display slots), as the reference allocates one per use.
fn push_exact<T>(vec: &mut BudgetVec<T>, value: T) -> Result<()> {
    vec.try_reserve_exact(1)?;
    push(vec, value)
}

/// Microseconds to milliseconds, rounded to nearest with ties away from zero.
fn millis(us: i64) -> i64 {
    let us = i128::from(us);
    // Within 64 bits: a thousandth of a 64-bit value.
    ((us + if us < 0 { -500 } else { 500 }) / 1000) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segment(kind: u8, body: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0x0f, kind, 0, 1];
        bytes.extend((body.len() as u16).to_be_bytes());
        bytes.extend(body);
        bytes
    }

    /// A page placing `regions`, a new epoch when `acquisition` is set.
    fn page(version: u8, acquisition: bool, regions: &[u8]) -> Vec<u8> {
        let mut body = vec![5, version << 4 | u8::from(acquisition) << 2];
        for &id in regions {
            body.extend([id, 0, 0, 10, 0, 10]);
        }
        segment(0x10, &body)
    }

    /// A 2-bit region of CLUT `clut`, placing object 1 `objects` times.
    fn region(id: u8, version: u8, width: u16, height: u16, clut: u8, objects: usize) -> Vec<u8> {
        let mut body = vec![id, version << 4];
        body.extend(width.to_be_bytes());
        body.extend(height.to_be_bytes());
        body.extend([1 << 5 | 1 << 2, clut, 0, 0]);
        for _ in 0..objects {
            body.extend([0, 1, 0, 0, 0, 0]);
        }
        segment(0x11, &body)
    }

    fn object() -> Vec<u8> {
        pixels(0x6d)
    }

    /// Object 1, one line of four 2-bit pixels coded in `codes` (two bits
    /// each, the leftmost first), repeated for the bottom field.
    fn pixels(codes: u8) -> Vec<u8> {
        segment(0x13, &[0, 1, 0, 0, 4, 0, 0, 0x10, codes, 0x00, 0xf0])
    }

    fn clut(id: u8) -> Vec<u8> {
        segment(0x12, &[id, 0, 1, 0x41, 9, 9, 9, 9])
    }

    fn end() -> Vec<u8> {
        segment(0x80, &[])
    }

    /// What the decoder counts as held is what it holds in its pool, through
    /// regions growing and shrinking, CLUTs, presentations with and without
    /// automatic palettes, a page presented from the next one, and a new
    /// epoch.
    #[test]
    fn retained_bytes_are_the_bytes_held() {
        let packets = [
            [
                page(0, true, &[0]),
                region(0, 0, 64, 4, 0, 1),
                object(),
                end(),
            ]
            .concat(),
            [
                page(1, false, &[0, 1]),
                region(1, 0, 320, 20, 2, 3),
                clut(2),
                object(),
                end(),
            ]
            .concat(),
            [
                page(2, false, &[1, 0]),
                region(0, 1, 128, 8, 0, 2),
                object(),
                end(),
            ]
            .concat(),
            [
                page(3, true, &[2]),
                region(2, 0, 16, 2, 0, 1),
                object(),
                end(),
            ]
            .concat(),
        ];
        for options in [
            Options::default(),
            Options {
                end_from_next: true,
                automatic_palette: AutomaticPalette::Always,
                service: None,
            },
        ] {
            let pool = Budget::new(1 << 22).unwrap();
            let cancel = AtomicBool::new(false);
            let mut decoder = ReceiverDecoder::new(&pool, &[], options);
            for (i, packet) in packets.iter().enumerate() {
                let mut work = WorkBudget::new(u64::MAX);
                decoder
                    .decode(packet, Some(i as i64 * 1_000_000), &mut work, &cancel)
                    .unwrap();
                let held = pool.used() - Budget::control_storage_bytes();
                assert_eq!(decoder.retained(), held, "packet {i}, {options:?}");
            }
        }
    }

    /// The palettes presented for `packets`: one per packet, of its first
    /// rectangle.
    fn palettes(options: Options, packets: &[Vec<u8>]) -> Vec<Vec<u32>> {
        let pool = Budget::new(1 << 22).unwrap();
        let cancel = AtomicBool::new(false);
        let mut decoder = ReceiverDecoder::new(&pool, &[], options);
        packets
            .iter()
            .enumerate()
            .map(|(i, packet)| {
                let mut work = WorkBudget::new(u64::MAX);
                let status = decoder
                    .decode(packet, Some(i as i64 * 1_000_000), &mut work, &cancel)
                    .unwrap();
                let rect = status.subtitle().unwrap().rects().next().unwrap();
                rect.palette.to_vec()
            })
            .collect()
    }

    /// `compute_clut` −2 keeps a region's first automatic palette through a
    /// redraw, and starts afresh with the region of a new epoch; −1
    /// recomputes it after the redraw.
    #[test]
    fn a_kept_automatic_palette_lasts_until_a_new_epoch() {
        // Pixels 1 2 3 3, then 2 2 2 2 drawn over them, then 2 2 2 2 in a new
        // epoch's region.
        let packets = [
            [
                page(0, true, &[0]),
                region(0, 0, 4, 2, 9, 1),
                pixels(0x6f),
                end(),
            ]
            .concat(),
            [
                page(1, false, &[0]),
                region(0, 1, 4, 2, 9, 1),
                pixels(0xaa),
                end(),
            ]
            .concat(),
            [
                page(2, true, &[0]),
                region(0, 0, 4, 2, 9, 1),
                pixels(0xaa),
                end(),
            ]
            .concat(),
        ];
        let options = |automatic_palette| Options {
            automatic_palette,
            ..Options::default()
        };
        let kept = palettes(options(AutomaticPalette::WithoutClutOnce), &packets);
        let recomputed = palettes(options(AutomaticPalette::WithoutClut), &packets);
        assert_eq!(kept[0], recomputed[0]);
        assert_ne!(kept[0][2], 0);
        assert_eq!(kept[1], kept[0]);
        assert_eq!(recomputed[1][2], 0);
        assert_eq!(kept[2], [0; 4]);
        assert_eq!(kept[2], recomputed[2]);
    }

    /// Under `compute_clut` −2 a fill without a drawing keeps the region's
    /// palette, as every mode does, and so does a drawing after it; a mode
    /// change starts a new epoch whose region computes its palette afresh.
    #[test]
    fn a_kept_automatic_palette_lasts_through_a_fill_until_a_mode_change() {
        // Region 0 filled with pixel code 0 and no object placed.
        let fill = {
            let mut body = vec![0, 1 << 4 | 1 << 3];
            body.extend(4u16.to_be_bytes());
            body.extend(2u16.to_be_bytes());
            body.extend([1 << 5 | 1 << 2, 9, 0, 0]);
            segment(0x11, &body)
        };
        // Page version 3 at a mode change.
        let mode_change = segment(0x10, &[5, 3 << 4 | 2 << 2, 0, 0, 0, 10, 0, 10]);
        let packets = [
            [
                page(0, true, &[0]),
                region(0, 0, 4, 2, 9, 1),
                pixels(0x6f),
                end(),
            ]
            .concat(),
            [page(1, false, &[0]), fill, end()].concat(),
            [
                page(2, false, &[0]),
                region(0, 2, 4, 2, 9, 1),
                pixels(0xaa),
                end(),
            ]
            .concat(),
            [mode_change, region(0, 0, 4, 2, 9, 1), pixels(0xaa), end()].concat(),
        ];
        let options = |automatic_palette| Options {
            automatic_palette,
            ..Options::default()
        };
        let kept = palettes(options(AutomaticPalette::WithoutClutOnce), &packets);
        let recomputed = palettes(options(AutomaticPalette::WithoutClut), &packets);
        assert_ne!(kept[0][2], 0);
        assert_eq!(kept[1], kept[0]);
        assert_eq!(recomputed[1], kept[0]);
        assert_eq!(kept[2], kept[0]);
        assert_eq!(recomputed[2][2], 0);
        assert_eq!(kept[3], [0; 4]);
        assert_eq!(kept[3], recomputed[3]);
    }
}

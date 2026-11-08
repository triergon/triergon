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
mod palette;
mod pixels;
mod segment;

use crate::dvb::{Depth, Size};
use epoch::{Epoch, Region};
use palette::{Census, DEFAULT};
use segment::{
    ClutDefinition, DisplayDefinition, Kind, ObjectData, PageComposition, Placement,
    RegionComposition, Segment, Segments,
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, BudgetVec, Error, Result, WorkBudget};

/// When a region's palette is replaced by the automatic one (`compute_clut`).
///
/// The automatic palette ranks a region's pixel values from its edge inwards,
/// whatever its depth, and gives them evenly spaced grey-green levels from
/// transparent to opaque.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AutomaticPalette {
    /// Never: regions keep their CLUT, or the default CLUT.
    Never,
    /// For regions whose CLUT was never defined.
    #[default]
    WithoutClut,
    /// For every region.
    Always,
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
    /// not decoded yet. The work counts towards the next attempt: decode the
    /// same packet again.
    Yield,
}

impl<'a> Status<'a> {
    /// The presented subtitle, if any.
    pub fn subtitle(self) -> Option<Subtitle<'a>> {
        match self {
            Self::Subtitle(subtitle) => Some(subtitle),
            Self::Nothing | Self::Yield => None,
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

/// A packet whose decoding stopped before a segment, to continue there.
#[derive(Clone, Copy, Debug)]
struct Resume {
    length: usize,
    pts: Option<i64>,
    /// Where the next segment starts; the packet length once only the
    /// end-of-packet presentation remains.
    offset: usize,
}

/// How far a call decoded its packet.
#[derive(Clone, Copy, Debug)]
enum Progress {
    Done,
    /// Stopped before the segment at `at`, for want of work or by
    /// cancellation.
    Halted {
        at: usize,
        cancelled: bool,
    },
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
    /// Where the packet that yielded or was cancelled continues.
    resume: Option<Resume>,
    /// Work granted to calls on the current packet and left unspent.
    credit: u64,
    budget: Budget,
}

impl ReceiverDecoder {
    /// `configuration` is the stream's codec configuration (the services'
    /// page ids), read when [`Options::service`] selects one.
    pub fn new(budget: &Budget, configuration: &[u8], options: Options) -> Self {
        Self {
            options,
            pages: Pages::select(configuration, options.service),
            epoch: Epoch::new(budget),
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
            resume: None,
            credit: 0,
            budget: budget.clone(),
        }
    }

    /// Decode one codec packet with its presentation time in microseconds.
    /// A packet of a segment header or less, or one that does not start with
    /// a segment, is refused (`Error::Invalid`) and changes nothing.
    ///
    /// Each segment's work, one unit per byte, per object placement it visits
    /// and per pixel it may draw, copy or present, is charged to `work` before
    /// the segment is decoded. When the budget cannot pay for the next one,
    /// the call yields ([`Status::Yield`]) before it. Call again with the same
    /// packet and time to continue: the work left over counts towards the
    /// next call, so the packet completes after enough calls. Cancellation is
    /// checked before each segment and each object placement, and a cancelled
    /// packet continues the same way. A different packet drops the rest of a
    /// yielded or cancelled one.
    pub fn decode(
        &mut self,
        packet: &[u8],
        pts: Option<i64>,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Status<'_>> {
        if packet.len() <= 6 || packet[0] != 0x0f {
            return Err(Error::Invalid("DVB subtitle packet"));
        }
        let start = match self.resume.take() {
            Some(resume) if resume.length == packet.len() && resume.pts == pts => resume.offset,
            _ => {
                self.credit = 0;
                self.presented = None;
                self.held = Held::default();
                0
            }
        };
        let granted = work.remaining();
        let mut banked = WorkBudget::new(granted.saturating_add(self.credit));
        let progress = self.run(packet, start, pts, &mut banked, cancel);
        if let Ok(Progress::Halted { at, cancelled }) = progress {
            work.take(granted);
            self.credit = banked.remaining();
            self.resume = Some(Resume {
                length: packet.len(),
                pts,
                offset: at,
            });
            return if cancelled {
                Err(Error::Cancelled)
            } else {
                Ok(Status::Yield)
            };
        }
        work.take(banked.spent().saturating_sub(self.credit));
        self.credit = 0;
        progress?;
        Ok(match self.presented {
            Some(p) => Status::Subtitle(Subtitle {
                pts: p.pts,
                start_display_ms: 0,
                end_display_ms: p.end_display_ms,
                display_size: self.display_size,
                slots: &self.slots.as_slice()[..p.rects],
                epoch: &self.epoch,
            }),
            None => Status::Nothing,
        })
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
    ) -> Result<Progress> {
        let mut segments = Segments::new(&packet[start..]);
        loop {
            let at = packet.len() - segments.remaining();
            let Some(segment) = segments.next() else {
                break;
            };
            if cancel.load(Ordering::Relaxed) {
                return Ok(Progress::Halted {
                    at,
                    cancelled: true,
                });
            }
            let Ok(segment) = segment else {
                return Ok(Progress::Done);
            };
            let parsed = self.pages.admit(segment.page).then(|| Parsed::of(&segment));
            let bytes = 6 + segment.body.len() as u64;
            let cost = bytes + parsed.as_ref().map_or(0, |parsed| self.work(parsed));
            if !work.try_charge(cost) {
                return Ok(Progress::Halted {
                    at,
                    cancelled: false,
                });
            }
            let Some(parsed) = parsed else {
                continue;
            };
            match self.apply(parsed, pts, cancel) {
                Ok(Flow::Continue) => {}
                Ok(Flow::Stop) => return Ok(Progress::Done),
                Err(Error::Cancelled) => {
                    return Ok(Progress::Halted {
                        at,
                        cancelled: true,
                    });
                }
                Err(error) => return Err(error),
            }
        }
        let presents = self.held.display_set() && !self.options.end_from_next;
        if presents && !work.try_charge(self.present_work().0) {
            return Ok(Progress::Halted {
                at: packet.len(),
                cancelled: false,
            });
        }
        if self.held.display_set() {
            self.display_size.get_or_insert(DEFAULT_DISPLAY);
        }
        if presents {
            self.present(pts, self.timeout_ms())?;
        }
        self.previous_pts = pts;
        Ok(Progress::Done)
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
                self.present(pts, self.timeout_ms())?;
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
        if self.options.end_from_next {
            let previous = self.previous_pts;
            let end = match (pts, previous) {
                (Some(pts), Some(previous)) => millis(pts.saturating_sub(previous)) - 1,
                _ => -1,
            };
            // The receiver's display time is 32 bits: a negative end wraps.
            self.present(previous, end as u32)?;
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
            AutomaticPalette::WithoutClut => self.epoch.tables(region.clut).is_none(),
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
    /// packet presented already.
    fn present(&mut self, pts: Option<i64>, end_display_ms: u32) -> Result<()> {
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
            if let Some(index) = slot.region.take() {
                slot.pixels.clear();
                slot.pixels
                    .try_extend_from_slice(self.epoch.region_at(index).pixels())?;
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

/// Microseconds to milliseconds, rounded to nearest.
fn millis(us: i64) -> i64 {
    (us + if us < 0 { -500 } else { 500 }) / 1000
}

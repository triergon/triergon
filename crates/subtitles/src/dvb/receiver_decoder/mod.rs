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
use epoch::Epoch;
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
            budget: budget.clone(),
        }
    }

    /// Decode one codec packet with its presentation time in microseconds.
    /// A packet of a segment header or less, or one that does not start with
    /// a segment, is refused (`Error::Invalid`) and changes nothing.
    ///
    /// A packet is decoded whole. Its work, one unit per packet byte, per
    /// object placement an object segment visits and per pixel its segments
    /// may draw, copy or present, is charged to `work`, which stops at zero.
    /// A cooperative caller decodes the next packet while its budget lasts.
    /// Cancellation is checked before each segment and each object placement;
    /// a cancelled packet keeps the segments decoded before it.
    pub fn decode(
        &mut self,
        packet: &[u8],
        pts: Option<i64>,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<Subtitle<'_>>> {
        self.presented = None;
        if packet.len() <= 6 || packet[0] != 0x0f {
            return Err(Error::Invalid("DVB subtitle packet"));
        }
        charge(work, packet.len());
        self.held = Held::default();
        let mut flow = Flow::Continue;
        for segment in Segments::new(packet) {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            flow = match segment {
                Ok(segment) if !self.pages.admit(segment.page) => Flow::Continue,
                Ok(segment) => self.segment(&segment, pts, work, cancel)?,
                Err(_) => Flow::Stop,
            };
            if flow == Flow::Stop {
                break;
            }
        }
        if flow == Flow::Continue {
            if self.held.display_set() {
                self.display_size.get_or_insert(DEFAULT_DISPLAY);
            }
            if self.held.display_set() && !self.options.end_from_next {
                self.present(pts, self.timeout_ms(), work)?;
            }
            self.previous_pts = pts;
        }
        Ok(self.presented.map(|p| Subtitle {
            pts: p.pts,
            start_display_ms: 0,
            end_display_ms: p.end_display_ms,
            display_size: self.display_size,
            slots: &self.slots.as_slice()[..p.rects],
            epoch: &self.epoch,
        }))
    }

    fn timeout_ms(&self) -> u32 {
        u32::from(self.page.timeout) * 1000
    }

    fn segment(
        &mut self,
        segment: &Segment<'_>,
        pts: Option<i64>,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Flow> {
        if self.presented.is_some()
            && matches!(
                segment.kind,
                Kind::PageComposition | Kind::RegionComposition | Kind::ObjectData
            )
        {
            self.detach(work)?;
        }
        match segment.kind {
            Kind::PageComposition => {
                self.held.page = true;
                self.compose_page(&PageComposition::parse(segment), pts, work)?;
            }
            Kind::RegionComposition => {
                let Some(region) = RegionComposition::parse(segment.body) else {
                    return Ok(Flow::Stop);
                };
                if self.epoch.compose_region(&region, work)? == Flow::Stop {
                    return Ok(Flow::Stop);
                }
                self.held.region = true;
            }
            Kind::ClutDefinition => {
                if let Some(clut) = ClutDefinition::parse(segment) {
                    self.epoch.define_clut(&clut)?;
                }
            }
            Kind::ObjectData => {
                let Ok(object) = ObjectData::parse(segment.body) else {
                    return Ok(Flow::Stop);
                };
                if self.epoch.draw_object(&object, work, cancel)? == Flow::Stop {
                    return Ok(Flow::Stop);
                }
                self.held.object = true;
            }
            Kind::DisplayDefinition => match DisplayDefinition::parse(segment.body) {
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
            Kind::EndOfDisplaySet if !self.options.end_from_next => {
                self.present(pts, self.timeout_ms(), work)?;
            }
            Kind::EndOfDisplaySet | Kind::Other => {}
        }
        Ok(Flow::Continue)
    }

    /// A new page version replaces the region list; at an acquisition point
    /// or mode change it also starts a new epoch. A repeated version, or one
    /// that cannot be allocated, changes nothing.
    fn compose_page(
        &mut self,
        page: &PageComposition<'_>,
        pts: Option<i64>,
        work: &mut WorkBudget,
    ) -> Result<()> {
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
            self.present(previous, end as u32, work)?;
        }
        let starts_epoch = page.state.starts_epoch();
        if starts_epoch {
            self.detach(work)?;
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

    /// Present the page as it stands, unless no page was composed yet or this
    /// call presented already.
    fn present(
        &mut self,
        pts: Option<i64>,
        end_display_ms: u32,
        work: &mut WorkBudget,
    ) -> Result<()> {
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
            let slot = &mut self.slots.as_mut_slice()[count];
            slot.x = i32::from(placement.x) + i32::from(dx);
            slot.y = i32::from(placement.y) + i32::from(dy);
            slot.width = region.width;
            slot.height = region.height;
            slot.depth = region.depth;
            slot.region = Some(index);
            let tables = self.epoch.tables(region.clut);
            let automatic = match self.options.automatic_palette {
                AutomaticPalette::Never => false,
                AutomaticPalette::WithoutClut => tables.is_none(),
                AutomaticPalette::Always => true,
            };
            if automatic {
                slot.palette = *self
                    .epoch
                    .automatic_palette(index, &mut self.census, work)?;
            } else {
                let table = tables.unwrap_or(&DEFAULT).get(region.depth);
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

    /// Copy the presented pixels out of the regions before a segment of the
    /// same packet changes them.
    fn detach(&mut self, work: &mut WorkBudget) -> Result<()> {
        let count = self.presented.map_or(0, |p| p.rects);
        for slot in &mut self.slots.as_mut_slice()[..count] {
            if let Some(index) = slot.region.take() {
                let pixels = self.epoch.region_at(index).pixels();
                charge(work, pixels.len());
                slot.pixels.clear();
                slot.pixels.try_extend_from_slice(pixels)?;
            }
        }
        Ok(())
    }
}

/// Charge work done, down to an empty budget.
fn charge(work: &mut WorkBudget, units: usize) {
    work.take(units as u64);
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

//! The resumable Ogg reader.
use crate::compat::ReferenceClock;
use crate::mapping::{Configuration, HeaderStep, Headers, Mapping, Previous};
use crate::packets::{Assembler, Completed, Continuity};
use crate::page::{MAX_PAGE, Page, PageHeader, PageReader, PageStep};
use crate::queue::Queue;
use crate::seek::{Bisection, Landing, Output, Probe};
use crate::timing::{Clock, PageClock, Slot};
use crate::track::{Packet, Track};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::container::{
    ContainerReader, Event, ReaderStep, SeekMethod, SeekPolicy, SeekRequest, TrackInfo,
};
use triergon_core::{
    Backpressure, Budget, BudgetVec, Bytes, Error, InputCursor, InputRequest, InputToken,
    InputWindow, Result, Rounding, SourceCapabilities, SourceFailure, SourceId, SourceLocation,
    Step, Time, WorkBudget,
};

/// Events a page adds besides its packets, at most: a resync, a lost page, a link, a
/// track, a seek receipt and the packet held for it. The end of the input adds fewer.
const PAGE_EVENTS: usize = 6;

/// Bounds of one [`Reader`], on top of the caller's memory [`Budget`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// The pool [`BlockingReader`](crate::BlockingReader) creates.
    pub live_storage_bytes: usize,
    /// Largest packet. Ogg allows any size; 16 MiB holds any audio packet and a
    /// comment header with cover art.
    pub packet_bytes: usize,
    /// Largest configuration of one track, its comment header included.
    pub header_bytes: usize,
    /// Logical streams in one link.
    pub streams: usize,
    /// Tracks across every link of a chained source.
    pub tracks: usize,
    /// Bytes skipped in a row looking for a page: garbage and pages with a bad CRC.
    pub sync_scan_bytes: u64,
    /// Bytes one seek probe reads looking for a page of the track.
    pub seek_probe_bytes: u64,
    /// Bytes read at the end of a random-access source of known length for the last
    /// granule positions ([`Track::end`]); 0 skips the scan.
    pub end_scan_bytes: u64,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            live_storage_bytes: 64 << 20,
            packet_bytes: 16 << 20,
            header_bytes: 16 << 20,
            streams: 64,
            tracks: 1024,
            sync_scan_bytes: 2 * MAX_PAGE as u64,
            seek_probe_bytes: 4 * MAX_PAGE as u64,
            end_scan_bytes: 2 * MAX_PAGE as u64,
        }
    }
}

/// Reader events other than packets and seeks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReaderEvent {
    /// The track at this index has read its headers (or its mapping has none) and is
    /// configured; its packets follow.
    Track(usize),
    /// A chained link starts at this source offset; its tracks follow.
    Link { index: u32, offset: u64 },
    /// Bytes skipped before the page at `offset + bytes`: garbage, pages with a bad CRC,
    /// or a partial page at the end of the input.
    Resync { offset: u64, bytes: u64 },
    /// Pages of the track were lost before the page at this offset; the packet they
    /// carried is dropped.
    Lost { track: usize, page: u64 },
}

/// The state of one logical stream of the current link.
#[derive(Debug)]
struct Stream {
    serial: u32,
    track: Option<usize>,
    assembler: Assembler,
    headers: Headers,
    previous: Previous,
    clock: Clock,
    reference: ReferenceClock,
}

impl Stream {
    fn new(pool: &Budget, serial: u32, limits: &Limits) -> Stream {
        Stream {
            serial,
            track: None,
            assembler: Assembler::new(pool, limits.packet_bytes),
            headers: Headers::Done,
            previous: Previous::Start,
            clock: Clock::at(None),
            reference: ReferenceClock::default(),
        }
    }

    fn configured(&self) -> bool {
        self.track.is_some() && self.headers == Headers::Done
    }
}

/// Where a link of a chained source starts, and where its headers end.
#[derive(Clone, Copy, Debug)]
struct Link {
    start: u64,
    data: Option<u64>,
}

#[derive(Debug)]
enum Phase {
    Pages,
    Bisect(Bisection),
    /// Reading the last pages; then delivery resumes at the offset.
    EndScan {
        resume: u64,
        chained: bool,
    },
    Ended(SourceLocation),
}

/// A media packet of the page being processed.
#[derive(Debug)]
struct Media {
    payload: Bytes,
    slot: Slot,
    last_on_page: bool,
}

impl AsMut<Slot> for Media {
    fn as_mut(&mut self) -> &mut Slot {
        &mut self.slot
    }
}

/// The page being processed; kept until every packet is handled, so a step that
/// fails for want of memory resumes it.
#[derive(Debug)]
struct Current {
    page: Page,
    stream: Option<usize>,
    assembled: bool,
    next: usize,
}

/// A resumable Ogg reader. Errors other than backpressure are sticky until
/// [`ContainerReader::reset`].
///
/// Tracks appear as their logical streams begin and are announced with
/// [`ReaderEvent::Track`] once configured; header packets are never delivered. A
/// chained source (a new group of logical streams after the previous ones) adds the
/// next link's tracks after [`ReaderEvent::Link`], timed to follow the previous link.
#[derive(Debug)]
pub struct Reader {
    input: InputCursor,
    pages: PageReader,
    limits: Limits,
    tracks: BudgetVec<Track>,
    streams: BudgetVec<Stream>,
    links: BudgetVec<Link>,
    link: usize,
    /// A media packet has been read in the current link: a new logical stream starts
    /// the next link.
    link_media: bool,
    /// Where the previous link ended, which the current link's tracks start from.
    link_start: Option<Time>,
    current: Option<Current>,
    /// Packets the current page completed (at most 255).
    completed: Vec<Completed>,
    /// Media packets of the current page (at most 255).
    media: Vec<Media>,
    /// Events to deliver. Room for what a page adds is charged before it is handled.
    pending: Queue<Output>,
    phase: Phase,
    landing: Option<Landing>,
    end_scanned: bool,
    failed: Option<Error>,
}

impl Reader {
    /// A reader for one source, requesting windows of at most `window` bytes.
    pub fn new(
        pool: &Budget,
        source: SourceId,
        capabilities: SourceCapabilities,
        limits: Limits,
        window: usize,
    ) -> Result<Reader> {
        let input = InputCursor::new(pool, source, capabilities, window)?;
        Ok(Reader {
            pages: PageReader::new(&input, limits.sync_scan_bytes),
            input,
            limits,
            tracks: BudgetVec::new(pool),
            streams: BudgetVec::new(pool),
            links: BudgetVec::new(pool),
            link: 0,
            link_media: false,
            link_start: None,
            current: None,
            // Fixed by the 255 lacing values of a page, allocated once.
            completed: Vec::with_capacity(255),
            media: Vec::with_capacity(255),
            pending: Queue::new(pool),
            phase: Phase::Pages,
            landing: None,
            end_scanned: false,
            failed: None,
        })
    }

    /// Every track seen so far, in the order their logical streams began.
    pub fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    /// The request for the input the reader needs next.
    pub fn request(&self) -> InputRequest {
        self.input.request()
    }

    fn advance(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<Output, Backpressure>> {
        loop {
            if let Some(output) = self.pending.pop() {
                return Ok(Step::Ready(output));
            }
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if self.current.is_some() {
                self.process()?;
                continue;
            }
            self.pending.reserve(PAGE_EVENTS)?;
            let step = match &mut self.phase {
                Phase::Ended(location) => return Ok(Step::End(*location)),
                Phase::Pages => self.pages.next(&mut self.input, work, cancel)?,
                Phase::Bisect(_) => match self.bisect(work, cancel)? {
                    Some(step) => step,
                    None => continue,
                },
                Phase::EndScan { .. } => match self.end_scan(work, cancel)? {
                    Some(step) => step,
                    None => continue,
                },
            };
            match step {
                PageStep::Page(page) => {
                    self.current = Some(Current {
                        page,
                        stream: None,
                        assembled: false,
                        next: 0,
                    });
                }
                PageStep::NeedInput(request) => return Ok(Step::NeedInput(request)),
                PageStep::Yield => return Ok(Step::Yield),
                PageStep::End { location, skipped } => {
                    if skipped > 0 {
                        self.pending.push(Event::Other(ReaderEvent::Resync {
                            offset: location.offset - skipped,
                            bytes: skipped,
                        }))?;
                    }
                    if let Some(landing) = self.landing.take() {
                        landing.finish(&mut self.pending)?;
                    }
                    self.phase = Phase::Ended(location);
                }
            }
        }
    }

    /// Handle the current page. A failure keeps it, so a step after backpressure
    /// resumes where it stopped.
    fn process(&mut self) -> Result<()> {
        let Some(mut current) = self.current.take() else {
            return Ok(());
        };
        let page = &current.page;
        let events = page.header.completed_packets(&page.bytes) + PAGE_EVENTS;
        let result = self
            .pending
            .reserve(events)
            .and_then(|()| self.process_page(&mut current));
        if result.is_err() {
            self.current = Some(current);
        }
        result
    }

    fn process_page(&mut self, current: &mut Current) -> Result<()> {
        let header = current.page.header;
        let offset = current.page.offset;
        let found = match current.stream {
            Some(stream) => Some(stream),
            None => self.stream_for(&header, offset)?,
        };
        let Some(stream) = found else {
            return self.resync_event(&mut current.page);
        };
        current.stream = Some(stream);
        if !current.assembled {
            let s = &mut self.streams[stream];
            let continuity = s
                .assembler
                .push(&header, &current.page.bytes, &mut self.completed)?;
            current.assembled = true;
            if continuity == Continuity::Lost {
                s.previous = Previous::Unknown;
                s.clock = Clock::at(None);
            }
            let track = s.track;
            self.resync_event(&mut current.page)?;
            if let (Continuity::Lost, Some(track)) = (continuity, track) {
                self.pending.push(Event::Other(ReaderEvent::Lost {
                    track,
                    page: offset,
                }))?;
            }
        }
        while current.next < self.completed.len() {
            self.packet(stream, current.next)?;
            current.next += 1;
        }
        self.finish_page(stream, &current.page)
    }

    /// Report the bytes skipped before a page, once.
    fn resync_event(&mut self, page: &mut Page) -> Result<()> {
        let bytes = std::mem::take(&mut page.skipped);
        if bytes == 0 {
            return Ok(());
        }
        self.pending.push(Event::Other(ReaderEvent::Resync {
            offset: page.offset - bytes,
            bytes,
        }))
    }

    /// The stream a page belongs to; a new one for a beginning-of-stream page, which
    /// after media starts the next link. `None` ignores the page.
    fn stream_for(&mut self, header: &PageHeader, offset: u64) -> Result<Option<usize>> {
        let found = self.streams.iter().position(|s| s.serial == header.serial);
        // A stream that begins again after media is a chained link reusing the serial.
        let restarted = header.begin_of_stream() && self.link_media;
        match found {
            Some(index) if !restarted => return Ok(Some(index)),
            None if !header.begin_of_stream() => return Ok(None),
            _ => {}
        }
        if self.link_media {
            self.next_link();
        }
        if self.streams.len() >= self.limits.streams {
            return Err(Error::Policy("Ogg logical streams in one link"));
        }
        if self.links.len() <= self.link {
            self.links
                .try_push(Link {
                    start: offset,
                    data: None,
                })
                .map_err(|e| e.error)?;
            if self.link > 0 {
                self.pending.push(Event::Other(ReaderEvent::Link {
                    index: u32::try_from(self.link).map_err(|_| Error::Overflow)?,
                    offset,
                }))?;
            }
        }
        let stream = Stream::new(self.input.budget(), header.serial, &self.limits);
        self.streams.try_push(stream).map_err(|e| e.error)?;
        Ok(Some(self.streams.len() - 1))
    }

    /// End the current link: its streams stop, and the next link's tracks start where
    /// its packets ended.
    fn next_link(&mut self) {
        let mut end: Option<Time> = None;
        for stream in self.streams.iter() {
            let Some(index) = stream.track else { continue };
            let track = &mut self.tracks[index];
            track.reference_end = stream.reference.end();
            let ticks = stream.clock.end().map(|ticks| ticks + track.offset);
            if let Some(time) =
                ticks.and_then(|t| Time::from_ticks(i128::from(t), track.time_base).ok())
                && track.timeline().timed()
            {
                end = Some(end.map_or(time, |end| end.max(time)));
            }
        }
        self.link_start = end.or(self.link_start);
        self.streams.clear();
        self.link += 1;
        self.link_media = false;
    }

    /// Handle one completed packet of the current page.
    fn packet(&mut self, stream: usize, index: usize) -> Result<()> {
        let completed = &self.completed[index];
        let payload = completed.payload.clone();
        let last_on_page = completed.last_on_page;
        let Some(track) = self.streams[stream].track else {
            return self.attach(stream, &payload);
        };
        let headers = self.streams[stream].headers;
        if headers != Headers::Done {
            let step = headers.classify(&payload);
            let configuring = !self.tracks[track].configured;
            match step {
                Ok(HeaderStep::Media) => {}
                Ok(step) => {
                    if configuring && self.tracks[track].header_error.is_none() {
                        match self.tracks[track].config.header(headers, &payload) {
                            Ok(()) => {}
                            Err(error @ Error::Backpressure { .. }) => return Err(error),
                            Err(error) => self.tracks[track].header_error = Some(error),
                        }
                    }
                    let next = if step == HeaderStep::Last {
                        Headers::Done
                    } else {
                        headers.next()
                    };
                    self.streams[stream].headers = next;
                    if next == Headers::Done {
                        self.configured(stream, track)?;
                    }
                    return Ok(());
                }
                Err(error) => {
                    if configuring {
                        self.tracks[track].header_error = Some(error);
                    }
                }
            }
            self.streams[stream].headers = Headers::Done;
            self.configured(stream, track)?;
        }
        let s = &mut self.streams[stream];
        let durations = self.tracks[track]
            .timeline()
            .durations(&payload, &mut s.previous);
        self.link_media = true;
        self.media.push(Media {
            payload,
            slot: Slot {
                durations,
                ..Slot::default()
            },
            last_on_page,
        });
        Ok(())
    }

    /// The first packet of a stream: identify it and find or make its track.
    fn attach(&mut self, stream: usize, first: &Bytes) -> Result<()> {
        let serial = self.streams[stream].serial;
        let link = u32::try_from(self.link).map_err(|_| Error::Overflow)?;
        let (mapping, tag) = Mapping::identify(first);
        let found = self
            .tracks
            .iter()
            .position(|track| track.link == link && track.serial == serial);
        let index = match found {
            Some(index) => index,
            None => self.new_track(serial, link, mapping, tag, first)?,
        };
        let track = &self.tracks[index];
        let chain_end = link
            .checked_sub(1)
            .and_then(|previous| {
                self.tracks
                    .iter()
                    .find(|t| t.link == previous && t.position == track.position)
            })
            .and_then(|previous| previous.reference_end);
        let headers = if track.header_error.is_some() {
            Headers::Done
        } else {
            Headers::after_first(mapping, first)
        };
        let s = &mut self.streams[stream];
        s.track = Some(index);
        s.headers = headers;
        s.reference = ReferenceClock::new(chain_end);
        if headers == Headers::Done {
            self.configured(stream, index)?;
        }
        Ok(())
    }

    /// A track for a logical stream seen for the first time.
    fn new_track(
        &mut self,
        serial: u32,
        link: u32,
        mapping: Mapping,
        tag: &[u8],
        first: &[u8],
    ) -> Result<usize> {
        if self.tracks.len() >= self.limits.tracks {
            return Err(Error::Policy("Ogg tracks"));
        }
        let pool = self.input.budget();
        let (config, error) =
            match Configuration::first(pool, mapping, first, self.limits.header_bytes) {
                Ok(config) => (config, None),
                Err(error @ Error::Backpressure { .. }) => return Err(error),
                Err(error) => (Configuration::opaque(pool, 0), Some(error)),
            };
        let position = self
            .tracks
            .iter()
            .filter(|track| track.link == link)
            .count();
        let mut track = Track::new(
            serial,
            link,
            u32::try_from(position).map_err(|_| Error::Overflow)?,
            mapping,
            tag,
            config,
            0,
        );
        track.header_error = error;
        self.tracks.try_push(track).map_err(|e| e.error)?;
        Ok(self.tracks.len() - 1)
    }

    /// A stream's headers are read: start its clock and announce a new configuration.
    fn configured(&mut self, stream: usize, index: usize) -> Result<()> {
        let track = &mut self.tracks[index];
        let start = track.timeline().timed().then(|| track.timeline().ticks(0));
        let s = &mut self.streams[stream];
        s.previous = Previous::Start;
        s.clock = Clock::at(start);
        if track.configured {
            return Ok(());
        }
        track.configured = true;
        track.time_base = track.config.time_base();
        if let Some(start) = self.link_start
            && track.timeline().timed()
        {
            track.offset = start
                .to_i64_ticks(track.time_base, Rounding::NearestAway)
                .unwrap_or(0);
        }
        self.pending.push(Event::Other(ReaderEvent::Track(index)))
    }

    /// Time the page's media packets and deliver them.
    fn finish_page(&mut self, stream: usize, page: &Page) -> Result<()> {
        let s = &mut self.streams[stream];
        if let Some(index) = s.track {
            let track = &self.tracks[index];
            let timeline = *track.timeline();
            let header = &page.header;
            let granule = header
                .granule_position()
                .filter(|_| timeline.timed())
                .map(|granule| timeline.ticks(granule));
            let eos = header.end_of_stream();
            let clock = PageClock {
                granule,
                end_of_stream: eos,
            };
            if self.media.is_empty() {
                s.clock.pass(clock);
                s.reference.header_page(granule);
            } else {
                s.clock.page(&timeline, clock, &mut self.media);
                s.reference.page(
                    &timeline,
                    granule,
                    header.granule == 0,
                    eos,
                    &mut self.media,
                );
                let offset = track.offset;
                let timed = timeline.timed();
                let template = |media: Media| Packet {
                    track: track.id(),
                    index,
                    payload: media.payload,
                    time_base: track.time_base,
                    pts: media.slot.pts.map(|pts| pts + offset),
                    duration: media.slot.durations.exact.map(i64::from),
                    trim: media.slot.trim,
                    granule: media.last_on_page.then_some(header.granule),
                    page: page.offset,
                    timed,
                    reference: media.slot.reference,
                };
                for media in self.media.drain(..) {
                    let packet = template(media);
                    match self.landing.as_mut() {
                        Some(landing) => {
                            if landing.offer(packet, &mut self.pending)? {
                                self.landing = None;
                            }
                        }
                        None => self.pending.push(Event::Packet(packet))?,
                    }
                }
            }
        }
        self.media.clear();
        if let Some(index) = self.streams[stream].track {
            self.tracks[index].reference_shift = self.streams[stream].reference.shift();
        }
        let configured = self.streams.iter().all(Stream::configured);
        if let Some(link) = self.links.get_mut(self.link)
            && link.data.is_none()
            && configured
        {
            link.data = Some(page.offset + page.bytes.len() as u64);
            self.start_end_scan()?;
        }
        Ok(())
    }

    /// Read the last pages of a seekable source once the first link's headers are read.
    fn start_end_scan(&mut self) -> Result<()> {
        let capabilities = self.input.capabilities();
        if self.end_scanned
            || self.link != 0
            || !capabilities.random_access
            || self.limits.end_scan_bytes == 0
            || self.landing.is_some()
        {
            return Ok(());
        }
        let Some(length) = capabilities.length else {
            return Ok(());
        };
        self.end_scanned = true;
        let resume = self.pages.position(&self.input);
        let from = length
            .saturating_sub(self.limits.end_scan_bytes)
            .max(resume);
        if from >= length {
            return Ok(());
        }
        self.input.request_range(from)?;
        self.pages.clear();
        self.phase = Phase::EndScan {
            resume,
            chained: false,
        };
        Ok(())
    }

    fn end_scan(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Option<PageStep>> {
        let Phase::EndScan { resume, chained } = &mut self.phase else {
            return Ok(None);
        };
        let resume = *resume;
        let step = match self.pages.next(&mut self.input, work, cancel) {
            Ok(PageStep::Page(page)) => {
                let header = page.header;
                if header.begin_of_stream()
                    && !self.streams.iter().any(|s| s.serial == header.serial)
                {
                    *chained = true;
                }
                if !*chained
                    && let Some(granule) = header.granule_position()
                    && let Some(track) = self
                        .tracks
                        .iter_mut()
                        .find(|track| track.link == 0 && track.serial == header.serial)
                    && track.timeline().timed()
                {
                    track.end = Some(track.timeline().ticks(granule) + track.offset);
                }
                return Ok(None);
            }
            Ok(PageStep::End { .. }) | Err(Error::Policy(_)) => None,
            Ok(step @ (PageStep::NeedInput(_) | PageStep::Yield)) => return Ok(Some(step)),
            Err(error) => return Err(error),
        };
        if *chained {
            for track in self.tracks.iter_mut() {
                track.end = None;
            }
        }
        self.input.request_range(resume)?;
        self.pages.clear();
        self.phase = Phase::Pages;
        Ok(step)
    }

    fn bisect(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Option<PageStep>> {
        let Phase::Bisect(bisection) = &mut self.phase else {
            return Ok(None);
        };
        if !bisection.probing() {
            if let Some(offset) = bisection.next_probe() {
                self.input.request_range(offset)?;
                self.pages.clear();
            } else {
                let resume = bisection.resume();
                if let Some(landing) = self.landing.as_mut() {
                    landing.scanned = bisection.scanned;
                }
                self.resume(resume)?;
            }
            return Ok(None);
        }
        match self.pages.next(&mut self.input, work, cancel) {
            Ok(PageStep::Page(page)) => {
                let known = self.streams.iter().any(|s| s.serial == page.header.serial)
                    || self
                        .tracks
                        .iter()
                        .any(|t| t.link as usize == self.link && t.serial == page.header.serial);
                if let Phase::Bisect(bisection) = &mut self.phase
                    && bisection.page(&page, known) == Probe::Settled
                {
                    self.pages.clear();
                }
                Ok(None)
            }
            Ok(PageStep::End { .. }) | Err(Error::Policy(_)) => {
                if let Phase::Bisect(bisection) = &mut self.phase {
                    bisection.exhausted();
                }
                Ok(None)
            }
            Ok(step) => Ok(Some(step)),
            Err(error) => Err(error),
        }
    }

    /// Deliver from a page start of the current link: at the link start the headers are
    /// read again, elsewhere every stream continues mid-stream.
    fn resume(&mut self, offset: u64) -> Result<InputRequest> {
        let request = self.input.request_range(offset)?;
        self.pages.clear();
        self.streams.clear();
        self.current = None;
        self.link_media = false;
        let at_start = self
            .links
            .get(self.link)
            .is_none_or(|link| link.start == offset);
        if !at_start {
            let link = self.link;
            for index in 0..self.tracks.len() {
                let track = &self.tracks[index];
                if track.link as usize != link {
                    continue;
                }
                let mut stream = Stream::new(self.input.budget(), track.serial, &self.limits);
                stream.track = Some(index);
                stream.previous = Previous::Unknown;
                stream.reference = ReferenceClock::resumed(track.reference_shift);
                self.streams.try_push(stream).map_err(|e| e.error)?;
            }
            self.link_media = true;
        }
        self.phase = Phase::Pages;
        Ok(request)
    }

    /// Forget every stream and read again from the source start.
    fn restart(&mut self) -> Result<InputRequest> {
        let request = self.input.reset(0)?;
        self.pages.clear();
        self.streams.clear();
        self.current = None;
        self.link = 0;
        self.link_media = false;
        self.link_start = None;
        self.phase = Phase::Pages;
        Ok(request)
    }
}

fn pressure(error: Error) -> Result<Step<Output, Backpressure>> {
    match error {
        Error::Backpressure {
            requested,
            available,
        } => Ok(Step::Backpressure(Backpressure {
            requested,
            available,
        })),
        other => Err(other),
    }
}

impl ContainerReader for Reader {
    type Packet = Packet;
    type Track = Track;
    /// Track configuration, chain links, resynchronisation and lost pages.
    type Other = ReaderEvent;

    fn provide(&mut self, window: &InputWindow) -> Result<()> {
        self.input.provide(window)
    }
    fn finish(&mut self, source: SourceId, token: InputToken) -> Result<()> {
        self.input.finish(source, token)
    }
    fn fail(&mut self, source: SourceId, token: InputToken, failure: SourceFailure) -> Result<()> {
        self.input.fail(source, token, failure)
    }
    /// Keeps nothing: tracks are found again from the source start.
    fn reset(&mut self) -> Result<InputRequest> {
        let request = self.restart()?;
        self.tracks.clear();
        self.links.clear();
        self.pending.clear();
        self.landing = None;
        self.end_scanned = false;
        self.failed = None;
        Ok(request)
    }
    fn step(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<ReaderStep<Self>> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        self.advance(work, cancel)
            .or_else(|error| pressure(error).inspect_err(|error| self.failed = Some(*error)))
    }
    /// Seek a configured, timed track. The target is on the physical stream's timeline;
    /// see [`crate::seek`] for the landing. Packets of other tracks resume after the
    /// landing in source order. The receipt arrives before the first packet.
    fn request_seek(&mut self, request: SeekRequest) -> Result<InputRequest> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let index = self
            .tracks
            .iter()
            .position(|track| track.id() == request.track)
            .ok_or(Error::Invalid("seek track is not in the source"))?;
        let track = &self.tracks[index];
        let timeline = *track.timeline();
        if !track.configured || !timeline.timed() {
            return Err(Error::Unsupported("seek on a track without packet times"));
        }
        let target = request
            .target
            .to_i64_ticks(track.time_base, Rounding::Floor)?;
        let preroll = match request.policy {
            SeekPolicy::PlaybackPreroll => timeline.preroll(),
            SeekPolicy::PreviousRandomAccess => 0,
        };
        let threshold = target.saturating_sub(preroll);
        let capabilities = self.input.capabilities();
        let link = track.link as usize;
        let bounds = capabilities
            .length
            .filter(|_| capabilities.random_access)
            .zip(self.links.get(link).copied());
        let (serial, offset, base) = (track.serial, track.offset, track.time_base);
        self.pending
            .retain(|output| matches!(output, Event::Other(_)));
        self.current = None;
        self.media.clear();
        let (method, request_next) = match bounds {
            Some((length, start)) => {
                let end = self.links.get(link + 1).map_or(length, |next| next.start);
                let local = threshold - offset - timeline.longest_overlap();
                let mut bisection = Bisection::new(
                    serial,
                    timeline.granule(local),
                    start.start,
                    start.data.unwrap_or(end),
                    end,
                    self.limits.seek_probe_bytes,
                );
                self.link = link;
                self.link_start = None;
                let request = match bisection.next_probe() {
                    Some(probe) => {
                        let request = self.input.request_range(probe)?;
                        self.pages.clear();
                        self.phase = Phase::Bisect(bisection);
                        request
                    }
                    None => self.resume(bisection.resume())?,
                };
                (SeekMethod::BoundedScan, request)
            }
            None => (SeekMethod::Restart, self.restart()?),
        };
        self.landing = Some(Landing::new(
            index,
            request.target,
            request.policy,
            threshold,
            timeline.primes(),
            method,
            base,
        ));
        Ok(request_next)
    }
    fn track_count(&self) -> usize {
        self.tracks.len()
    }
    fn track(&self, index: usize) -> Option<&Track> {
        self.tracks.get(index)
    }
    fn capabilities(&self) -> SourceCapabilities {
        self.input.capabilities()
    }
}

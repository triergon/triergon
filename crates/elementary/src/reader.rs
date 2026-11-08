//! The frame-stream engine every elementary reader runs.
//!
//! A reader walks one source: leading ID3v2 tags, the format's opening (FLAC
//! metadata), a confirmed first sync, then access units until a trailing tag or the
//! end. Each unit is delimited from the bytes at the reader's position without
//! consuming them, so a step that runs out of input or work resumes where it stopped.
//! Bytes between frames go where the format's [`Gap`] policy puts them, which is where
//! FFmpeg's demuxer for that format puts them.
use crate::input::{Input, Wait};
use crate::stream::{Limits, Packet, ReaderEvent, SeekDetail, Track};
use crate::syntax::{Gap, Length, Plan, Prelude, Probe, SeekBasis, Syntax};
use crate::tags::{
    ID3V2_HEADER, Leading, TRAILING_DECIDE, TRAILING_PROBE, Tag, TagKind, Trailing, id3v2, trailing,
};
use std::sync::atomic::AtomicBool;

mod seek;
use seek::Seeking;
use triergon_core::container::{
    ContainerReader, Event, ReaderStep, SeekMethod, SeekPolicy, SeekRequest,
};
use triergon_core::{
    Budget, BudgetVec, Error, InputCursor, InputRequest, InputToken, InputWindow, Result,
    SourceCapabilities, SourceFailure, SourceId, SourceLocation, Step, TimeBase, WorkBudget,
};

/// A format an elementary [`Reader`] reads: [`crate::Flac`], [`crate::MpegAudio`],
/// [`crate::Adts`] or [`crate::Ac3`].
pub trait Format: Syntax {}

/// Bytes a sync scan examines before it drops what it has passed.
const SCAN_CHUNK: usize = 4096;

/// What follows a phase that consumes bytes.
#[derive(Clone, Copy, Debug)]
enum Then<H> {
    Leading,
    Prelude,
    Sync {
        scanned: u64,
    },
    /// A confirmed header starts the view.
    Found(H),
    /// The next unit, with this many bytes before its header.
    Frame(usize),
    Trailing,
    End,
}

/// Who keeps retained bytes.
#[derive(Clone, Copy, Debug)]
enum Owner {
    Tag(TagKind),
    Format,
}

/// The access unit being delimited. Offsets are from the start of the view.
#[derive(Clone, Copy, Debug)]
struct Cut<H> {
    /// Bytes before the unit's first header that belong to its packet.
    lead: usize,
    unit: Option<H>,
    /// Start of the frame being measured.
    frame: usize,
    header: Option<H>,
    end: Option<usize>,
    /// Next candidate of a frame-end search.
    search: usize,
    /// Next candidate of a resync after `end`.
    gap: usize,
}

impl<H: Copy> Cut<H> {
    fn at(lead: usize) -> Cut<H> {
        Cut {
            lead,
            unit: None,
            frame: lead,
            header: None,
            end: None,
            search: 0,
            gap: 0,
        }
    }

    fn found(header: H) -> Cut<H> {
        Cut {
            unit: Some(header),
            header: Some(header),
            ..Cut::at(0)
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Phase<H> {
    Leading,
    Prelude,
    Discard {
        left: u64,
        then: Then<H>,
    },
    Retain {
        len: usize,
        owner: Owner,
        then: Then<H>,
    },
    /// Searching for a confirmed sync from view offset `at`; `scanned` counts bytes
    /// already dropped.
    Sync {
        at: usize,
        scanned: u64,
    },
    Found(H),
    Frame(Cut<H>),
    Trailing,
    End,
}

/// What follows a delimited unit.
#[derive(Clone, Copy, Debug)]
enum After {
    /// The next unit, with this many bytes before its header.
    Frame {
        lead: usize,
    },
    /// Drop bytes, then the next unit.
    Drop(usize),
    Trailing,
    End,
}

impl After {
    /// Bytes to drop after the unit, and what follows them.
    fn then<H>(self) -> (u64, Then<H>) {
        match self {
            After::Frame { lead } => (0, Then::Frame(lead)),
            After::Drop(len) => (len as u64, Then::Frame(0)),
            After::Trailing => (0, Then::Trailing),
            After::End => (0, Then::End),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Unit<H> {
    len: usize,
    header: H,
    complete: bool,
    after: After,
}

/// One stretch of a sync search, as far as the step's work allows.
enum Scan {
    /// A sync byte at this offset.
    Found(usize),
    /// No sync byte before this offset.
    Passed(usize),
    /// No work left.
    Spent,
}

/// Look for `sync` in `view` from `from`, charging one work unit per byte examined.
fn scan(view: &[u8], from: usize, sync: u8, work: &mut WorkBudget) -> Scan {
    let Some(rest) = view.get(from..).filter(|rest| !rest.is_empty()) else {
        return Scan::Passed(from.max(view.len()));
    };
    let budget = usize::try_from(work.remaining()).unwrap_or(usize::MAX);
    if budget == 0 {
        return Scan::Spent;
    }
    let rest = &rest[..rest.len().min(budget)];
    if let Some(offset) = find(rest, sync) {
        work.take(offset as u64 + 1);
        Scan::Found(from + offset)
    } else {
        work.take(rest.len() as u64);
        Scan::Passed(from + rest.len())
    }
}

/// The first `byte` in `hay`, eight bytes at a time: frame payloads are long and sync
/// bytes rare.
#[inline]
fn find(hay: &[u8], byte: u8) -> Option<usize> {
    const LOW: u64 = 0x0101_0101_0101_0101;
    const HIGH: u64 = 0x8080_8080_8080_8080;
    let pattern = LOW * u64::from(byte);
    let mut words = hay.chunks_exact(8);
    for (index, word) in words.by_ref().enumerate() {
        let word = u64::from_le_bytes(word.try_into().unwrap_or([0; 8])) ^ pattern;
        let zero = word.wrapping_sub(LOW) & !word & HIGH;
        if zero != 0 {
            return Some(index * 8 + zero.trailing_zeros() as usize / 8);
        }
    }
    let tail = hay.len() - words.remainder().len();
    words
        .remainder()
        .iter()
        .position(|&b| b == byte)
        .map(|at| tail + at)
}

enum Flow<T> {
    Ready(T),
    Wait(Wait),
}

type Next = Flow<Option<Event<Packet, ReaderEvent>>>;

/// A resumable reader of one elementary audio stream. It performs no I/O: it asks
/// for byte ranges and accepts windows of any size.
#[derive(Debug)]
pub struct Reader<F: Format> {
    input: Input,
    format: F,
    limits: Limits,
    phase: Phase<F::Header>,
    /// The first header of the stream; later headers must continue it.
    reference: Option<F::Header>,
    last: Option<F::Header>,
    track: Option<Track>,
    tags: BudgetVec<Tag>,
    retained: usize,
    announce: bool,
    next_pts: i64,
    audio_start: Option<u64>,
    audio_end: Option<u64>,
    seeking: Option<Seeking>,
    last_seek: Option<SeekDetail>,
}

impl<F: Format> Reader<F> {
    pub fn new(
        pool: &Budget,
        source: SourceId,
        capabilities: SourceCapabilities,
        limits: Limits,
    ) -> Result<Reader<F>> {
        if limits.frame_bytes == 0 || limits.input_window == 0 {
            return Err(Error::Invalid("zero elementary stream limit"));
        }
        let cursor = InputCursor::new(pool, source, capabilities, limits.input_window)?;
        let view = limits
            .frame_bytes
            .saturating_add(usize::try_from(limits.sync_scan_bytes).unwrap_or(usize::MAX))
            .saturating_add(4 * TRAILING_DECIDE);
        Ok(Reader {
            input: Input::new(cursor, view),
            format: F::new(pool, &limits),
            limits,
            phase: Phase::Leading,
            reference: None,
            last: None,
            track: None,
            tags: BudgetVec::new(pool),
            retained: 0,
            announce: false,
            next_pts: 0,
            audio_start: None,
            audio_end: None,
            seeking: None,
            last_seek: None,
        })
    }

    /// The format's own facts: FLAC metadata, the MPEG audio information frame, ...
    pub fn format(&self) -> &F {
        &self.format
    }
    pub fn stream(&self) -> Option<&Track> {
        self.track.as_ref()
    }
    /// Tags found so far: ID3v2 at the start, ID3v1 and APEv2 once reached.
    pub fn tags(&self) -> &[Tag] {
        &self.tags
    }
    /// Offset of the first audio frame, once known.
    pub fn audio_start(&self) -> Option<u64> {
        self.audio_start
    }
    /// Offset where the trailing tags start, once reached.
    pub fn audio_end(&self) -> Option<u64> {
        self.audio_end
    }
    pub fn last_seek(&self) -> Option<SeekDetail> {
        self.last_seek
    }
    /// The byte range the reader wants next.
    pub fn request(&self) -> InputRequest {
        self.input.cursor().request()
    }

    /// `Ready(true)` when the view holds `len` bytes, `Ready(false)` when the source
    /// ended first.
    fn need(
        &mut self,
        len: usize,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Flow<bool>> {
        Ok(match self.input.fill(len, work, cancel)? {
            None => Flow::Ready(true),
            Some(Wait::End) => Flow::Ready(false),
            Some(wait) => Flow::Wait(wait),
        })
    }

    /// Bytes from view offset `at` to the end of the source, when its length is known.
    fn remaining_after(&self, at: usize) -> Option<u64> {
        self.input
            .remaining_in_source()
            .map(|left| left.saturating_sub(at as u64))
    }

    fn continues(&self, header: &F::Header) -> bool {
        self.reference
            .is_none_or(|reference| self.format.continues(&reference, header))
    }

    pub fn step(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<ReaderStep<Self>> {
        loop {
            let next = match self.phase {
                Phase::Leading => self.leading(work, cancel)?,
                Phase::Prelude => self.prelude(work, cancel)?,
                Phase::Discard { left, then } => self.discard(left, then, work, cancel)?,
                Phase::Retain { len, owner, then } => {
                    self.retain(len, owner, then, work, cancel)?
                }
                Phase::Sync { at, scanned } => self.sync(at, scanned, work, cancel)?,
                Phase::Found(header) => self.found(header, work, cancel)?,
                Phase::Frame(cut) => self.frame(cut, work, cancel)?,
                Phase::Trailing => self.trailing(work, cancel)?,
                Phase::End => {
                    return Ok(Step::End(SourceLocation {
                        source: self.input.cursor().source(),
                        offset: self.input.position(),
                    }));
                }
            };
            match next {
                Flow::Ready(None) => {}
                Flow::Ready(Some(event)) => return Ok(Step::Ready(event)),
                Flow::Wait(Wait::Step(Step::NeedInput(request))) => {
                    return Ok(Step::NeedInput(request));
                }
                Flow::Wait(Wait::Step(_)) => return Ok(Step::Yield),
                Flow::Wait(Wait::End) => self.phase = Phase::End,
            }
        }
    }

    // --- Opening ------------------------------------------------------------------

    fn leading(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Next> {
        if let Flow::Wait(wait) = self.need(ID3V2_HEADER, work, cancel)? {
            return Ok(Flow::Wait(wait));
        }
        self.phase = match id3v2(self.input.view()) {
            Leading::Tag(len) => self.keep(len, Owner::Tag(TagKind::Id3v2), Then::Leading),
            Leading::No | Leading::Short => Phase::Prelude,
        };
        Ok(Flow::Ready(None))
    }

    /// Retain `len` bytes if the limits allow it, or record their length and drop them.
    fn keep(&mut self, len: u64, owner: Owner, then: Then<F::Header>) -> Phase<F::Header> {
        let room = self.limits.retained_metadata_bytes - self.retained;
        match usize::try_from(len) {
            Ok(len) if len <= room && len <= self.limits.frame_bytes => {
                self.retained += len;
                Phase::Retain { len, owner, then }
            }
            _ => {
                if let Owner::Tag(kind) = owner {
                    self.record(Tag {
                        kind,
                        offset: self.input.position(),
                        len,
                        bytes: None,
                    });
                }
                Phase::Discard { left: len, then }
            }
        }
    }

    fn record(&mut self, tag: Tag) {
        // A tag list the budget cannot hold loses its tail, never the stream.
        let _ = self.tags.try_push(tag);
    }

    fn prelude(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Next> {
        let mut at_end = false;
        loop {
            match self.format.prelude(self.input.view(), at_end)? {
                Prelude::Need(len) => match self.need(len, work, cancel)? {
                    Flow::Wait(wait) => return Ok(Flow::Wait(wait)),
                    Flow::Ready(true) => {}
                    Flow::Ready(false) if at_end => return Err(Error::Truncated),
                    Flow::Ready(false) => at_end = true,
                },
                Prelude::Skip(len) => {
                    self.phase = Phase::Discard {
                        left: len,
                        then: Then::Prelude,
                    };
                    return Ok(Flow::Ready(None));
                }
                Prelude::Keep(len) => {
                    self.phase = self.keep(len, Owner::Format, Then::Prelude);
                    if matches!(self.phase, Phase::Discard { .. }) {
                        self.format.omit(len)?;
                    }
                    return Ok(Flow::Ready(None));
                }
                Prelude::Done => {
                    self.phase = Phase::Sync { at: 0, scanned: 0 };
                    return Ok(Flow::Ready(None));
                }
            }
        }
    }

    fn discard(
        &mut self,
        mut left: u64,
        then: Then<F::Header>,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Next> {
        while left > 0 {
            match self.input.skip(left, work, cancel)? {
                Ok(count) => left -= count,
                // A stream header that runs past the end leaves no stream to read.
                Err(Wait::End) if matches!(then, Then::Prelude) => return Err(Error::Truncated),
                Err(Wait::End) => return Ok(Flow::Wait(Wait::End)),
                Err(wait) => {
                    self.phase = Phase::Discard { left, then };
                    return Ok(Flow::Wait(wait));
                }
            }
        }
        self.phase = Self::resume(then);
        Ok(Flow::Ready(None))
    }

    fn resume(then: Then<F::Header>) -> Phase<F::Header> {
        match then {
            Then::Leading => Phase::Leading,
            Then::Prelude => Phase::Prelude,
            Then::Sync { scanned } => Phase::Sync { at: 0, scanned },
            Then::Found(header) => Phase::Found(header),
            Then::Frame(lead) => Phase::Frame(Cut::at(lead)),
            Then::Trailing => Phase::Trailing,
            Then::End => Phase::End,
        }
    }

    fn retain(
        &mut self,
        len: usize,
        owner: Owner,
        then: Then<F::Header>,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Next> {
        match self.need(len, work, cancel)? {
            Flow::Wait(wait) => return Ok(Flow::Wait(wait)),
            Flow::Ready(false) => return Err(Error::Truncated),
            Flow::Ready(true) => {}
        }
        let offset = self.input.position();
        let Some(bytes) = self.input.take(len, work, cancel)? else {
            return Ok(Flow::Wait(Wait::Step(Step::Yield)));
        };
        match owner {
            Owner::Tag(kind) => self.record(Tag {
                kind,
                offset,
                len: len as u64,
                bytes: Some(bytes),
            }),
            Owner::Format => self.format.keep(bytes)?,
        }
        self.phase = Self::resume(then);
        Ok(Flow::Ready(None))
    }

    // --- Synchronisation ----------------------------------------------------------

    /// Whether a sync candidate at `at` is followed by `depth` frames of the same
    /// stream, or by the end of the audio. `Err(len)` asks for a view of `len` bytes.
    fn chain(
        &self,
        view: &[u8],
        at: usize,
        header: F::Header,
        depth: u8,
        at_end: bool,
    ) -> std::result::Result<bool, usize> {
        if depth == 0 {
            return Ok(true);
        }
        let next = match self.format.length(&header) {
            Length::Known(len) => at + len,
            Length::Search { minimum } => {
                match self.confirmed_end(view, at, header, minimum, at_end)? {
                    Some(next) => next,
                    None => return Ok(false),
                }
            }
        };
        if next > view.len() {
            return if at_end {
                Ok(false)
            } else {
                Err(next + F::PROBE)
            };
        }
        let rest = &view[next..];
        if rest.is_empty() && at_end {
            return Ok(true);
        }
        match trailing(rest, self.remaining_after(next), at_end) {
            Trailing::Tag(_) => return Ok(true),
            Trailing::Undecided => return Err(next + TRAILING_DECIDE),
            Trailing::No => {}
        }
        match self.format.probe(rest) {
            Probe::Header(following)
                if self.format.continues(&header, &following)
                    || self.format.joins(&header, &following) =>
            {
                self.chain(view, next, following, depth - 1, at_end)
            }
            Probe::Short if !at_end => Err(next + F::PROBE),
            _ => Ok(false),
        }
    }

    /// The end of a frame at `at` that its own check confirms, `None` when there is none
    /// within the frame limit. Used to trust a sync candidate, so damaged frames fail.
    fn confirmed_end(
        &self,
        view: &[u8],
        at: usize,
        header: F::Header,
        minimum: usize,
        at_end: bool,
    ) -> std::result::Result<Option<usize>, usize> {
        let limit = at.saturating_add(self.limits.frame_bytes);
        let stop = view.len().min(limit);
        let mut p = at + minimum.max(1);
        while p < stop {
            let Some(offset) = view[p..stop].iter().position(|&b| b == F::SYNC) else {
                break;
            };
            p += offset;
            match self.format.probe(&view[p..]) {
                Probe::Header(next)
                    if self.format.continues(&header, &next)
                        && self.format.checks(&view[at..p]) =>
                {
                    return Ok(Some(p));
                }
                Probe::Short if !at_end => return Err(p + F::PROBE),
                _ => p += 1,
            }
        }
        if view.len() >= limit {
            return Ok(None);
        }
        if at_end {
            let confirmed = self.format.checks(&view[at..]);
            return Ok(confirmed.then_some(view.len()));
        }
        Err((view.len() * 2)
            .max(view.len() + SCAN_CHUNK)
            .min(limit + F::PROBE))
    }

    fn sync(
        &mut self,
        mut at: usize,
        scanned: u64,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Next> {
        loop {
            if scanned + at as u64 > self.limits.sync_scan_bytes {
                return Err(Error::Policy("no frame sync within the scan limit"));
            }
            if at >= SCAN_CHUNK {
                // Drop what the scan has passed: a candidate needs only what follows it.
                self.phase = Phase::Discard {
                    left: at as u64,
                    then: Then::Sync {
                        scanned: scanned + at as u64,
                    },
                };
                return Ok(Flow::Ready(None));
            }
            let at_end = match self.need(at + F::PROBE, work, cancel)? {
                Flow::Wait(wait) => {
                    self.phase = Phase::Sync { at, scanned };
                    return Ok(Flow::Wait(wait));
                }
                Flow::Ready(_) => self.input.ended(),
            };
            let view = self.input.view();
            let candidate = match scan(view, at, F::SYNC, work) {
                Scan::Found(candidate) => candidate,
                Scan::Passed(next) if next < view.len() || !at_end => {
                    at = next;
                    continue;
                }
                Scan::Passed(_) => return self.sync_exhausted(),
                Scan::Spent => {
                    self.phase = Phase::Sync { at, scanned };
                    return Ok(Flow::Wait(Wait::Step(Step::Yield)));
                }
            };
            let header = match self.format.probe(&view[candidate..]) {
                Probe::Header(header) if self.continues(&header) => header,
                Probe::Short if !at_end => {
                    at = candidate;
                    if let Flow::Wait(wait) = self.need(candidate + F::PROBE, work, cancel)? {
                        self.phase = Phase::Sync { at, scanned };
                        return Ok(Flow::Wait(wait));
                    }
                    continue;
                }
                _ => {
                    at = candidate + 1;
                    continue;
                }
            };
            match self.chain(view, candidate, header, F::CONFIRM, at_end) {
                Ok(true) => {
                    self.phase = if candidate == 0 {
                        Phase::Found(header)
                    } else {
                        Phase::Discard {
                            left: candidate as u64,
                            then: Then::Found(header),
                        }
                    };
                    if let Some(seeking) = self.seeking.as_mut() {
                        seeking.scanned += scanned + candidate as u64;
                    }
                    return Ok(Flow::Ready(None));
                }
                Ok(false) => at = candidate + 1,
                Err(len) => {
                    at = candidate;
                    if let Flow::Wait(wait) = self.need(len, work, cancel)? {
                        self.phase = Phase::Sync { at, scanned };
                        return Ok(Flow::Wait(wait));
                    }
                }
            }
        }
    }

    /// The source ended without a sync.
    fn sync_exhausted(&mut self) -> Result<Next> {
        if let Some(seeking) = self.seeking {
            if let Plan::Bisect { low, method, .. } = seeking.plan
                && !seeking.forward
            {
                // No frame starts after the probe: the target lies before it.
                self.set_plan(Plan::Bisect {
                    low,
                    high: seeking.probe,
                    method,
                });
                return self
                    .jump()
                    .map(|request| Flow::Wait(Wait::Step(Step::NeedInput(request))));
            }
            if let Plan::Estimate { .. } = seeking.plan
                && !seeking.forward
            {
                // The estimate fell past the last frame: count from the start instead.
                if let Some(seeking) = self.seeking.as_mut() {
                    seeking.plan = Plan::Scan;
                    seeking.method = SeekMethod::Restart;
                    seeking.basis = SeekBasis::Exact;
                }
                return self
                    .jump()
                    .map(|request| Flow::Wait(Wait::Step(Step::NeedInput(request))));
            }
            self.seeking = None;
            return Ok(Flow::Wait(Wait::End));
        }
        if self.track.is_none() {
            return Err(Error::Invalid("no frame in the source"));
        }
        Ok(Flow::Wait(Wait::End))
    }

    /// A confirmed header starts the view.
    fn found(
        &mut self,
        header: F::Header,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Next> {
        if self.seeking.is_some() {
            return self.seek_found(header);
        }
        if self.track.is_none() {
            let facts = self.format.adopt(&header, self.input.cursor().budget())?;
            let time_base = TimeBase::new(1, u64::from(facts.sample_rate))?;
            self.track = Some(Track { facts, time_base });
            self.reference = Some(header);
            self.announce = true;
        }
        if !self.announce {
            self.phase = Phase::Frame(Cut::found(header));
            return Ok(Flow::Ready(None));
        }
        let offset = self.input.position();
        let mut start = offset;
        if let Some(len) = self.format.inspect_first(&header) {
            if let Flow::Wait(wait) = self.need(len, work, cancel)? {
                return Ok(Flow::Wait(wait));
            }
            let view = self.input.view();
            let frame = &view[..len.min(view.len())];
            let length = self.input.capabilities().length;
            if self.format.describes(frame, &header, offset, length) {
                start += len as u64;
                self.phase = Phase::Discard {
                    left: len as u64,
                    then: Then::Frame(0),
                };
            } else {
                self.phase = Phase::Frame(Cut::found(header));
            }
        } else {
            self.phase = Phase::Frame(Cut::found(header));
        }
        self.audio_start = Some(start);
        self.announce = false;
        if let (Some(track), Some(duration)) = (self.track.as_mut(), self.format.duration()) {
            track.facts.duration = Some(duration);
        }
        Ok(Flow::Ready(Some(Event::Other(ReaderEvent::Opened))))
    }

    // --- Access units -------------------------------------------------------------

    fn frame(
        &mut self,
        mut cut: Cut<F::Header>,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Next> {
        match self.delimit(&mut cut, work, cancel)? {
            Flow::Wait(wait) => {
                self.phase = Phase::Frame(cut);
                Ok(Flow::Wait(wait))
            }
            Flow::Ready(None) => {
                // No frame where one should start: drop bytes up to the next sync.
                self.phase = Phase::Sync { at: 0, scanned: 0 };
                Ok(Flow::Ready(None))
            }
            Flow::Ready(Some(unit)) => self.emit(cut, unit, work, cancel),
        }
    }

    /// Find the extent of the unit at `cut` and what follows it. `Ready(None)` when no
    /// header starts the unit.
    fn delimit(
        &mut self,
        cut: &mut Cut<F::Header>,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Flow<Option<Unit<F::Header>>>> {
        loop {
            let header = if let Some(header) = cut.header {
                header
            } else {
                let at_end = match self.need(cut.frame + F::PROBE, work, cancel)? {
                    Flow::Wait(wait) => return Ok(Flow::Wait(wait)),
                    Flow::Ready(_) => self.input.ended(),
                };
                let view = self.input.view();
                if at_end && cut.frame >= view.len() {
                    // Bytes after the last frame that start none (FFmpeg delivers them).
                    return Ok(match (cut.lead, self.last) {
                        (0, _) | (_, None) => Flow::Wait(Wait::End),
                        (lead, Some(header)) => Flow::Ready(Some(Unit {
                            len: lead.min(view.len()),
                            header,
                            complete: false,
                            after: After::End,
                        })),
                    });
                }
                match self.format.probe(&view[cut.frame..]) {
                    Probe::Header(header) if self.continues(&header) => header,
                    _ if cut.frame == cut.lead => return Ok(Flow::Ready(None)),
                    _ => return Err(Error::Invalid("joined frame lost its header")),
                }
            };
            cut.header = Some(header);
            let unit = *cut.unit.get_or_insert(header);
            let end = match cut.end {
                Some(end) => end,
                None => match self.format.length(&header) {
                    Length::Known(len) => cut.frame + len,
                    Length::Search { minimum } => {
                        match self.search_end(cut, header, minimum, work, cancel)? {
                            Flow::Wait(wait) => return Ok(Flow::Wait(wait)),
                            Flow::Ready(end) => end,
                        }
                    }
                },
            };
            if end - cut.lead > self.limits.frame_bytes {
                return Err(Error::Limit {
                    requested: end - cut.lead,
                    maximum: self.limits.frame_bytes,
                });
            }
            cut.end = Some(end);
            let mut at_end = match self.need(end + F::PROBE.max(TRAILING_PROBE), work, cancel)? {
                Flow::Wait(wait) => return Ok(Flow::Wait(wait)),
                Flow::Ready(_) => self.input.ended(),
            };
            let view_len = self.input.view().len();
            if end > view_len {
                return Ok(Flow::Ready(Some(Unit {
                    len: view_len,
                    header: unit,
                    complete: false,
                    after: After::End,
                })));
            }
            if end == view_len && at_end {
                return Ok(Flow::Ready(Some(Unit {
                    len: end,
                    header: unit,
                    complete: true,
                    after: After::End,
                })));
            }
            let mut decision =
                trailing(&self.input.view()[end..], self.remaining_after(end), at_end);
            if decision == Trailing::Undecided {
                at_end = match self.need(end + TRAILING_DECIDE, work, cancel)? {
                    Flow::Wait(wait) => return Ok(Flow::Wait(wait)),
                    Flow::Ready(_) => self.input.ended(),
                };
                decision = trailing(&self.input.view()[end..], self.remaining_after(end), at_end);
            }
            if let Trailing::Tag(_) = decision {
                return Ok(Flow::Ready(Some(Unit {
                    len: end,
                    header: unit,
                    complete: true,
                    after: After::Trailing,
                })));
            }
            if let Probe::Header(next) = self.format.probe(&self.input.view()[end..])
                && self.continues(&next)
            {
                if self.format.joins(&unit, &next) {
                    cut.frame = end;
                    cut.header = Some(next);
                    cut.end = None;
                    continue;
                }
                return Ok(Flow::Ready(Some(Unit {
                    len: end,
                    header: unit,
                    complete: true,
                    after: After::Frame { lead: 0 },
                })));
            }
            let found = match self.resync(cut, end, work, cancel)? {
                Flow::Wait(wait) => return Ok(Flow::Wait(wait)),
                Flow::Ready(found) => found,
            };
            let view_len = self.input.view().len();
            let (len, after) = match (F::GAP, found) {
                (Gap::AppendToPrevious, Some(next)) => (next, After::Frame { lead: 0 }),
                (Gap::AppendToPrevious, None) => (view_len, After::End),
                (Gap::PrependToNext, Some(next)) => (end, After::Frame { lead: next - end }),
                (Gap::PrependToNext, None) => (
                    end,
                    After::Frame {
                        lead: view_len - end,
                    },
                ),
                (Gap::Drop, Some(next)) => (end, After::Drop(next - end)),
                (Gap::Drop, None) => (end, After::End),
            };
            return Ok(Flow::Ready(Some(Unit {
                len,
                header: unit,
                complete: true,
                after,
            })));
        }
    }

    /// Search for the end of a frame whose header gives no length: the next header of
    /// the stream that the format accepts as its end, or the end of the audio.
    fn search_end(
        &mut self,
        cut: &mut Cut<F::Header>,
        header: F::Header,
        minimum: usize,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Flow<usize>> {
        let mut p = cut.search.max(cut.frame + minimum.max(1));
        loop {
            let at_end = match self.need(p + F::PROBE, work, cancel)? {
                Flow::Wait(wait) => {
                    cut.search = p;
                    return Ok(Flow::Wait(wait));
                }
                Flow::Ready(_) => self.input.ended(),
            };
            if p - cut.frame > self.limits.frame_bytes {
                return Err(Error::Limit {
                    requested: p - cut.frame,
                    maximum: self.limits.frame_bytes,
                });
            }
            let view = self.input.view();
            let candidate = match scan(view, p, F::SYNC, work) {
                Scan::Found(candidate) => candidate,
                Scan::Passed(next) if next < view.len() || !at_end => {
                    p = next;
                    continue;
                }
                Scan::Passed(_) => {
                    return Ok(Flow::Ready(self.tail_end(cut.frame, minimum)));
                }
                Scan::Spent => {
                    cut.search = p;
                    return Ok(Flow::Wait(Wait::Step(Step::Yield)));
                }
            };
            match self.format.probe(&view[candidate..]) {
                Probe::Header(next)
                    if self.continues(&next)
                        && (self.format.numbers_on(&header, &next)
                            || self.format.checks(&view[cut.frame..candidate])) =>
                {
                    return Ok(Flow::Ready(candidate));
                }
                Probe::Short if !at_end => {
                    p = candidate;
                    if let Flow::Wait(wait) = self.need(candidate + F::PROBE, work, cancel)? {
                        cut.search = p;
                        return Ok(Flow::Wait(wait));
                    }
                }
                _ => p = candidate + 1,
            }
        }
    }

    /// The end of the last frame when the search reached the end of the source: before
    /// a trailing tag if the frame's own check confirms that end, else the source end.
    fn tail_end(&self, frame: usize, minimum: usize) -> usize {
        let view = self.input.view();
        let earliest = frame + minimum;
        let confirmed = |end: usize| self.format.checks(&view[frame..end]);
        if let Some(ape) = view
            .get(earliest..)
            .and_then(|rest| rest.windows(8).position(|w| w == b"APETAGEX"))
            .map(|at| earliest + at)
            && confirmed(ape)
        {
            return ape;
        }
        let id3v1 = view.len().saturating_sub(128);
        if id3v1 >= earliest && view[id3v1..].starts_with(b"TAG") && confirmed(id3v1) {
            return id3v1;
        }
        view.len()
    }

    /// The next confirmed header of the stream after a frame ending at `end`, `None`
    /// when the source ends first.
    fn resync(
        &mut self,
        cut: &mut Cut<F::Header>,
        end: usize,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Flow<Option<usize>>> {
        let mut q = cut.gap.max(end + 1);
        loop {
            if (q - end) as u64 > self.limits.sync_scan_bytes {
                return Err(Error::Policy("frame sync lost beyond the scan limit"));
            }
            let at_end = match self.need(q + F::PROBE, work, cancel)? {
                Flow::Wait(wait) => {
                    cut.gap = q;
                    return Ok(Flow::Wait(wait));
                }
                Flow::Ready(_) => self.input.ended(),
            };
            let view = self.input.view();
            let candidate = match scan(view, q, F::SYNC, work) {
                Scan::Found(candidate) => candidate,
                Scan::Passed(next) if next < view.len() || !at_end => {
                    q = next;
                    continue;
                }
                Scan::Passed(_) => return Ok(Flow::Ready(None)),
                Scan::Spent => {
                    cut.gap = q;
                    return Ok(Flow::Wait(Wait::Step(Step::Yield)));
                }
            };
            let header = match self.format.probe(&view[candidate..]) {
                Probe::Header(header) if self.continues(&header) => header,
                Probe::Short if !at_end => {
                    q = candidate;
                    if let Flow::Wait(wait) = self.need(candidate + F::PROBE, work, cancel)? {
                        cut.gap = q;
                        return Ok(Flow::Wait(wait));
                    }
                    continue;
                }
                _ => {
                    q = candidate + 1;
                    continue;
                }
            };
            match self.chain(view, candidate, header, 1, at_end) {
                Ok(true) => return Ok(Flow::Ready(Some(candidate))),
                Ok(false) => q = candidate + 1,
                Err(len) => {
                    q = candidate;
                    if let Flow::Wait(wait) = self.need(len, work, cancel)? {
                        cut.gap = q;
                        return Ok(Flow::Wait(wait));
                    }
                }
            }
        }
    }

    /// Deliver, skip or land on a delimited unit.
    fn emit(
        &mut self,
        cut: Cut<F::Header>,
        unit: Unit<F::Header>,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Next> {
        let header = unit.header;
        let pts = self
            .format
            .first_sample(&header)
            .map_or(Ok(self.next_pts), i64::try_from)
            .map_err(|_| Error::Overflow)?;
        let duration = self.format.samples(&header);
        let (extra, then) = unit.after.then();
        if let Some(seeking) = self.seeking
            && seeking.forward
        {
            let preroll = match seeking.policy {
                SeekPolicy::PreviousRandomAccess => 0,
                SeekPolicy::PlaybackPreroll => i64::from(F::PREROLL),
            };
            let reach = pts + i64::from(duration) * (1 + preroll);
            let target = i64::try_from(seeking.target).map_err(|_| Error::Overflow)?;
            if reach <= target && !matches!(unit.after, After::End) {
                // Before the target: read past the unit without delivering it.
                self.next_pts = pts + i64::from(duration);
                if let Some(seeking) = self.seeking.as_mut() {
                    seeking.scanned += unit.len as u64;
                }
                self.phase = Phase::Discard {
                    left: unit.len as u64 + extra,
                    then,
                };
                return Ok(Flow::Ready(None));
            }
            self.phase = Phase::Frame(cut);
            return Ok(Flow::Ready(Some(Event::Seek(self.land(pts)?))));
        }
        let offset = self.input.position();
        let Some(data) = self.input.take(unit.len, work, cancel)? else {
            self.phase = Phase::Frame(cut);
            return Ok(Flow::Wait(Wait::Step(Step::Yield)));
        };
        let time_base = self
            .track
            .as_ref()
            .map(Track::time_base)
            .ok_or(Error::Invalid("packet before the stream is open"))?;
        let packet = Packet {
            data,
            offset,
            pts,
            duration,
            time_base,
            trim: self.format.trim(pts, &header),
            complete: unit.complete,
        };
        self.next_pts = pts + i64::from(duration);
        self.last = Some(header);
        self.phase = if extra > 0 {
            Phase::Discard { left: extra, then }
        } else {
            Self::resume(then)
        };
        Ok(Flow::Ready(Some(Event::Packet(packet))))
    }

    fn trailing(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Next> {
        let at_end = match self.need(TRAILING_DECIDE, work, cancel)? {
            Flow::Wait(wait) => return Ok(Flow::Wait(wait)),
            Flow::Ready(_) => self.input.ended(),
        };
        let kind = match trailing(self.input.view(), self.input.remaining_in_source(), at_end) {
            Trailing::Tag(kind) => kind,
            _ => TagKind::Ape,
        };
        self.audio_end = Some(self.input.position());
        // An APE tag runs to the end, with any ID3v1 after it.
        let len = match (kind, self.input.remaining_in_source()) {
            (TagKind::Id3v1, _) => 128,
            (_, Some(left)) => left,
            (_, None) => {
                let room = self.limits.retained_metadata_bytes - self.retained;
                match self.need(room.saturating_add(1), work, cancel)? {
                    Flow::Wait(wait) => return Ok(Flow::Wait(wait)),
                    Flow::Ready(true) => u64::MAX,
                    Flow::Ready(false) => self.input.view().len() as u64,
                }
            }
        };
        self.phase = self.keep(len, Owner::Tag(kind), Then::End);
        Ok(Flow::Ready(None))
    }

    /// Restart from the start of the source, forgetting the stream.
    pub fn reset(&mut self) -> Result<InputRequest> {
        self.phase = Phase::Leading;
        self.reference = None;
        self.last = None;
        self.track = None;
        self.tags.clear();
        self.retained = 0;
        self.announce = false;
        self.next_pts = 0;
        self.audio_start = None;
        self.audio_end = None;
        self.seeking = None;
        self.format.reset();
        self.input.reset(0)
    }
}

impl<F: Format> ContainerReader for Reader<F> {
    type Packet = Packet;
    type Track = Track;
    type Other = ReaderEvent;

    fn provide(&mut self, window: &InputWindow) -> Result<()> {
        self.input.cursor_mut().provide(window)
    }
    fn finish(&mut self, source: SourceId, token: InputToken) -> Result<()> {
        self.input.cursor_mut().finish(source, token)
    }
    fn fail(&mut self, source: SourceId, token: InputToken, failure: SourceFailure) -> Result<()> {
        self.input.cursor_mut().fail(source, token, failure)
    }
    fn reset(&mut self) -> Result<InputRequest> {
        Reader::reset(self)
    }
    #[inline]
    fn step(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<ReaderStep<Self>> {
        Reader::step(self, work, cancel)
    }
    fn request_seek(&mut self, request: SeekRequest) -> Result<InputRequest> {
        Reader::request_seek(self, request)
    }
    fn track_count(&self) -> usize {
        usize::from(self.track.is_some())
    }
    fn track(&self, index: usize) -> Option<&Track> {
        self.track.as_ref().filter(|_| index == 0)
    }
    fn capabilities(&self) -> SourceCapabilities {
        self.input.capabilities()
    }
}

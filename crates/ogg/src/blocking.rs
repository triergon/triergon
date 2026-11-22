//! A blocking adapter over [`std::io`] sources.
use crate::queue::Queue;
use crate::{Limits, Mapping, Packet, Reader, ReaderEvent, Track};
use std::io::{Read, Seek};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::blocking::{Blocking, seekable_capabilities};
use triergon_core::container::{ContainerReader, Event, SeekReceipt, SeekRequest};
use triergon_core::{Budget, BudgetVec, Error, Result, SourceCapabilities, SourceId};

/// Bytes the reader requests per window.
const WINDOW: usize = 64 * 1024;

/// Drives a [`Reader`] from a [`Read`] source, seeking it when it is also [`Seek`].
#[derive(Debug)]
pub struct BlockingReader<R> {
    driver: Blocking<Reader, R>,
    /// Events read ahead while opening or seeking, charged to the reader's pool.
    queued: Queue<Event<Packet, ReaderEvent>>,
    recovery: Option<InitialVorbis>,
    delivered: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Initial {
    Unseen,
    Pending,
    Done,
}

#[derive(Debug)]
struct InitialVorbis {
    states: BudgetVec<Initial>,
    held: Queue<Event<Packet, ReaderEvent>>,
    max_events: usize,
    remaining: usize,
    pending: usize,
    draining: bool,
    error: Option<Error>,
}

impl InitialVorbis {
    fn abandon(&mut self) {
        self.remaining = 0;
        self.pending = 0;
        self.draining = true;
    }

    fn observe(&mut self, event: &Event<Packet, ReaderEvent>) {
        match event {
            Event::Other(ReaderEvent::Link { .. }) => self.abandon(),
            Event::Other(ReaderEvent::Lost { track, .. }) => {
                if self.states.get(*track) == Some(&Initial::Pending) {
                    self.states[*track] = Initial::Done;
                    self.remaining -= 1;
                    self.pending -= 1;
                }
            }
            Event::Other(ReaderEvent::Resync { .. }) => {
                for state in self.states.as_mut_slice() {
                    if *state == Initial::Pending {
                        *state = Initial::Done;
                    }
                }
                self.remaining -= self.pending;
                self.pending = 0;
            }
            Event::Packet(packet) => {
                let index = packet.track_index();
                let Some(state) = self.states.get(index).copied() else {
                    return;
                };
                if state == Initial::Done {
                    return;
                }
                let timing = packet.reference;
                let zero = packet.granule() == Some(0)
                    && timing.pts.is_none()
                    && timing.duration.is_some();
                match (state, zero) {
                    (Initial::Unseen, true) => {
                        self.states[index] = Initial::Pending;
                        self.pending += 1;
                    }
                    (Initial::Unseen, false) => {
                        self.states[index] = Initial::Done;
                        self.remaining -= 1;
                    }
                    (Initial::Pending, false) => {
                        if let Some(anchor) = timing.pts {
                            self.backfill(index, anchor);
                        }
                        self.states[index] = Initial::Done;
                        self.pending -= 1;
                        self.remaining -= 1;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn backfill(&mut self, index: usize, anchor: i64) {
        let mut time = Some(anchor);
        for event in self.held.iter_mut().rev() {
            if let Event::Packet(packet) = event
                && packet.track_index() == index
            {
                time = time
                    .zip(packet.reference.duration)
                    .and_then(|(t, d)| t.checked_sub(d));
            }
        }
        if time.is_none() {
            return;
        }
        let mut time = anchor;
        for event in self.held.iter_mut().rev() {
            if let Event::Packet(packet) = event
                && packet.track_index() == index
                && let Some(duration) = packet.reference.duration
            {
                time -= duration;
                packet.reference.pts = Some(time);
            }
        }
    }
}

impl<R: Read + Seek> BlockingReader<R> {
    /// Open a seekable source: seeking bisects and track ends are scanned.
    pub fn open(source: R, limits: Limits) -> Result<Self> {
        let pool = Budget::new(limits.live_storage_bytes)?;
        Self::open_in(source, limits, &pool)
    }

    /// [`BlockingReader::open`] charging a caller's pool.
    pub fn open_in(mut source: R, limits: Limits, pool: &Budget) -> Result<Self> {
        let capabilities = seekable_capabilities(&mut source)?;
        let reader = Reader::new(pool, SourceId(1), capabilities, limits, WINDOW)?;
        Self::start(Blocking::seekable(reader, source, pool), pool)
    }
}

impl<R: Read> BlockingReader<R> {
    /// Open a sequential source; it cannot seek.
    pub fn open_stream(source: R, limits: Limits) -> Result<Self> {
        let pool = Budget::new(limits.live_storage_bytes)?;
        let capabilities = SourceCapabilities {
            length: None,
            random_access: false,
        };
        let reader = Reader::new(&pool, SourceId(1), capabilities, limits, WINDOW)?;
        Self::start(Blocking::sequential(reader, source, &pool), &pool)
    }

    /// Read until the first packet, so the first link's tracks are known.
    fn start(driver: Blocking<Reader, R>, pool: &Budget) -> Result<Self> {
        let mut blocking = BlockingReader {
            driver,
            queued: Queue::new(pool),
            recovery: None,
            delivered: false,
        };
        loop {
            blocking.queued.reserve(1)?;
            let Some(event) = blocking.driver.next_event()? else {
                break;
            };
            let packet = matches!(event, Event::Packet(_));
            blocking.queued.push(event)?;
            if packet {
                break;
            }
        }
        Ok(blocking)
    }

    /// Enable bounded initial Vorbis clock recovery before reading events.
    /// A full prefix is delivered unchanged if `max_events` is reached.
    pub fn recover_initial_vorbis(
        &mut self,
        policy: crate::compat::clock::InitialVorbis,
    ) -> Result<()> {
        let max_events = policy.max_events;
        if self.delivered || self.recovery.is_some() {
            return Err(Error::Invalid(
                "initial Vorbis recovery must be enabled before delivery",
            ));
        }
        if max_events == 0 {
            return Err(Error::Invalid("initial Vorbis event limit is zero"));
        }
        let pool = self.queued.budget();
        let tracks = self.tracks();
        let remaining = tracks
            .iter()
            .filter(|t| t.mapping() == Mapping::Vorbis)
            .count();
        if remaining == 0 {
            return Ok(());
        }
        let mut states = BudgetVec::try_filled(pool, Initial::Done, tracks.len())?;
        for (state, track) in states.as_mut_slice().iter_mut().zip(tracks) {
            if track.mapping() == Mapping::Vorbis {
                *state = Initial::Unseen;
            }
        }
        self.recovery = Some(InitialVorbis {
            states,
            held: Queue::new(pool),
            max_events,
            remaining,
            pending: 0,
            draining: false,
            error: None,
        });
        Ok(())
    }

    /// Tracks seen so far; every track of the first link once open returns.
    pub fn tracks(&self) -> &[Track] {
        self.reader().tracks()
    }

    /// The underlying reader.
    pub fn reader(&self) -> &Reader {
        self.driver.reader()
    }

    /// The next event; `None` at the end of the source.
    pub fn next_event(
        &mut self,
        cancel: &AtomicBool,
    ) -> Result<Option<Event<Packet, ReaderEvent>>> {
        self.delivered = true;
        while let Some(recovery) = &mut self.recovery {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if recovery.draining {
                if let Some(event) = recovery.held.pop() {
                    return Ok(Some(event));
                }
                if recovery.remaining == 0 {
                    let error = recovery.error.take();
                    self.recovery = None;
                    if let Some(error) = error {
                        return Err(error);
                    }
                    break;
                }
                recovery.draining = false;
                if let Some(error) = recovery.error.take() {
                    return Err(error);
                }
            }
            if recovery.held.len() == recovery.max_events {
                recovery.abandon();
                continue;
            }
            if let Err(error) = recovery.held.reserve(1) {
                if recovery.held.len() == 0 {
                    return Err(error);
                }
                recovery.error = Some(error);
                recovery.abandon();
                continue;
            }
            let result = match self.queued.pop() {
                Some(event) => Ok(Some(event)),
                None => self.driver.next_event_or_cancel(cancel),
            };
            match result {
                Ok(Some(event)) => {
                    recovery.observe(&event);
                    recovery.held.push(event)?;
                    if recovery.pending == 0 {
                        recovery.draining = true;
                    }
                }
                Ok(None) => {
                    if recovery.held.len() == 0 {
                        self.recovery = None;
                        return Ok(None);
                    }
                    recovery.abandon();
                }
                Err(Error::Cancelled) => return Err(Error::Cancelled),
                Err(error) => {
                    if recovery.held.len() == 0 {
                        return Err(error);
                    }
                    recovery.error = Some(error);
                    recovery.abandon();
                }
            }
        }
        match self.queued.pop() {
            Some(event) => Ok(Some(event)),
            None => self.driver.next_event_or_cancel(cancel),
        }
    }

    /// The next packet, passing other events by.
    pub fn next_packet(&mut self, cancel: &AtomicBool) -> Result<Option<Packet>> {
        while let Some(event) = self.next_event(cancel)? {
            if let Event::Packet(packet) = event {
                return Ok(Some(packet));
            }
        }
        Ok(None)
    }

    /// Seek and wait for the receipt; the packets after it follow from
    /// [`BlockingReader::next_packet`].
    pub fn seek(&mut self, request: SeekRequest, cancel: &AtomicBool) -> Result<SeekReceipt> {
        if !self.reader().capabilities().random_access {
            return Err(Error::Unsupported("seeking a sequential source"));
        }
        self.delivered = true;
        if let Some(recovery) = &mut self.recovery {
            recovery
                .held
                .retain(|event| matches!(event, Event::Other(_)));
            recovery.abandon();
            recovery.error = None;
        }
        self.queued.retain(|event| matches!(event, Event::Other(_)));
        self.driver.seek(request)?;
        loop {
            self.queued.reserve(1)?;
            match self.driver.next_event_or_cancel(cancel)? {
                Some(Event::Seek(receipt)) => return Ok(receipt),
                Some(other) => self.queued.push(other)?,
                None => return Err(Error::Truncated),
            }
        }
    }
}

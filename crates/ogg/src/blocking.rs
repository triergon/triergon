//! A blocking adapter over [`std::io`] sources.
use crate::queue::Queue;
use crate::{Limits, Packet, Reader, ReaderEvent, Track};
use std::io::{Read, Seek};
use std::sync::atomic::AtomicBool;
use triergon_core::blocking::{Blocking, seekable_capabilities};
use triergon_core::container::{ContainerReader, Event, SeekReceipt, SeekRequest};
use triergon_core::{Budget, Error, Result, SourceCapabilities, SourceId};

/// Bytes the reader requests per window.
const WINDOW: usize = 64 * 1024;

/// Drives a [`Reader`] from a [`Read`] source, seeking it when it is also [`Seek`].
#[derive(Debug)]
pub struct BlockingReader<R> {
    driver: Blocking<Reader, R>,
    /// Events read ahead while opening or seeking, charged to the reader's pool.
    queued: Queue<Event<Packet, ReaderEvent>>,
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
        Self::start(Blocking::seekable(reader, source, pool), pool, limits)
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
        Self::start(Blocking::sequential(reader, source, &pool), &pool, limits)
    }

    /// Read until the first packet after every stream of the first link has read its
    /// headers, queueing at most `limits.header_bytes` of packets meanwhile.
    fn start(driver: Blocking<Reader, R>, pool: &Budget, limits: Limits) -> Result<Self> {
        let mut blocking = BlockingReader {
            driver,
            queued: Queue::new(pool),
        };
        let mut queued_bytes = 0usize;
        loop {
            blocking.queued.reserve(1)?;
            let Some(event) = blocking.driver.next_event()? else {
                break;
            };
            let packet = match &event {
                Event::Packet(packet) => Some(packet.payload.len()),
                _ => None,
            };
            blocking.queued.push(event)?;
            if let Some(len) = packet {
                queued_bytes = queued_bytes.saturating_add(len);
                if blocking.reader().headers_read() || queued_bytes > limits.header_bytes {
                    break;
                }
            }
        }
        Ok(blocking)
    }

    /// Tracks seen so far; every track of the first link, configured unless its headers
    /// are broken or still missing after `Limits::header_bytes` of other packets, once
    /// open returns.
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

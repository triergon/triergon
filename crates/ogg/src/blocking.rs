//! A blocking adapter over [`std::io`] sources.
use crate::{Limits, Packet, Reader, ReaderEvent, Track};
use std::io::{Read, Seek, SeekFrom};
use std::sync::atomic::AtomicBool;
use triergon_core::container::{ContainerReader, Event, SeekReceipt, SeekRequest};
use triergon_core::{
    Budget, Bytes, Error, InputRequest, InputWindow, Result, SourceCapabilities, SourceId, Step,
    WorkBudget,
};

const WINDOW: usize = 64 * 1024;
const WORK: u64 = 1 << 20;

type Reposition<R> = fn(&mut R, u64) -> std::io::Result<u64>;

/// Drives a [`Reader`] from a [`Read`] source, seeking it when it is also [`Seek`].
#[derive(Debug)]
pub struct BlockingReader<R> {
    reader: Reader,
    source: R,
    pool: Budget,
    position: u64,
    reposition: Option<Reposition<R>>,
    buffer: Vec<u8>,
    /// Events read ahead while opening.
    queued: std::collections::VecDeque<Event<Packet, ReaderEvent>>,
}

impl<R: Read + Seek> BlockingReader<R> {
    /// Open a seekable source: seeking bisects and track ends are scanned.
    pub fn open(source: R, limits: Limits) -> Result<Self> {
        let pool = Budget::new(limits.live_storage_bytes)?;
        Self::open_in(source, limits, &pool)
    }

    /// [`BlockingReader::open`] charging a caller's pool.
    pub fn open_in(mut source: R, limits: Limits, pool: &Budget) -> Result<Self> {
        let length = source
            .seek(SeekFrom::End(0))
            .map_err(|e| Error::Io(e.kind()))?;
        source.rewind().map_err(|e| Error::Io(e.kind()))?;
        let capabilities = SourceCapabilities {
            length: Some(length),
            random_access: true,
        };
        Self::construct(
            source,
            limits,
            pool,
            capabilities,
            Some(|r, at| r.seek(SeekFrom::Start(at))),
        )
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
        Self::construct(source, limits, &pool, capabilities, None)
    }

    fn construct(
        source: R,
        limits: Limits,
        pool: &Budget,
        capabilities: SourceCapabilities,
        reposition: Option<Reposition<R>>,
    ) -> Result<Self> {
        let reader = Reader::new(pool, SourceId(1), capabilities, limits, WINDOW)?;
        let mut blocking = BlockingReader {
            reader,
            source,
            pool: pool.clone(),
            position: 0,
            reposition,
            buffer: vec![0; WINDOW],
            queued: std::collections::VecDeque::new(),
        };
        // Read until the first packet, so the first link's tracks are known.
        let cancel = AtomicBool::new(false);
        while let Some(event) = blocking.pull(&cancel)? {
            let packet = matches!(event, Event::Packet(_));
            blocking.queued.push_back(event);
            if packet {
                break;
            }
        }
        Ok(blocking)
    }

    /// Tracks seen so far; every track of the first link once open returns.
    pub fn tracks(&self) -> &[Track] {
        self.reader.tracks()
    }

    /// The underlying reader.
    pub fn reader(&self) -> &Reader {
        &self.reader
    }

    fn supply(&mut self, request: &InputRequest) -> Result<()> {
        if request.offset != self.position {
            let reposition = self
                .reposition
                .ok_or(Error::Unsupported("seeking a sequential source"))?;
            reposition(&mut self.source, request.offset).map_err(|e| Error::Io(e.kind()))?;
            self.position = request.offset;
        }
        let length = request.maximum.min(self.buffer.len()).max(1);
        let read = loop {
            match self.source.read(&mut self.buffer[..length]) {
                Ok(read) => break read,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(Error::Io(error.kind())),
            }
        };
        if read == 0 {
            return self.reader.finish(request.source, request.token.clone());
        }
        let bytes = Bytes::copy_from_slice(&self.pool, &self.buffer[..read])?;
        self.position += read as u64;
        self.reader.provide(&InputWindow::new(
            request.source,
            request.token.clone(),
            request.offset,
            bytes,
        )?)
    }

    fn pull(&mut self, cancel: &AtomicBool) -> Result<Option<Event<Packet, ReaderEvent>>> {
        loop {
            match self.reader.step(&mut WorkBudget::new(WORK), cancel)? {
                Step::Ready(event) => return Ok(Some(event)),
                Step::NeedInput(request) => self.supply(&request)?,
                Step::Yield => {}
                Step::Backpressure(pressure) => {
                    return Err(Error::Backpressure {
                        requested: pressure.requested,
                        available: pressure.available,
                    });
                }
                Step::End(_) => return Ok(None),
            }
        }
    }

    /// The next event; `None` at the end of the source.
    pub fn next_event(
        &mut self,
        cancel: &AtomicBool,
    ) -> Result<Option<Event<Packet, ReaderEvent>>> {
        match self.queued.pop_front() {
            Some(event) => Ok(Some(event)),
            None => self.pull(cancel),
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
        if self.reposition.is_none() {
            return Err(Error::Unsupported("seeking a sequential source"));
        }
        self.queued.retain(|event| matches!(event, Event::Other(_)));
        self.reader.request_seek(request)?;
        while let Some(event) = self.pull(cancel)? {
            match event {
                Event::Seek(receipt) => return Ok(receipt),
                other => self.queued.push_back(other),
            }
        }
        Err(Error::Truncated)
    }
}

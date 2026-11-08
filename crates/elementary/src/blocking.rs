//! A blocking convenience over [`std::io::Read`] + [`std::io::Seek`].
use crate::reader::{Format, Reader};
use crate::stream::{Limits, Packet, ReaderEvent};
use std::io::{Read, Seek, SeekFrom};
use std::sync::atomic::AtomicBool;
use triergon_core::container::{ContainerReader, Event, SeekReceipt, SeekRequest};
use triergon_core::{
    Budget, BudgetVec, Error, InputRequest, InputWindow, Result, SourceCapabilities, SourceId,
    Step, WorkBudget,
};

/// Work units per step; the reader yields between steps and the loop continues.
const STEP_WORK: u64 = 1 << 20;

/// Drives a [`Reader`] from a seekable byte source.
#[derive(Debug)]
pub struct BlockingReader<F: Format, S> {
    reader: Reader<F>,
    source: S,
    position: u64,
    pool: Budget,
    window: usize,
    cancel: AtomicBool,
}

impl<F: Format, S: Read + Seek> BlockingReader<F, S> {
    /// Open `source` with a pool of [`Limits::live_storage_bytes`].
    pub fn open(source: S, limits: Limits) -> Result<Self> {
        let pool = Budget::new(limits.live_storage_bytes)?;
        Self::open_in(source, limits, &pool)
    }

    /// Open `source` charging everything to `pool`.
    pub fn open_in(mut source: S, limits: Limits, pool: &Budget) -> Result<Self> {
        let length = source.seek(SeekFrom::End(0))?;
        source.seek(SeekFrom::Start(0))?;
        let capabilities = SourceCapabilities {
            length: Some(length),
            random_access: true,
        };
        Ok(BlockingReader {
            reader: Reader::new(pool, SourceId(1), capabilities, limits)?,
            source,
            position: 0,
            pool: pool.clone(),
            window: limits.input_window,
            cancel: AtomicBool::new(false),
        })
    }

    pub fn reader(&self) -> &Reader<F> {
        &self.reader
    }

    pub fn into_inner(self) -> S {
        self.source
    }

    /// The next event, `None` at the end of the stream.
    pub fn next_event(&mut self) -> Result<Option<Event<Packet, ReaderEvent>>> {
        loop {
            match self
                .reader
                .step(&mut WorkBudget::new(STEP_WORK), &self.cancel)?
            {
                Step::Ready(event) => return Ok(Some(event)),
                Step::NeedInput(request) => self.supply(&request)?,
                Step::Yield | Step::Backpressure(_) => {}
                Step::End(_) => return Ok(None),
            }
        }
    }

    /// The next packet, `None` at the end of the stream.
    pub fn next_packet(&mut self) -> Result<Option<Packet>> {
        while let Some(event) = self.next_event()? {
            if let Event::Packet(packet) = event {
                return Ok(Some(packet));
            }
        }
        Ok(None)
    }

    /// Read until the track is known.
    pub fn open_stream(&mut self) -> Result<()> {
        while self.reader.stream().is_none() {
            if self.next_event()?.is_none() {
                return Err(Error::Invalid("no frame in the source"));
            }
        }
        Ok(())
    }

    /// Seek and return the receipt; packets from the landing point follow.
    pub fn seek(&mut self, request: SeekRequest) -> Result<SeekReceipt> {
        self.open_stream()?;
        let first = self.reader.request_seek(request)?;
        self.supply(&first)?;
        loop {
            match self.next_event()? {
                Some(Event::Seek(receipt)) => return Ok(receipt),
                Some(_) => {}
                None => return Err(Error::Invalid("seek target beyond the stream")),
            }
        }
    }

    fn supply(&mut self, request: &InputRequest) -> Result<()> {
        if request.offset != self.position {
            self.position = self.source.seek(SeekFrom::Start(request.offset))?;
        }
        let maximum = request.maximum.min(self.window);
        let mut buffer = BudgetVec::new(&self.pool);
        buffer.try_resize(maximum, 0)?;
        let read = loop {
            match self.source.read(&mut buffer) {
                Ok(read) => break read,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error.into()),
            }
        };
        if read == 0 {
            return self.reader.finish(request.source, request.token.clone());
        }
        buffer.truncate(read);
        let bytes = buffer.freeze().map_err(|failure| failure.error)?;
        let window =
            InputWindow::new(request.source, request.token.clone(), request.offset, bytes)?;
        self.position += read as u64;
        self.reader.provide(&window)
    }
}

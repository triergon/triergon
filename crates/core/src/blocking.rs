//! Driving any [`ContainerReader`] from a [`std::io`] source.
use crate::container::{ContainerReader, Event, SeekRequest};
use crate::{Budget, Bytes, Error, InputRequest, InputWindow, Result, SourceCapabilities, Step};
use crate::{SourceFailure, WorkBudget};
use std::io::{Read, Seek, SeekFrom};
use std::sync::atomic::AtomicBool;

/// Work units per reader step.
const WORK: u64 = 1 << 20;

type Reposition<S> = fn(&mut S, u64) -> std::io::Result<u64>;

/// A reader fed from a blocking [`Read`] source, which is seeked on demand when it is
/// also [`Seek`]. Each requested window is copied into the reader's pool.
#[derive(Debug)]
pub struct Blocking<R, S> {
    reader: R,
    source: S,
    pool: Budget,
    position: u64,
    reposition: Option<Reposition<S>>,
    buffer: Vec<u8>,
    cancel: AtomicBool,
}

/// The capabilities of a seekable source: its length, found by seeking to the end, and
/// random access. The source is left at its start.
pub fn seekable_capabilities(source: &mut impl Seek) -> Result<SourceCapabilities> {
    let length = source
        .seek(SeekFrom::End(0))
        .map_err(|e| Error::Io(e.kind()))?;
    source.rewind().map_err(|e| Error::Io(e.kind()))?;
    Ok(SourceCapabilities {
        length: Some(length),
        random_access: true,
    })
}

impl<R: ContainerReader, S: Read> Blocking<R, S> {
    /// Feed `reader` from a sequential source; the reader must not ask for random access.
    pub fn sequential(reader: R, source: S, pool: &Budget) -> Self {
        Self::with(reader, source, pool, None)
    }

    fn with(reader: R, source: S, pool: &Budget, reposition: Option<Reposition<S>>) -> Self {
        Blocking {
            reader,
            source,
            pool: pool.clone(),
            position: 0,
            reposition,
            buffer: Vec::new(),
            cancel: AtomicBool::new(false),
        }
    }

    pub fn reader(&self) -> &R {
        &self.reader
    }

    pub fn reader_mut(&mut self) -> &mut R {
        &mut self.reader
    }

    /// The next event; `None` at the end of the source.
    pub fn next_event(&mut self) -> Result<Option<Event<R::Packet, R::Other>>> {
        loop {
            match self.reader.step(&mut WorkBudget::new(WORK), &self.cancel)? {
                Step::Ready(event) => return Ok(Some(event)),
                Step::Yield => {}
                Step::NeedInput(request) => self.serve(request)?,
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

    /// The next packet, skipping other events; `None` at the end of the source.
    pub fn next_packet(&mut self) -> Result<Option<R::Packet>> {
        while let Some(event) = self.next_event()? {
            if let Event::Packet(packet) = event {
                return Ok(Some(packet));
            }
        }
        Ok(None)
    }

    /// Start a seek; its receipt arrives as an [`Event::Seek`].
    pub fn seek(&mut self, request: SeekRequest) -> Result<()> {
        let request = self.reader.request_seek(request)?;
        self.serve(request)
    }

    fn serve(&mut self, request: InputRequest) -> Result<()> {
        if request.offset != self.position {
            let Some(reposition) = self.reposition else {
                return Err(Error::Unsupported("random-access input"));
            };
            if let Err(error) = reposition(&mut self.source, request.offset) {
                self.reader
                    .fail(request.source, request.token, SourceFailure::Transport)?;
                return Err(Error::Io(error.kind()));
            }
            self.position = request.offset;
        }
        self.buffer.resize(request.maximum, 0);
        let read = loop {
            match self.source.read(&mut self.buffer) {
                Ok(read) => break read,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => {
                    self.reader
                        .fail(request.source, request.token, SourceFailure::Transport)?;
                    return Err(Error::Io(error.kind()));
                }
            }
        };
        if read == 0 {
            return self.reader.finish(request.source, request.token);
        }
        let bytes = Bytes::copy_from_slice(&self.pool, &self.buffer[..read])?;
        self.position += read as u64;
        let window = InputWindow::new(request.source, request.token, request.offset, bytes)?;
        self.reader.provide(&window)
    }
}

impl<R: ContainerReader, S: Read + Seek> Blocking<R, S> {
    /// Feed `reader` from a seekable source; the reader's capabilities should come from
    /// [`seekable_capabilities`].
    pub fn seekable(reader: R, source: S, pool: &Budget) -> Self {
        Self::with(
            reader,
            source,
            pool,
            Some(|source, at| source.seek(SeekFrom::Start(at))),
        )
    }
}

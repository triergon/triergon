//! Driving any [`ContainerReader`] from a [`std::io`] source.
use crate::container::{ContainerReader, Event, SeekRequest};
use crate::{
    Budget, BudgetVec, Bytes, Error, InputRequest, InputWindow, Result, SourceCapabilities,
    SourceFailure, Step, WorkBudget,
};
use std::io::{ErrorKind, Read, Seek, SeekFrom};
use std::sync::atomic::AtomicBool;

/// Work units per reader step.
const WORK: u64 = 1 << 20;

/// The flag of the calls that take none: never set.
static NOT_CANCELLED: AtomicBool = AtomicBool::new(false);

type Reposition<S> = fn(&mut S, u64) -> std::io::Result<u64>;

/// A reader fed from a blocking [`Read`] source, which is seeked on demand when it is
/// also [`Seek`]. Windows are read into one allocation charged to the reader's pool and
/// refilled in place once the reader has released it; a new one is made only while the
/// reader or its packets still hold the previous window.
#[derive(Debug)]
pub struct Blocking<R, S> {
    reader: R,
    source: S,
    pool: Budget,
    position: u64,
    reposition: Option<Reposition<S>>,
    /// The last window read.
    window: Option<Bytes>,
}

/// The capabilities of a seekable source: its length, found by seeking to the end, and
/// random access. The source is left at its start.
pub fn seekable_capabilities(source: &mut impl Seek) -> Result<SourceCapabilities> {
    let length = source.seek(SeekFrom::End(0))?;
    source.rewind()?;
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
            window: None,
        }
    }

    pub fn reader(&self) -> &R {
        &self.reader
    }

    pub fn reader_mut(&mut self) -> &mut R {
        &mut self.reader
    }

    pub fn into_inner(self) -> S {
        self.source
    }

    /// The next event; `None` at the end of the source.
    pub fn next_event(&mut self) -> Result<Option<Event<R::Packet, R::Other>>> {
        self.next_event_or_cancel(&NOT_CANCELLED)
    }

    /// [`Blocking::next_event`], failing with [`Error::Cancelled`] once `cancel` is set.
    pub fn next_event_or_cancel(
        &mut self,
        cancel: &AtomicBool,
    ) -> Result<Option<Event<R::Packet, R::Other>>> {
        loop {
            match self.reader.step(&mut WorkBudget::new(WORK), cancel)? {
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
                return self.transport(request, error);
            }
            self.position = request.offset;
        }
        let bytes = match self.fill(request.maximum)? {
            Ok(bytes) => bytes,
            Err(error) => return self.transport(request, error),
        };
        if bytes.is_empty() {
            return self.reader.finish(request.source, request.token);
        }
        self.position += bytes.len() as u64;
        let window = InputWindow::new(request.source, request.token, request.offset, bytes)?;
        self.reader.provide(&window)
    }

    /// Read up to `maximum` bytes into the window: in place when nothing else holds it,
    /// otherwise into a new charged allocation. The outer error is the pool's, the inner
    /// one the source's.
    fn fill(&mut self, maximum: usize) -> Result<std::io::Result<Bytes>> {
        // A new allocation is unshared and holds `maximum` bytes: at most two rounds.
        loop {
            let source = &mut self.source;
            if let Some(window) = self.window.as_mut()
                && let Some(read) =
                    window.try_refill(maximum, |buffer| read_retrying(source, buffer))
            {
                return Ok(read.map(|_| window.clone()));
            }
            self.window = None;
            let buffer = BudgetVec::try_filled(&self.pool, 0, maximum)?;
            self.window = Some(buffer.freeze().map_err(|refused| refused.error)?);
        }
    }

    /// Report a failed source to the reader, which fails from then on.
    fn transport(&mut self, request: InputRequest, error: std::io::Error) -> Result<()> {
        self.reader
            .fail(request.source, request.token, SourceFailure::Transport)?;
        Err(error.into())
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

fn read_retrying(source: &mut impl Read, buffer: &mut [u8]) -> std::io::Result<usize> {
    loop {
        match source.read(buffer) {
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            result => return result,
        }
    }
}

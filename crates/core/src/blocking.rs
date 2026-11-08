//! Driving any [`ContainerReader`], or a parser's [`InputCursor`], from a [`std::io`]
//! source.
use crate::container::{ContainerReader, Event, SeekRequest};
use crate::{
    Budget, BudgetVec, Bytes, Error, InputCursor, InputRequest, InputWindow, Result,
    SourceCapabilities, SourceFailure, Step, WorkBudget,
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

/// Serves the requests of a parser's [`InputCursor`] from a blocking seekable source, for
/// parsers driven below the [`ContainerReader`] level. The source is seeked only when a
/// request does not continue the previous response. A response that fits the cursor's
/// inline storage is copied there without an allocation; a larger one is copied into
/// storage charged to the pool, sized to the bytes read.
#[derive(Debug)]
pub struct Feed<S> {
    source: S,
    pool: Budget,
    /// The source's offset after the last response; unknown after a failed read.
    position: Option<u64>,
}

impl<S: Read + Seek> Feed<S> {
    /// Feed from `source`, rewound to its start; larger responses are charged to `pool`.
    pub fn new(mut source: S, pool: &Budget) -> Result<Self> {
        source.rewind()?;
        Ok(Self {
            source,
            pool: pool.clone(),
            position: Some(0),
        })
    }

    pub fn source_mut(&mut self) -> &mut S {
        &mut self.source
    }

    pub fn into_inner(self) -> S {
        self.source
    }

    /// Answer `request` on `input`: with the bytes read, or with the end of the source
    /// when none are left. A failed read marks the cursor's source as failed and returns
    /// the error.
    // Inlined into the caller's polling loop, which runs once per window: once per byte
    // for one-byte windows.
    #[inline(always)]
    pub fn supply(&mut self, input: &mut InputCursor, request: InputRequest) -> Result<()> {
        let maximum = request.maximum;
        if maximum <= 1 {
            self.supply_from::<1>(input, request)
        } else if maximum <= InputCursor::INLINE_CAPACITY {
            self.supply_from::<{ InputCursor::INLINE_CAPACITY }>(input, request)
        } else if maximum <= 4096 {
            self.supply_large::<4096>(input, request)
        } else {
            self.supply_large::<65536>(input, request)
        }
    }

    // Keeps large read buffers out of the small-window path.
    #[inline(never)]
    fn supply_large<const CAPACITY: usize>(
        &mut self,
        input: &mut InputCursor,
        request: InputRequest,
    ) -> Result<()> {
        self.supply_from::<CAPACITY>(input, request)
    }

    /// Read into `buffer` from `offset`, seeking only when the last response ended
    /// elsewhere. Inlined, unlike `read_retrying`: one-byte windows read once per byte.
    #[inline(always)]
    fn read_at(&mut self, offset: u64, buffer: &mut [u8]) -> std::io::Result<usize> {
        if self.position != Some(offset) {
            self.position = None;
            self.source.seek(SeekFrom::Start(offset))?;
        }
        loop {
            match self.source.read(buffer) {
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                result => return result,
            }
        }
    }

    #[inline(always)]
    fn supply_from<const CAPACITY: usize>(
        &mut self,
        input: &mut InputCursor,
        request: InputRequest,
    ) -> Result<()> {
        let mut buffer = [0u8; CAPACITY];
        let maximum = request.maximum.min(CAPACITY);
        let read = self.read_at(request.offset, &mut buffer[..maximum]);
        let count = match read {
            Ok(count) => count,
            Err(error) => {
                self.position = None;
                input.fail(request.source, request.token, SourceFailure::Transport)?;
                return Err(error.into());
            }
        };
        self.position = Some(
            request
                .offset
                .checked_add(u64::try_from(count).map_err(|_| Error::Overflow)?)
                .ok_or(Error::Overflow)?,
        );
        let bytes = &buffer[..count];
        if bytes.is_empty() {
            input.finish(request.source, request.token)
        } else if count <= InputCursor::INLINE_CAPACITY {
            input.provide_inline(request.source, request.token, request.offset, bytes)
        } else {
            let bytes = Bytes::copy_from_slice(&self.pool, bytes)?;
            input.provide_owned(InputWindow::new(
                request.source,
                request.token,
                request.offset,
                bytes,
            )?)
        }
    }
}

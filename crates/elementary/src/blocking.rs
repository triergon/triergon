//! A blocking convenience over [`std::io::Read`] + [`std::io::Seek`].
use crate::reader::{Format, Reader};
use crate::stream::{Limits, Packet, ReaderEvent};
use std::io::{Read, Seek};
use triergon_core::blocking::{Blocking, seekable_capabilities};
use triergon_core::container::{Event, SeekReceipt, SeekRequest};
use triergon_core::{Budget, Error, Result, SourceId};

/// Drives a [`Reader`] from a seekable byte source.
#[derive(Debug)]
pub struct BlockingReader<F: Format, S> {
    driver: Blocking<Reader<F>, S>,
}

impl<F: Format, S: Read + Seek> BlockingReader<F, S> {
    /// Open `source` with a pool of [`Limits::live_storage_bytes`].
    pub fn open(source: S, limits: Limits) -> Result<Self> {
        let pool = Budget::new(limits.live_storage_bytes)?;
        Self::open_in(source, limits, &pool)
    }

    /// Open `source` charging everything to `pool`.
    pub fn open_in(mut source: S, limits: Limits, pool: &Budget) -> Result<Self> {
        let capabilities = seekable_capabilities(&mut source)?;
        let reader = Reader::new(pool, SourceId(1), capabilities, limits)?;
        Ok(BlockingReader {
            driver: Blocking::seekable(reader, source, pool),
        })
    }

    pub fn reader(&self) -> &Reader<F> {
        self.driver.reader()
    }

    pub fn into_inner(self) -> S {
        self.driver.into_inner()
    }

    /// The next event, `None` at the end of the stream.
    pub fn next_event(&mut self) -> Result<Option<Event<Packet, ReaderEvent>>> {
        self.driver.next_event()
    }

    /// The next packet, `None` at the end of the stream.
    pub fn next_packet(&mut self) -> Result<Option<Packet>> {
        self.driver.next_packet()
    }

    /// Read until the track is known.
    pub fn open_stream(&mut self) -> Result<()> {
        while self.reader().stream().is_none() {
            if self.next_event()?.is_none() {
                return Err(Error::Invalid("no frame in the source"));
            }
        }
        Ok(())
    }

    /// Seek and return the receipt; packets from the landing point follow.
    pub fn seek(&mut self, request: SeekRequest) -> Result<SeekReceipt> {
        self.open_stream()?;
        self.driver.seek(request)?;
        loop {
            match self.next_event()? {
                Some(Event::Seek(receipt)) => return Ok(receipt),
                Some(_) => {}
                None => return Err(Error::Invalid("seek target beyond the stream")),
            }
        }
    }
}

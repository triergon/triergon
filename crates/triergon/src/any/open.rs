//! The blocking openers: probe a `std::io` source, then hand it to its reader.
use super::AnyReader;
use crate::probe::{ELEMENTARY, LIMIT, SourceFormat, container, elementary, probe_whole, select};
use std::io::{ErrorKind, Read, Seek};
use triergon_core::blocking::Blocking;
use triergon_core::signature::{Selection, TransportSync::M2ts192};
use triergon_core::{Budget, Error, Reservation, Result, SourceCapabilities, SourceId};

/// Probe up to 64 KiB of a seekable source, then feed its reader from the start. The
/// reader reads it as `SourceId(1)`.
///
/// Returns an error if reading or seeking fails, no format the facade opens starts the
/// source ([`Error::Unsupported`]), or reader construction fails ([`AnyReader::open`]).
pub fn open_blocking<S: Read + Seek>(
    mut source: S,
    pool: &Budget,
) -> Result<Blocking<AnyReader, S>> {
    let capabilities = triergon_core::blocking::seekable_capabilities(&mut source)?;
    let format = ProbeBuffer::new(pool).identify(&mut source, capabilities.length);
    source.rewind()?;
    let reader = AnyReader::of(format?, pool, SourceId(1), capabilities)?;
    Ok(Blocking::seekable(reader, source, pool))
}

/// Probe up to 64 KiB of a sequential source and replay the consumed prefix.
/// The reader sees a pipe with unknown length and no random access, as `SourceId(1)`.
///
/// Returns an error if the budget cannot hold the probe or reader, reading fails,
/// or the prefix cannot select an enabled reader that supports sequential input.
pub fn open_sequential<S: Read>(
    mut source: S,
    pool: &Budget,
) -> Result<Blocking<AnyReader, Replayed<S>>> {
    let mut prefix = ProbeBuffer::new(pool);
    let format = prefix.identify_pipe(&mut source)?;
    let capabilities = SourceCapabilities {
        length: None,
        random_access: false,
    };
    let reader = AnyReader::of(format, pool, SourceId(1), capabilities)?;
    let source = Replayed {
        prefix: Some(prefix.into_replay()?),
        at: 0,
        rest: source,
    };
    Ok(Blocking::sequential(reader, source, pool))
}

/// A sequential source with its probed prefix put back in front; the prefix is freed
/// once it has been read.
#[derive(Debug)]
pub struct Replayed<S> {
    prefix: Option<Prefix>,
    at: usize,
    rest: S,
}

impl<S: Read> Read for Replayed<S> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if let Some(prefix) = &self.prefix {
            let count = out.len().min(prefix.bytes.len() - self.at);
            out[..count].copy_from_slice(&prefix.bytes[self.at..self.at + count]);
            self.at += count;
            if self.at == prefix.bytes.len() {
                self.prefix = None;
            }
            return Ok(count);
        }
        self.rest.read(out)
    }
}

/// A replay allocation and the reservation covering its capacity.
#[derive(Debug)]
struct Prefix {
    bytes: Vec<u8>,
    charge: Reservation,
}

impl Prefix {
    // Allocate once, after the signature or the first block tells us how much
    // space to use. A pipe can already hold the reservation for this allocation.
    fn allocate(&mut self, capacity: usize) -> Result<()> {
        self.charge.resize_to(capacity)?;
        self.bytes
            .try_reserve_exact(capacity)
            .map_err(|_| Error::Allocation)?;
        self.charge.resize_to(self.bytes.capacity())
    }
}

/// Probe fixed signatures on the stack. Only pipes need to retain these bytes.
struct ProbeBuffer {
    header: [u8; M2ts192.prefix_len()],
    used: usize,
    prefix: Prefix,
    maximum: usize,
}

impl ProbeBuffer {
    fn new(pool: &Budget) -> Self {
        let bound = if ELEMENTARY {
            LIMIT
        } else {
            M2ts192.prefix_len()
        };
        Self {
            header: [0; M2ts192.prefix_len()],
            used: 0,
            prefix: Prefix {
                bytes: Vec::new(),
                charge: Reservation::empty(pool),
            },
            maximum: bound.min(pool.limit() - Budget::control_storage_bytes()),
        }
    }

    /// A pipe cannot put bytes back if its allocation is refused midway.
    /// Claim its possible replay capacity before the first irreversible read.
    fn identify_pipe(&mut self, source: &mut impl Read) -> Result<Option<SourceFormat>> {
        self.prefix.charge.resize_to(self.maximum)?;
        let kind = self.identify(source, None)?;
        if self.prefix.bytes.is_empty() {
            // Return unused headroom before the selected reader is constructed.
            self.prefix.charge.resize_to(self.used.min(self.maximum))?;
        }
        Ok(kind)
    }

    /// The format of the source, read from its start: `None` when none the facade opens
    /// starts it.
    fn identify(
        &mut self,
        source: &mut impl Read,
        length: Option<u64>,
    ) -> Result<Option<SourceFormat>> {
        loop {
            let bytes = &self.header[..self.used];
            match select(bytes) {
                Selection::Container(kind) => return Ok(Some(container(kind))),
                Selection::Need(end) => {
                    let count = read(source, &mut self.header[self.used..end])?;
                    if count == 0 {
                        return Ok(probe_whole(&self.header[..self.used]));
                    }
                    self.used += count;
                }
                Selection::Unrecognised | Selection::Ambiguous => {
                    if !ELEMENTARY || bytes.starts_with(b"fLaC") {
                        return Ok(elementary(bytes));
                    }
                    return self.read_elementary(source, length);
                }
            }
        }
    }

    fn into_replay(mut self) -> Result<Prefix> {
        if self.prefix.bytes.is_empty() {
            self.prefix.allocate(self.used)?;
            self.prefix
                .bytes
                .extend_from_slice(&self.header[..self.used]);
        }
        Ok(self.prefix)
    }

    /// Search the full bounded prefix once. Buffer the first block on the stack
    /// so a short input needs only its actual length. A longer input takes one
    /// allocation and never has to charge both an old and a replacement window.
    fn read_elementary(
        &mut self,
        source: &mut impl Read,
        length: Option<u64>,
    ) -> Result<Option<SourceFormat>> {
        let hint = length.unwrap_or(0).min(self.maximum as u64) as usize;
        let mut block = [0; 4096];
        block[..self.used].copy_from_slice(&self.header[..self.used]);
        let mut used = self.used;
        while self.prefix.bytes.len() < LIMIT {
            // A file can grow past its hint. Keep the original block boundaries
            // when checking the pool limit on the remaining bytes.
            let want = block.len() - self.prefix.bytes.len() % block.len();
            used += fill(source, &mut block[used..want])?;
            let end = self.prefix.bytes.len() + used;
            if end > self.maximum {
                return Err(Error::Limit {
                    requested: end,
                    maximum: self.maximum,
                });
            }
            if self.prefix.bytes.is_empty() {
                self.prefix
                    .allocate(if used < want { used } else { self.maximum })?;
            }
            self.prefix.bytes.extend_from_slice(&block[..used]);
            if used < want {
                break;
            }
            if end < hint {
                // Read a seekable file directly into the reserved prefix.
                self.prefix.bytes.resize(hint, 0);
                let actual = end + fill(source, &mut self.prefix.bytes[end..])?;
                self.prefix.bytes.truncate(actual);
                if actual < hint {
                    break;
                }
            }
            used = 0;
        }
        Ok(elementary(&self.prefix.bytes))
    }
}

fn fill(source: &mut impl Read, buffer: &mut [u8]) -> Result<usize> {
    let mut used = 0;
    while used < buffer.len() {
        let count = read(source, &mut buffer[used..])?;
        if count == 0 {
            break;
        }
        used += count;
    }
    Ok(used)
}

fn read(source: &mut impl Read, out: &mut [u8]) -> Result<usize> {
    loop {
        match source.read(out) {
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            result => return Ok(result?),
        }
    }
}

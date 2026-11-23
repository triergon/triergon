//! Facade format selection for a supplied prefix or a blocking source.
use std::io::{ErrorKind, Read};
use triergon_core::signature::{
    self, Container, Selection,
    TransportSync::{M2ts192, Ts188},
    TransportTie,
};
use triergon_core::{Budget, Error, Reservation, Result};

/// A source format the facade opens, as [`probe`] recognises it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceFormat {
    /// An EBML header (Matroska or WebM).
    Matroska,
    /// An ISO BMFF box sequence (`ftyp`, `styp`, `moov`, `moof`, `wide`, ...).
    Isobmff,
    /// 188-byte MPEG transport packets.
    Ts,
    /// 192-byte M2TS packets with their four-byte arrival prefixes.
    M2ts,
    /// An Ogg page (`OggS`).
    Ogg,
    /// Native FLAC (`fLaC`, or a frame header the next one numbers on from), possibly
    /// after an ID3v2 tag.
    Flac,
    /// Two consecutive MPEG audio frames, possibly after an ID3v2 tag.
    MpegAudio,
    /// Two consecutive ADTS frames.
    Adts,
    /// Two consecutive AC-3 or E-AC-3 syncframes.
    Ac3,
}

/// What a source prefix says about its format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Probed {
    /// The source starts this format.
    Format(SourceFormat),
    /// Undecided: a prefix of this many bytes decides. A source that ends first is decided
    /// by [`probe_whole`].
    Need(usize),
    /// No format the facade opens starts with these bytes, whatever follows them.
    Unrecognised,
}

/// Classify a source from a prefix that more bytes may follow. A container is decided as
/// soon as its signature is (Matroska from four bytes, Ogg from five, ISO BMFF from
/// eight, MPEG transport from three sync bytes); an elementary stream (MPEG audio, ADTS,
/// AC-3) once the prefix holds the start of its second frame and any ID3v2 tag whole,
/// and the facade's openers look for one in the first 64 KiB, so a shorter prefix that
/// holds none yet is [`Probed::Need`] of 64 KiB.
pub fn probe(prefix: &[u8]) -> Probed {
    match select(prefix) {
        Selection::Container(kind) => Probed::Format(container(kind)),
        Selection::Need(end) => Probed::Need(end),
        Selection::Unrecognised | Selection::Ambiguous => match elementary(prefix) {
            Some(format) => Probed::Format(format),
            None if ELEMENTARY && prefix.len() < LIMIT => Probed::Need(LIMIT),
            None => Probed::Unrecognised,
        },
    }
}

/// Classify a whole source, which no more bytes follow: [`probe`]'s answer where that
/// decides, else the format the bytes it has select. A source that ends before a third
/// transport sync byte is MPEG transport when its first two sync bytes are in place (one
/// packet and all or part of a second); a container signature cut short is not one.
/// `None` when no format the facade opens starts the source.
pub fn probe_whole(source: &[u8]) -> Option<SourceFormat> {
    match signature::select_whole(source, &POLICY) {
        Selection::Container(kind) => Some(container(kind)),
        Selection::Need(_) | Selection::Unrecognised | Selection::Ambiguous => elementary(source),
    }
}

/// What the readers select where the signatures leave a choice: every box a source
/// they read may open with, a fragment or an index included; Ogg; and TS188 for a
/// prefix that fits both transport spacings. `triergon-playback` selects for a
/// player instead (`triergon_playback::probe::SELECTION`): no `moof`, `sidx` or `skip`,
/// no Ogg, and no packet size for such a prefix.
const POLICY: signature::Policy = signature::Policy {
    opening_boxes: &[
        *b"ftyp", *b"styp", *b"moov", *b"moof", *b"mdat", *b"free", *b"skip", *b"sidx", *b"wide",
    ],
    ogg: true,
    transport_tie: TransportTie::Ts188,
};

/// The container `prefix` starts under [`POLICY`].
fn select(prefix: &[u8]) -> Selection {
    // One more byte settles an Ogg page, and tells a FLAC marker from the size of an
    // opening box, where a box type would ask for four.
    if prefix.len() == 4 && (prefix == b"OggS" || prefix == b"fLaC") {
        return Selection::Need(5);
    }
    signature::select(prefix, &POLICY)
}

/// Whether the elementary stream readers are built in.
const ELEMENTARY: bool = cfg!(feature = "elementary");

fn container(container: Container) -> SourceFormat {
    match container {
        Container::Matroska => SourceFormat::Matroska,
        Container::Isobmff => SourceFormat::Isobmff,
        Container::Ogg => SourceFormat::Ogg,
        Container::Transport(Ts188) => SourceFormat::Ts,
        Container::Transport(M2ts192) => SourceFormat::M2ts,
    }
}

/// The elementary stream `bytes` starts, when that reader is built in.
fn elementary(bytes: &[u8]) -> Option<SourceFormat> {
    #[cfg(feature = "elementary")]
    {
        use triergon_elementary::Kind;
        triergon_elementary::probe(bytes).map(|kind| match kind {
            Kind::Flac => SourceFormat::Flac,
            Kind::MpegAudio => SourceFormat::MpegAudio,
            Kind::Adts => SourceFormat::Adts,
            Kind::Ac3 => SourceFormat::Ac3,
        })
    }
    #[cfg(not(feature = "elementary"))]
    {
        let _ = bytes;
        None
    }
}

const LIMIT: usize = 64 * 1024;

/// A replay allocation and the reservation covering its capacity.
#[derive(Debug)]
pub(super) struct Prefix {
    pub(super) bytes: Vec<u8>,
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
pub(super) struct ProbeBuffer {
    header: [u8; M2ts192.prefix_len()],
    used: usize,
    prefix: Prefix,
    maximum: usize,
}

impl ProbeBuffer {
    pub(super) fn new(pool: &Budget) -> Self {
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
    pub(super) fn identify_pipe(&mut self, source: &mut impl Read) -> Result<Option<SourceFormat>> {
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
    pub(super) fn identify(
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

    pub(super) fn into_replay(mut self) -> Result<Prefix> {
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
            result => return result.map_err(|error| Error::Io(error.kind())),
        }
    }
}

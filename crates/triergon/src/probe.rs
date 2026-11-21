//! Facade format selection for a supplied prefix or a blocking source.
use std::io::{ErrorKind, Read};
use triergon_core::signature::{
    self, Container, Selection,
    TransportSync::{M2ts192, Ts188},
    TransportTie,
};
use triergon_core::{Budget, Error, Reservation, Result};

/// What the first bytes of a source look like.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Probe {
    /// An EBML header (Matroska or WebM).
    Matroska,
    /// An ISO BMFF box sequence (`ftyp`, `styp`, `moov`, `moof`, ...).
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
    /// Not recognised from these bytes.
    Unknown,
}

/// Classify a source from its first bytes. A short prefix may give `Unknown`: an
/// elementary stream (MPEG audio, ADTS, AC-3) is recognised only once the prefix holds
/// the start of its second frame, and an ID3v2 tag must fit in it whole.
pub fn probe(bytes: &[u8]) -> Probe {
    match container(bytes) {
        Selection::Container(kind) => format(kind),
        _ => elementary(bytes),
    }
}

/// What the readers select where the signatures leave a choice: every box a source
/// they read may open with, a fragment or an index included; Ogg; and TS188 for a
/// prefix that fits both transport spacings. `triergon-playback` selects for a
/// player instead (`triergon_playback::probe::SELECTION`): `wide` but no `moof`,
/// `sidx` or `skip`, no Ogg, and no packet size for such a prefix.
const POLICY: signature::Policy = signature::Policy {
    opening_boxes: &[
        b"ftyp", b"styp", b"moov", b"moof", b"mdat", b"free", b"skip", b"sidx",
    ],
    ogg: true,
    transport_tie: TransportTie::Ts188,
};

fn format(container: Container) -> Probe {
    match container {
        Container::Matroska => Probe::Matroska,
        Container::Isobmff => Probe::Isobmff,
        Container::Ogg => Probe::Ogg,
        Container::Transport(Ts188) => Probe::Ts,
        Container::Transport(M2ts192) => Probe::M2ts,
    }
}

fn container(bytes: &[u8]) -> Selection {
    // One more byte settles an Ogg page, and tells a FLAC marker from the size of
    // an opening box, where a box type would ask for four.
    if bytes.len() == 4 && (bytes == b"OggS" || bytes == b"fLaC") {
        return Selection::Need(5);
    }
    signature::select(bytes, &POLICY)
}

/// The elementary stream `bytes` starts, when that reader is built in.
fn elementary(bytes: &[u8]) -> Probe {
    #[cfg(feature = "elementary")]
    {
        use triergon_elementary::Kind;
        match triergon_elementary::probe(bytes) {
            Some(Kind::Flac) => Probe::Flac,
            Some(Kind::MpegAudio) => Probe::MpegAudio,
            Some(Kind::Adts) => Probe::Adts,
            Some(Kind::Ac3) => Probe::Ac3,
            None => Probe::Unknown,
        }
    }
    #[cfg(not(feature = "elementary"))]
    {
        let _ = bytes;
        Probe::Unknown
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
        let bound = if cfg!(feature = "elementary") {
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
    pub(super) fn identify_pipe(&mut self, source: &mut impl Read) -> Result<Probe> {
        self.prefix.charge.resize_to(self.maximum)?;
        let kind = self.identify(source, None)?;
        if self.prefix.bytes.is_empty() {
            // Return unused headroom before the selected reader is constructed.
            self.prefix.charge.resize_to(self.used.min(self.maximum))?;
        }
        Ok(kind)
    }

    pub(super) fn identify(
        &mut self,
        source: &mut impl Read,
        length: Option<u64>,
    ) -> Result<Probe> {
        loop {
            let bytes = &self.header[..self.used];
            match container(bytes) {
                Selection::Container(kind) => return Ok(format(kind)),
                Selection::Need(end) => {
                    let count = read(source, &mut self.header[self.used..end])?;
                    if count == 0 {
                        return Ok(elementary(&self.header[..self.used]));
                    }
                    self.used += count;
                }
                Selection::Unrecognised | Selection::Ambiguous => {
                    if !cfg!(feature = "elementary") || bytes.starts_with(b"fLaC") {
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
    fn read_elementary(&mut self, source: &mut impl Read, length: Option<u64>) -> Result<Probe> {
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

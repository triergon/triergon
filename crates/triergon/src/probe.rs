//! Facade format selection for a supplied prefix or a blocking source.
use std::io::{ErrorKind, Read};
use triergon_core::signature::{
    self,
    TransportSync::{M2ts192, Ts188},
};
use triergon_core::{Budget, BudgetVec, Error, Result};

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
        Selection::Format(kind) => kind,
        Selection::Need(_) | Selection::Elementary => elementary(bytes),
    }
}

enum Selection {
    Format(Probe),
    Need(usize),
    Elementary,
}

// Opening-box policy belongs to the facade. In particular, an arbitrary first
// word (even a FLAC marker) can be the size of a recognised opening box.
const BOXES: [&[u8; 4]; 8] = [
    b"ftyp", b"styp", b"moov", b"moof", b"mdat", b"free", b"skip", b"sidx",
];

fn container(bytes: &[u8]) -> Selection {
    if signature::ebml(bytes) {
        return Selection::Format(Probe::Matroska);
    }
    if signature::opening_box(bytes).is_some_and(|kind| BOXES.contains(&kind)) {
        return Selection::Format(Probe::Isobmff);
    }
    if signature::ogg(bytes) {
        return Selection::Format(Probe::Ogg);
    }
    if bytes.len() < 4 {
        return Selection::Need(4);
    }
    if bytes.len() == 4 && (bytes == b"OggS" || bytes == b"fLaC") {
        return Selection::Need(5);
    }
    if bytes.len() < 8 && BOXES.iter().any(|kind| kind.starts_with(&bytes[4..])) {
        return Selection::Need(8);
    }
    // TS188 wins even when both three-sync patterns are present.
    for (sync, kind) in [(Ts188, Probe::Ts), (M2ts192, Probe::M2ts)] {
        match sync.observed(bytes) {
            Some(true) => return Selection::Format(kind),
            Some(false) => {}
            None => return Selection::Need(sync.prefix_len()),
        }
    }
    Selection::Elementary
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

/// Probe fixed signatures on the stack. Only a sequential reader needs to
/// retain these bytes. Frame-based probing also needs temporary storage.
pub(super) struct ProbeBuffer {
    header: [u8; M2ts192.prefix_len()],
    used: usize,
    extended: BudgetVec<u8>,
}

impl ProbeBuffer {
    pub(super) fn new(pool: &Budget) -> Self {
        Self {
            header: [0; M2ts192.prefix_len()],
            used: 0,
            extended: BudgetVec::new(pool),
        }
    }

    pub(super) fn identify(&mut self, source: &mut impl Read) -> Result<Probe> {
        loop {
            let bytes = &self.header[..self.used];
            match container(bytes) {
                Selection::Format(kind) => return Ok(kind),
                Selection::Need(end) => {
                    let count = read(source, &mut self.header[self.used..end])?;
                    if count == 0 {
                        return Ok(elementary(&self.header[..self.used]));
                    }
                    self.used += count;
                }
                Selection::Elementary => {
                    if !cfg!(feature = "elementary") || bytes.starts_with(b"fLaC") {
                        return Ok(elementary(bytes));
                    }
                    self.extended.try_extend_from_slice(bytes)?;
                    return read_elementary(source, &mut self.extended);
                }
            }
        }
    }

    pub(super) fn into_replay(mut self) -> Result<BudgetVec<u8>> {
        if self.extended.is_empty() {
            self.extended
                .try_extend_from_slice(&self.header[..self.used])?;
        }
        Ok(self.extended)
    }
}

/// Search the full bounded prefix once. A later MPEG audio or FLAC candidate
/// can change which elementary stream a shorter prefix appears to contain.
fn read_elementary(source: &mut impl Read, prefix: &mut BudgetVec<u8>) -> Result<Probe> {
    const LIMIT: usize = 64 * 1024;
    let mut block = [0; 4096];
    while prefix.len() < LIMIT {
        // End at block boundaries so amortised capacity growth stops at LIMIT.
        let want = block.len() - prefix.len() % block.len();
        let mut used = 0;
        while used < want {
            let count = read(source, &mut block[used..want])?;
            if count == 0 {
                break;
            }
            used += count;
        }
        // A tighter pool can grant an exact intermediate capacity. Cap later
        // growth even if capacity is available on the next iteration.
        if prefix.capacity() <= LIMIT / 2 {
            prefix.try_reserve(used)?;
        }
        prefix.try_extend_from_slice(&block[..used])?;
        if used < want {
            break;
        }
    }
    Ok(elementary(prefix))
}

fn read(source: &mut impl Read, out: &mut [u8]) -> Result<usize> {
    loop {
        match source.read(out) {
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            result => return result.map_err(|error| Error::Io(error.kind())),
        }
    }
}

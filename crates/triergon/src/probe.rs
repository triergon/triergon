//! Format selection from the first bytes of a source.
use triergon_core::signature::{
    self, Container, Selection,
    TransportSync::{M2ts192, Ts188},
    TransportTie,
};

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
    /// Undecided until at least this many prefix bytes are available. For elementary
    /// streams this can be the 64 KiB probe limit. Call [`probe_whole`] at EOF.
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
pub(crate) fn select(prefix: &[u8]) -> Selection {
    // Shared Ogg/FLAC prefix tests measured up to +0.0508% on PGS, within observed count variation.
    // One more byte settles an Ogg page, and tells a FLAC marker from the size of an
    // opening box, where a box type would ask for four.
    if prefix.len() == 4 && (prefix == b"OggS" || prefix == b"fLaC") {
        return Selection::Need(5);
    }
    signature::select(prefix, &POLICY)
}

/// Whether the elementary stream readers are built in.
pub(crate) const ELEMENTARY: bool = cfg!(feature = "elementary");

pub(crate) fn container(container: Container) -> SourceFormat {
    match container {
        Container::Matroska => SourceFormat::Matroska,
        Container::Isobmff => SourceFormat::Isobmff,
        Container::Ogg => SourceFormat::Ogg,
        Container::Transport(Ts188) => SourceFormat::Ts,
        Container::Transport(M2ts192) => SourceFormat::M2ts,
    }
}

/// The elementary stream `bytes` starts, when that reader is built in.
pub(crate) fn elementary(bytes: &[u8]) -> Option<SourceFormat> {
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

/// The most bytes the openers read before they decide.
pub(crate) const LIMIT: usize = 64 * 1024;

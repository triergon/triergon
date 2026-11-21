//! Track metadata of an encoded stream as FFmpeg's MOV demuxer reads it: the `mdhd`
//! creation time and language, the `hdlr` name, the fields of a QuickTime sound
//! description version 1 and the `sdtp` dependency byte. Each value keeps the source
//! box or word it was read from beside the demuxer's reading.
use crate::{
    boxes::BoxView,
    fields,
    movie::{SampleDescription, TrackHeader},
};
use triergon_core::{Bytes, Error, Result};

/// Opt-in token for the metadata view: the legacy QuickTime forms of
/// `EncodedStreamJob::new_description_with_metadata`, the creation tag of
/// [`EncodedCreationSource`] and the `sdtp` disposable flag of
/// `EncodedEditJob::new_with_dependency_table`.
#[derive(Clone, Copy, Debug)]
pub struct EncodedMetadataProfile;
/// Which description of a track to admit, under which major brand and limits.
#[derive(Clone, Copy, Debug)]
pub struct EncodedSelection {
    /// The file's `ftyp` major brand; `None` is `Error::Unsupported`.
    pub major_brand: Option<[u8; 4]>,
    /// 1-based index of the `stsd` entry.
    pub description: u32,
    /// Bounds on the entry's child boxes, descriptors and configuration size.
    pub limits: crate::config::ConfigurationLimits,
}
/// How [`EncodedCreationSource`] read the `mdhd` creation time, or why it has no tag.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CreationCause {
    /// The creation time is zero: no tag.
    SourceZero,
    /// A version-0 time below 2,082,844,800, read as seconds since 1970 rather than 1904.
    VersionZeroUnixObservation,
    /// Seconds since 1904-01-01 UTC, as specified.
    QuickTime1904Seconds,
    /// A version-1 time above `i64::MAX`, which the demuxer reads as negative: no tag.
    VersionOneDemuxNegativeObservation,
    /// The time since 1970 in microseconds does not fit in an `i64`: no tag.
    DemuxMicrosecondsUnrepresentable,
}
/// The `mdhd` creation time and the `creation_time` tag FFmpeg's MOV demuxer derives
/// from it in the metadata view.
#[derive(Clone, Debug)]
pub struct EncodedCreationSource {
    raw: BoxView,
    source_version: u8,
    source_seconds: u64,
    demux: Option<[u8; 27]>,
    cause: CreationCause,
}
impl EncodedCreationSource {
    /// Reads the creation time of `header`'s `mdhd` box. `Error::Unsupported` for
    /// nonzero flags, a version above 1 or a year past 9999.
    pub fn from_source(header: &TrackHeader, _profile: EncodedMetadataProfile) -> Result<Self> {
        let raw = header.raw_mdhd.clone();
        if raw.header().kind != *b"mdhd" {
            return Err(Error::Invalid("creation source is not mdhd"));
        }
        let (version, flags) = fields::full(raw.payload())?;
        if flags != 0 {
            return Err(Error::Unsupported("encoded creation header flags"));
        }
        let seconds = match version {
            0 => u64::from(fields::u32_at(raw.payload(), 4)?),
            1 => fields::u64_at(raw.payload(), 4)?,
            _ => return Err(Error::Unsupported("encoded creation header version")),
        };
        const EPOCH: i128 = 2_082_844_800;
        let (demux, cause) = if seconds == 0 {
            (None, CreationCause::SourceZero)
        } else if version == 1 && seconds > i64::MAX as u64 {
            (None, CreationCause::VersionOneDemuxNegativeObservation)
        } else {
            // A version-bound receiver observation, not a QuickTime epoch rule.
            // The source word and width survive even when this view omits a tag.
            let legacy = version == 0 && i128::from(seconds) < EPOCH;
            let unix = if legacy {
                i128::from(seconds)
            } else {
                i128::from(seconds) - EPOCH
            };
            let micros = unix.checked_mul(1_000_000).ok_or(Error::Overflow)?;
            if i64::try_from(micros).is_err() {
                (None, CreationCause::DemuxMicrosecondsUnrepresentable)
            } else {
                let represented = u64::try_from(unix + EPOCH).map_err(|_| Error::Overflow)?;
                (
                    Some(crate::compat::mov::encoded_configs::calendar_qt_seconds(
                        represented,
                    )?),
                    if legacy {
                        CreationCause::VersionZeroUnixObservation
                    } else {
                        CreationCause::QuickTime1904Seconds
                    },
                )
            }
        };
        Ok(Self {
            raw,
            source_version: version,
            source_seconds: seconds,
            demux,
            cause,
        })
    }
    /// The `mdhd` box.
    pub fn raw(&self) -> &BoxView {
        &self.raw
    }
    /// The `mdhd` version: 0 for a 32-bit creation time, 1 for a 64-bit one.
    pub fn source_version(&self) -> u8 {
        self.source_version
    }
    /// The creation time as stored, nominally in seconds since 1904-01-01 UTC.
    pub fn source_seconds(&self) -> u64 {
        self.source_seconds
    }
    /// The tag text, `YYYY-MM-DDTHH:MM:SS.000000Z` in UTC; `None` when there is no tag.
    pub fn demux(&self) -> Option<&[u8; 27]> {
        self.demux.as_ref()
    }
    /// How the tag was derived, or why there is none.
    pub fn cause(&self) -> CreationCause {
        self.cause
    }
}

/// The four fields a QuickTime sound description version 1 adds, as stored. They
/// describe the codec's packets and never replace the sample table's durations,
/// timescale, sizes or sample boundaries.
#[derive(Clone, Debug)]
pub struct SoundV1Source {
    raw: BoxView,
    /// Uncompressed frames per compressed packet (`samplesPerPacket`).
    pub samples_per_packet: u32,
    /// Bytes per packet for a single channel (`bytesPerPacket`).
    pub bytes_per_packet: u32,
    /// Bytes per frame across all channels (`bytesPerFrame`).
    pub bytes_per_frame: u32,
    /// Bytes of one uncompressed sample (`bytesPerSample`).
    pub bytes_per_sample: u32,
}
impl SoundV1Source {
    pub(crate) fn parse(entry: &SampleDescription) -> Result<Self> {
        Self::parse_mode(entry, false)
    }
    pub(crate) fn parse_contexts(entry: &SampleDescription) -> Result<Self> {
        Self::parse_mode(entry, true)
    }
    pub(crate) fn compression_id(&self) -> i16 {
        let b = self.raw.payload();
        i16::from_be_bytes([b[20], b[21]])
    }
    fn parse_mode(entry: &SampleDescription, context_receiver: bool) -> Result<Self> {
        let raw = entry.raw().clone();
        let b = raw.payload();
        if fields::u16_at(b, 8)? != 1 {
            return Err(Error::Invalid("not sound description version1"));
        }
        fields::slice(b, 0, 44)?;
        if fields::u16_at(b, 20)? != 0xfffe && !(context_receiver && fields::u16_at(b, 20)? == 0) {
            return Err(Error::Unsupported(
                "encoded sound-v1 frame-table compression ID",
            ));
        }
        Ok(Self {
            samples_per_packet: fields::u32_at(b, 28)?,
            bytes_per_packet: fields::u32_at(b, 32)?,
            bytes_per_frame: fields::u32_at(b, 36)?,
            bytes_per_sample: fields::u32_at(b, 40)?,
            raw,
        })
    }
}

/// How the `mdhd` language word was read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LanguageCause {
    /// Three packed ISO 639-2/T letters.
    IsoPacked,
    /// Zero, the QuickTime code for English: reported as `eng`.
    QuickTimeEnglishZero,
    /// `0x7fff`, QuickTime's unspecified language: reported as `und`. Range playback of
    /// `qt  ` files only.
    QuickTimeUnspecified,
}
/// The `mdhd` language word and how FFmpeg's MOV demuxer reads it. The code it
/// reports is the stream view's `language`.
#[derive(Clone, Copy, Debug)]
pub struct LanguageSource {
    /// The word as stored: a pad bit and three 5-bit letters, or a QuickTime code.
    pub raw_word: u16,
    /// The ISO 639-2/T code the demuxer reports for the word.
    pub demux: [u8; 3],
    /// How the word was read.
    pub cause: LanguageCause,
}
pub(crate) fn language(header: &TrackHeader, allow_unspecified: bool) -> Result<LanguageSource> {
    let b = header.raw_mdhd.payload();
    let (version, flags) = fields::full(b)?;
    if flags != 0 {
        return Err(Error::Unsupported("encoded language header flags"));
    }
    let at = match version {
        0 => 20,
        1 => 32,
        _ => return Err(Error::Unsupported("encoded language header version")),
    };
    let word = fields::u16_at(b, at)?;
    if word == 0 {
        return Ok(LanguageSource {
            raw_word: word,
            demux: *b"eng",
            cause: LanguageCause::QuickTimeEnglishZero,
        });
    }
    if word == 0x7fff && allow_unspecified {
        return Ok(LanguageSource {
            raw_word: word,
            demux: *b"und",
            cause: LanguageCause::QuickTimeUnspecified,
        });
    }
    let mut demux = [0; 3];
    for (i, byte) in demux.iter_mut().enumerate() {
        let value = ((word >> (10 - 5 * i)) & 31) as u8;
        if word & 0x8000 != 0 || !(1..=26).contains(&value) {
            return Err(Error::Unsupported(
                "encoded unadmitted legacy language code",
            ));
        }
        *byte = value + b'a' - 1;
    }
    Ok(LanguageSource {
        raw_word: word,
        demux,
        cause: LanguageCause::IsoPacked,
    })
}

/// The `hdlr` name as FFmpeg's MOV demuxer reads it.
#[derive(Clone, Debug)]
pub struct HandlerSource {
    raw: BoxView,
    /// The name bytes, without the terminator or the Pascal length byte; `None` when
    /// the name is empty.
    pub demux: Option<Bytes>,
    /// Whether the name is a Pascal string, as in a QuickTime `mhlr` handler of a `qt  `
    /// file, rather than NUL-terminated.
    pub source_pascal: bool,
}
pub(crate) fn handler(header: &TrackHeader, major: Option<[u8; 4]>) -> Result<HandlerSource> {
    let raw = header.raw_hdlr.clone();
    let b = raw.payload();
    fields::full_zero(b)?;
    let component = fields::fourcc(b, 4)?;
    if fields::slice(b, 12, 12)? != [0; 12] {
        return Err(Error::Unsupported("encoded handler manufacturer/flags"));
    }
    let tail = b.get(24..).ok_or(Error::Truncated)?;
    if tail.len() > 256 {
        return Err(Error::Limit {
            requested: tail.len(),
            maximum: 256,
        });
    }
    let (skip, length, pascal) = if component == [0; 4] {
        let n = tail
            .iter()
            .position(|b| *b == 0)
            .ok_or(Error::Unsupported("encoded unterminated handler"))?;
        (0, n, false)
    } else if component == *b"mhlr" && major == Some(*b"qt  ") {
        let n = usize::from(*tail.first().ok_or(Error::Truncated)?);
        if tail.len() != n.checked_add(1).ok_or(Error::Overflow)? {
            return Err(Error::Unsupported("encoded Pascal handler extent"));
        }
        (1, n, true)
    } else {
        return Err(Error::Unsupported("encoded legacy handler namespace"));
    };
    let text = fields::slice(tail, skip, length)?;
    if !text.iter().all(|b| b.is_ascii() && *b != 0) {
        return Err(Error::Unsupported("encoded handler text character set"));
    }
    let start = usize::from(raw.header().header_size) + 24 + skip;
    let demux = if length == 0 {
        None
    } else {
        Some(raw.bytes().slice(start..start + length)?)
    };
    Ok(HandlerSource {
        raw,
        demux,
        source_pascal: pascal,
    })
}
impl HandlerSource {
    /// The `hdlr` box.
    pub fn raw(&self) -> &BoxView {
        &self.raw
    }
}

/// The track metadata an [`EncodedStream`](crate::compat::mov::encoded::EncodedStream)
/// was derived from, for streams admitted by the metadata, context and range playback
/// views. Each part keeps the source box or word it was read from.
#[derive(Clone, Debug)]
pub struct EncodedSource {
    /// The `mdhd` language.
    pub language: LanguageSource,
    /// The `hdlr` name.
    pub handler: HandlerSource,
    /// The QuickTime sound description version 1 fields of an AAC entry; `None`
    /// otherwise.
    pub sound: Option<SoundV1Source>,
    /// The entry's `wave` box, when its configuration is read from one.
    pub wave: Option<BoxView>,
}

/// The `sample_is_depended_on` field of an `sdtp` byte (bits 3 and 2). FFmpeg's MOV
/// demuxer marks a packet disposable when it is `Unreferenced`; the field says nothing
/// about sync samples or decoder recovery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DependedOn {
    /// 0: unknown.
    Unspecified,
    /// 1: other samples depend on this one.
    Referenced,
    /// 2: no other sample depends on this one.
    Unreferenced,
    /// 3: reserved.
    Reserved,
}
/// One `sdtp` byte with its decoded `sample_is_depended_on` field.
#[derive(Clone, Copy, Debug)]
pub struct DependencyView {
    /// The byte as stored.
    pub raw: u8,
    /// Its `sample_is_depended_on` field.
    pub depended_on: DependedOn,
}
impl DependencyView {
    pub(crate) fn from_source(raw: u8) -> Self {
        let depended_on = match raw & 0x0c {
            0 => DependedOn::Unspecified,
            4 => DependedOn::Referenced,
            8 => DependedOn::Unreferenced,
            _ => DependedOn::Reserved,
        };
        Self { raw, depended_on }
    }
    /// Whether FFmpeg's MOV demuxer marks the packet disposable: only for `Unreferenced`.
    /// The metadata view refuses the reserved value; the recovery view reads it through
    /// `DependencyObservation`. Leading and sync facts are not used.
    pub(crate) fn demux_disposable_hint(self) -> bool {
        self.depended_on == DependedOn::Unreferenced
    }
}

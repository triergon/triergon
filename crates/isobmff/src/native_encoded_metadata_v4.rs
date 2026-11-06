//! Source-owned legacy metadata and separately named pinned receiver facts.
//! No source creation word, handler bytes, language word or audio unit changes.
use crate::{BoxView, SampleDescription, TrackHeader, fields};
use triergon_core::{Bytes, Error, Result};

#[derive(Clone, Copy, Debug)]
pub struct PinnedMovD32EncodedRegularV4;
#[derive(Clone, Copy, Debug)]
pub struct Mov9EncodedSelectionV4 {
    pub major_brand: Option<[u8; 4]>,
    pub description: u32,
    pub limits: crate::Mov9EncodedLimitsV1,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mov9CreationCauseV4 {
    SourceZero,
    VersionZeroUnixObservation,
    QuickTime1904Seconds,
    VersionOneNativeNegativeObservation,
    NativeMicrosecondsUnrepresentable,
}
#[derive(Clone, Debug)]
pub struct Mov9EncodedCreationV4 {
    raw: BoxView,
    source_version: u8,
    source_seconds: u64,
    native: Option<[u8; 27]>,
    cause: Mov9CreationCauseV4,
}
impl Mov9EncodedCreationV4 {
    pub fn from_source(
        header: &TrackHeader,
        _profile: PinnedMovD32EncodedRegularV4,
    ) -> Result<Self> {
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
        let (native, cause) = if seconds == 0 {
            (None, Mov9CreationCauseV4::SourceZero)
        } else if version == 1 && seconds > i64::MAX as u64 {
            (
                None,
                Mov9CreationCauseV4::VersionOneNativeNegativeObservation,
            )
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
                (None, Mov9CreationCauseV4::NativeMicrosecondsUnrepresentable)
            } else {
                let represented = u64::try_from(unix + EPOCH).map_err(|_| Error::Overflow)?;
                (
                    Some(crate::native_encoded_configs_v3::calendar_qt_seconds_v4(
                        represented,
                    )?),
                    if legacy {
                        Mov9CreationCauseV4::VersionZeroUnixObservation
                    } else {
                        Mov9CreationCauseV4::QuickTime1904Seconds
                    },
                )
            }
        };
        Ok(Self {
            raw,
            source_version: version,
            source_seconds: seconds,
            native,
            cause,
        })
    }
    pub fn raw(&self) -> &BoxView {
        &self.raw
    }
    pub fn source_version(&self) -> u8 {
        self.source_version
    }
    pub fn source_seconds(&self) -> u64 {
        self.source_seconds
    }
    pub fn native(&self) -> Option<&[u8; 27]> {
        self.native.as_ref()
    }
    pub fn cause(&self) -> Mov9CreationCauseV4 {
        self.cause
    }
}

/// Audio-v1 words retain their actual units. In particular samples-per-packet
/// never replaces STTS, MDHD timescale, byte count or source sample boundaries.
#[derive(Clone, Debug)]
pub struct Mov9SoundV1SourceV4 {
    raw: BoxView,
    pub samples_per_packet: u32,
    pub bytes_per_packet: u32,
    pub bytes_per_frame: u32,
    pub bytes_per_sample: u32,
}
impl Mov9SoundV1SourceV4 {
    pub(crate) fn parse(entry: &SampleDescription) -> Result<Self> {
        Self::parse_mode(entry, false)
    }
    pub(crate) fn parse_v7(entry: &SampleDescription) -> Result<Self> {
        Self::parse_mode(entry, true)
    }
    pub fn compression_id(&self) -> i16 {
        let b = self.raw.payload();
        i16::from_be_bytes([b[20], b[21]])
    }
    fn parse_mode(entry: &SampleDescription, receiver_v7: bool) -> Result<Self> {
        let raw = entry.raw().clone();
        let b = raw.payload();
        if fields::u16_at(b, 8)? != 1 {
            return Err(Error::Invalid("not sound description version1"));
        }
        fields::slice(b, 0, 44)?;
        if fields::u16_at(b, 20)? != 0xfffe && !(receiver_v7 && fields::u16_at(b, 20)? == 0) {
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
    pub fn raw(&self) -> &BoxView {
        &self.raw
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mov9LanguageCauseV4 {
    IsoPacked,
    QuickTimeEnglishZero,
    QuickTimeUnspecified,
}
#[derive(Clone, Copy, Debug)]
pub struct Mov9LanguageV4 {
    pub raw_word: u16,
    pub native: [u8; 3],
    pub cause: Mov9LanguageCauseV4,
}
pub(crate) fn language(header: &TrackHeader, allow_unspecified: bool) -> Result<Mov9LanguageV4> {
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
        return Ok(Mov9LanguageV4 {
            raw_word: word,
            native: *b"eng",
            cause: Mov9LanguageCauseV4::QuickTimeEnglishZero,
        });
    }
    if word == 0x7fff && allow_unspecified {
        return Ok(Mov9LanguageV4 {
            raw_word: word,
            native: *b"und",
            cause: Mov9LanguageCauseV4::QuickTimeUnspecified,
        });
    }
    let mut native = [0; 3];
    for (i, byte) in native.iter_mut().enumerate() {
        let value = ((word >> (10 - 5 * i)) & 31) as u8;
        if word & 0x8000 != 0 || !(1..=26).contains(&value) {
            return Err(Error::Unsupported(
                "encoded unadmitted legacy language code",
            ));
        }
        *byte = value + b'a' - 1;
    }
    Ok(Mov9LanguageV4 {
        raw_word: word,
        native,
        cause: Mov9LanguageCauseV4::IsoPacked,
    })
}

#[derive(Clone, Debug)]
pub struct Mov9HandlerV4 {
    raw: BoxView,
    pub native: Option<Bytes>,
    pub source_pascal: bool,
}
pub(crate) fn handler(header: &TrackHeader, major: Option<[u8; 4]>) -> Result<Mov9HandlerV4> {
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
    let native = if length == 0 {
        None
    } else {
        Some(raw.bytes().slice(start..start + length)?)
    };
    Ok(Mov9HandlerV4 {
        raw,
        native,
        source_pascal: pascal,
    })
}
impl Mov9HandlerV4 {
    pub fn raw(&self) -> &BoxView {
        &self.raw
    }
}

/// Only the explicitly selected V4 receiver uses these metadata-presence facts.
/// The original source owners retain every byte, including unknown extension
/// atoms, the Pascal length and the legacy language word.
#[derive(Clone, Debug)]
pub struct Mov9EncodedSourceV4 {
    pub language: Mov9LanguageV4,
    pub handler: Mov9HandlerV4,
    pub sound: Option<Mov9SoundV1SourceV4>,
    pub wave: Option<BoxView>,
}

/// A packet hint in the separate D32 view. Source dependency byte and STSS
/// access remain distinct; this grants no decoder or random-access proof.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mov9DependedOnV4 {
    Unspecified,
    Referenced,
    Unreferenced,
    Reserved,
}
#[derive(Clone, Copy, Debug)]
pub struct Mov9DependencyViewV4 {
    pub raw: u8,
    pub depended_on: Mov9DependedOnV4,
}
impl Mov9DependencyViewV4 {
    pub fn from_source(raw: u8) -> Self {
        let depended_on = match raw & 0x0c {
            0 => Mov9DependedOnV4::Unspecified,
            4 => Mov9DependedOnV4::Referenced,
            8 => Mov9DependedOnV4::Unreferenced,
            _ => Mov9DependedOnV4::Reserved,
        };
        Self { raw, depended_on }
    }
    /// Only the separate V4 edit-plan view uses this hint after checking its
    /// source/group context. It does not reinterpret source leading/key facts.
    /// Source reserved values remain refused by V4 and distinct in V5.
    pub fn native_disposable_hint(self) -> bool {
        self.depended_on == Mov9DependedOnV4::Unreferenced
    }
}

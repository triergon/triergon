//! Track entries of a Segment: the source fields of each `TrackEntry`.
use crate::storage::{SharedBytes, SharedText};

pub(crate) mod extensions;
pub(crate) mod parse;

/// AAC profiles carried by deprecated Matroska CodecIDs.
///
/// See sections 3.4.2–3.4.10 of the Matroska codec mapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LegacyAacProfile {
    Main,
    LowComplexity,
    ScalableSamplingRate,
    LowComplexitySbr,
    LongTermPrediction,
}

impl LegacyAacProfile {
    pub(crate) fn from_codec_id(codec: &str) -> Option<Self> {
        Some(match codec {
            "A_AAC/MPEG2/MAIN" | "A_AAC/MPEG4/MAIN" => Self::Main,
            "A_AAC/MPEG2/LC" | "A_AAC/MPEG4/LC" => Self::LowComplexity,
            "A_AAC/MPEG2/SSR" | "A_AAC/MPEG4/SSR" => Self::ScalableSamplingRate,
            "A_AAC/MPEG2/LC/SBR" | "A_AAC/MPEG4/LC/SBR" => Self::LowComplexitySbr,
            "A_AAC/MPEG4/LTP" => Self::LongTermPrediction,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug)]
pub struct Track {
    pub(crate) packet_prefix: SharedBytes,
    pub(crate) packet_decoder: Option<crate::content_encoding::Decoder>,
    codec_private_present: bool,
    audio_settings_present: bool,
    /// Complete bounded TrackEntry payload, retaining extensions not interpreted yet.
    pub source_metadata: SharedBytes,
    pub number: u64,
    pub uid: u64,
    pub kind: u64,
    pub codec: SharedText,
    /// Original or restored codec configuration. An original source view can
    /// retain the whole charged Tracks allocation; len() is its logical size.
    pub codec_private: SharedBytes,
    pub name: SharedText,
    /// Legacy Matroska language. Use effective_language() for source BCP47 precedence.
    pub language: SharedText,
    pub language_bcp47: Option<SharedText>,
    pub enabled: bool,
    pub lacing: bool,
    pub hearing_impaired: Option<bool>,
    pub visual_impaired: Option<bool>,
    pub text_descriptions: Option<bool>,
    pub original: Option<bool>,
    pub commentary: Option<bool>,
    pub default: bool,
    pub forced: bool,
    pub default_duration_ns: Option<u64>,
    pub codec_delay_ns: u64,
    pub seek_preroll_ns: u64,
    pub width: u64,
    pub height: u64,
    pub sample_rate: f64,
    pub channels: u64,
    /// Bounded static colour/HDR metadata; complete source TrackEntry remains above.
    pub colour: Option<crate::colour::Colour>,
}

impl Track {
    pub fn effective_language(&self) -> &str {
        self.language_bcp47.as_deref().unwrap_or(&self.language)
    }

    /// Whether the TrackEntry wrote a CodecPrivate element, including an empty one.
    pub const fn has_codec_private(&self) -> bool {
        self.codec_private_present
    }

    /// Whether the TrackEntry contains an Audio element.
    pub const fn has_audio_settings(&self) -> bool {
        self.audio_settings_present
    }

    /// The profile signalled by a deprecated AAC CodecID on an audio track.
    pub fn legacy_aac_profile(&self) -> Option<LegacyAacProfile> {
        if self.kind == 2 {
            LegacyAacProfile::from_codec_id(&self.codec)
        } else {
            None
        }
    }
}

fn static_metadata_text(bytes: &[u8]) -> Option<&'static str> {
    Some(match bytes {
        b"V_MPEG4/ISO/AVC" => "V_MPEG4/ISO/AVC",
        b"V_MPEGH/ISO/HEVC" => "V_MPEGH/ISO/HEVC",
        b"A_PCM/INT/LIT" => "A_PCM/INT/LIT",
        b"A_AAC" => "A_AAC",
        b"A_AC3" => "A_AC3",
        b"A_EAC3" => "A_EAC3",
        b"A_VORBIS" => "A_VORBIS",
        b"S_TEXT/UTF8" => "S_TEXT/UTF8",
        b"S_TEXT/ASS" => "S_TEXT/ASS",
        b"S_VOBSUB" => "S_VOBSUB",
        b"eng" => "eng",
        b"und" => "und",
        _ => return None,
    })
}

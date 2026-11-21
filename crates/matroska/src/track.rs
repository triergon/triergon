//! Track entries of a Segment: the source fields of each `TrackEntry`.
use crate::storage::{SharedBytes, SharedText};

pub(crate) mod extensions;
pub(crate) mod parse;

/// AAC profiles carried by deprecated Matroska CodecIDs.
///
/// See sections 3.4.2–3.4.10 of the Matroska codec mapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LegacyAacProfile {
    /// AAC Main: `A_AAC/MPEG2/MAIN` or `A_AAC/MPEG4/MAIN`.
    Main,
    /// AAC LC: `A_AAC/MPEG2/LC` or `A_AAC/MPEG4/LC`.
    LowComplexity,
    /// AAC SSR: `A_AAC/MPEG2/SSR` or `A_AAC/MPEG4/SSR`.
    ScalableSamplingRate,
    /// AAC LC with SBR: `A_AAC/MPEG2/LC/SBR` or `A_AAC/MPEG4/LC/SBR`.
    LowComplexitySbr,
    /// AAC LTP: `A_AAC/MPEG4/LTP`.
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

/// One TrackEntry's source fields, with the specification default for each
/// absent optional element.
#[derive(Clone, Debug)]
pub struct Track {
    pub(crate) packet_prefix: SharedBytes,
    pub(crate) packet_decoder: Option<crate::content_encoding::Decoder>,
    codec_private_present: bool,
    audio_settings_present: bool,
    /// Complete bounded TrackEntry payload, retaining extensions not interpreted yet.
    pub source_metadata: SharedBytes,
    /// TrackNumber: the nonzero number Blocks reference, unique in the Segment.
    pub number: u64,
    /// TrackUID: the nonzero identifier tags and chapters reference.
    pub uid: u64,
    /// TrackType: 1 video, 2 audio, 3 complex, 16 logo, 17 subtitle, 18 buttons,
    /// 32 control, 33 metadata; nonzero.
    pub kind: u64,
    /// CodecID, for example `V_VP9` or `A_OPUS`; never empty.
    pub codec: SharedText,
    /// Original or restored codec configuration. An original source view can
    /// retain the whole charged Tracks allocation; len() is its logical size.
    pub codec_private: SharedBytes,
    /// Name, a human-readable track name; empty when absent.
    pub name: SharedText,
    /// Legacy Matroska language. Use effective_language() for source BCP47 precedence.
    pub language: SharedText,
    /// LanguageBCP47; `None` when absent.
    pub language_bcp47: Option<SharedText>,
    /// FlagEnabled; `true` when absent.
    pub enabled: bool,
    /// FlagLacing: whether Blocks of the track may be laced; `true` when absent.
    pub lacing: bool,
    /// FlagHearingImpaired; `None` when absent.
    pub hearing_impaired: Option<bool>,
    /// FlagVisualImpaired; `None` when absent.
    pub visual_impaired: Option<bool>,
    /// FlagTextDescriptions; `None` when absent.
    pub text_descriptions: Option<bool>,
    /// FlagOriginal; `None` when absent.
    pub original: Option<bool>,
    /// FlagCommentary; `None` when absent.
    pub commentary: Option<bool>,
    /// FlagDefault; `true` when absent.
    pub default: bool,
    /// FlagForced; `false` when absent.
    pub forced: bool,
    /// DefaultDuration: nanoseconds per frame; `None` when absent.
    pub default_duration_ns: Option<u64>,
    /// CodecDelay in nanoseconds, subtracted from presentation times; 0 when absent.
    pub codec_delay_ns: u64,
    /// SeekPreRoll in nanoseconds of decoding needed before a seek target; 0 when absent.
    pub seek_preroll_ns: u64,
    /// PixelWidth of a video track; 0 when absent.
    pub width: u64,
    /// PixelHeight of a video track; 0 when absent.
    pub height: u64,
    /// SamplingFrequency of an audio track in Hz, finite and positive; 8000 when absent.
    pub sample_rate: f64,
    /// Channels of an audio track; 1 when absent.
    pub channels: u64,
    /// Bounded static colour/HDR metadata; complete source TrackEntry remains above.
    pub colour: Option<crate::colour::Colour>,
}

impl Track {
    /// LanguageBCP47 when present, otherwise the Language element (`eng` when absent).
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

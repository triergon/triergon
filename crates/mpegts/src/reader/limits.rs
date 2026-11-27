//! Reader bounds and the policies for unfinished input.
use crate::clock::SourceTimePolicy;
use crate::compat::adts::{self, PacketPolicy};
use crate::compat::latm;
use crate::psi::DescriptorNamespace;
use crate::{PacketFormat, ParseLimits};
use triergon_framing::nal::RawAnnexBLimits;

/// Reporting of unfinished audio bytes at logical source EOF.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AudioEofPolicy {
    /// Keep the audio framer's truncation error.
    #[default]
    RejectIncomplete,
    /// Report validated PES bytes as a source fragment, never as a complete AU.
    ReportIncomplete,
}

/// Reporting of an overlong final DVB subtitle segment in a validated finite PES.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DvbSubtitleTailPolicy {
    /// Fail with `Truncated`.
    #[default]
    RejectTruncated,
    /// Report complete preceding segments as a source event with the original PES.
    ReportCompletePrefix,
}

/// Reporting of an unframed AVC or HEVC byte run when the source ends it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UnframedVideo {
    /// Keep the video framer's leading-byte error.
    #[default]
    Refuse,
    /// Report a nonempty validated PES run with invalid leading bytes and no Annex B start code
    /// as [`crate::ReaderEvent::IncompleteVideo`] when EOF, a fault, a signalled discontinuity,
    /// a clock epoch change or a table change ends it. A caller-declared discontinuity
    /// discards the run and emits [`crate::ReaderEvent::Interrupted`].
    Report,
}

/// Reporting of unframed payload in a private-audio carriage when the source ends its
/// byte run, at logical EOF or before a fault or boundary.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UnframedAudioPolicy {
    /// Require the source namespace before admitting a private-audio framer.
    #[default]
    Refuse,
    /// With unspecified namespace, admit one finite private-audio PES per uninterrupted run.
    /// Its payload must be free of AC-3 and E-AC-3 syncwords and their first byte at the tail.
    /// The observation preserves the track's source codec and namespace.
    Report,
}

/// Bounds and source interpretation for the AU reader, independent of its live storage budget.
/// Multiprogram transports require explicit selection.
#[derive(Clone, Copy, Debug)]
pub struct ReaderLimits {
    /// The packet format of the source: 188-byte TS or 192-byte M2TS.
    pub format: PacketFormat,
    /// The program number to read; `None` selects the sole media program of the PAT. Must not be 0.
    pub program: Option<u16>,
    /// Out-of-band interpretation of private descriptors, applied to every PMT.
    pub descriptor_namespace: DescriptorNamespace,
    /// Limits for transport packets, PSI sections, tables and PES packets.
    pub transport: ParseLimits,
    /// Limits for AVC and HEVC Annex B access-unit framing.
    pub video: RawAnnexBLimits,
    /// Nonzero byte cap for one audio frame. E-AC-3 groups have a separate derived cap.
    pub maximum_audio_frame_bytes: usize,
    /// Nonzero byte cap for a finite aligned DVB DTS-HD PES.
    pub maximum_dts_hd_pes_bytes: usize,
    /// Reporting of unfinished audio bytes at logical source EOF.
    pub audio_eof_policy: AudioEofPolicy,
    /// Handling of private-audio payload that no framer is admitted for.
    pub unframed_audio_policy: UnframedAudioPolicy,
    /// Explicit DTS core identity for one unlabelled private-data route.
    pub unlabelled_dts_core: Option<crate::compat::dts::UnlabelledCore>,
    /// Probe the initial unlabelled private PES for two canonical DTS core envelopes.
    /// Disabled by default. See [DTS admission](crate::compat::dts).
    pub probe_unlabelled_dts_core: bool,
    /// Compatibility policy for the reserved bits in a program clock reference.
    pub pcr_policy: crate::compat::pes::PcrPolicy,
    /// Framing policy for ADTS. The compatibility policy handles ADTS EOF itself.
    pub adts_packet_policy: PacketPolicy,
    /// Wire policy for ADTS PES with a zero packet-length field.
    pub adts_pes_policy: adts::PesPolicy,
    /// How LATM PES are delivered: framed from their LOAS envelopes (the default), or whole,
    /// one observation per PES.
    pub latm_packet_policy: latm::PacketPolicy,
    /// Reporting of an overlong final DVB subtitle segment.
    pub dvb_subtitle_tail_policy: DvbSubtitleTailPolicy,
    /// Reporting of an unframed AVC or HEVC byte run when the source ends it.
    pub unframed_video_policy: UnframedVideo,
    /// Nonzero cap on units a track can publish before its lane is reset. An additional
    /// unit returns `Limit`. Codec-specific framing limits also apply.
    pub maximum_access_units: usize,
    /// Nonzero cap on retained PES packets contributing to one unit. A larger extent
    /// returns `Limit`.
    pub maximum_pes_per_access_unit: usize,
    /// Input window, in bytes, of the elementary-stream framers; must be nonzero.
    pub elementary_input_window: usize,
    /// Largest physical byte extent examined by a seek. Zero disables seeking.
    pub maximum_seek_scan_bytes: u64,
    /// How packet timestamps are lifted to exact time; [`SourceTimePolicy::RelativeEpochZero`]
    /// by default. Each carries its [`SourceTimeStatus`](crate::clock::SourceTimeStatus); a
    /// missing DTS or a per-frame timestamp is never made up.
    pub time_policy: SourceTimePolicy,
}

impl Default for ReaderLimits {
    fn default() -> Self {
        Self {
            format: PacketFormat::Ts188,
            program: None,
            descriptor_namespace: DescriptorNamespace::Unspecified,
            transport: ParseLimits::default(),
            video: RawAnnexBLimits::default(),
            maximum_audio_frame_bytes: 8192,
            maximum_dts_hd_pes_bytes: 65_535,
            audio_eof_policy: AudioEofPolicy::RejectIncomplete,
            unframed_audio_policy: UnframedAudioPolicy::Refuse,
            unlabelled_dts_core: None,
            probe_unlabelled_dts_core: false,
            pcr_policy: crate::compat::pes::PcrPolicy::Strict,
            adts_packet_policy: PacketPolicy::Strict,
            adts_pes_policy: adts::PesPolicy::Strict,
            latm_packet_policy: latm::PacketPolicy::Strict,
            dvb_subtitle_tail_policy: DvbSubtitleTailPolicy::RejectTruncated,
            unframed_video_policy: UnframedVideo::Refuse,
            maximum_access_units: 32_000_000,
            maximum_pes_per_access_unit: 4096,
            elementary_input_window: 64 * 1024,
            maximum_seek_scan_bytes: 64 * 1024 * 1024,
            time_policy: SourceTimePolicy::RelativeEpochZero,
        }
    }
}

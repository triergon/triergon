//! Reader bounds and the policies for unfinished input.
use crate::clock::SourceTimePolicy;
use crate::compat::adts::{self, PacketPolicy};
use crate::compat::annex_b::VideoEofPolicy;
use crate::compat::latm;
use crate::compat::pes::UnframedAudioPolicy;
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

/// Bounds and source interpretation for the AU reader, independent of its live
/// storage budget. An absent program selects the sole PAT media program;
/// transports with several programs require an explicit selection.
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
    /// Maximum bytes in one audio syncframe. E-AC-3 groups have a derived bound.
    pub maximum_audio_frame_bytes: usize,
    /// Maximum bytes in a finite aligned DVB DTS-HD PES.
    pub maximum_dts_hd_pes_bytes: usize,
    /// Reporting of unfinished audio bytes at logical source EOF.
    pub audio_eof_policy: AudioEofPolicy,
    /// Handling of private-audio payload that no framer is admitted for.
    pub unframed_audio_policy: UnframedAudioPolicy,
    /// Explicit DTS core identity for one unlabelled private-data route.
    pub unlabelled_dts_core: Option<crate::compat::dts::UnlabelledCore>,
    /// Compatibility policy for the reserved bits in a program clock reference.
    pub pcr_policy: crate::compat::pes::PcrPolicy,
    /// Framing policy for ADTS. The compatibility policy handles ADTS EOF itself.
    pub adts_packet_policy: PacketPolicy,
    /// Wire policy for ADTS PES with a zero packet-length field.
    pub adts_pes_policy: adts::PesPolicy,
    /// LOAS envelope framing by default. PES preservation emits source events.
    pub latm_packet_policy: latm::PacketPolicy,
    /// Reporting of an overlong final DVB subtitle segment.
    pub dvb_subtitle_tail_policy: DvbSubtitleTailPolicy,
    /// Reporting of an unframed AVC or HEVC run at logical source EOF.
    pub video_eof_policy: VideoEofPolicy,
    /// Maximum access units one ADTS stream delivers between resets; must be nonzero. A further
    /// unit fails with `Limit`.
    pub maximum_access_units: usize,
    /// Maximum PES packets that contribute to one access unit; must be nonzero. A larger access
    /// unit
    /// fails with `Limit`.
    pub maximum_pes_per_access_unit: usize,
    /// Input window, in bytes, of the elementary-stream framers; must be nonzero.
    pub elementary_input_window: usize,
    /// Largest physical byte extent examined by a seek. Zero disables seeking.
    pub maximum_seek_scan_bytes: u64,
    /// Period-zero timestamps by default. Every lifted value carries its
    /// SourceTimeStatus; no missing DTS or per-frame timestamp is synthesized.
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
            unframed_audio_policy: UnframedAudioPolicy::Strict,
            unlabelled_dts_core: None,
            pcr_policy: crate::compat::pes::PcrPolicy::Strict,
            adts_packet_policy: PacketPolicy::Strict,
            adts_pes_policy: adts::PesPolicy::Strict,
            latm_packet_policy: latm::PacketPolicy::Strict,
            dvb_subtitle_tail_policy: DvbSubtitleTailPolicy::RejectTruncated,
            video_eof_policy: VideoEofPolicy::RejectUnframed,
            maximum_access_units: 32_000_000,
            maximum_pes_per_access_unit: 4096,
            elementary_input_window: 64 * 1024,
            maximum_seek_scan_bytes: 64 * 1024 * 1024,
            time_policy: SourceTimePolicy::RelativeEpochZero,
        }
    }
}

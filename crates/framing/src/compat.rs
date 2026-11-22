//! Reference-compatible policies.
//!
//! The types at the crate root follow the specifications and refuse what they do
//! not allow. Where FFmpeg accepts or frames an input differently and a consumer
//! needs FFmpeg's result, the policies here reproduce FFmpeg's behaviour for that
//! input. Each module is one policy family; its documentation names the FFmpeg build
//! it reproduces.
//!
//! Projections that exist only to compare traces with FFmpeg are not policies. They
//! are compiled only with the `reference-trace` feature.

/// The NAL packet receiver's configuration acceptance.
///
/// [`NalPacketSession`](crate::NalPacketSession) frames length-prefixed AVC and
/// HEVC samples as FFmpeg's `h264_mp4toannexb` and `hevc_mp4toannexb` bitstream
/// filters do. The record parsers at the crate root
/// ([`AvcConfigParser`](crate::AvcConfigParser),
/// [`HvcConfigParser`](crate::HvcConfigParser)) refuse a three-byte NAL length
/// (`lengthSizeMinusOne = 2`); the filters accept it. The parsers here accept what
/// the receiver accepts and keep the strict refusal readable through
/// `strict_source_refusal()`.
///
/// Pass their output as
/// [`NalPacketConfiguration::ReceiverAvc`](crate::NalPacketConfiguration::ReceiverAvc)
/// or [`ReceiverHevc`](crate::NalPacketConfiguration::ReceiverHevc), and AVC
/// side-data updates to
/// [`NalPacketSession::receiver_packet`](crate::NalPacketSession::receiver_packet).
/// That path also frames NAL units whose header the strict path refuses and records
/// the refusal on the output or rejection.
///
/// Reference: FFmpeg git revision `d32b387f` on the 9.0 branch.
pub mod nal {
    pub use crate::nal_receiver_config::{
        ReceiverAvcConfigParser, ReceiverAvcConfiguration, ReceiverAvcUpdateRejection,
        ReceiverHevcConfigParser, ReceiverHevcConfiguration,
    };
}

/// VVC Annex B picture framing with three-byte prefixes at embedded headers.
///
/// FFprobe 9.0.2 recognizes a subsequent picture from an embedded picture
/// header even when its access-unit prefix omits the extra zero byte required
/// by H.266 Annex B. The returned ranges retain the original prefix and
/// payload bytes. A standalone picture header still requires the strict
/// access-unit prefix.
pub mod vvc {
    use crate::{RawAnnexBLimits, VvcAccessUnits, VvcAnnexBReader};
    use triergon_core::{Budget, Result, SourceCapabilities, SourceId};

    /// Group a complete, bounded layer zero stream into borrowed access units.
    pub fn access_units(data: &[u8], maximum_bytes: usize) -> Result<VvcAccessUnits<'_>> {
        VvcAccessUnits::allow_short_embedded_prefix(data, maximum_bytes)
    }

    /// Incremental framing with the same embedded-header prefix policy.
    pub fn access_unit_reader(
        pool: &Budget,
        source: SourceId,
        caps: SourceCapabilities,
        limits: RawAnnexBLimits,
    ) -> Result<VvcAnnexBReader> {
        Ok(VvcAnnexBReader::new(pool, source, caps, limits)?.with_short_embedded_prefix())
    }
}

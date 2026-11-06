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
        ReceiverAvcConfigParser, ReceiverAvcConfigViewParser, ReceiverAvcConfiguration,
        ReceiverAvcConfigurationView, ReceiverAvcUpdateRejection, ReceiverHevcConfigParser,
        ReceiverHevcConfigViewParser, ReceiverHevcConfiguration, ReceiverHevcConfigurationView,
    };
}

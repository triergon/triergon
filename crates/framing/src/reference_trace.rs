//! Comparison-only projections, compiled with the `reference-trace` feature.
//!
//! These report values in the form FFmpeg's public API exposes them, so the
//! harness can compare traces byte for byte. They are not behaviour a consumer
//! selects, and they are absent from default builds. With the feature enabled,
//! [`NalPacketRejection::native_receive_result`](crate::NalPacketRejection::native_receive_result),
//! [`ReceiverAvcUpdateRejection::native_receive_result`](crate::compat::nal::ReceiverAvcUpdateRejection::native_receive_result),
//! [`NalPacketConfigRejection::native_initialize_result`](crate::NalPacketConfigRejection::native_initialize_result)
//! and [`ParserPacket::parser_hints`](crate::compat::dts::ParserPacket::parser_hints)
//! are also available. The reference revision is the one named by the matching
//! [`compat`](crate::compat) family.

/// The `dca` parser's byte positions for one parser packet.
pub mod dts {
    pub use crate::dts14_parser_view::{PARSER_HINT_PROFILE, ParserHintOrigin, ParserHints};
}

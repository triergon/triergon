//! Comparison-only projections, compiled with the `reference-trace` feature.
//!
//! These report values in the form FFmpeg's public API exposes them, so the
//! harness can compare traces byte for byte. They are not behaviour a consumer
//! selects outside a comparison, and they are absent from default builds. With the
//! feature enabled,
//! [`NalPacketRejection::native_receive_result`](crate::NalPacketRejection::native_receive_result),
//! [`ReceiverAvcUpdateRejection::native_receive_result`](crate::compat::nal::ReceiverAvcUpdateRejection::native_receive_result)
//! and
//! [`NalPacketConfigRejection::native_initialize_result`](crate::NalPacketConfigRejection::native_initialize_result)
//! are also available; their reference revision is the one named on
//! [`compat::nal`](crate::compat::nal).

/// The raw 14-bit DTS parser's packets, clock and byte positions.
///
/// [`ParserOperation`](dts::ParserOperation) reads one raw 14-bit DTS entity
/// through two cursors fed from the same bytes: the complete source frames
/// ([`Dts14FrameReader`](crate::Dts14FrameReader)) and the packets FFmpeg's `dca`
/// parser cuts ([`Dts14CarrierReader`](crate::Dts14CarrierReader)). Each
/// [`ParserPacket`](dts::ParserPacket) carries the parser's clock
/// ([`ParserClock`](dts::ParserClock)): the first accepted core sample rate is
/// latched and later coding windows are rounded in it. That clock is not a source
/// duration. [`ParserHints`](dts::ParserHints) are the parser's internal byte
/// counters, not source positions. [`FactAssociationJob`](dts::FactAssociationJob)
/// relates a source frame to the parser packet that contains it.
///
/// Reference: the `dca` parser with default flags at FFmpeg git revision
/// `d32b387f` on the 9.0 branch.
pub mod dts {
    pub use crate::dts_operation::{
        FactAssociation, FactAssociationJob, ParserClock, ParserClockDisposition, ParserOperation,
        ParserOperationContext, ParserOperationEvent, ParserOperationLane, ParserOperationLimits,
        ParserOperationStep, ParserOperationTerminal, ParserPacket,
    };
    pub use crate::dts14_parser_view::{PARSER_HINT_PROFILE, ParserHintOrigin, ParserHints};
}

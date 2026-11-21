//! Comparison-only projections, compiled with the `reference-trace` feature.
//!
//! These report values in the form FFmpeg's public API exposes them, so tests
//! can compare them with recorded reference output. They are not behaviour a consumer
//! selects outside a comparison, and they are absent from default builds. With the
//! feature enabled,
//! [`NalPacketRejection::native_receive_result`](crate::nal::NalPacketRejection::native_receive_result),
//! [`ReceiverAvcUpdateRejection::native_receive_result`](crate::compat::nal::ReceiverAvcUpdateRejection::native_receive_result)
//! and
//! [`NalPacketConfigRejection::native_initialize_result`](crate::nal::NalPacketConfigRejection::native_initialize_result)
//! are also available; their reference revision is the one named on
//! [`compat::nal`](crate::compat::nal).

pub mod dts;

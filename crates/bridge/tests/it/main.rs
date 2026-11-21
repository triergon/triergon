//! Integration tests of the packet and subtitle bridges.
#![forbid(unsafe_code)]
mod access;
mod dvb_ts;
mod framing;
mod matroska_bitmap;
mod matroska_discovery;
mod matroska_feed;
mod mp4_avc_av1;
mod mp4_hevc_aac;
mod subtitle_bridges;
mod subtitle_receivers;
#[path = "../../../playback/tests/it/support/mod.rs"]
#[allow(
    dead_code,
    reason = "the bridge tests use the playback fixtures of their inputs only"
)]
mod support;

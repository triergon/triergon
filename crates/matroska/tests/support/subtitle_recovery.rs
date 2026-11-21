//! The recorded subtitle-recovery file and the packet a playback seek into it
//! replays first, shared by the unit and integration tests. Library names resolve
//! at `crate::`: the library itself, or an integration test that imports them.
// Each test binary uses part of this file.
#![allow(dead_code)]
use crate::TypedPacket;

/// A recorded file whose subtitle track has no duration or end for its packets.
pub(crate) const INPUT: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/seek-subtitle-recovery/observed-11.mkv"
));

/// The first packet after the 8 s playback seek: the subtitle that starts before
/// the cut and has no known end.
pub(crate) fn assert_prefix(packet: &TypedPacket) {
    assert_eq!(packet.packet.track, 2);
    assert_eq!(packet.packet.pts_ns, Some(7_200_000_000));
    assert_eq!(packet.packet.dts_ns, None);
    assert_eq!(packet.packet.duration_ns, None);
    assert_eq!(packet.packet.data.as_ref(), b"Compiler sample 9");
    assert!(packet.packet.timing.source().unwrap().duration.is_none());
}

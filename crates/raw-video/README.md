# triergon-raw-video

Raw AVC and HEVC Annex B input over the source protocol of
[`triergon-playback`](../playback/README.md): a probe that tells raw video from the
containers playback opens, a source that replays the probed prefix and keeps reading,
the active parameter-set configuration, and a feed of timed access units. It does not
decode media.

| Area | Entry points |
|---|---|
| Probe | `PlaybackProbe`: Matroska, MP4, TS188, M2TS192, AVC and HEVC Annex B; `RawVideoProbe` for raw input alone |
| Source | `RawVideoSource`, `PlaybackInput` (a container `SelectedInput` or a `RawVideoSource`) |
| Configuration | `RawVideoConfigTracker`, `RawVideoConfiguration`, `AvcVuiTiming`, `HevcVuiTiming` |
| Feed | `RawVideoFeed`, `RawVideoFeedPacket`, `RawFeedClock` |
| HEVC startup | `hevc_startup`: the parameter-set chain a feed starts from |

## Behaviour and limits

`PlaybackProbe` holds a `SelectedInput` or a `RawVideoSource` that replays the probed
prefix and then continues reading. It accepts the MP4 opening boxes `ftyp`, `styp`,
`moov`, `free`, `wide` and `mdat`. For transport streams it examines a 389-byte prefix
and requires exactly one of the TS188 and M2TS192 three-sync patterns to match.
The raw Annex B probe selects AVC or HEVC at the first slice after its
parameter sets. It reads each NAL's first five bytes and skips the rest of the
NAL body in word-sized steps, charging one work unit per byte either way.
Raw Annex B carries no container clock: the feed's duration is the active SPS
picture period, derived again whenever the configuration changes. A feed packet's
access unit aliases several NAL owners, so it has no single payload slice and no
`PacketInfo` view; `RawAccessUnit::segments` reads it in order.

## Conformance

- AVC/HEVC Annex B and parameter sets: ITU-T H.264 §7.3 and Annex B, ITU-T H.265.

The integration tests (`tests/it`) read the fixtures of `crates/playback/tests/fixtures`.

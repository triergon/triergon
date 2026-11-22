# triergon-raw-video

Probes and reads raw AVC and HEVC Annex B over the
[`triergon-playback`](../playback/README.md) source protocol, delivering access
units with their active parameter sets and an optional submission clock.

| Area | Entry points |
|---|---|
| Probe | `PlaybackProbe`: Matroska, MP4, TS188, M2TS192, AVC and HEVC Annex B; `RawVideoProbe` for raw input alone |
| Source | `RawVideoSource`, `PlaybackInput` (a container `SelectedInput` or a `RawVideoSource`) |
| Configuration | `RawVideoConfigTracker`, `RawVideoConfiguration`, `AvcVuiTiming`, `HevcVuiTiming` |
| Feed | `RawVideoFeed`, `RawVideoFeedPacket`, `RawFeedClock` |
| HEVC startup | `hevc_startup`: the parameter-set chain a feed starts from |

## Behaviour and limits

`PlaybackProbe` shares the [container signatures](../playback/README.md) and
replays the read prefix. The raw Annex B probe selects AVC or HEVC at the
first slice after its parameter sets, charging one work unit per scanned byte.
The packet carries its active SPS timing. An optional submission clock uses
the caller's cadence. Borrow the NAL slices in order with
`RawAccessUnit::segments`.

## Conformance

- AVC/HEVC Annex B and parameter sets: ITU-T H.264 §7.3 and Annex B, ITU-T H.265.

The [probe comparison](../../harness/features/playback-probe/contract.json)
checks the combined probe's container and raw-video choices. The
[raw source views](../../harness/features/playback-source-view/contract.json)
compare the initial SPS-derived frame rate and the default unstamped queue timestamps.

The integration tests (`tests/it`) read the fixtures of `crates/playback/tests/fixtures`.

# triergon-raw-video

Probes and reads raw AVC and HEVC Annex B over the source protocol of
`triergon_core::source`, delivering access units with their active parameter sets
and an optional submission clock.

| Area | Entry points |
|---|---|
| Probe | `PlaybackProbe`: Matroska, MP4, TS188, M2TS192, AVC and HEVC Annex B; `RawVideoProbe` for raw input alone |
| Source | `RawVideoSource`, `PlaybackInput` (a container `SelectedInput` or a `RawVideoSource`) |
| Configuration | `RawVideoConfigTracker`, `RawVideoConfiguration`, `AvcVuiTiming`, `HevcVuiTiming` |
| Feed | `RawVideoFeed`, `RawVideoFeedPacket`, `RawFeedClock` |
| HEVC startup | `hevc_startup`: the parameter-set chain a feed starts from |

## Usage

Use `RawVideoProbe` when the codec is unknown. With a known codec, construct
`RawVideoSource` directly from a `RawAnnexBReader`. A source that returns
`Pending` can be resumed on the same reader when input becomes available.

```rust
use std::sync::atomic::AtomicBool;
use triergon_core::source::ByteSource;
use triergon_core::{Budget, Bytes, Result, SourceCapabilities, SourceId, Step, WorkBudget};
use triergon_raw_video::{RawVideoProbe, RawVideoProbeStep};

fn read_annex_b(
    pool: &Budget, bytes: Bytes, mut submit: impl FnMut(&[u8]),
) -> Result<()> {
    let id = SourceId(1);
    let caps = SourceCapabilities { length: Some(bytes.len() as u64), random_access: false };
    let source = ByteSource::new(id, bytes, 4096)?;
    let mut probe = RawVideoProbe::new(pool, id, caps, source, Default::default(), Default::default())?;
    let cancel = AtomicBool::new(false);
    loop {
        if let RawVideoProbeStep::Detected(_) =
            probe.advance(&mut WorkBudget::new(16_384), &cancel)?
        {
            break;
        }
    }
    let mut reader = probe.finish()?;
    loop {
        match reader.poll(&mut WorkBudget::new(16_384), &cancel)? {
            Step::Ready(unit) => unit.segments().for_each(&mut submit),
            Step::End(_) => return Ok(()),
            Step::NeedInput(_) | Step::Yield => continue,
        }
    }
}
```

## Behaviour and limits

`PlaybackProbe` shares the [container signatures](../playback/README.md) and
replays the read prefix. The raw Annex B probe selects AVC or HEVC at the
first slice after its parameter sets, charging one work unit per scanned byte.
The NAL count bound fails with `Error::Policy`.
The packet carries its active SPS timing. An optional submission clock uses
the caller's cadence. Borrow the NAL slices in order with
`RawAccessUnit::segments`.

## Conformance

- AVC/HEVC Annex B and parameter sets: ITU-T H.264 §7.3 and Annex B, ITU-T H.265.

The [probe](../../harness/features/playback-probe/contract.json),
[source](../../harness/features/playback-source-view/contract.json),
[configuration](../../harness/features/raw-video-configuration/contract.json) and
[Annex B](../../harness/features/raw-annex-b/contract.json) contracts cover the
format selection, access units, parameter sets and clocks.

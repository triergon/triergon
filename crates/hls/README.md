# triergon-hls

Access units from complete HLS TS188 media segments and forward-only TS188
input, read through the [`triergon-playback`](../playback/README.md) PES feed.

| Area | Entry points |
|---|---|
| HLS media segments | `segment::HlsSegmentReader` (AVC + AAC), `HlsMediaSegmentReader` (AVC/HEVC + AAC/AC-3/E-AC-3) |
| Progressive TS188 | `progressive::TsProgressiveReader` (AVC + ADTS AAC) |
| Access-unit readers | `avc::AvcTsReader`, `aac::HlsAac` |
| Segment clocks | `clock::resolve_hls_aac_clocks`, `HlsAacClock` |
| Packet views | `TsProgressiveReader::packet` and `HlsSegment::packet` borrow AVC and AAC units as `PacketInfo`. AVC uses 90 kHz ticks and AAC uses a clock that represents PES times and sample durations exactly |

## Usage

Each reader takes a source and its input cursor. These examples use finite,
in-memory bytes. With a source that may return `Pending`, wait for the source
before advancing again after `NeedInput`.

```rust
use std::sync::atomic::AtomicBool;
use triergon_core::source::ByteSource;
use triergon_core::{Backpressure, Budget, Bytes, Error, InputCursor, Result,
    SourceCapabilities, SourceId, WorkBudget};
use triergon_hls::progressive::{TsProgressiveReader, TsProgressiveStep, TsProgressiveUnit};
use triergon_hls::segment::{
    HlsMediaProfile, HlsMediaSegment, HlsMediaSegmentReader, HlsSegment,
    HlsSegmentProgress, HlsSegmentReader,
};

fn input(pool: &Budget, bytes: Bytes) -> Result<(InputCursor, ByteSource)> {
    let id = SourceId(1);
    let window = 4096;
    let caps = SourceCapabilities { length: Some(bytes.len() as u64), random_access: false };
    Ok((InputCursor::new(pool, id, caps, window)?, ByteSource::new(id, bytes, window)?))
}

fn segment(pool: &Budget, bytes: Bytes) -> Result<Option<HlsSegment>> {
    let (cursor, source) = input(pool, bytes)?;
    let mut reader = HlsSegmentReader::new(cursor, source, Default::default(), Default::default())?;
    let cancel = AtomicBool::new(false);
    loop {
        match reader.advance(&mut WorkBudget::new(16_384), &cancel)? {
            HlsSegmentProgress::Ready => return reader.take(&cancel),
            HlsSegmentProgress::Yield | HlsSegmentProgress::NeedInput(_) => continue,
            HlsSegmentProgress::Backpressure { requested, available } =>
                return Err(Error::Backpressure(Backpressure { requested, available })),
        }
    }
}

fn media_segment(
    pool: &Budget, bytes: Bytes, profile: HlsMediaProfile,
) -> Result<Option<HlsMediaSegment>> {
    let (cursor, source) = input(pool, bytes)?;
    let mut reader = HlsMediaSegmentReader::new(cursor, source, profile, Default::default())?;
    let cancel = AtomicBool::new(false);
    loop {
        match reader.advance(&mut WorkBudget::new(16_384), &cancel)? {
            HlsSegmentProgress::Ready => return reader.take(&cancel),
            HlsSegmentProgress::Yield | HlsSegmentProgress::NeedInput(_) => continue,
            HlsSegmentProgress::Backpressure { requested, available } =>
                return Err(Error::Backpressure(Backpressure { requested, available })),
        }
    }
}

fn progressive(
    pool: &Budget, bytes: Bytes, mut consume: impl FnMut(&TsProgressiveUnit),
) -> Result<()> {
    let (cursor, source) = input(pool, bytes)?;
    let mut reader = TsProgressiveReader::new(cursor, source, Default::default())?;
    let cancel = AtomicBool::new(false);
    loop {
        match reader.advance(&mut WorkBudget::new(16_384), &cancel)? {
            TsProgressiveStep::Unit(unit) => {
                consume(&unit);
                reader.reuse_unit(unit);
            }
            TsProgressiveStep::End => return Ok(()),
            TsProgressiveStep::Yield | TsProgressiveStep::NeedInput(_) => continue,
            TsProgressiveStep::Backpressure { requested, available } =>
                return Err(Error::Backpressure(Backpressure { requested, available })),
        }
    }
}
```

## Behaviour and limits

**HLS segments.** One independent TS188 segment with in-band PAT and PMT.
The feed preserves PES event positions. `HlsMediaVideoUnit::feed_spans`
borrows the complete AVC or HEVC Annex B feed, including inserted parameter sets,
for the lifetime of the unit. The reader releases units after EOF and after both
assemblers drain. Defaults are 64 MiB source, 64 MiB feed and 8,192 access units.
A failed reader releases its source. Retry with a new reader.

**Progressive TS188.** One sequential input with a stable PAT, PMT and clock,
one AVC track and one ADTS AAC track. AVC starts on a timed IDR with an
access-unit delimiter. AAC needs an initial PES timestamp, then advances
by the exact ADTS duration when an anchor is absent. Source timestamps stay
available alongside the playback clocks. Completed units can be delivered
before the end of input. The AVC reader transfers its assembly buffer to each
completed unit. `TsProgressiveReader::reuse_unit` takes it back when the caller
has finished with it. The reader reuses just-published, unshared video storage
with capacity up to twice the last unit's length. Reuse preserves output and
work counts. A request stays pending if the source runs out of work before it
can reply. Start a new reader to begin at another offset.
Input requests prefer a TS188 boundary within the cursor's window limit. Short
replies continue through the ordinary packet carry path.
On 64-bit targets, several carriers in a progressive PES packet can share an
allocation. An AAC frame that borrows those bytes keeps the full allocation
charged while you hold it. The standalone AVC and AAC readers accept both
`CompactDemuxPacket` through `push_compact` and ordinary PES packets through
`push`, with the same track, configuration and clock checks.

**ADTS AAC.** Complete frames contained in one source span can share its charged
backing storage when its capacity fits the configured maximum frame size. After
header validation, a fragmented frame can use one copy of the remaining PES tail
when that tail fits the frame size limit (at most 8,191 bytes) and the work budget
covers validation, copying, allocation and span retirement. Every remaining span
must use the same pool and their lengths must exactly match the declared extent.
Frames then share the joined storage and keep its full capacity charged while
retained. Insufficient work, invalid tail metadata or allocation pressure keeps
ordered span assembly, so a later invalid span still follows earlier valid frames.

The [progressive TS188 contract](../../harness/features/progressive-ts188/contract.json)
checks complete packet fields and payloads with two retained units per stream.
It also records the reference refusal of the legal duplicate control.

The [packet-view comparison](../../harness/features/hls-unit-packets/contract.json)
reads AVC and AAC through `PacketInfo`. The [AAC clock comparison](../../harness/features/hls-aac-clocks/contract.json)
checks exact timestamps, their anchor origins and the 90 kHz projection.
The [compact input comparison](../../harness/features/hls-compact-push/contract.json)
drives the standalone readers with compact PES payloads. The transport
reader admits one copy of an exact adjacent duplicate carrier. The
standalone AVC reader requires an access-unit delimiter before the first
video access unit. The comparison contracts declare the reference's
different results for these inputs. The
[segment push comparison](../../harness/features/hls-push-input/contract.json)
checks the complete feed and final timestamps with 1-, 188- and 4096-byte
replies through both segment readers.

## Conformance

- MPEG-TS: ITU-T H.222.0 (§2.14.1 AVC delimiters).
- AVC/HEVC parameter sets: ITU-T H.264 §7.3 and Annex B, ITU-T H.265.

The integration tests (`tests/it`) read the fixtures of
`crates/playback/tests/fixtures/hls`. Gates and measurement policy:
[docs/quality.md](../../docs/quality.md).

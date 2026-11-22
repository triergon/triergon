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
On 64-bit targets, progressive PES payloads retain one shared backing handle per
admitted carrier run. The AVC and AAC cursors borrow each logical carrier range, preserving its
validation, work and retirement boundaries. Independently retained AAC frames
still keep the full backing allocation charged. Both framers also expose
`push_compact` for explicit `CompactDemuxPacket` delivery alongside ordinary `push`.
Both paths share track, configuration-epoch and source-clock admission checks.

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
drives the standalone readers with compact PES payloads. The
[segment push comparison](../../harness/features/hls-push-input/contract.json)
checks the complete feed and final timestamps with 1-, 188- and 4096-byte
replies through both segment readers.

## Conformance

- MPEG-TS: ITU-T H.222.0 (§2.14.1 AVC delimiters).
- AVC/HEVC parameter sets: ITU-T H.264 §7.3 and Annex B, ITU-T H.265.

The integration tests (`tests/it`) read the fixtures of
`crates/playback/tests/fixtures/hls`. Gates and measurement policy:
[docs/quality.md](../../docs/quality.md).

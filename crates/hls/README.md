# triergon-hls

Access units of transport stream media read through the
[`triergon-playback`](../playback/README.md) PES feed: complete HLS TS188 media
segments, and forward-only progressive TS188 input. It does not fetch over HTTP,
parse playlists, retry transports, decode media or render.

| Area | Entry points |
|---|---|
| HLS media segments | `segment::HlsSegmentReader` (AVC + AAC), `HlsMediaSegmentReader` (AVC/HEVC + AAC/AC-3/E-AC-3) |
| Progressive TS188 | `progressive::TsProgressiveReader` (AVC + ADTS AAC) |
| Access-unit readers | `avc::AvcTsReader`, `aac::HlsAac` |
| Segment clocks | `clock::resolve_hls_aac_clocks`, `HlsAacClock` |
| Packet views | `packet::UnitPacket` through `PacketInfo`, from `TsProgressiveReader::packet` and `HlsSegment::packet`: AVC units on the 90 kHz clock, AAC frames on the finest clock their PES times and sample duration count exactly in. An HEVC picture of a media segment is several slices and has no packet view; `contiguous_feed` copies it |

## Behaviour and limits

**HLS segments.** One independent TS188 segment with in-band PAT and PMT.
The feed consumes validated PES continuations directly and preserves source
event positions. AVC fixed-width fields consume buffered RBSP bits in byte-sized
groups while retaining emulation-prevention validation. The segment reader uses one caller-owned player output slot.
`HlsMediaVideoUnit::feed_spans` borrows the complete AVC or HEVC Annex B
feed, including any inserted parameter sets. Its spans remain valid while the
video unit is retained.
AVC delimiter lookbehind uses compact indices into four retained PES clocks.
Repeated AVC parameter sets reuse their parsed values when their bytes exactly
match the last stored set. Different bytes follow normal parsing and ID lookup.
Units are released only after EOF and both the video and audio assemblers drain. Defaults: 64 MiB
source, 64 MiB feed, 8,192 access units. A failed reader releases its source;
retry with a new reader.

**Progressive TS188.** One sequential input with a stable PAT, PMT and clock,
one AVC track and one ADTS AAC track. AVC starts on a timed IDR with an
access-unit delimiter. AAC needs an initial PES timestamp, then advances
by the exact ADTS duration when an anchor is absent. Source timestamps stay
available alongside the playback clocks. Completed units can be delivered
before the end of input. The AVC reader transfers its assembly buffer to each
completed unit in progressive and HLS playback. It copies only the delimiter
lookahead into the next buffer. A player that is done with a unit may hand it
back with `TsProgressiveReader::reuse_unit` instead of dropping it. Hand back
each unit when it would otherwise be dropped, and keep advancing as usual.
When no other handle shares a video unit's storage and the reader has just
published, that storage becomes the buffer for the next unit, provided its
capacity is at most twice the last published unit: the slack amortised growth
already allows, so reuse never holds much more memory than the units need.
Handing units back is optional: dropping them gives the same units, clocks and
work counts. The progressive and HLS readers hand each consumed PES's span and
header lists back to the transport observer themselves, which keeps them under
the same rule.
A unit that is still shared, or that arrives once assembly has moved on, is
simply dropped. Each advance can process several internal steps
within the caller's work budget. A source that spends its remaining work before
a reply is ready leaves the request pending for the next advance. Start a new
reader to begin at another offset.
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

`cargo xtask progressive` checks three comparable AVC/AAC TS188 packet folds and
all five authored functional controls through the bounded sequential reader.
All ten literal worker outcomes are required, including positive Rust output and
the recorded reference refusal on the legal duplicate control. The
[contract](../../harness/features/progressive-ts188/contract.json) preserves complete
packet fields and payloads with two retained units per stream. `--benchmark --memory`
uses the standard paired harness for the three original comparable cases only;
native gate-build qualification remains pending.

## Conformance

- MPEG-TS: ITU-T H.222.0 (§2.14.1 AVC delimiters).
- AVC/HEVC parameter sets: ITU-T H.264 §7.3 and Annex B, ITU-T H.265.

The integration tests (`tests/it`) read the fixtures of
`crates/playback/tests/fixtures/hls`. Gates and measurement policy:
[docs/quality.md](../../docs/quality.md).

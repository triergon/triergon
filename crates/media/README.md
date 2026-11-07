# Matroska / WebM reader (triergon-matroska)

Package `triergon-matroska`. A bounded Matroska and WebM demuxer: EBML parsing, tracks,
packets with lacing and content encoding, Cues-based seeking, static colour/HDR,
block additions and segment metadata (Info, chapters, tags, attachments). It is a
format-layer crate on top of [triergon-core](../core/README.md) and has no other
dependency; its push reader implements the core `ContainerReader`. It does not
decode codecs, schedule playback or render metadata; it returns source facts, and
the `compat` modules reproduce FFmpeg's choices where a consumer needs them.

## Usage

Blocking `std::io` adapter:

```rust
use std::{fs::File, sync::atomic::AtomicBool};
use triergon_matroska::{Limits, typed::BlockingReader};

# let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/seek/cues-front.mkv");
let cancel = AtomicBool::new(false);
let mut reader = BlockingReader::open_seekable(File::open(path)?, Limits::default())?;
for track in reader.tracks() { /* number, kind, codec, codec_private, colour, languages … */ }
let video = reader.tracks().iter().position(|t| t.kind == 1).ok_or("no video track")?;
let receipt = reader.seek_for_playback(video, 1_000_000_000, &cancel)?; // SeekReceipt
while let Some(p) = reader.next_typed_packet(&cancel)? {
    // p.packet: track, pts_ns, duration_ns, keyframe, data, timing
    // p.configuration: active track configuration; p.group: BlockGroup side data
}
# Ok::<(), Box<dyn std::error::Error>>(())
```

`open`/`open_in` accept any `Read` (sequential, no seeking); `open_seekable*`
need `Read + Seek`; `open_owned_seekable_in` reads an `OwnedInputBytes` and lets
packets alias the input allocation. The `*_in` variants share a caller `Budget`.
`next_packet` returns the plain `matroska::Packet` and refuses packets that carry
block additions or CodecState; use `next_typed_packet` for those.
Typed configurations expose their allocation accounting through `Configuration::budget()`.

`examples/matroska_packets.rs FILE` prints the tracks and packets of a file.

Push / poll reader (no I/O; the caller supplies bytes):

```rust
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, InputWindow, SourceCapabilities, SourceId, Step, WorkBudget};
use triergon_matroska::{Limits, typed::{Event, Reader}};

# let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/seek/cues-front.mkv");
let file = std::fs::read(path)?;                     // any transport; the reader does no I/O
let pool = Budget::new(Limits::default().live_storage_bytes)?;
let caps = SourceCapabilities { length: Some(file.len() as u64), random_access: true };
let mut reader = Reader::new(&pool, SourceId(1), caps, Limits::default(), 64 * 1024)?;
let cancel = AtomicBool::new(false);
let mut work = WorkBudget::new(1 << 16);
loop {
    match reader.step(&mut work, &cancel)? {         // Step<Event, Backpressure>
        Step::NeedInput(req) => match file.get(usize::try_from(req.offset)?..) {
            Some(rest) if !rest.is_empty() => {
                let bytes = Bytes::copy_from_slice(&pool, &rest[..rest.len().min(req.maximum)])?;
                reader.provide(&InputWindow::new(req.source, req.token, req.offset, bytes)?)?;
            }
            _ => reader.finish(req.source, req.token)?,
        },
        Step::Yield => work = WorkBudget::new(1 << 16),
        Step::Backpressure(_) => { /* release retained packets, retry */ }
        Step::Ready(Event::Packet(p)) => { /* p.packet: track, pts_ns, keyframe, data */ }
        Step::Ready(_) => {} // Envelope, Configuration, Document, Seek
        Step::End(_) => break, // end of the Segment
    }
}
# Ok::<(), Box<dyn std::error::Error>>(())
```

`Reader::seek_previous` / `seek_for_playback` return the first `InputRequest`;
the seek completes with `Event::Seek(SeekReceipt)` before any packet.

`typed::Reader` also implements `triergon_core::container::ContainerReader`, so it
works in generic code and inside `triergon::AnyReader`:

| Neutral view | Matroska source |
|---|---|
| `PacketInfo` | `typed::Packet`: time base nanoseconds (TimestampScale applied), `pts`/`duration` from the packet, `dts` `None`, `RandomAccess` from the Block keyframe flag, `Trim::end` from DiscardPadding |
| `TrackInfo` | `matroska::Track`: `TrackId` = TrackNumber, `codec_tag` = CodecID, `codec` from an exact CodecID match with any other CodecID as `CodecId::Other`, `configuration` = CodecPrivate, effective language, default and forced flags, `colour` as a `ColourDescription` with `Origin::SpecificationDefault` for omitted Colour children |
| `Event::Seek` | `matroska::SeekReceipt::neutral()`: method `Index` or `BoundedScan`; Cue and preroll detail stay on the Matroska receipt |
| `Event::Other` | the other `typed::Event`s (Envelope, Configuration, Document) |

`request_seek` maps the `TrackId` to a track index (`Error::Invalid` when absent),
floors the target to nanoseconds and calls `seek_previous` for
`SeekPolicy::PreviousRandomAccess` or `seek_for_playback` for
`SeekPolicy::PlaybackPreroll`.

## Supported

| Area | Coverage |
|---|---|
| EBML | RFC 8794 headers, IDs up to 4 bytes, sizes up to 8 bytes, unknown-size Segment and Cluster; DocType `matroska` or `webm` |
| Tracks | Number, UID, type, codec ID/private/name, legacy and BCP 47 language, enabled/default/forced and accessibility flags (absence kept distinct from false), DefaultDuration, CodecDelay, SeekPreRoll, video size, audio rate/channels, raw `TrackEntry` bytes |
| Blocks | SimpleBlock and BlockGroup; Xiph, fixed and EBML lacing (up to 256 frames); BlockDuration, ReferenceBlock, DiscardPadding |
| Side data | BlockAdditions (raw, with mapping evidence from `BlockAdditionMapping`), CodecState with configuration events before affected packets, HDR10+ (ITU-T T.35, Application 4) via `Hdr10Plus::parse_t35` |
| Content encoding | Header stripping and zlib, per frame (scope 1), CodecPrivate (scope 2) or both; ordered chains of up to eight stages |
| Colour | All 13 `Colour` integers and 10 `MasteringMetadata` floats (`colour::Colour`) |
| Segment metadata | Info (title, apps, scale, IEEE duration, DateUTC), editions and nested chapters with displays, tags with nested SimpleTags and all target UIDs, attachments |
| Seeking | SeekHead chains, Cues (incl. CueRelativePosition, CueBlockNumber), bounded fallback scan, all-track preroll |

Refused with `Error::Unsupported`: encryption, bzip2/lzo compression, preset
zlib dictionaries, ContentEncoding scope 4, `TrackTimestampScale` other than 1.0,
CueCodecState, multiple EBML documents or Segments, EBMLReadVersion above 1,
DocTypeReadVersion above 4, EBMLMaxIDLength above 4, EBMLMaxSizeLength above 8,
and seeking on a non-video track.

## Behaviour and limits

**Bounds.** Every reader takes `Limits`; nothing buffers a whole file. Defaults:

| Field | Default | Bounds |
|---|---:|---|
| `live_storage_bytes` | 128 MiB | all live reader-owned storage, including packets the caller still holds |
| `metadata_bytes` | 1 MiB | one structural metadata element; also each decoded CodecPrivate |
| `block_bytes` | 16 MiB | one block, and all decoded frames of one lace together |
| `tracks` | 64 | tracks per Segment |
| `metadata_storage_bytes` | 16 MiB | all segment-metadata documents incl. validation workspace |
| `metadata_nodes` / `metadata_depth` | 4096 / 32 | document nodes, nesting (hard ceiling 64) |
| `attachment_bytes` | 8 MiB | attachment payload |
| `elements` | 1,000,000 | elements visited |
| `scan_bytes` | 64 MiB | bytes consumed in one open/seek scan phase |
| `index_entries` | 16,384 | Cues entries |
| `decompression_ratio` / `decompression_work` | 1024 / 128 Mi | expansion vs. input; work units per encoding |

Exceeding a policy is `Error::Limit`; output that fits once the caller drops
retained packets is `Step::Backpressure` from `Reader::step` (`Error::Backpressure`
from `BlockingReader`) and is retryable. `PayloadOwnership::OwnedExact` (default) gives each block payload
storage of exactly its size; `SharedBounded` lets payloads alias their parent
input allocation and reports that parent's full capacity as retained.

**Opening.** Sequential input requires metadata before the first Cluster. Seekable
input follows bounded SeekHead chains to load late Tracks, Cues, chapters, tags
and attachments; it never scans media to build an index.

**Timing.** `packet.timing.source()` gives exact rational nanoseconds with
CodecDelay subtracted (RFC 9559 §11.2); later laced frames get a PTS only when a
duration is known, and fractional lace durations and negative PTS are kept.
`pts_ns`/`duration_ns` are that value rounded once to the nearest nanosecond.
`dts_ns` is not derived. `compat::clock::packet_timing` is an opt-in view in
Segment ticks that reproduces FFmpeg's packet clock, including its omissions.

**Content encoding.** Encodings apply in descending `ContentEncodingOrder`; each
laced frame is decoded separately. `zlib::decompress` reads one complete RFC 1950
stream (stored, fixed and dynamic blocks), checks Adler-32 before returning
anything and rejects trailing bytes. Cancellation is checked between blocks,
symbols and checksum chunks.

**Seeking.** Requests name a track index (must be video) and a presentation time in
nanoseconds. `seek_previous` lands on the latest video keyframe at or before the
target. `seek_for_playback` first subtracts the largest track SeekPreRoll, then
replays the whole landing Cluster so interleaved audio before the keyframe is kept;
when subtitle tracks exist it also recovers earlier subtitle packets that are
still in range. A cue is checked against the real Cluster, track, timestamp and
keyframe; a forged index fails instead of landing elsewhere. The gap after the
last cue is scanned for uncued keyframes. The `SeekReceipt` reports `actual_ns`,
`used_index`, `scanned_bytes`, `preroll_ns`, `discard_until_ns` (decode, but do
not present, earlier packets) and `replays_from_start` (taken from source position,
so one-time start trimming is not repeated). After any failure or cancellation the
caller must seek or `reset` before reading more packets.

**Colour.** `Track::colour` keeps every source field: bit depth, both chroma and Cb
subsampling axes, siting, optional MaxCLL/MaxFALL and each optional mastering
value as exact `f64`. Zero-length numbers take the schema default. Duplicates,
bad widths, non-finite values, negative luminance and chromaticity outside [0, 1]
fail. With the `reference-trace` feature, `Colour::trace_fields()` gives the
values FFmpeg reports for comparison: signed 32-bit rationals from continued
fractions, with FFmpeg's omissions (no bit depth, incomplete light levels,
incomplete/zero primaries); values that cannot fit return `Overflow`.

**Reference compatibility.** The reader follows the specifications. The `compat`
modules reproduce FFmpeg's Matroska demuxer where a consumer needs its result, and
each names the FFmpeg revision it reproduces. `compat::clock` gives FFmpeg's packet
clock. `compat::block_additions` builds readers that admit WebM VP9's implicit
ITU-T T.35 addition (BlockAddID 4 without a declared MaxBlockAdditionID), and
projects block additions to the packet side data and HDR10+ events FFmpeg attaches
to packets. Comparison-only projections (`Colour::trace_fields`) are compiled only
with the non-default `reference-trace` feature.

**Segment metadata.** Each `metadata::SharedDocument` owns one source buffer
plus a compact node arena; `Edition`, `Chapter`, `Tag`, `SimpleTag`,
`SegmentInfo` and `Attachment` are borrowed views with no string or payload
copies. Attachments stay valid after the reader is dropped. Chapter times are
exact unsigned nanoseconds; ordered editions are reported, not executed.

## Conformance

Authored from RFC 8794 §§4–7, 11 (EBML) and RFC 9559 §§5, 10–11, 22 (Matroska:
§5.1.3.5 block side data, §5.1.4.1.16–17 block addition mappings,
§5.1.4.1.28 Colour, §5.1.4.1.31 ContentEncoding, §5.1.5 Cues, §§5.1.6–8
chapters/tags/attachments), RFC 1950/1951 (zlib/DEFLATE) and ITU-T T.35
HDR10+ Application 4 carriage.

Verified by unit and integration tests (malformed input, limits, cancellation,
ownership, seek edge cases; `tests/container.rs` reads packets and tracks through
the neutral views only); independent Python oracles for EBML structure, exact
source time, zlib content, colour and metadata; and a native FFmpeg comparison
worker over the authored corpora in `harness/` plus the upstream FATE vectors
`matroska-xiph-lacing`, `matroska-zlib-decompression` and
`matroska-zero-length-block`. Packet bytes, timestamps, configuration, colour,
metadata and seek landings must match exactly, with no epsilon. Where FFmpeg's
representation differs (packet clock, colour rationals, flattened chapters,
collapsed tag targets) the source facts and the FFmpeg view are each checked.
Gates and harness: [docs/quality.md](../../docs/quality.md).

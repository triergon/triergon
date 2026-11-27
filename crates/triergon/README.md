# triergon

Triergon exposes container readers, packet playback, software decode, text and
bitmap subtitles, and ASS rendering through one crate. Enable the groups you use.
The default feature enables the Matroska and WebM reader, and the neutral container
model is always available.

## Usage

Open a file and read its packets, with the default `matroska` feature or any other
reader feature:

```rust,no_run
# fn main() -> Result<(), Box<dyn std::error::Error>> {
use triergon::{Budget, PacketInfo, open_blocking};

let pool = Budget::new(64 << 20)?;
let mut source = open_blocking(std::fs::File::open("movie.mkv")?, &pool)?;
while let Some(packet) = source.next_packet()? {
    let (track, pts, bytes) = (packet.track(), packet.pts(), packet.payload().len());
    # let _ = (track, pts, bytes);
}
# Ok(())
# }
```

Track metadata becomes available as the reader consumes input. Before reading its
opening events, `source.reader().track_count()` can be zero. ISO BMFF publishes
its tracks with `ReaderEvent::Movie`. `next_event` lets you inspect the opening
events before pulling packets.

Use `triergon::access::AccessUnits` with `framing` or `codecs` when you have
track metadata and packets from a container reader.

To decode and render as well, with `playback,codecs,render`, open the source with
`playback::probe::Probe` and
read outputs from a `playback::Player`. Look up the packet's track with
`Engine::track`, load the audio and video sessions with the matching decoders,
and step `AccessUnits::for_track` once per selected track to obtain its framer.
For indexed MP4, finish `PreparedMp4::identify_tracks` before selecting tracks
or creating framers so their codec identities and sample descriptions are available.
Use `run_build` to drive a framing job to its next output with a fresh work
budget each time it yields.

```rust,no_run
# #[cfg(all(feature = "playback", feature = "codecs", feature = "render"))]
# fn play_one(
#     track: &dyn triergon::TrackInfo, packet: &impl triergon::PacketInfo,
#     units: &mut triergon::access::AccessUnits,
#     cancel: &std::sync::atomic::AtomicBool, video: &mut triergon::session::VideoSession,
#     audio: &mut triergon::session::AudioSession, pcm: &mut triergon::session::PcmBlock,
#     renderer: &mut triergon::render::ass::Renderer, script: &mut triergon::render::ass::Script,
#     time_ms: i64,
# ) -> Result<(), Box<dyn std::error::Error>> {
use triergon::{Error, MediaKind, run_build};

let mut job = units.packet(packet)?;
let unit = run_build(|work| job.step(work, cancel))?
    .ok_or(Error::Invalid("missing access unit"))?;
match track.kind() {
    MediaKind::Video => { video.feed_packet(&unit)?; while video.step(None) {} }
    MediaKind::Audio => { audio.feed_packet(&unit)?; audio.pull(None, 0, pcm); }
    _ => {}
}
let images = renderer.render(script, time_ms)?.images;
# let _ = images;
# Ok(())
# }
```

The [play example](examples/play/main.rs) uses `open_blocking` to read a Matroska file.
It shows track selection,
decoder configuration, draining both sessions at EOF, and rendering an ASS
script at a requested time. From the workspace root:

```sh
cargo run -p triergon --features playback,codecs,render --example play -- \
  crates/playback/tests/fixtures/mkv-feed/three-tracks.mkv \
  harness/fixtures/ass-render/decorations-orientation.ass 500
```

For caller-driven input, use `AnyReader::open` with the source prefix and
capabilities, or `AnyReader::of` with a format already selected by `probe`.
Supply the ranges requested by `ContainerReader::step`. After
`reader.request_seek(SeekRequest { track, target, policy })`, the reader delivers
`Event::Seek(SeekReceipt)` before the next packet.

`open_blocking(source, &pool)` does the probing and input handling for a
`std::io::Read + Seek` source and returns a `Blocking` driver with `next_event`,
`next_packet` and `seek`. `reader()` gives access to tracks and format-specific
metadata. `into_inner()` returns the source at its current read position.
`open_sequential` supports pipes without random access or a known length. Its
`Replayed` source serves the unread prefix before the rest of the pipe. Calling
`Replayed::into_inner()` yields the pipe at its current position and discards the
unread prefix. Both adapters stop
reading as soon as a fixed container signature selects the format, by the rules under
[Behaviour and limits](#behaviour-and-limits). A native FLAC marker also suffices once
higher-priority container signatures are ruled out. Frame-based and ID3-prefixed
inputs use the elementary probe once at EOF or 64 KiB, whichever comes first.
After the first 4 KiB of an elementary stream, the seekable adapter uses the reported
file length to size its next read. It follows the bytes available through EOF or the
probe limit if the file length changes, then rewinds the source. Before reading a pipe, the
sequential adapter reserves room for the largest replay the pool can admit, up to 64 KiB
with elementary readers or 389 bytes without them. A caller holding that room
can release it and retry the same pipe. Fixed signatures allocate only the bytes
consumed. Longer elementary probes use one buffer. The sequential adapter
releases its retained capacity after replaying the last byte.

The [facade probe comparison](../../harness/features/facade-probe/contract.json)
checks prefix verdicts and the format selected by both adapters. For admitted inputs the
comparison checks every supplied byte through EOF after a rewind or sequential replay,
and for ISO BMFF on a sequential source it reads the movie: the tracks, every packet's
track, size, times and bytes, and how the read ended, against FFmpeg's MOV demuxer on
the same pipe.

## Features and lower-level crates

| Feature | Default | Module | Through `AnyReader` |
|---|---|---|---|
| `matroska` | yes | `triergon::matroska` ([triergon-matroska](../matroska/README.md)) | yes |
| `isobmff` | no | `triergon::isobmff` ([triergon-isobmff](../isobmff/README.md)) | yes; from a sequential source when `moov` comes first |
| `mpegts` | no | `triergon::mpegts` ([triergon-mpegts](../mpegts/README.md)) | yes, source access units in TS188/M2TS192 |
| `ogg` | no | `triergon::ogg` ([triergon-ogg](../ogg/README.md)) | yes |
| `elementary` | no | `triergon::elementary` ([triergon-elementary](../elementary/README.md)): native FLAC, MPEG audio, ADTS, AC-3 | yes |
| `framing` | no | `triergon::framing` ([triergon-framing](../framing/README.md)) | not a reader |
| `subtitles` | no | `triergon::subtitles` ([triergon-subtitles](../subtitles/README.md)) | not a reader |
| `playback` | no | `playback` ([engine and player](../playback/README.md)), `hls` ([segments and progressive feeds](../hls/README.md)), `raw_video` ([Annex B feeds and probing](../raw-video/README.md)), `bridge` ([decoder access units](../bridge/README.md)). Also enables `framing`. | independent |
| `session` | no | `session` ([decode queues](../session/README.md)), `codec` ([decoder traits](../codec/README.md)) and `codec::pixel` ([picture and colour types](../pixel/README.md)) | independent |
| `codecs` | no | Enables `session` and adds the built-in decoders under `codec` | independent |
| `render` | no | `render::ass` ([scripts and images](../ass/README.md)), `render::font` ([font faces](../font/README.md)), `render::shape` ([shaping](../shape/README.md)), `render::raster` ([rasterising](../raster/README.md)) | independent |

The decoder crates can also be selected individually.
[AAC](../codec-aac/README.md), [AC-3 and E-AC-3](../codec-ac3/README.md),
[DTS](../codec-dca/README.md), [FLAC](../codec-flac/README.md),
[H.264](../codec-h264/README.md), [HEVC](../codec-hevc/README.md),
[MPEG audio](../codec-mp3/README.md), [Opus](../codec-opus/README.md) and
[Vorbis](../codec-vorbis/README.md).

Using `default-features = false` without selecting another feature exposes the
shared types and `probe`; `AnyReader` and the openers come with the first reader feature.

Re-exported from triergon-core, independent of features:

| Area | Items |
|---|---|
| Container model | `ContainerReader`, `ReaderStep`, `Event`, `PacketInfo`, `TrackInfo`, `TrackId`, `MediaKind`, `CodecId`, `RandomAccess`, `Trim`, `SeekRequest`, `SeekPolicy`, `SeekReceipt`, `SeekMethod`, `ColourDescription`, `ColourRange`, `MasteringDisplay`, `ContentLight`, `Sourced`, `Origin` |
| Input and work | `Step`, `Backpressure`, `InputCursor`, `InputRequest`, `InputWindow`, `InputToken`, `SourceId`, `SourceCapabilities`, `SourceLocation`, `SourceFailure`, `WorkBudget` |
| Storage, time, errors | `Budget`, `BudgetVec`, `Bytes`, `Time`, `TimeBase`, `Rounding`, `Error`, `Result` |

The meaning of each type is documented in
[triergon-core](../core/README.md#container-model).

## Behaviour and limits

- **Probing.** `probe(prefix)` answers `Probed::Format(SourceFormat::…)`,
  `Probed::Need(n)` (wait for at least `n` bytes) or `Probed::Unrecognised`.
  `Matroska` is an EBML header (`1A 45 DF A3`, four bytes); `Isobmff` a first box whose
  type, bytes 4–8, is `ftyp`, `styp`, `moov`, `moof`, `mdat`, `free`, `skip`, `sidx` or
  `wide`; `Ogg` an `OggS` page with its version byte (five bytes); `Ts` sync bytes `0x47`
  at offsets 0, 188 and 376 (377 bytes) and `M2ts` at 4, 196 and 388 (389 bytes), behind
  the arrival prefixes. When both spacings match, the facade selects TS188. With the
  `elementary` feature, `triergon_elementary::probe` recognises native FLAC (`fLaC`, or a
  frame header that a second one continues) and MPEG audio, ADTS and AC-3 by two frames
  that agree, after any ID3v2 tag; a prefix shorter than 64 KiB that holds none of them
  yet is `Need(65536)`, the most the openers read. `probe_whole(source)` decides a source
  no more bytes follow: a container signature cut short is not one, and a transport
  stream that ends before its third sync byte is selected by the first two (one packet
  and all or part of a second). The openers and `AnyReader::open` with a prefix of the
  source's known length decide the same way at the end of the input.
- **Opening.** MPEG-TS opens through `AnyReader::MpegTs` with its `mpegts`
  feature. Its default program selection requires a single PAT media program;
  use `mpegts::ReaderLimits` for an explicit selection. An unrecognised source (`source
  format not recognised`) and a format without its feature (`reader feature not
  enabled`) are `Error::Unsupported`.
- **ISO BMFF from a sequential source.** With random access the ISO BMFF reader reads
  the movie index where it lies. Without it, a pipe through `open_sequential`
  included, the reader only moves forward: `moov` must precede the first `mdat`, each
  later `moof` is indexed as it arrives, and nothing is kept for the length of the
  source. An `mdat` before `moov` fails with `Error::Unsupported("progressive MP4
  needs front metadata")` when its header is read, and a sample that starts before
  the read position with `Error::Unsupported("sample before the read position needs
  random access")`. See [the reader's limits](../isobmff/README.md#container-reader).
- **Defaults.** `open` builds each reader with its default limits
  (`triergon_matroska::Limits`, `triergon_isobmff::ReaderLimits`,
  `triergon_mpegts::ReaderLimits`) and asks for at most
  64 KiB per `NeedInput`. For other limits or windows, construct
  `matroska::Reader` or `isobmff::Reader` directly and wrap it as
  `AnyReader::Matroska` or `AnyReader::Isobmff`.
- **Dispatch.** Forwarding is one `match` per call with no allocation and no
  dynamic dispatch on the packet path. Tracks are returned as `&dyn TrackInfo`.
  `AnyPacket` implements `PacketInfo`; match `AnyPacket::Matroska(p)`,
  `AnyPacket::Isobmff(p)`, `AnyPacket::MpegTs(p)`, `AnyPacket::Ogg(p)` or `AnyPacket::Elementary(p)` for the
  format's own packet type.
- **Other events.** `Event::Other` carries an `AnyOther`; match
  `AnyOther::Matroska(event)` for a Matroska `ReaderEvent` carrying configuration,
  a validated document or envelope structure,
  or `AnyOther::Isobmff(event)` for root boxes and the end of opening. ISO BMFF has
  no tracks until `ReaderEvent::Movie`.
  `AnyOther::MpegTs(event)` retains source transport/PES events, header-only
  video bytes and explicit unsupported/interrupted framing outcomes. A PES
  never becomes a neutral packet. See the MPEG-TS README for admitted carriages.
- **Seeking.** `Blocking::seek` returns the receipt before packet delivery resumes.
  Decode from `receipt.landing` and present from `receipt.discard_until`. Flush
  the sessions and rebuild track framers before feeding packets from the landing
  point. Use `Blocking::begin_seek` to read the format events and `Event::Seek`
  yourself with `next_event`. For Matroska the
  `TrackId` is the TrackNumber, the target is floored to nanoseconds, and only
  video tracks can be seek targets. For ISO BMFF the `TrackId` is the `track_ID`, the
  target is on that track's media clock without edits, and any track can be a target.
  MPEG-TS can seek within a length-known, seekable entity for
  [supported access units](../mpegts/README.md#access-units).

## Conformance

The facade adds no parsing: format semantics and their comparisons with FFmpeg live in
the format crates, and every reader follows the
[container contract](../../docs/architecture.md#container-contract). The facade's own
comparisons check what it adds: probing and the two adapters
([facade-probe](../../harness/features/facade-probe/contract.json)), source capabilities,
restarts and sticky failures through `AnyReader`
([facade-source-control](../../harness/features/facade-source-control/contract.json)),
MPEG-TS access units and transport events through `AnyOther::MpegTs`
([facade-mpegts](../../harness/features/facade-mpegts/contract.json),
[facade-mpegts-controls](../../harness/features/facade-mpegts-controls/contract.json)),
seeking ([facade-seek](../../harness/features/facade-seek/contract.json)), and the play
example's decoded pictures, PCM and rendered subtitle images
([play corpus](../../harness/codecs/play-manifest.json)). The gates are in
[docs/quality.md](../../docs/quality.md).

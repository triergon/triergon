# triergon

Triergon exposes container readers, packet playback, software decode, text and
bitmap subtitles, and ASS rendering through one crate. Enable the groups you use.
The default feature enables the Matroska and WebM reader, and the neutral container
model is always available.

## Usage

With `playback,codecs,render`, open the source with `playback::probe::Probe` and
read outputs from a `playback::Player`. Look up the packet's track with
`Engine::track`, load the audio and video sessions with the matching decoders,
and step `AccessUnits::for_track` once per selected track to obtain its framer.
Here `ready` supplies a fresh work budget when the framing job yields.

```rust,no_run
# #[cfg(all(feature = "playback", feature = "codecs", feature = "render"))]
# fn play_one(
#     track: &dyn triergon::TrackInfo, packet: &impl triergon::PacketInfo,
#     units: &mut triergon::bridge::access::AccessUnits,
#     cancel: &std::sync::atomic::AtomicBool, video: &mut triergon::session::VideoSession,
#     audio: &mut triergon::session::AudioSession, pcm: &mut triergon::session::PcmBlock,
#     renderer: &mut triergon::render::ass::Renderer, script: &mut triergon::render::ass::Script,
#     time_ms: i64,
# ) -> Result<(), Box<dyn std::error::Error>> {
# use triergon::framing::BuildStep;
# fn ready<T>(mut step: impl FnMut(&mut triergon::WorkBudget) -> triergon::Result<BuildStep<T>>) -> triergon::Result<T> {
#     loop {
#         match step(&mut triergon::WorkBudget::new(1 << 20))? {
#             BuildStep::Ready(value) => return Ok(value),
#             BuildStep::Yield => {}
#             BuildStep::Complete => return Err(triergon::Error::Invalid("no access unit")),
#         }
#     }
# }
use triergon::{MediaKind, PacketInfo, Rounding, TimeBase, session::Discard};

let mut job = units.packet(packet)?;
let unit = ready(|work| job.step(work, cancel))?;
let pts = unit.pts().map(|t| unit.time_base()
    .rescale(t, TimeBase::NANOSECONDS, Rounding::Floor)).transpose()?;
match track.kind() {
    MediaKind::Video => { video.feed(unit.payload(), pts)?; while video.step(None) {} }
    MediaKind::Audio => { audio.feed_discarding(unit.payload(), pts, Discard::of(&unit))?;
                          audio.pull(None, 0, pcm); }
    _ => {}
}
let images = renderer.render(script, time_ms)?.images;
# let _ = images;
# Ok(())
# }
```

The [play example](examples/play.rs) shows source opening, track selection,
decoder configuration, draining both sessions at EOF, and rendering an ASS
script at a requested time. From the workspace root:

```sh
cargo run -p triergon --features playback,codecs,render --example play -- \
  crates/playback/tests/fixtures/mkv-feed/three-tracks.mkv \
  harness/fixtures/ass-render/decorations-orientation.ass 500
```

For demuxing alone, use `AnyReader::open` with the source prefix and capabilities,
then supply the ranges requested by `ContainerReader::step`. After
`reader.request_seek(SeekRequest { track, target, policy })`, the reader delivers
`Event::Seek(SeekReceipt)` before the next packet.

`open_blocking(source, &pool)` does the probing and input handling for a
`std::io::Read + Seek` source and returns a `Blocking` driver with `next_event`,
`next_packet` and `seek`; `open_sequential` does the same for a pipe, which the readers
then treat as FFmpeg treats one (no seeking, no known length). Both adapters stop
reading as soon as a fixed container signature selects the format. Matroska needs
four bytes, Ogg five and ISO BMFF eight. A native FLAC marker also suffices once
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
checks prefix verdicts, the format selected by both adapters and the bytes
available again after rewinding or replay.

## Features and lower-level crates

| Feature | Default | Module | Through `AnyReader` |
|---|---|---|---|
| `matroska` | yes | `triergon::matroska` ([triergon-matroska](../matroska/README.md)) | yes |
| `isobmff` | no | `triergon::isobmff` ([triergon-isobmff](../isobmff/README.md)) | yes, for seekable sources |
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
shared types and leaves `AnyReader` with no enabled formats.

Re-exported from triergon-core, independent of features:

| Area | Items |
|---|---|
| Container model | `ContainerReader`, `ReaderStep`, `Event`, `PacketInfo`, `TrackInfo`, `TrackId`, `MediaKind`, `CodecId`, `RandomAccess`, `Trim`, `SeekRequest`, `SeekPolicy`, `SeekReceipt`, `SeekMethod`, `ColourDescription`, `ColourRange`, `MasteringDisplay`, `ContentLight`, `Sourced`, `Origin` |
| Input and work | `Step`, `Backpressure`, `InputCursor`, `InputRequest`, `InputWindow`, `InputToken`, `SourceId`, `SourceCapabilities`, `SourceLocation`, `SourceFailure`, `WorkBudget` |
| Storage, time, errors | `Budget`, `BudgetVec`, `Bytes`, `Time`, `TimeBase`, `Rounding`, `Error`, `Result` |

The meaning of each type is documented in
[triergon-core](../core/README.md#container-model).

## Behaviour and limits

- **Probing.** `probe` returns `Probe::Matroska` for an EBML header
  (`1A 45 DF A3`), `Probe::Isobmff` when bytes 4–8 are `ftyp`, `styp`, `moov`,
  `moof`, `mdat`, `free`, `skip` or `sidx`, and `Probe::Ts` when sync bytes `0x47`
  sit at offsets 0, 188 and 376 (at least 377 bytes). `Probe::M2ts` checks sync
  at offsets 4, 196 and 388 behind the original arrival prefixes. When both patterns
  match, the facade selects TS188.
  `Probe::Ogg` is an `OggS` page. With the `elementary` feature,
  `triergon_elementary::probe` recognises native FLAC (`fLaC`, or a frame header that a
  second one continues) and MPEG audio, ADTS and AC-3 by two frames that agree, after
  any ID3v2 tag. Anything else, including a prefix too short to decide, is
  `Probe::Unknown`.
- **Opening.** MPEG-TS opens through `AnyReader::MpegTs` with its `mpegts`
  feature. Its default program selection requires a single PAT media program;
  use `mpegts::ReaderLimits` for an explicit selection. Unknown sources, a format without its feature, and ISO
  BMFF on a source without random access are also `Error::Unsupported`.
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
  `AnyOther::Matroska(event)` for Matroska configuration, documents and structure,
  or `AnyOther::Isobmff(event)` for root boxes and the end of opening. ISO BMFF has
  no tracks until `ReaderEvent::Movie`.
  `AnyOther::MpegTs(event)` retains source transport/PES events, header-only
  video bytes and explicit unsupported/interrupted framing outcomes. A PES
  never becomes a neutral packet. See the MPEG-TS README for admitted carriages.
- **Seeking.** `request_seek` forwards to the format reader. For Matroska the
  `TrackId` is the TrackNumber, the target is floored to nanoseconds, and only
  video tracks can be seek targets. For ISO BMFF the `TrackId` is the `track_ID`, the
  target is on that track's media clock without edits, and any track can be a target.
  MPEG-TS can seek within a length-known, seekable entity for
  [supported access units](../mpegts/README.md#access-units).

## Conformance

The facade adds no parsing; format semantics and their FFmpeg comparisons live in
the format crates, and the rules every reader follows are the
[container contract](../../docs/architecture.md#container-contract).
`tests/any_reader.rs` checks that `probe` classifies the three container families,
that an authored Matroska file reads through `AnyReader` with the expected track and
packet, and that a disabled MPEG-TS feature is refused. `tests/any_reader_mpegts.rs`
checks TS188/M2TS192 dispatch, full source AUs and unchanged PES payload bytes.
`tests/any_reader_isobmff.rs` reads and seeks an
authored MP4 through `AnyReader`; `tests/any_reader_ogg.rs` and
`tests/any_reader_elementary.rs` read authored Ogg Opus, MPEG audio and headerless FLAC
streams with their trims. `tests/blocking_probe.rs` checks short reads, interruptions,
reader features, container precedence and budgeted prefix replay.
`tests/play.rs` runs the example pipeline with the tracked AVC/AAC and ASS
fixtures under plain `cargo test`.

The playback comparisons run the example's demux, framing and decode loop over
Matroska, MP4 and MPEG-TS, checking packet fields and payloads, ordered RGBA
pictures with presentation times, and stereo PCM. Matroska and MP4 use
`Player`. MPEG-TS uses its source access-unit reader. The facade also has
separate comparisons for strict MPEG-TS access units and seek delivery.
See the [play corpus](../../harness/codecs/play-manifest.json) and
[quality gates](../../docs/quality.md).

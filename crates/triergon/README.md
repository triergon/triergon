# triergon

The consumer boundary of the workspace. It re-exports the shared vocabulary and the
neutral container model of [triergon-core](../core/README.md), classifies a source
from its first bytes (`probe`), opens it as one closed reader enum (`AnyReader`), and
exposes each format crate behind its own Cargo feature. It is a composition-layer
crate that depends on triergon-core and the enabled format crates. It parses nothing
itself and does no I/O.

## Usage

```rust
use std::sync::atomic::AtomicBool;
use triergon::{
    AnyReader, Budget, Bytes, ContainerReader, Event, InputWindow, PacketInfo,
    SourceCapabilities, SourceId, Step, TrackInfo, WorkBudget,
};

# let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../media/tests/fixtures/seek/cues-front.mkv");
let file = std::fs::read(path)?;                      // any transport; the reader does no I/O
let pool = Budget::new(64 << 20)?;
let caps = SourceCapabilities { length: Some(file.len() as u64), random_access: true };
let mut reader = AnyReader::open(&file[..file.len().min(64)], &pool, SourceId(1), caps)?;
let cancel = AtomicBool::new(false);
loop {
    match reader.step(&mut WorkBudget::new(1 << 16), &cancel)? {
        Step::Ready(Event::Packet(p)) => {
            // p.track(), p.payload(), p.pts(), p.dts(), p.time_base(), p.random_access()
        }
        Step::Ready(Event::Seek(receipt)) => { /* receipt.landing, receipt.discard_until */ }
        Step::Ready(Event::Other(_)) => {}            // configuration, documents, structure
        // Supply 1..=req.maximum bytes at req.offset, or finish at end of source.
        Step::NeedInput(req) => match file.get(usize::try_from(req.offset)?..) {
            Some(rest) if !rest.is_empty() => {
                let bytes = Bytes::copy_from_slice(&pool, &rest[..rest.len().min(req.maximum)])?;
                reader.provide(&InputWindow::new(req.source, req.token, req.offset, bytes)?)?;
            }
            _ => reader.finish(req.source, req.token)?,
        },
        Step::Yield => {}                             // step budget spent
        Step::Backpressure(_) => { /* drop retained packets, then step again */ }
        Step::End(_) => break,
    }
}
for i in 0..reader.track_count() {
    let track = reader.track(i).ok_or("no such track")?; // &dyn TrackInfo
    // track.id(), track.kind(), track.codec(), track.codec_tag(), track.configuration()
}
# Ok::<(), Box<dyn std::error::Error>>(())
```

A seek is `reader.request_seek(SeekRequest { track, target, policy })?`; the reader
then asks for input and delivers `Event::Seek(SeekReceipt)` before the next packet.

`open_blocking(source, &pool)` does the probing and input handling for a
`std::io::Read + Seek` source and returns a `Blocking` driver with `next_event`,
`next_packet` and `seek`; `open_sequential` does the same for a pipe, which the readers
then treat as FFmpeg treats one (no seeking, no known length).

## Supported

| Feature | Default | Module | Through `AnyReader` |
|---|---|---|---|
| `matroska` | yes | `triergon::matroska` ([triergon-matroska](../media/README.md)) | yes |
| `isobmff` | no | `triergon::isobmff` ([triergon-isobmff](../isobmff/README.md)) | yes, for seekable sources |
| `mpegts` | no | `triergon::mpegts` ([triergon-mpegts](../mpegts/README.md)) | yes, source access units in TS188/M2TS192 |
| `ogg` | no | `triergon::ogg` ([triergon-ogg](../ogg/README.md)) | yes |
| `elementary` | no | `triergon::elementary` ([triergon-elementary](../elementary/README.md)): native FLAC, MPEG audio, ADTS, AC-3 | yes |
| `framing` | no | `triergon::framing` ([triergon-framing](../framing/README.md)) | not a reader |
| `subtitles` | no | `triergon::subtitles` ([triergon-subtitles](../subtitles/README.md)) | not a reader |

Re-exported from triergon-core, independent of features:

| Area | Items |
|---|---|
| Container model | `ContainerReader`, `ReaderStep`, `Event`, `PacketInfo`, `TrackInfo`, `TrackId`, `MediaKind`, `CodecId`, `RandomAccess`, `Trim`, `SeekRequest`, `SeekPolicy`, `SeekReceipt`, `SeekMethod`, `ColourDescription`, `ColourRange`, `MasteringDisplay`, `ContentLight`, `Sourced`, `Origin` |
| Input and work | `Step`, `Backpressure`, `InputRequest`, `InputWindow`, `InputToken`, `SourceId`, `SourceCapabilities`, `SourceLocation`, `SourceFailure`, `WorkBudget` |
| Storage, time, errors | `Budget`, `Bytes`, `Time`, `TimeBase`, `Error`, `Result` |

The meaning of each type is documented in
[triergon-core](../core/README.md#container-model).

## Behaviour and limits

- **Probing.** `probe` returns `Probe::Matroska` for an EBML header
  (`1A 45 DF A3`), `Probe::Isobmff` when bytes 4–8 are `ftyp`, `styp`, `moov`,
  `moof`, `mdat`, `free`, `skip` or `sidx`, and `Probe::Ts` when sync bytes `0x47`
  sit at offsets 0, 188 and 376 (at least 377 bytes). `Probe::M2ts` checks sync
  at offsets 4, 196 and 388 behind the original arrival prefixes.
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
  `matroska::typed::Reader` or `isobmff::Reader` directly and wrap it as
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
  MPEG-TS currently refuses seeking and can reset to the source start.

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
streams with their trims. Gates: [docs/quality.md](../../docs/quality.md).

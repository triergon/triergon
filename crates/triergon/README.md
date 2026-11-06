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

let pool = Budget::new(64 << 20)?;
let caps = SourceCapabilities { length: Some(len), random_access: true };
let mut reader = AnyReader::open(&first_bytes, &pool, SourceId(1), caps)?;
let cancel = AtomicBool::new(false);
loop {
    match reader.step(&mut WorkBudget::new(1 << 16), &cancel)? {
        Step::Ready(Event::Packet(p)) => { /* p.track(), p.payload(), p.pts(), p.time_base(), p.random_access() */ }
        Step::Ready(Event::Seek(receipt)) => { /* receipt.landing, receipt.discard_until */ }
        Step::Ready(Event::Other(_)) => {}            // configuration, documents, structure
        Step::NeedInput(req) => {                     // supply 1..=req.maximum bytes at req.offset
            let bytes = Bytes::copy_from_slice(&pool, fetch(req.offset, req.maximum))?;
            reader.provide(&InputWindow::new(req.source, req.token, req.offset, bytes)?)?;
            // or reader.finish(req.source, req.token)? at end of source
        }
        Step::Yield => {}                             // step budget spent
        Step::Backpressure(_) => { /* drop retained packets, then step again */ }
        Step::End(_) => break,
    }
}
for i in 0..reader.track_count() {
    let track = reader.track(i).unwrap();             // &dyn TrackInfo
    // track.id(), track.kind(), track.codec_tag(), track.configuration(), track.colour()
}
```

A seek is `reader.request_seek(SeekRequest { track, target, policy })?`; the reader
then asks for input and delivers `Event::Seek(SeekReceipt)` before the next packet.

## Supported

| Feature | Default | Module | Through `AnyReader` |
|---|---|---|---|
| `matroska` | yes | `triergon::matroska` ([triergon-matroska](../media/README.md)) | yes |
| `isobmff` | no | `triergon::isobmff` ([triergon-isobmff](../isobmff/README.md)) | probed, refused by `open` |
| `mpegts` | no | `triergon::mpegts` ([triergon-mpegts](../mpegts/README.md)) | probed, refused by `open` |
| `framing` | no | `triergon::framing` ([triergon-framing](../framing/README.md)) | not a reader |
| `subtitles` | no | `triergon::subtitles` ([triergon-subtitles](../subtitles/README.md)) | not a reader |

Re-exported from triergon-core, independent of features:

| Area | Items |
|---|---|
| Container model | `ContainerReader`, `ReaderStep`, `Event`, `PacketInfo`, `TrackInfo`, `TrackId`, `MediaKind`, `RandomAccess`, `Trim`, `SeekRequest`, `SeekPolicy`, `SeekReceipt`, `SeekMethod`, `ColourDescription`, `ColourRange`, `MasteringDisplay`, `ContentLight`, `Sourced`, `Origin` |
| Input and work | `Step`, `Backpressure`, `InputRequest`, `InputWindow`, `InputToken`, `SourceId`, `SourceCapabilities`, `SourceLocation`, `SourceFailure`, `WorkBudget` |
| Storage, time, errors | `Budget`, `Bytes`, `Time`, `TimeBase`, `Error`, `Result` |

The meaning of each type is documented in
[triergon-core](../core/README.md#container-model).

## Behaviour and limits

- **Probing.** `probe` returns `Probe::Matroska` for an EBML header
  (`1A 45 DF A3`), `Probe::Isobmff` when bytes 4–8 are `ftyp`, `styp`, `moov`,
  `moof`, `mdat`, `free`, `skip` or `sidx`, and `Probe::Ts` when sync bytes `0x47`
  sit at offsets 0, 188 and 376 (at least 377 bytes). 192-byte M2TS is not probed.
  Anything else, including a prefix too short to decide, is `Probe::Unknown`.
- **Opening.** `AnyReader::open` refuses ISO BMFF and MPEG-TS with
  `Error::Unsupported` whatever features are enabled, because their readers do not
  implement `ContainerReader`; use `triergon::isobmff` and `triergon::mpegts`
  directly. Unknown sources, and Matroska without the `matroska` feature, are also
  `Error::Unsupported`.
- **Matroska defaults.** `open` builds the Matroska reader with
  `triergon_matroska::Limits::default()` and a one-byte maximum input window, so
  every `NeedInput` asks for at most one byte. For other limits or larger windows,
  construct `matroska::typed::Reader` directly and wrap it as `AnyReader::Matroska`.
- **Dispatch.** Forwarding is one `match` per call with no allocation and no
  dynamic dispatch on the packet path. Tracks are returned as `&dyn TrackInfo`.
  `AnyPacket` implements `PacketInfo`; match `AnyPacket::Matroska(p)` for the
  format's own packet type.
- **Other events.** The wrapper type inside `Event::Other` is not re-exported, so
  non-packet events can be skipped but not inspected through the facade; read them
  from the concrete format reader.
- **Seeking.** `request_seek` forwards to the format reader. For Matroska the
  `TrackId` is the TrackNumber, the target is floored to nanoseconds, and only
  video tracks can be seek targets.

## Conformance

The facade adds no parsing; format semantics and their FFmpeg comparisons live in
the format crates, and the rules every reader follows are the
[container contract](../../docs/architecture.md#container-contract).
`tests/any_reader.rs` checks that `probe` classifies the three container families,
that an authored Matroska file reads through `AnyReader` with the expected track and
packet, and that ISO BMFF is refused. Gates: [docs/quality.md](../../docs/quality.md).

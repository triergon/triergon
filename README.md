# Triergon

Media containers, packet framing, subtitles and decoders in safe Rust, with no
dependencies.

Triergon reads Matroska/WebM, MP4/MOV and MPEG-TS, frames H.264, HEVC, AV1, VP9, AAC,
AC-3 and DTS packets for hardware or software decoders, decodes text and bitmap
subtitles, and includes software decoders for H.264, HEVC, AAC, AC-3/E-AC-3 and DTS.
It is written from the published standards and measured case by case against FFmpeg.

> **Status: experimental.** APIs change without notice and nothing is published to
> crates.io yet. See [status](docs/status.md) for what is verified today.

## Principles

- **Safe Rust, zero dependencies.** Every crate forbids `unsafe`. The dependency graph
  is `std` and nothing else.
- **As good as FFmpeg, or it does not ship.** Each module must match FFmpeg on its whole
  declared feature set, run in at most 1.20× FFmpeg's time per case (target 0.80×) and
  use less memory. Results are measured, paired and published per case.
- **Built for untrusted input.** Every input-dependent allocation is budgeted, every
  loop is bounded, and malformed data produces an error, never a panic or a hang.
- **You own the I/O.** Parsers take byte windows and tell you which bytes they need
  next. They work with files, HTTP ranges, pipes and live segments, with no async
  runtime and no blocking inside the library.
- **Exact timing.** Signed timestamps, exact rational time bases, and no invented
  values: a missing DTS stays missing, and a seek returns where it actually landed.

## Crates

| Crate | What it gives you |
|---|---|
| [`triergon`](crates/triergon) | Facade: core vocabulary, container model, `AnyReader` and one Cargo feature per format |
| [`triergon-core`](crates/core) | Budgets, shared byte buffers, exact time, input windows, resumable steps, the neutral container model |
| [`triergon-matroska`](crates/media) | Matroska and WebM reader with seeking, chapters, tags, attachments, colour and HDR |
| [`triergon-isobmff`](crates/isobmff) | MP4, MOV and fragmented MP4: sample tables, edit lists, range-based playback |
| [`triergon-mpegts`](crates/mpegts) | MPEG-TS and M2TS: PSI, PES assembly, timestamps, HLS segments |
| [`triergon-framing`](crates/framing) | Codec configuration and packet conversion for H.264, HEVC, AV1, VP9, AAC, AC-3, DTS |
| [`triergon-subtitles`](crates/subtitles) | SRT, WebVTT, ASS, mov_text and tx3g; PGS, DVB and DVD bitmap subtitles |
| [`triergon-pixel`](crates/pixel) | Decoded-picture views, BT.601 RGBA to YUV 4:2:0, letterboxing into a 1920×1080 RGBA canvas |
| [`triergon-codec`](crates/codec) | Decoder traits, audio frames, channel layouts and the shared channel order |
| [`triergon-codec-h264`](crates/codec-h264), [`-hevc`](crates/codec-hevc) | Software video decoders |
| [`triergon-codec-aac`](crates/codec-aac), [`-ac3`](crates/codec-ac3), [`-dca`](crates/codec-dca) | Software audio decoders |
| [`triergon-codec-mpeg1`](crates/codec-mpeg1) | MPEG-1 video encoder with I and P pictures |
| [`triergon-playback`](crates/playback) | Container-independent packet feeds with seeking |
| [`triergon-session`](crates/session) | Decode sessions: queues, clocks and output over the decoders |
| [`triergon-capture`](crates/capture) | Capture session: RGBA frames in, MPEG-1 video in MPEG-TS out |

## Getting started

```sh
git clone https://github.com/triergon/triergon && cd triergon
cargo xtask fixtures   # generate the authored test corpus
cargo test --workspace
```

Reading a Matroska file:

```rust
use std::{fs::File, sync::atomic::AtomicBool};
use triergon_matroska::{Limits, typed::BlockingReader};

let cancel = AtomicBool::new(false);
let mut reader = BlockingReader::open_seekable(File::open("movie.mkv")?, Limits::default())?;
for track in reader.tracks() {
    println!("{track:?}");
}
while let Some(packet) = reader.next_typed_packet(&cancel)? {
    // packet.packet: track, pts_ns, duration_ns, keyframe, data
}
```

Each crate README shows its entry points, and the `examples/` directories contain
small programs.

## Documentation

- [Architecture](docs/architecture.md): layers, input and resource model, container contract
- [Quality](docs/quality.md): the FFmpeg gates, corpus, harness and measurement rules
- [Status](docs/status.md): verified scope and current results per module
- [Contributing](CONTRIBUTING.md)

## Licence

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
Test media and specification texts keep their own terms. Patent licensing for codecs
is the responsibility of whoever ships them.

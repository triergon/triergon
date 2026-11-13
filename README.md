# Triergon

Media containers, packet framing, subtitles, fonts and decoders in safe Rust, with no
dependencies.

Triergon reads Matroska/WebM, MP4/MOV, MPEG-TS, Ogg, native FLAC and raw MPEG audio,
ADTS and AC-3 streams, frames H.264, HEVC, AV1, VP9, AAC, AC-3 and DTS packets for
hardware or software decoders, decodes text and bitmap subtitles, closed captions and
teletext, reads OpenType and TrueType fonts, shapes text, rasterises glyphs and subtitle
bitmaps, reads ASS scripts and their override tags, and includes software decoders for
H.264, HEVC, AAC, AC-3/E-AC-3, DTS, FLAC, Opus, Vorbis and MPEG audio Layer I, II and
III. It is written from the published standards and measured case by case against a
pinned reference: FFmpeg for media, FreeType for fonts, HarfBuzz and FriBidi for shaping,
libass for rasterising and ASS parsing.

> **Status: experimental.** APIs change without notice and nothing is published to
> crates.io yet. See [status](docs/status.md) for what is verified today.

## Principles

- **Safe Rust, zero dependencies.** Every crate forbids `unsafe`. The dependency graph
  is `std` and nothing else.
- **As good as the reference, or it does not ship.** Each module must match its pinned
  reference (FFmpeg for media, FreeType for fonts, HarfBuzz and FriBidi for shaping,
  libass for rasterising and subtitle rendering) on its
  whole declared feature set, stay within the [runtime ceiling](docs/quality.md#gates) of
  the reference's time per case and use less memory. Results are measured, paired and
  published per case.
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
| [`triergon-bitstream`](crates/bitstream) | Bit readers, exp-Golomb, RBSP, CABAC and CRCs shared by the format, framing and codec crates |
| [`triergon-matroska`](crates/media) | Matroska and WebM reader with seeking, chapters, tags, attachments, colour and HDR |
| [`triergon-isobmff`](crates/isobmff) | MP4, MOV and fragmented MP4: sample tables, edit lists, range-based playback |
| [`triergon-mpegts`](crates/mpegts) | MPEG-TS and M2TS: PSI, PES assembly, timestamps, HLS segments |
| [`triergon-ogg`](crates/ogg) | Ogg with the FLAC, Opus and Vorbis mappings: chained and multiplexed streams, granule timing, trims, seeking |
| [`triergon-elementary`](crates/elementary) | Native FLAC, MPEG audio (ID3, Xing/Info/VBRI, LAME gapless), ADTS and AC-3 streams |
| [`triergon-framing`](crates/framing) | Codec configuration and packet conversion for H.264, HEVC, AV1, VP9, AAC, AC-3, DTS |
| [`triergon-subtitles`](crates/subtitles) | SRT, WebVTT, ASS, mov_text and tx3g; PGS, DVB and DVD bitmap subtitles; CEA-608 closed captions; DVB teletext |
| [`triergon-font`](crates/font) | OpenType and TrueType faces and collections: cmap, names, `glyf` and CFF/CFF2 outlines, metrics, kerning, face matching |
| [`triergon-shape`](crates/shape) | OpenType shaping with the script shapers, the bidirectional algorithm, Unicode properties |
| [`triergon-raster`](crates/raster) | Glyph outlines, border stroking, the tile rasteriser, blur, composition and clipping of subtitle bitmaps |
| [`triergon-ass`](crates/ass) | ASS and SSA scripts and Matroska packets, override tags, per-glyph state over time, karaoke and drawings |
| [`triergon-pixel`](crates/pixel) | Decoded-picture views, BT.601 RGBA to YUV 4:2:0, letterboxing into a 1920×1080 RGBA canvas |
| [`triergon-codec`](crates/codec) | Decoder traits, audio frames, channel layouts and orders, the shared inverse MDCT |
| [`triergon-codec-h264`](crates/codec-h264), [`-hevc`](crates/codec-hevc) | Software video decoders |
| [`triergon-codec-aac`](crates/codec-aac), [`-ac3`](crates/codec-ac3), [`-dca`](crates/codec-dca), [`-flac`](crates/codec-flac), [`-opus`](crates/codec-opus), [`-vorbis`](crates/codec-vorbis), [`-mp3`](crates/codec-mp3) | Software audio decoders |
| [`triergon-codec-mpeg1`](crates/codec-mpeg1) | MPEG-1 video encoder with I and P pictures |
| [`triergon-playback`](crates/playback) | Container-independent packet feeds with seeking |
| [`triergon-session`](crates/session) | Decode sessions: queues, clocks and output over the decoders, container track decoding with trims |
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

The published results assume this release profile in the program that uses the crates:

```toml
[profile.release]
lto = "fat"
codegen-units = 1
```

Cargo's default profile runs measurably slower on 32-bit ARM: without fat LTO the
playback worker executes up to 25.0% more instructions, and 16 codegen units add up to
another 2.5%. `panic = "abort"` saves a further 2 to 3%; it is safe unless the program
wraps calls into these crates in `catch_unwind`. See
[measuring performance](docs/quality.md#measuring-performance).

## Documentation

- [Architecture](docs/architecture.md): layers, input and resource model, container contract
- [Quality](docs/quality.md): the reference gates, corpus, harness and measurement rules
- [Status](docs/status.md): verified scope and current results per module
- [Contributing](CONTRIBUTING.md)

## Licence

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
Test media and specification texts keep their own terms. Patent licensing for codecs
is the responsibility of whoever ships them.

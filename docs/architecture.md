# Architecture

Triergon is a set of small crates that read media containers, frame codec packets,
decode subtitles and selected codecs, read fonts, render ASS subtitles, and compose them
into playback feeds and decode sessions. Every crate is safe Rust with no third-party
dependencies. The design favours incremental input, explicit resource bounds and exact
timing over convenience.

## Layers

The table lists the layers from the bottom up. A crate depends only on crates in its own
row or in rows above it; `triergon-core` depends on no format. Formats, subtitles and
decoders use `triergon-framing` for packet boundaries, header metadata, configuration
records and NAL unit splitting. The AAC, AC-3 and DTS decoders read the complete
syntax with their decoder state, including bit positions, CRC coverage and refusals.

| Layer | Crates | Role |
|---|---|---|
| Foundation | [`triergon-core`](../crates/core), [`triergon-bitstream`](../crates/bitstream) | Budgets, shared byte storage, exact time, input windows, resumable steps, the workspace error, the neutral container model (`ContainerReader`); bit readers, RBSP, CABAC and CRCs |
| Pixels | [`triergon-pixel`](../crates/pixel) | Decoded-picture views, coded YUV 4:2:0 pictures, BT.601 RGBA to YUV, letterboxing into an RGBA canvas, subtitle palette Y'CbCr to RGB |
| Framing | [`triergon-framing`](../crates/framing) | Codec configuration and packet-format adapters: Annex B, H.264 and HEVC NAL units and their type classes, AV1, VP9, ADTS, AC-3 and E-AC-3, DTS core and DTS-HD substreams; FLAC, Opus, Vorbis and MPEG audio headers and their canonical configurations |
| Formats | [`triergon-matroska`](../crates/matroska), [`triergon-isobmff`](../crates/isobmff), [`triergon-mpegts`](../crates/mpegts), [`triergon-ogg`](../crates/ogg), [`triergon-elementary`](../crates/elementary) | Container readers: tracks, packets, metadata, seeking |
| Subtitles | [`triergon-subtitles`](../crates/subtitles) | Text cue extraction, bitmap subtitle decoding, closed captions and teletext |
| Fonts | [`triergon-font`](../crates/font) | OpenType and TrueType faces: tables, collections, cmap, names, `glyf` and CFF/CFF2 outlines, metrics, kerning, face matching |
| Shaping | [`triergon-shape`](../crates/shape) | OpenType shaping with the script shapers, the bidirectional algorithm, Unicode properties |
| Rasterising | [`triergon-raster`](../crates/raster) | Glyph outlines in 26.6 fixed point and their transforms, border stroking, the tile rasteriser, blur, composition and clipping of the alpha bitmaps a subtitle is drawn from |
| Subtitle scripts | [`triergon-ass`](../crates/ass) | ASS and SSA scripts and Matroska packet streams, styles, typed override tags, the state of every glyph of an event at a time, karaoke, drawings, and rendering them to positioned alpha images |
| Codecs | [`triergon-codec`](../crates/codec); [`triergon-codec-aac`](../crates/codec-aac), [`-ac3`](../crates/codec-ac3), [`-dca`](../crates/codec-dca), [`-flac`](../crates/codec-flac), [`-mp3`](../crates/codec-mp3), [`-opus`](../crates/codec-opus), [`-vorbis`](../crates/codec-vorbis), [`-h264`](../crates/codec-h264), [`-hevc`](../crates/codec-hevc); [`triergon-codec-mpeg1`](../crates/codec-mpeg1) | Decoder traits, audio vocabulary and the shared inverse MDCT; software decoders; MPEG-1 video encoder |
| Composition | [`triergon-playback`](../crates/playback), [`triergon-raw-video`](../crates/raw-video), [`triergon-hls`](../crates/hls), [`triergon-bridge`](../crates/bridge), [`triergon-session`](../crates/session), [`triergon-capture`](../crates/capture), [`triergon`](../crates/triergon) | Matroska, MP4 and MPEG-TS packet feeds with probing, track selection and seeking; raw AVC and HEVC Annex B feeds; HLS segments and forward-only TS188; decoder access units and subtitle receivers over playback packets; decode sessions over the codecs; capture from RGBA to MPEG-1 video in MPEG-TS; the consumer facade with `AnyReader` |
| Tooling | [`media-worker`](../crates/media-worker), [`codec-worker`](../crates/codec-worker), [`capture-worker`](../crates/capture-worker), [`playback-worker`](../crates/playback-worker), [`transport-worker`](../crates/transport-worker), [`subtitle-worker`](../crates/subtitle-worker), [`font-worker`](../crates/font-worker), [`shape-worker`](../crates/shape-worker), [`raster-worker`](../crates/raster-worker), [`ass-worker`](../crates/ass-worker), [`triergon-harness-protocol`](../crates/harness-protocol), [`triergon-harness-runner`](../crates/harness-runner), [`triergon-worker-entry`](../crates/worker-entry), [`harness/`](../harness), [`tools/`](../tools), `xtask` | Differential and benchmark workers and the repository tooling; not part of the library |

The reference workers (FFmpeg, mpg123, FreeType, HarfBuzz and FriBidi, libass) live in
`harness/` as separate processes and are never linked into a library crate.

## Input and work model

Parsers never perform I/O. The caller owns the transport and feeds bounded windows:

- `InputWindow` carries bytes at a known source offset. A parser answers with a
  `Step`: an event (`Ready`), a request for more bytes or a specific byte range
  (`NeedInput`), a pause because the work budget is spent (`Yield`), backpressure
  (`Backpressure`), or the end of a source entity (`End`). The backpressure payload
  is a type parameter: steps that retain no output use an uninhabited type and
  cannot report it. A missing length is not end of file.
- Each step runs under a `WorkBudget` and a cancellation flag, so a slow or hostile
  input cannot stall the caller.
- Parser state survives across windows. Zero bytes of new input is never permission to
  loop.
- Transport failure, temporary starvation, malformed data and a known end of file are
  distinct outcomes. A parser never invents a valid-looking packet from incomplete data.

## Resources

Every allocation that depends on input is charged to a `Budget` before it happens,
using checked arithmetic. Shared buffers (`Bytes`, `BudgetVec`) carry their charge
through clones and caller retention. Work, decompression ratio, nesting depth, index
size and scan distance have separate limits. Each format exposes its limits as a plain
struct (`Limits`, `ReaderLimits`) with conservative defaults; there is no unlimited
mode. Codecs take a `Limits` of the same kind (`live_storage_bytes`, with a default that
holds every conformance stream) and charge pictures, reference pools, NAL and slice
storage, per-channel state and the output frames they fill; a refused charge is the
decoder's `Budget` error. Fixed-size state allocated at construction and short lists
the specification caps (reference lists, parameter-set slots) are not charged.

Two outcomes are kept apart everywhere: a *limit* violation is permanent for that
input, while *backpressure* means the caller is holding too much output and the
parser can continue once it releases some.

## Container contract

All readers follow the same semantic rules, and the harness checks them:

1. Never fabricate a value the source does not carry. A missing DTS stays missing.
   Derived durations or random-access flags record how they were derived.
2. Unknown is never a sentinel. Timestamps are signed; negative values are kept.
3. Specification defaults are distinguishable from explicit values.
4. No container fact is silently dropped: tracks, colour, timing, configuration,
   attachments and chapters either surface as typed data or as bounded opaque data.
5. Packets keep per-track decode order. Decode and presentation order are never
   conflated.
6. A seek returns a receipt with the actual landing point, the time before which
   output is decoded but not presented, and the method used (index, bounded scan or
   restart). A timestamp alone is not proof that the next packet is decodable.
7. A track reports its container's own codec tag unchanged and one neutral `CodecId`.
   Each reader owns one mapping from its native identifier. A codec the source does
   not identify is `CodecId::Other` with that tag, never a guess.

## Reference-compatible behaviour

Each module's reference is its behavioural reference, not its specification. Where the
specification leaves room and consumers depend on the reference's choice (FFmpeg's
malformed-MOV recovery, edit-list clocks, transport-stream PES completion and subtitle
timing; libass's face metrics, shaping and composition), the crates implement that
choice deliberately and document it. A crate keeps three kinds of API apart:

- **Specification behaviour** lives at the crate root.
- **Compatibility policies** reproduce the reference where a consumer may choose its
  result. They live in `compat::<family>` (for example `triergon_framing::compat::nal`
  or `triergon_font::compat::libass`) and are named for what they do. A version,
  revision or history marker (a revision hash, `Pinned…`, `…V1`) is never part of an
  identifier: each family's module documentation names the reference revision it
  reproduces, once.
- **Comparison-only projections** are anything a consumer cannot use outside a
  comparison with the reference, such as FFmpeg return codes, parser byte positions or
  a whole parser model (`triergon_framing::reference_trace::dts`). They are compiled
  only with the crate's non-default `reference-trace` feature: types under
  `reference_trace::<family>`, accessors on other types behind
  `#[cfg(feature = "reference-trace")]`. Examples and tests that use them declare
  `required-features = ["reference-trace"]` or gate the individual assertion; workers
  enable the feature on their dependency.

The text stack exposes each layer’s reference-compatible operations at its crate root.
Its `compat` modules hold the choices a caller makes when composing the layers, such as
the face metrics used to request a size or the transforms used to composite glyphs.

A rename keeps every value that reaches a trace (profile strings, error texts)
unchanged, so worker traces stay byte-identical.

A format whose only consumer need is FFmpeg's result has no specification decoder.
The DVB, PGS and DVD bitmap subtitles in `triergon-subtitles` are decoded only by
their receivers in `compat`. The crate root keeps the segment, packet and container
configuration types that playback binds its tracks with.

## Dispatch and errors

Readers are concrete types implementing `ContainerReader`, used through generics. A
consumer that accepts several formats uses the closed enum `triergon::AnyReader`.
There is no global registry and no dynamic dispatch on the per-packet path.

Readers, framers, subtitle decoders, the font and rendering crates and the composition
crates report `triergon_core::Error`: a small `Copy` enum whose variant is the class of the failure
(malformed input, a limit, backpressure, truncation, a transport failure,
cancellation, an unsupported feature) and keeps its detail, such as the requested and
maximum size of a limit violation or a `&'static str` naming the failed check.
Decoders report `triergon_codec::DecodeError`, or an enum of their own that names the
same classes, and convert into `Error`. The capture pipeline's `CaptureError` wraps the
pixel, encoder and multiplex errors it meets.

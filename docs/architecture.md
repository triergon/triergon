# Architecture

Triergon is a set of small crates that read media containers, frame codec packets,
decode subtitles and selected codecs, read fonts, and compose them into a playback feed. Every
crate is safe Rust with no third-party dependencies. The design favours incremental
input, explicit resource bounds and exact timing over convenience.

## Layers

Crates depend only on layers below them. Core never depends on a format. The subtitle and
H.264/HEVC decoder crates take NAL unit splitting, headers and type classes from
`triergon-framing`.

| Layer | Crates | Role |
|---|---|---|
| Foundation | [`triergon-core`](../crates/core), [`triergon-bitstream`](../crates/bitstream) | Budgets, shared byte storage, exact time, input windows, resumable steps, errors, the neutral container model (`ContainerReader`); bit reading, RBSP, CABAC and CRCs |
| Formats | [`triergon-matroska`](../crates/media) (Matroska/WebM), [`triergon-isobmff`](../crates/isobmff), [`triergon-mpegts`](../crates/mpegts), [`triergon-ogg`](../crates/ogg), [`triergon-elementary`](../crates/elementary) (native FLAC, MPEG audio, ADTS, AC-3) | Container readers: tracks, packets, metadata, seeking |
| Framing | [`triergon-framing`](../crates/framing) | Codec configuration and packet-format adapters (Annex-B, H.264 and HEVC NAL units and their type classes, ADTS, AC-3, DTS core, AV1, VP9; FLAC, Opus, Vorbis and MPEG audio headers and their canonical configurations) |
| Subtitles | [`triergon-subtitles`](../crates/subtitles) | Text cue extraction and bitmap subtitle decoding |
| Fonts | [`triergon-font`](../crates/font) | OpenType and TrueType faces: tables, collections, cmap, names, `glyf` and CFF/CFF2 outlines, metrics, kerning, face matching |
| Shaping | [`triergon-shape`](../crates/shape) | OpenType shaping with the script shapers, the bidirectional algorithm, Unicode properties |
| Rasterising | [`triergon-raster`](../crates/raster) | Glyph outlines in 26.6 fixed point, their transforms, border stroking, the tile rasteriser, blur, composition and clipping of the alpha bitmaps a subtitle is drawn from |
| Subtitle scripts | [`triergon-ass`](../crates/ass) | ASS and SSA scripts and Matroska packet streams, styles, typed override tags, the state of every glyph of an event at a time, karaoke, drawings, and rendering them to positioned alpha images |
| Pixels | [`triergon-pixel`](../crates/pixel) | Decoded-picture views, coded YUV 4:2:0 pictures, BT.601 RGBA to YUV, letterbox into 1920×1080 RGBA, subtitle palette Y'CbCr to RGB |
| Codecs | [`triergon-codec`](../crates/codec); [`triergon-codec-aac`](../crates/codec-aac), [`-ac3`](../crates/codec-ac3), [`-dca`](../crates/codec-dca), [`-flac`](../crates/codec-flac), [`-mp3`](../crates/codec-mp3), [`-opus`](../crates/codec-opus), [`-vorbis`](../crates/codec-vorbis), [`-h264`](../crates/codec-h264), [`-hevc`](../crates/codec-hevc); [`triergon-codec-mpeg1`](../crates/codec-mpeg1) | Decoder traits, audio vocabulary and the shared inverse MDCT; software decoders; MPEG-1 video encoder |
| Composition | [`triergon`](../crates/triergon), [`triergon-playback`](../crates/playback), [`triergon-session`](../crates/session), [`triergon-capture`](../crates/capture) | Consumer facade with `AnyReader`; packet feeds over the readers; decode sessions over the codecs; capture sessions from RGBA to MPEG-TS |
| Tooling | [`media-worker`](../crates/worker), [`codec-worker`](../crates/codec-worker), [`capture-worker`](../crates/capture-worker), [`playback-worker`](../crates/playback-worker), [`subtitle-worker`](../crates/subtitle-worker), [`triergon-harness-runner`](../crates/harness-runner), [`font-worker`](../crates/font-worker), [`shape-worker`](../crates/shape-worker), [`raster-worker`](../crates/raster-worker), [`ass-worker`](../crates/ass-worker), [`harness/`](../harness), [`tools/`](../tools) | Differential and benchmark workers; not part of the library |

The FFmpeg reference workers, the FreeType font reference, the HarfBuzz and FriBidi
shaping reference and the libass rasterising and ASS parsing references live in
`harness/` as separate processes. They are never linked into a library crate.

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
size and scan distance have separate limits. Formats expose their limits as plain
`Limits` structs with conservative defaults; there is no unlimited mode. Codecs take
the same kind of `Limits` (`live_storage_bytes`, with a default that holds every
conformance stream) and charge pictures, reference pools, NAL and slice storage,
per-channel state and the output frames they fill; a refused charge is the codec's
`Budget` error. Fixed-size state allocated at construction and short lists the
specification caps (reference lists, parameter-set slots) are not charged.

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
6. A seek returns a receipt with the actual landing point, the preceding
   random-access point, preroll and the method used (index, bounded scan or restart).
   A timestamp alone is not proof that the next packet is decodable.
7. A track reports its container's own codec tag unchanged and one neutral `CodecId`.
   Each reader owns one mapping from its native identifier. A codec the source does
   not identify is `CodecId::Other` with that tag, never a guess.

## Reference-compatible behaviour

FFmpeg is the behavioural reference. Where the specification leaves room and
consumers depend on FFmpeg's choice (malformed-MOV recovery, edit-list clocks,
transport-stream PES completion, subtitle timing), the format crates implement that
choice deliberately and document it. A crate keeps three kinds of API apart:

- **Specification behaviour** lives at the crate root.
- **Compatibility policies** reproduce FFmpeg where a consumer may choose its
  result. They live in `compat::<family>` (for example
  `triergon_framing::compat::nal`) and are named for what they do. A version, revision or
  history marker (a revision hash, `Pinned…`, `…V1`) is never part of an
  identifier: each family's module documentation names the FFmpeg revision it
  reproduces, once.
- **Comparison-only projections** are anything a consumer cannot use outside a
  comparison with FFmpeg, such as FFmpeg return codes, parser byte positions or a
  whole parser model (`triergon_framing::reference_trace::dts`). They are compiled only with the crate's non-default
  `reference-trace` feature: types under `reference_trace::<family>`, accessors on
  other types behind `#[cfg(feature = "reference-trace")]`. Examples and tests that
  use them declare `required-features = ["reference-trace"]` or gate the individual
  assertion; workers enable the feature on their dependency.

A rename keeps every value that reaches a trace (profile strings, error texts)
unchanged, so worker traces stay byte-identical.

A format whose only consumer need is FFmpeg's result has no specification decoder.
The DVB, PGS and DVD bitmap subtitles in `triergon-subtitles` are decoded only by
their receivers in `compat`. The crate root keeps the segment, packet and container
configuration types that playback binds its tracks with.

## Dispatch and errors

Readers are concrete types implementing `ContainerReader`, used through generics. A
consumer that accepts several formats uses the closed enum `triergon::AnyReader`.
There is no global registry and no dynamic dispatch on the per-packet path. Errors
are typed and keep their detail (for example the requested and maximum size of a
limit violation).

## Direction

Open structural work. Each step keeps worker traces byte-identical and re-runs the
performance gates where it touches hot code.

1. **Formats.** MPEG-TS needs a `ContainerReader` with full access-unit demux and
   an `AnyReader` variant. Playback uses format-owned track classification.
2. **Reference compatibility.** `triergon-framing`, `triergon-matroska`,
   `triergon-subtitles`, `triergon-mpegts` and `triergon-isobmff` follow the convention
   in [reference-compatible behaviour](#reference-compatible-behaviour). Playback moves
   to it next.
3. **Composition.** HLS and codec bridges become separate crates. Playback keeps its own
   container engines: it carries source and native clocks, range plans and per-step
   parsing that `ContainerReader` does not.
5. **Subtitle rendering.** ASS/SSA scripts are to render to positioned alpha bitmaps that
   match libass exactly, measured against a pinned libass build through the same gates.
   Four crates, each depending only on those before it:

   | Crate | Role |
   |---|---|
   | `triergon-font` | Faces, `cmap`, names, outlines, metrics, `kern`, face matching, a budgeted value `Store`; raw table access for the shaper |
   | `triergon-shape` | Unicode data generated from the pinned UCD releases its references compiled, the bidirectional algorithm as FriBidi resolves it, and OpenType shaping (GDEF, GSUB, GPOS, `kern` fallback) with the script shapers libass reaches through HarfBuzz: default, Arabic, Hebrew, Indic, Khmer, Myanmar, Thai, Hangul and the Universal Shaping Engine |
   | `triergon-raster` | Outlines in 26.6 fixed point, the glyph transform and its quantisation, flattening, stroking for borders, the tile rasteriser, Gaussian and box blur, the alpha bitmaps borders and shadows combine into, and their rectangle clipping |
   | `triergon-ass` | Script and Matroska-packet parsing, styles, typed override tags, animation, karaoke, drawings, font selection and fallback, line wrapping, alignment, collisions, `\clip`/`\iclip`, and the renderer |

   All four exist. `triergon-ass` reads scripts and Matroska packets, styles,
   override tags, animation, karaoke and drawings, and its `Renderer` selects
   fonts with fallback, shapes, wraps, lays out, resolves collisions and draws
   the image list through `triergon-raster`. A Triergon worker and a libass
   worker render the same scripts and packet streams at the same times and
   frame sizes, and the harness compares every image exactly; vertical text
   (`@` fonts) is open. Behaviour where libass departs from the ASS
   specification by choice will follow the compat-policy convention above.
   The renderer offers no hinting option until libass's hinting-enabled modes and
   FreeType's auto-hinter pass the reference and are gated.

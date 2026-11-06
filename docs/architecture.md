# Architecture

Triergon is a set of small crates that read media containers, frame codec packets,
decode subtitles and selected codecs, and compose them into a playback feed. Every
crate is safe Rust with no third-party dependencies. The design favours incremental
input, explicit resource bounds and exact timing over convenience.

## Layers

Crates depend only on layers below them. Core never depends on a format.

| Layer | Crates | Role |
|---|---|---|
| Foundation | [`triergon-core`](../crates/core) | Budgets, shared byte storage, exact time, input windows, resumable steps, errors |
| Formats | [`triergon`](../crates/media) (Matroska/WebM), [`triergon-isobmff`](../crates/isobmff), [`triergon-mpegts`](../crates/mpegts) | Container readers: tracks, packets, metadata, seeking |
| Framing | [`triergon-framing`](../crates/framing) | Codec configuration and packet-format adapters (Annex-B, ADTS, AC-3, DTS core, AV1, VP9) |
| Subtitles | [`triergon-subtitles`](../crates/subtitles) | Text cue extraction and bitmap subtitle decoding |
| Codecs | [`triergon-codec-aac`](../crates/codec-aac), [`-ac3`](../crates/codec-ac3), [`-dca`](../crates/codec-dca), [`-h264`](../crates/codec-h264), [`-hevc`](../crates/codec-hevc) | Software decoders |
| Composition | [`triergon-playback`](../crates/playback), [`triergon-sim`](../crates/sim) | Packet feeds over the readers; decode sessions over the codecs |
| Encoding | [`triergon-capture`](../crates/capture) | RGBA to YUV conversion, MPEG-1 video encoding, MPEG-TS muxing |
| Tooling | [`media-worker`](../crates/worker), [`harness/`](../harness), [`tools/`](../tools) | Differential and benchmark workers; not part of the library |

The FFmpeg reference workers live in `harness/` as separate processes. They are
never linked into a library crate.

## Input and work model

Parsers never perform I/O. The caller owns the transport and feeds bounded windows:

- `InputWindow` carries bytes at a known source offset. A parser answers with a
  `Step` that yields an event, asks for more bytes or a specific byte range, reports
  the end of a source entity, or finishes. A missing length is not end of file.
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
`Limits` structs with conservative defaults; there is no unlimited mode.

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

## Reference-compatible behaviour

FFmpeg is the behavioural reference. Where the specification leaves room and
consumers depend on FFmpeg's choice (malformed-MOV recovery, edit-list clocks,
transport-stream PES completion, subtitle timing), the format crates implement that
choice deliberately and document it. Comparison-only projections used by the harness
are not part of the consumer API.

## Dispatch and errors

Readers are concrete types used through generics. A consumer that accepts several
formats uses one closed enum. There is no global registry and no dynamic dispatch on
the per-packet path. Errors are typed and keep their detail (for example the
requested and maximum size of a limit violation).

## Direction

The workspace is being reorganised without behaviour changes. Each step keeps worker
traces byte-identical and re-runs the performance gates where it touches hot code.

1. **Foundation.** One `Step` type crossing crate boundaries, extended with
   backpressure. A neutral container vocabulary in core: `Track`, `Packet` (timing as
   ticks in the track time base, `RandomAccess::{Sync, NotSync, Unknown}`, trim,
   configuration and clock epochs), `SeekRequest`, `SeekReceipt` and a neutral colour
   description (ITU-T H.273 codes). One error type with typed detail. A shared
   `triergon-bitstream` crate for bit reading, exp-Golomb, RBSP unescaping, CABAC
   and CRCs.
2. **Formats.** The Matroska package becomes `triergon-matroska`. Matroska, ISO BMFF
   and MPEG-TS implement one `ContainerReader` trait
   (`provide`/`step`/`request_seek`/`tracks`), with one generic blocking adapter.
   Box walking and track classification move out of playback into the readers.
   Reference-compatible behaviour moves into named `compat` policies; comparison
   projections move behind a non-default `reference-trace` feature.
3. **Codecs.** A `triergon-codec` crate with `VideoDecoder` and `AudioDecoder`
   traits, shared `Picture`, `AudioFrame` and a documented channel order. Codecs
   allocate through core budgets. Large decoder files split by stage.
4. **Composition.** A `triergon` facade with an `AnyReader` enum and one Cargo
   feature per format and codec. Playback runs over `AnyReader`; HLS and codec
   bridges become separate crates. Decode sessions join playback; the capture
   encoder splits into a muxer in `triergon-mpegts`, an MPEG-1 encoder crate and a
   shared pixel-conversion crate.

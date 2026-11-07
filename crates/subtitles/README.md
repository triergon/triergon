# triergon-subtitles

Subtitle ingestion and decoding for demuxed packets: text cue extraction with
explicit charset conversion, and bitmap decoding of DVB, PGS and DVD subpictures
into indexed planes with their palettes. It is a codec-layer crate that depends
only on [`triergon-core`](../core) for budgets, `Bytes`, work accounting and exact
time. It does not demux containers, render text glyphs, scale or composite
bitmaps, or schedule presentation; those belong to the caller.

## Usage

Every operation follows the same cooperative model. Memory is charged to a
shared `Budget`, each `step`/`poll` spends a caller `WorkBudget` and checks an
`AtomicBool` for cancellation, and output is immutable `Bytes` that stay valid
after the decoder is reset or dropped. Starvation (`NeedInput`/`NeedPacket`),
work exhaustion (`Yield`) and output pressure (`NeedOutput`) are distinct,
resumable states. Rejected input is handed back unchanged.

```rust
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, SourceId, Time, WorkBudget};
use triergon_subtitles::text::{Encoding, Event, Extractor, Format, Input, Limits, Status, Timing};

let pool = Budget::new(1 << 20)?;
let cancel = AtomicBool::new(false);
let srt = b"1\n00:00:01,000 --> 00:00:02,500\nHello\n\n";
let input = Input {
    format: Format::SubRip,
    encoding: Encoding::Utf8, // stated by the caller, never guessed
    payload: Bytes::copy_from_slice(&pool, srt)?,
    codec_id: None,
    codec_private: None,
    charset_label: None,
    addition: None,
    timing: Timing::unknown(), // a whole document carries its own cue times
    source_id: SourceId(1),
    generation: 0,
    sequence: 0,
    configuration_epoch: 0,
    container_position: None,
};
let mut extractor = Extractor::new(&pool, Limits::default(), 0);
extractor.provide(input).map_err(|rejected| rejected.error)?; // one complete document or sample
extractor.finish_input();
let mut cues = Vec::new();
loop {
    match extractor.step(&mut WorkBudget::new(4096), &cancel)? {
        Status::Event(Event::Cue(cue)) => cues.push(cue),
        Status::Event(Event::Metadata(_)) | Status::Yield => {}
        Status::NeedOutput => { /* release held output, then step again */ }
        Status::NeedInput | Status::Finished => break,
    }
}
assert_eq!(cues.len(), 1);
assert_eq!(cues[0].start, Some(Time::new(1, 1)?));
assert_eq!(cues[0].end, Some(Time::new(5, 2)?));
assert_eq!(cues[0].decoded_utf8.as_deref(), Some(&b"Hello"[..]));
# Ok::<(), Box<dyn std::error::Error>>(())
```

Bitmap decoders follow the same shape: `pgs::DisplayDecoder::push(packet, clock,
Envelope::Packet, &cancel)` then `poll`/`take_output` for each `DisplaySet`;
`dvb::Decoder::new_configured(..)` then `provide(&DvbPacket)` and `step` for each
`DisplayEvent`; `dvd::SpuAssembly` then `dvd::TraditionalSpuJob` for each
`TraditionalSubpicture`.

| Module | Entry points |
| --- | --- |
| `text` | `Extractor`, `charset::CharsetJob`, `tx3g_config::ConfigJob`, `tx3g_semantics::SemanticJob` |
| `dvb` | `Decoder`, `SourceConfiguration`, `Raster` |
| `pgs` | `DisplayDecoder`, `IndexedDisplayDecoder`, `SegmentIter`, `RasterJob` |
| `dvd` | `SpuAssembly`, `ConfigJob`, `TraditionalSpuJob`, `ProgramJob`, `DvdRasterJob`, `HdSpuJob`, `HdReceiverJob`, `HdFragmentJob` |
| `context` | `Builder`: track header, metadata and attachment bytes handed to a renderer |
| `compat::dvb` | `PesParser`, `Publication`, `Palette`, `GeometryPolicy` (receiver view of `dvb::Decoder`) |
| `compat::pgs` | `RecoveryDecoder`, `IndexedRecoveryDecoder`, `ReceiverDecoder`, `ColourMatrix`, `ArgbView` |
| `compat::dvd` | `CropJob`, `CroppedView`, `resolve_palette`, `Presentation`, `hd_argb` |
| `compat::charset` | `AssPacketJob`, `receiver_charset` |
| `compat::mov_text` | `Decoder`: tx3g sample to ASS event |

## Supported

**Source and receiver views.** Each format has a default *source* operation
that follows the specification strictly and keeps every original byte, clock and
palette fact. The `compat` modules, and the `Receiver` selectors that enable them
(`dvb::ReceiverView::Receiver`, `dvd::ControlPolicy::Receiver`,
`dvd::HdPolicy::Receiver`), reproduce the *receiver*: the output of the pinned
FFmpeg build (git `d32b387f` on the 9.0 branch), including its tolerances. Fields
and accessors named `receiver_*` on the source types carry that view beside the
source facts. The two never share or rewrite each other's output.

**Comparison-only projections.** The non-default `reference-trace` feature adds
`reference_trace`: the receiver's profile labels and the reference revision, the
DVB receiver's packet refusal (`dvb::Decoder::receiver_failure`), the PGS
`Projector` of strict display sets onto FFmpeg's API rectangles, and the text
`carriage` session that reproduces FFmpeg's demuxer packets. They exist so tests
can compare them with recorded FFmpeg output, and are absent from default builds.

### Text

| Format | `text::Format` | What is decoded | What is passed through |
| --- | --- | --- | --- |
| SubRip | `SubRip` | Cue numbers, exact times, multi-line text, CRLF/LF/CR framing | Raw cue byte ranges |
| WebVTT file | `WebVtt` | Header, cue identifiers, times, settings, payload; NOTE/STYLE/REGION blocks in order | Settings and payload as raw text (no CSS, no cue-text parsing) |
| WebVTT in Matroska | `MatroskaWebVtt` | Block-addition fields: settings, identifier, comments | `CodecPrivate`, unknown additions, block-relative inline times |
| tx3g / mov_text | `Tx3g` | Text length, UTF-8 or UTF-16BE text; optional style/effect resolution and FFmpeg-compatible ASS output (below) | Modifier boxes in original order |
| UTF-8 packet | `Utf8Packet` | Strict UTF-8 validation | The payload itself |
| ASS/SSA | `OpaqueAss` | Nothing: no syntax validation or reordering | Packet, header, charset label and timing |

tx3g has three additional operations. `tx3g_config::ConfigJob` parses the
sample-description defaults and font table. `tx3g_semantics::SemanticJob`
resolves all defined sample modifiers into style runs, highlight, karaoke,
hyperlink, blink, wrap, scroll and disparity intents; see
[TX3G-SEMANTICS.md](src/text/TX3G-SEMANTICS.md). `compat::mov_text::Decoder`
converts a sample to the ASS event and header FFmpeg's mov_text decoder emits.

**Charsets.** Selected explicitly with `text::Encoding` or a label resolver;
there is no detection from content, language or file name.

| Family | Profiles |
| --- | --- |
| Unicode | UTF-8, UTF-16 with BOM, UTF-16BE, UTF-16LE, tx3g in-band (UTF-8 or FEFF UTF-16BE) |
| Single-byte | ASCII, ISO-8859-1, Windows-1251, Windows-1252 (`Charset::from_label`) |
| Multibyte | GBK/CP936, Big5, Shift-JIS, EUC-KR, Windows-932/949/950 (`MultibyteCharset::from_label`) |
| FFmpeg/glibc labels | `compat::charset::receiver_charset` and `receiver_charset_with_glibc_euc_kr` map ASS charset labels to the repertoire the reference build accepts |

Mapping tables, sources and licences are listed in
[MULTIBYTE-CONTRACT.md](src/text/MULTIBYTE-CONTRACT.md). Undefined or unmapped
bytes are errors; nothing is replaced with U+FFFD.

### Bitmap

| Format | Decoded | Output |
| --- | --- | --- |
| DVB (EN 300 743) | DDS, PCS, RCS, CDS and method-0 (pixel) ODS; 2/4/8-bit interlaced RLE with map tables; default CLUTs; page/region/CLUT/object state per epoch | `DisplayEvent` with present/update/clear action, PTS, timeout-based expiry, indexed region planes, Y/Cr/Cb/T palette facts, and `SOURCE_PROJECTION` / `STUDIO_UNREPAIRED_PROJECTION` RGBA views |
| PGS (Blu-ray) | PCS, PDS, WDS, ODS, END in packet or SUP framing; fragmented objects; palette updates; object cache; acquisition, epoch and forced flags | `DisplaySet` with index planes, Y/Cr/Cb/alpha palette, crop/window geometry; `IndexedDisplayDecoder` adds on-demand ARGB |
| DVD SPU / VobSub | SPU reassembly from arbitrary fragments, control program (start, forced start, stop, palette, alpha, area, field offsets), interlaced 2-bit RLE, VobSub `CodecPrivate` (size, palette, custom colours) | Index plane, control facts with 90 kHz dates, native crop and ARGB with a caller-supplied palette |
| Extended DVD SPU | 8-bit extended subpictures (`Hd*` types), including cross-packet palette context and fragmented physical packets | Index plane and ARGB under the selected policy |

## Behaviour and limits

- **Input.** Text operations take one complete document or demuxed sample per
  `provide`. Bitmap decoders take complete packets; PES/TS reassembly is upstream
  (`compat::dvb::PesParser` is an optional FFmpeg-compatible PES-to-segment step).
  An empty packet is an event, not end of stream; only `finish_input` ends input.
- **Bounds.** Every operation takes a finite `Limits` struct (input bytes, cues,
  modifiers, planes, objects, palettes). Exceeding one returns `Error::Limit`;
  a valid but oversized input is an explicit failure, never silently truncated.
- **Errors.** Malformed syntax and resource failures are sticky until `reset`.
  Cancellation is resumable in the text operations; the bitmap decoders treat it
  as a failure that needs `reset`. A failed display set publishes nothing.
- **Time.** Raw PTS/DTS/duration ticks and their clock are kept next to exact
  rational `Time` values. Unknown time stays `None`; no duration is inferred from
  the next packet and nothing is rounded to microseconds except in named native
  fields.
- **Text normalisation.** `decoded_utf8` is a strict character conversion only:
  no Unicode, line-ending, whitespace, entity or markup rewriting. A UTF-8 BOM is
  framing in a document and content in a demuxed packet.
- **Colour.** Palettes keep the encoded Y/Cr/Cb/T or Y/Cr/Cb/alpha values. RGBA
  conversions are separately named projections; the source data is never
  overwritten by them.
- **DVB objects.** `ObjectRetention::CurrentDisplay` (default) uses ODS from the
  current display set; `EpochExtension` also replays earlier objects in the
  epoch. The receiver view adds the reference's region ordering, automatic
  palettes without a CDS, non-modifying-colour handling and EOD tolerance.
- **Not handled.** DVB character-coded and progressive objects, depth-reduction
  modes and disparity; DVD colour-change commands (retained as raw bytes);
  glyph rendering, ASS styling and font loading; WebVTT browser recovery and CSS.

The text profile names, default limits and timing rules are in
[src/text/README.md](src/text/README.md).

## Conformance

| Format | Reference |
| --- | --- |
| DVB | ETSI EN 300 743 V1.6.1 §§5.1, 7.2.1–7.2.6, 8, 10 |
| tx3g | ETSI TS 126 245 V18.0.0 §§5.1–5.2, 5.6–5.8, 5.15–5.18 |
| WebVTT | W3C WebVTT Candidate Recommendation Draft §§2.2, 4.1, 6.1, 6.3 |
| Matroska carriage | RFC 9559; Matroska codec mapping Internet-Draft §§3.5.11–3.5.12, 5.2–5.4 |
| Unicode | RFC 3629 (UTF-8), RFC 2781 (UTF-16) |
| PGS, DVD SPU | No normative specification is available; behaviour follows public documentation and the FFmpeg reference, with no conformance claim |

The implementation is original safe Rust. Tests use independently authored
fixtures whose expected index planes, RGBA and cue values were specified from
the standards, not produced by this code: `tests/text-fixtures/` (with a
separate Python oracle), `tests/fixtures/`, `tests/state-fixtures/` and the
DVB regression sets. Receiver views are checked against recorded output of the
pinned FFmpeg build. Every operation is also exercised at several work quanta,
under allocation pressure and with cancellation. Gates and the reference build
are described in [docs/quality.md](../../docs/quality.md); current results are
in [docs/status.md](../../docs/status.md).

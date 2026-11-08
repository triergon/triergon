# triergon-subtitles

Subtitle ingestion and decoding for demuxed packets: text cue extraction with
explicit charset conversion; bitmap decoding of DVB, PGS and DVD subpictures into
indexed planes with their palettes; CEA-608/708 closed captions taken from MPEG-2,
H.264 and HEVC video and decoded to timed text; and DVB teletext pages. It is a
codec-layer crate that depends only on [`triergon-core`](../core) for budgets,
`Bytes`, work accounting and exact time, and on
[`triergon-bitstream`](../bitstream) for RBSP reading. It does not demux
containers (apart from the VobSub `.idx`/`.sub` pair and DVD program streams,
which only carry subtitles), render glyphs, scale or composite bitmaps, or
schedule presentation; those belong to the caller.

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

The FFmpeg-compatible receivers in `compat` take one codec packet per call and
return the subtitle FFmpeg's decoder returns for it, if any:
`compat::dvb::ReceiverDecoder::decode`, `compat::dvd::Decoder`,
`compat::pgs::ReceiverDecoder`, `compat::cc::Decoder::decode` (fed by
`compat::cc::CaptionExtractor` from video access units) and
`compat::teletext::Decoder::decode`. The DVB and DVD receivers charge their work
before doing it and return `Status::Yield` when the budget runs out; the caller
decodes the same packet again, and the work already granted counts towards it,
so a packet completes after enough calls. A DVB receiver rectangle carries its
region depth's `palette`, as FFmpeg reports it, and `colours`, the colour of
every pixel value: a region recomposed at a smaller depth without a fill keeps
larger pixel values, which read as zero, or under the automatic palette as the
level it ranks them at. The specification decoders keep the source
facts: `pgs::DisplayDecoder::push(packet, clock, Envelope::Packet, &cancel)` then
`poll`/`take_output` for each `DisplaySet`; `dvb::Decoder::new_configured(..)` then
`provide(&DvbPacket)` and `step` for each `DisplayEvent`; `dvd::SpuAssembly`, then
`dvd::ProgramJob` and `dvd::DvdRasterJob`; `teletext::Assembler` and
`teletext::Display`; `cc::line21` and `cc::dtvcc`.

| Module | Entry points |
| --- | --- |
| `text` | `Extractor`, `charset::CharsetJob`, `tx3g_config::ConfigJob`, `tx3g_semantics::SemanticJob` |
| `dvb` | `Decoder`, `SourceConfiguration`, `Raster` |
| `pgs` | `DisplayDecoder`, `IndexedDisplayDecoder`, `SegmentIter`, `RasterJob` |
| `dvd` | `SpuAssembly`, `ConfigJob`, `ProgramJob`, `DvdRasterJob`, `HdSpuJob`; `vobsub` (`.idx` text), `program_stream` (MPEG-2 program streams) |
| `cc` | `cc_data`, `line21` (CEA-608), `dtvcc` (CEA-708 packets and service blocks), `user_data` (A/53, SCTE 20, DVD captions), `video` (MPEG-2 pictures, H.264/HEVC SEI) |
| `teletext` | `DataField`, `Packet`, `Assembler`, `Presentation`, `Display`, `charset` |
| `context` | `Builder`: track header, metadata and attachment bytes handed to a renderer |
| `compat::dvb` | `ReceiverDecoder`, `Status`, `Options`, `AutomaticPalette`; `PesParser` (PES data field to codec packets) |
| `compat::pgs` | `ReceiverDecoder`, `RecoveryDecoder`, `IndexedRecoveryDecoder`, `Refusal`, `ColourMatrix`, `ArgbView` |
| `compat::dvd` | `Decoder`, `Settings`, `ifo_palette`, `VobSubIndex`, `SubpictureParser`, `mp4_configuration` |
| `compat::cc` | `Decoder`, `Options`, `CaptionExtractor`, `SccPackets` |
| `compat::teletext` | `Decoder`, `Options`, `Format`, `PageSelection` |
| `compat::charset` | `AssPacketJob`, `receiver_charset` |
| `compat::mov_text` | `Decoder`: tx3g sample to ASS event |

## Supported

**Source and receiver views.** Each format has a *source* operation that follows
the specification strictly and keeps every original byte, clock and palette fact.
One receiver per format in `compat` reproduces the output of the pinned FFmpeg
build (git `d32b387f` on the 9.0 branch; teletext with libzvbi 0.2.46), including
its tolerances of malformed input. The two never share or rewrite each other's
output.

**Comparison-only projections.** The non-default `reference-trace` feature adds
`reference_trace`: the PGS `Projector` of strict display sets onto FFmpeg's API
rectangles, and the text `carriage` session that reproduces FFmpeg's demuxer
packets. They exist so tests can compare them with recorded FFmpeg output, and are
absent from default builds.

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
| DVB (EN 300 743) | DDS, PCS, RCS, CDS and method-0 (pixel) ODS; 2/4/8-bit interlaced RLE with map tables; default CLUTs; page/region/CLUT/object state per epoch | Source: `DisplayEvent` with present/update/clear action, PTS, expiry, indexed region planes and Y/Cr/Cb/T palette facts. Receiver: FFmpeg's subtitles, with its display window, region order, automatic palette and `compute_edt`, `compute_clut`, `dvb_substream` |
| PGS (Blu-ray) | PCS, PDS, WDS, ODS, END in packet or SUP framing; fragmented objects; palette updates; object cache; acquisition, epoch and forced flags | Source: `DisplaySet` with index planes, Y/Cr/Cb/alpha palette, crop/window geometry. Receiver: every packet FFmpeg decodes, malformed ones included, with its refusals and `forced_subs_only` |
| DVD SPU / VobSub | SPU reassembly, control program (start, forced start, stop, palette, alpha, area, field offsets), interlaced 2-bit and extended 8-bit RLE; `.idx`/`.sub` pairs, program streams, Matroska and MP4 configuration | Source: index plane and control facts with 90 kHz dates. Receiver: FFmpeg's crop, colours carried between units, ARGB from the idx, private-data, `palette` or IFO palette, and `forced_subs_only` |

### Closed captions and teletext

| Format | Decoded | Output |
| --- | --- | --- |
| CEA-608/708 in video | `cc_data` from ATSC A/53 (`GA94`) and DVD caption user data in MPEG-2 pictures, SCTE 20, Dish Network user data, and A/53 SEI in H.264 and HEVC; CEA-608 control codes, PACs, mid-row codes, special and extended characters, XDS; CEA-708 caption channel packets and service blocks | Receiver: the caption data of each frame in FFmpeg's output order and times (`CaptionExtractor`), decoded to the ASS events of its `ccaption` decoder with `real_time` and `data_field`; SCC files read as its `scc` demuxer does. CEA-708 services are surfaced but, as in FFmpeg, not decoded to text |
| DVB teletext (EN 300 472, EN 300 706) | Data units, Hamming 8/4 and 24/18, parity, headers and rows, parallel and serial magazines, subpages, national option subsets and G0/G1/G2/G3 sets, X/26 enhancements and local objects (active, adaptive, passive), X/28 and M/29 designations and colour tables | Receiver: libzvbi's subtitle pages as FFmpeg's `libzvbi_teletextdec` presents them in its `text` and `ass` formats, with its page selection and options |

## Behaviour and limits

- **Input.** Text operations take one complete document or demuxed sample per
  `provide`. Bitmap, caption and teletext decoders take complete packets; PES/TS
  reassembly is upstream (`triergon-mpegts`; `compat::dvb::PesParser` cuts a DVB
  PES data field into codec packets as FFmpeg's demuxer does).
  An empty packet is an event, not end of stream; only `finish_input` ends input.
- **Bounds.** Every operation takes a finite `Limits` struct (input bytes, cues,
  modifiers, planes, objects, palettes). Exceeding one returns `Error::Limit`;
  a valid but oversized input is an explicit failure, never silently truncated.
- **Errors.** In the source operations malformed syntax and resource failures are
  sticky until `reset`; a failed display set publishes nothing. The receivers
  recover as the reference does: a packet it refuses is refused and the next one
  is decoded. Cancellation is resumable in the text operations and receivers.
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
  epoch.
- **Not handled.** DVB character-coded and progressive objects, depth-reduction
  modes and disparity (FFmpeg drops them too, and the receiver does as it does);
  DVD colour-change commands (retained as raw bytes); teletext's `bitmap` format,
  which renders libzvbi's own font, and objects from POP and GPOP pages (pages invoking them are presented at Level 1, as the reference presents them); CEA-708 text; glyph rendering, ASS styling and
  font loading; WebVTT browser recovery and CSS.

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
| Teletext | ETSI EN 300 472 (data units); ETSI EN 300 706 (packets, pages, presentation) |
| Closed captions | CEA-608 (line 21 data); CEA-708 (caption channel packets and service blocks); ATSC A/53 Part 4 (`GA94` caption data); SCTE 20 |
| PGS, DVD SPU, Dish captions | No normative specification is available; behaviour follows public documentation and the FFmpeg reference, with no conformance claim |

The implementation is original safe Rust. Tests use independently authored
fixtures whose expected index planes, RGBA and cue values were specified from
the standards, not produced by this code: `tests/text-fixtures/` (with a
separate Python oracle), `tests/fixtures/`, `tests/state-fixtures/` and the
DVB regression sets. Receivers are checked against recorded output of the
pinned FFmpeg build, and case by case against the build itself through the
subtitle runner (`crates/subtitle-worker`, `harness/features/subtitle-*`). Every operation is also exercised at several work quanta,
under allocation pressure and with cancellation. Gates and the reference build
are described in [docs/quality.md](../../docs/quality.md); current results are
in [docs/status.md](../../docs/status.md).

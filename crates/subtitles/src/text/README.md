# Text ingestion profiles

Exact rules for `triergon_subtitles::text`. The crate overview is in
[../../README.md](../../README.md).

## Profiles

| Profile | `Format` | Rules |
| --- | --- | --- |
| `SubRipStrict1` | `SubRip` | Numeric cue number; `hh:mm:ss,mmm --> hh:mm:ss,mmm` with two or more hour digits; multi-line text; CRLF, LF or CR line ends. A declared strict profile, not a universal SRT grammar. |
| `WebVttSourceStrict1` | `WebVtt` | UTF-8 with optional BOM; exact cue times; identifier, settings, payload, NOTE, STYLE and REGION kept in source order. Malformed input and zero or negative intervals are errors (no browser-style recovery). |
| `Tx3gCarriage1` | `Tx3g` | Big-endian u16 text length (the 2048-byte recommendation is not enforced); UTF-8 or FEFF-prefixed UTF-16BE; modifier boxes kept whole and in order. Byte-reversed UTF-16 is rejected. |
| ISO-box tx3g | `Tx3g` | Selected with `Extractor::tx3g_iso_boxes`: additionally reads ISO size-0 (to end of sample), size-1 (64-bit) and `uuid` modifier boxes. `Extractor::new` reports them as unsupported. |
| `Utf8Packet1` | `Utf8Packet` | Strict UTF-8; the payload is aliased, not copied; no document framing. |
| `MatroskaWebVttCarriage1` | `MatroskaWebVtt` | Raw cue text; BlockAddition ID 1 split into settings, identifier and comments; other additions and `CodecPrivate` kept opaque. Inline timestamps stay block-relative. Not WebM carriage. |
| `OpaqueAss1` | `OpaqueAss` | Packet, configuration, charset label and timing carried unchanged. No ASS validation and no charset assertion. |

Every `Cue` keeps its complete `Input`, the raw and text byte ranges, and
`decoded_utf8` when the encoding is known. Already published records survive a
later framing error. Whole-input character validation happens before the first
cue is published.

## Encodings

`Encoding::Utf8` rejects overlong forms, surrogates and values above U+10FFFF.
`Utf16Bom` requires a BOM; `Utf16Be`/`Utf16Le` treat a leading FEFF as content.
`Ascii`, `Latin1`, `Windows1251`, `Windows1252` and `Multibyte(_)` are accepted
by SubRip only; WebVTT and `Utf8Packet` require UTF-8. Unknown document
encodings return `Error::Unsupported`.

`charset::CharsetJob` converts a whole byte stream with an explicit `Charset`.
It validates and counts the input first, then writes into exactly sized storage.
Single-byte tables come from the Unicode/Microsoft mapping files; multibyte
profiles are in [MULTIBYTE-CONTRACT.md](MULTIBYTE-CONTRACT.md).

## tx3g configuration

`tx3g_config::ConfigJob::new` parses an explicitly identified layout
(`ConfigLayout`): a full `SampleEntry` body or the 30-byte defaults record.
It keeps flags, signed justification and box geometry, RGBA, the default style
record and the font table as charged ranges; unknown child boxes stay raw.
`ConfigJob::new_unicode_fonts` also accepts FEFF-prefixed UTF-16BE font names.
Absent values stay absent; no default font, style or geometry is invented.

## Timing

`Timing::from_ticks(RawClock)` keeps the clock ratio and optional PTS, DTS and
duration ticks and derives exact rational `Time` values. End time exists only
when start and duration are both known. Unknown time, zero duration and negative
PTS are distinct.

## Carriage session

`carriage::Session` publishes an immutable `Snapshot` per input with the
FFmpeg-compatible packet view (`Snapshot::native_view()` returns
`NativeView::D32`): millisecond document timing
that keeps zero or negative intervals, and the reference's UTF-16 SubRip
line-entry byte behaviour. `recover_pending` returns inputs after cancellation
or a resource failure.

## Default limits

`Limits::default()`: input 4 MiB; configuration and BlockAddition 1 MiB each;
labels 256 bytes; source line 64 KiB; 4096 cues per document; 4096 modifiers per
sample; decoded output 4 MiB per cue. Limits are caller policy, not syntax.

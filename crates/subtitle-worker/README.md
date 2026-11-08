# subtitle-worker

Subtitle decode measurement worker for the harness. It decodes one subtitle stream
through the FFmpeg-compatible receivers of [triergon-subtitles](../subtitles/README.md),
as the subtitles build of the reference worker (`harness/reference_subtitles.h`) does
with FFmpeg's `pgssub`, `dvbsub`, `dvdsub`, `libzvbi_teletextdec` and `ccaption`
decoders. It is tooling (`publish = false`), not part of the library.

## Command line

```text
subtitle-worker sub-fold   FILE KIND [ARG...]
subtitle-worker sub-bench  FILE ITERATIONS KIND [ARG...]
subtitle-worker sub-memory FILE KIND [ARG...]
subtitle-worker sub-trace  FILE KIND [ARG...]
subtitle-worker sub-input  FILE KIND [ARG...]
subtitle-capsule-worker pgs-{fold,memory} CAPSULE | pgs-bench CAPSULE ITERATIONS
subtitle-capsule-worker subtitle-text-{fold,memory} FILE KIND [ENCODING]
subtitle-capsule-worker subtitle-text-bench FILE ITERATIONS KIND [ENCODING]
```

| KIND | Input | Receiver |
|---|---|---|
| `pgs` | SUP, Matroska, MPEG-TS, M2TS | `compat::pgs::ReceiverDecoder` |
| `dvb` | MPEG-TS, M2TS, Matroska | `compat::dvb::ReceiverDecoder` |
| `dvd` | VobSub `.idx`/`.sub`, program streams (`.vob` or a bare `.sub`), Matroska `S_VOBSUB`, MP4 `mp4s` | `compat::dvd::Decoder` |
| `teletext` | MPEG-TS | `compat::teletext::Decoder` |
| `cc` | MPEG-2 video as an elementary stream or in MPEG-TS; H.264 and HEVC in MPEG-TS, Matroska or MP4; SCC files | `compat::cc::CaptionExtractor`, `compat::cc::Decoder` |

`ARG` is `stream=N` (the Nth stream of that kind, from 0), `opt:NAME=VALUE` (an
option of the reference decoder with the same meaning; values the reference would
refuse are refused) or, for `cc`, `source=video` (caption data from the video
stream, the default) or `source=packets` (608 byte pairs, such as an SCC file).

Preparation is outside the timed interval and the memory window: reading the file
through the Triergon container readers, selecting the stream, copying its packets
in demux order, for captions extracting the caption data of every picture in
output order, and creating the pool the decoder is charged to. The operation creates the decoder, decodes every prepared packet with
one call each, drains it, folds every output subtitle (`src/sink.rs`) and frees
everything.

The fold, bench and memory modes run through
[triergon-harness-runner](../harness-runner/README.md), which fails an operation that
leaves storage charged to the pool, and print the harness protocol's one JSON line:
`packets` counts output subtitles, `bytes` folded bytes and `digest` folds every
subtitle's packet index, times, and per rectangle its geometry, flags, palette,
pixels, text and ASS line. A decoder error, such as memory the pool refuses, ends
the operation: the line counts what was folded before it and carries the error as
its `refusal`, with the packet it stopped at as its `cause`. The bench mode adds the
elapsed time of the timed loop and the memory mode brackets one operation with the
allocation-window markers. The
trace mode prints the same values as lines, identical to the reference's trace, so a
failing case can be diffed; the input mode prints the prepared packets. The features
are `harness/features/subtitle-{pgs,dvb,dvd,cc,teletext}/`.

## Recorded PGS cases and text subtitles

The `subtitle-capsule-worker` binary of this package runs these modes, so that the
`sub-` operations of `subtitle-worker` compile as they would without them: a second
caller of the PGS receiver changes how its code is inlined. They pair with the
formats build of the reference worker (`harness/reference_pgs.h`,
`harness/reference_subtitle_text.h`) and fold their own byte transcripts; a decoder
refusal is a marker in the transcript.

The `pgs-` modes decode PGS packets through
`triergon_subtitles::compat::pgs::ReceiverDecoder`. The input is a case capsule
(`SMBMO001`): the packet time base, the forced-only option and up to 64 records,
each a complete PGS packet with its raw PTS and DTS or a hard reset that reopens
the decoder. Every published subtitle is folded with its packet, consumed bytes,
PTS in microseconds, display window and canvas, and per rectangle the geometry,
forced flag, stride, index plane, palette and the palette value of every pixel. The
fold is the digest of the byte transcript stored as `expected.bin` beside each case
in `crates/subtitles/tests/native-compatibility-gold/`, and a test checks all 229
in work slices of any size. The corpus is `harness/features/subtitle-pgs-capsules/`.

The `subtitle-text-` modes take KIND `packets`, `srt` or `vtt`. `packets` reads a
session capsule (`TRSTXT01`): a codec byte (1 tx3g/mov_text, 2 ASS), an optional
charset label, then decoder opens (packet time base, frame size, extradata),
packets (optional PTS and duration) and flushes. tx3g samples go through
`compat::mov_text::Decoder`; ASS packets through the opaque ASS extractor, or with
a label through `compat::charset::AssPacketJob` with the glibc receiver
repertoire. `srt` and `vtt` read one SubRip or WebVTT document through
`text::Extractor` with ENCODING `utf8` (default), `utf16-bom`, `utf16be` or
`utf16le`, folding every cue's start, duration, text and WebVTT identifier and
settings. The capsules are written by `harness/subtitle_corpus.py`; the corpus is
`harness/features/subtitle-text/`, and a test checks every case against the fold the
reference recorded, except those whose difference the corpus names.

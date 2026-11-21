# subtitle-worker

Subtitle decode measurement worker for the harness. It decodes one subtitle stream
through the FFmpeg-compatible receivers of [triergon-subtitles](../subtitles/README.md),
as the subtitles build of the reference worker (`harness/reference_subtitles.h`) does
with FFmpeg's `pgssub`, `dvbsub`, `dvdsub`, `libzvbi_teletextdec` and `cc_dec`
decoders, and reads text subtitles as the formats build
(`harness/reference_subtitle_text.h`) does with its `mov_text` and `ass` decoders
and `srt` and `webvtt` demuxers. It is tooling (`publish = false`), not part of the
library.

## Command line

```text
subtitle-worker sub-fold   FILE KIND [ARG...]
subtitle-worker sub-bench  FILE ITERATIONS KIND [ARG...]
subtitle-worker sub-memory FILE KIND [ARG...]
subtitle-worker sub-trace  FILE KIND [ARG...]
subtitle-worker sub-input  FILE KIND [ARG...]
subtitle-worker subtitle-text-{fold,memory} FILE TEXT
subtitle-worker subtitle-text-bench FILE ITERATIONS TEXT
subtitle-capsule-worker pgs-{fold,memory} CAPSULE | pgs-bench CAPSULE ITERATIONS
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
output order (H.264 and HEVC in Matroska or MP4 with the parameter sets of the
track's configuration before the first packet's), and creating the pool the decoder is charged to. The operation creates the decoder, decodes every prepared packet with
one call each, drains it, folds every output subtitle (`src/sink.rs`) and frees
everything.

The fold, bench and memory modes run through
[triergon-harness-runner](../harness-runner/README.md), which fails an operation that
leaves storage charged to the pool, and print the harness protocol's one JSON line:
`packets` counts output subtitles, `bytes` folded bytes and `digest` folds every
subtitle's packet index, times, and per rectangle its geometry, flags, palette,
pixels, text and ASS line. A decoder error, such as memory the pool refuses, ends
the operation: the line counts what was folded before it and carries the error as
its `refusal`, with the packet it stopped at as its `cause`. The fold, bench and
memory modes also print `input_digest`, worked out outside the operation from the
prepared packets as the reference works it out: their count, then per packet whether it has a PTS,
the PTS and the duration, in order, folded word by word (d = 31d + w, wrapping); the
`subtitle-cc` contract compares it, so the prepared packets' order and times must
match the reference's exactly. PGS works it out from its prepared packets' clocks
without the input mode's view, which copies every payload, so a timed or measured
run holds its input once. The bench mode adds the
elapsed time of the timed loop and the memory mode brackets one operation with the
allocation-window markers. The
trace mode prints the same values as lines, identical to the reference's trace, so a
failing case can be diffed; the input mode prints the prepared packets. The features
are `harness/features/subtitle-{pgs,dvb,dvd,cc,teletext}/`.

## Text subtitles

The `subtitle-text-` modes take TEXT `packets`, `srt` or `vtt` and fold their own
byte transcript, as the formats build of the reference worker folds it. `packets`
reads a session capsule (`TRSTXT01`): a codec byte (1 tx3g/mov_text, 2 ASS), an
optional charset label, then decoder opens (packet time base, frame size,
extradata), packets (optional PTS and duration) and flushes. tx3g samples go
through `compat::mov_text::Decoder` and ASS packets through `compat::ass::Decoder`,
with the capsule's label if it has one. `srt` and `vtt` read one SubRip or WebVTT
document through `compat::document::Reader`, folding every cue's start, duration,
text and WebVTT identifier and settings. The inputs are written by
`harness/subtitle_corpus.py`; the corpus is `harness/features/subtitle-text/`, and a
test checks every case against the fold the reference recorded, in work slices of
any size.

## Recorded PGS cases

The `subtitle-capsule-worker` binary of this package runs the `pgs-` modes, so that
the `sub-` operations of `subtitle-worker` compile as they would without them: a
second caller of the PGS receiver changes how its code is inlined. They pair with
the formats build of the reference worker (`harness/reference_pgs.h`) and fold
their own byte transcript; a decoder refusal is a marker in the transcript.

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

## Recording and verifying subtitle fixtures

`cargo xtask subtitle-fixtures verify FEATURE REFERENCE` runs the independent
FFmpeg worker on every authored case in order. It checks the complete manifest,
including input SHA-256s, decoder arguments, packet counts, folded byte counts
and digests. Each worker reads a snapshot of the hashed input and must report
EOF. If the source SHA-256 differs when the worker returns, the command fails.
Use `record` in place of `verify` to write the manifest after all folds succeed.

| Feature | Reference in `target/harness/` | Operation | Input location |
| --- | --- | --- | --- |
| `subtitle-text`, `subtitle-text-host` | `ffmpeg-formats-worker` | `subtitle-text-fold` | Repository fixtures |
| `subtitle-pgs-capsules` | `ffmpeg-formats-worker` | `pgs-fold` | Repository PGS capsules |
| `subtitle-dvb`, `subtitle-teletext` | `ffmpeg-subtitles-worker` | `sub-fold` | `.research/subtitle-corpus` |

For `subtitle-text-host`, pass `--target TRIPLE` for the reference worker's
target. Recording replaces that target's fold in each case and preserves the
others when the input SHA-256 and decoder arguments match their recorded values.
To change those inputs, record every target into a fresh manifest. The recorder
uses an adjacent `.record-lock` file to serialize writes to the same manifest.
The operating system releases the lock when the recorder exits.
`--corpus DIR` selects the input root and `--manifest FILE` selects the
manifest to check or write. The text capsule and broadcast stream authors
continue to generate their input bytes and enumerate their cases in Python.


## Source views

`subtitle-views-worker` compares subtitle source data independently of the
receiver operations. Its prefixes are `text-extract-`, `renderer-context-`,
`charset-`, `charset-packets-`, `tx3g-config-`, `tx3g-semantics-`,
`dvb-source-`, `pgs-segments-`, `vobsub-lines-` and `carriage-`. Append
`fold`, `trace`, `memory` or `bench` and use the trailing arguments the
feature's corpus lists. Trace mode prints the folded values before the work
line. The formats reference publishes its demuxer and decoder views through
`ffmpeg-subtitle-views-worker`; the field-level oracles emit the same trace rows
directly from their inputs.

`cargo xtask subtitle-views fixtures FEATURE` verifies input hashes.
`cargo xtask subtitle-views manifest --record FEATURE` records reference folds
or oracle trace hashes. `cargo xtask subtitle-views fixtures --write-to DIR`
writes authored assets and draft manifests to a separate directory.

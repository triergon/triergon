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

Reading the file, selecting the stream, copying its packets in demux order (for
captions, extracting every picture's caption data in output order) and creating the
decoder's pool happen outside the timed interval and the memory window. The fold, bench
and memory modes run through [triergon-harness-runner](../harness-runner/README.md):
`packets` counts output subtitles, `bytes` folded bytes, and `digest` folds every
subtitle. A decoder error, such as memory the pool refuses, ends the operation with its
`refusal` and the packet it stopped at as its `cause`. The line also carries
`input_digest`, a fold of the prepared packets' count, PTS and durations that must match
the reference's. The trace mode prints the folded values as lines, identical to the
reference's trace; the input mode prints the prepared packets. The features are
`harness/features/subtitle-{pgs,dvb,dvd,cc,teletext}/`.

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
`harness/subtitle_corpus.py`; the corpus is `harness/features/subtitle-text/`.

## Recorded PGS cases

The `subtitle-capsule-worker` binary runs the `pgs-` modes, kept apart so the `sub-`
operations of `subtitle-worker` compile as they would without them. They pair with the
formats build of the reference worker (`harness/reference_pgs.h`) and fold their own
byte transcript; a decoder refusal is a marker in the transcript. The input is a case
capsule (`SMBMO001`): the packet time base, the forced-only option and up to 64 records,
each a complete PGS packet with its raw PTS and DTS or a hard reset that reopens the
decoder. The fold is the digest of the byte transcript stored as `expected.bin` beside
each case in `crates/subtitles/tests/native-compatibility-gold/`. The corpus is
`harness/features/subtitle-pgs-capsules/`.

## Recording and verifying subtitle fixtures

`cargo xtask subtitle-fixtures verify FEATURE REFERENCE` runs the FFmpeg reference
worker on every authored case and checks the complete manifest (input SHA-256s,
decoder arguments, packet and byte counts, digests; every run must reach EOF);
`record` writes the manifest after all folds succeed.

| Feature | Reference in `target/harness/` | Operation | Input location |
| --- | --- | --- | --- |
| `subtitle-text`, `subtitle-text-host` | `ffmpeg-formats-worker` | `subtitle-text-fold` | Repository fixtures |
| `subtitle-pgs-capsules` | `ffmpeg-formats-worker` | `pgs-fold` | Repository PGS capsules |
| `subtitle-dvb`, `subtitle-teletext` | `ffmpeg-subtitles-worker` | `sub-fold` | `.research/subtitle-corpus` |

`subtitle-text-host` takes `--target TRIPLE`, the reference worker's target;
recording replaces that target's fold and keeps the others while the input SHA-256
and decoder arguments match. `--corpus DIR` and `--manifest FILE` select the input
root and the manifest. `harness/subtitle_corpus.py` and the generators in
`harness/subtitles/` author the inputs and enumerate the cases.

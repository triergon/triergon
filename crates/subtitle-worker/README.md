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
in demux order and, for captions, extracting the caption data of every picture in
output order. The operation creates the decoder, decodes every prepared packet with
one call each, drains it, folds every output subtitle (`src/sink.rs`) and frees
everything.

The fold, bench and memory modes print the harness protocol's one JSON line:
`packets` counts output subtitles, `bytes` folded bytes and `digest` folds every
subtitle's packet index, times, and per rectangle its geometry, flags, palette,
pixels, text and ASS line. The bench mode adds the elapsed time of the timed loop
and the memory mode brackets one operation with the allocation-window markers. The
trace mode prints the same values as lines, identical to the reference's trace, so a
failing case can be diffed; the input mode prints the prepared packets. The features
are `harness/features/subtitle-{pgs,dvb,dvd,cc,teletext}/`.

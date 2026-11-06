# HLS segment inputs

Synthetic TS188 media segments for the `hls-segments` contract
(`harness/features/hls-segments/`). Each holds one video stream (AVC or HEVC) and one
audio stream (AAC, AC-3 or E-AC-3). FFmpeg 9.0.2 encoded them from synthetic test
signals; they are committed and pinned by SHA-256 because encoder output is not
reproducible across encoder builds.

| Source | Segments | What they cover |
|---|---|---|
| `vod` | 60 | consecutive segments of six video/audio pairings from a VOD playlist |
| `system-b` | 6 | the same pairings from a second multiplexer layout, including other audio rates |
| `closed-gop-vod` | 4 | entry segments with closed GOPs and other audio rates |
| `allocation-control` | 1 | a control input for allocation checks; not scored |

`corpus.json` lists all 71 with their independently recorded packet count, byte count
and fold; `measured.json` lists the ten that are timed and measured.

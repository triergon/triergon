# playback-worker

Playback measurement worker for the harness. It opens a Matroska file through the
[triergon-playback](../playback/README.md) engine with a 4 KiB read window, discovers
the streams, selects the first video track and produces its AVC or HEVC packets with
native PTS, DTS and duration, either from startup or after a backward seek. The FFmpeg
reference worker in `harness/` performs the same operation. It is tooling
(`publish = false`), not part of the library.

## Command line

```text
playback-worker feed-bench  CAPSULE ITERATIONS
playback-worker feed-fold   CAPSULE
playback-worker feed-memory CAPSULE
playback-worker hls-fold    FILE VIDEO AUDIO
playback-worker hls-bench   FILE ITERATIONS VIDEO AUDIO
playback-worker hls-memory  FILE VIDEO AUDIO
```

A capsule is `SMVCL001`, one operation byte (0 startup, 1 seek after discovery,
2 seek after reading to the end), the seek target in milliseconds as a big-endian
`i64`, then the Matroska file. Every mode prints one JSON line with the packet and
byte counts and an ordered digest of every selected packet's timing and payload;
`feed-bench` adds the elapsed time of the timed loop and `feed-memory` brackets the
operation with the allocation-window markers. The corpus is
`harness/features/matroska-startup-seek/`.

The `hls-fold`, `hls-bench` and `hls-memory` modes accept one TS188 segment
and selected AVC/HEVC video with AAC, AC-3 or E-AC-3 audio. They retain every
access unit through segment EOF, then call `contiguous_feed` for every video
unit and fold its complete payload, audio bytes, timing and both stream tails.
Allocation, copying and release of each contiguous feed are inside the timed
and memory operations, in the order specified by
`harness/features/hls-segments/contract.json`.

```text
playback-worker mp4-range-bench  FILE ITERATIONS [VIDEO_TICKS]
playback-worker mp4-range-fold   FILE [VIDEO_TICKS]
playback-worker mp4-range-memory FILE [VIDEO_TICKS]
```

The range operation reads every AVC and AAC packet of a regular MP4 file. With
a seek timestamp, it reads one packet, seeks backward from the supplied video
timestamp and reads through EOF. Its digest includes the packet order, source
extents, clocks, flags, trim and bytes. `cargo xtask fixtures` writes the
comparison inputs to `.research/mp4-range-fixtures`. The corpus contract is
`harness/features/mp4-range/contract.json`.

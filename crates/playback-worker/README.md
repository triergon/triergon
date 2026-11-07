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
access unit through segment EOF, then fold complete borrowed video spans,
audio bytes, timing and both stream tails in the order specified by
`harness/features/hls-segments/contract.json`.

HLS payloads use the shared ordered polynomial digest, including its grouped
arithmetic for larger spans. Every byte contributes in feed order. The reference adapter uses the same
ordered polynomial expansion.

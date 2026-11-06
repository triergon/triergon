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
```

A capsule is `SMVCL001`, one operation byte (0 startup, 1 seek after discovery,
2 seek after reading to the end), the seek target in milliseconds as a big-endian
`i64`, then the Matroska file. Every mode prints one JSON line with the packet and
byte counts and an ordered digest of every selected packet's timing and payload;
`feed-bench` adds the elapsed time of the timed loop and `feed-memory` brackets the
operation with the allocation-window markers. The corpus is
`harness/features/matroska-startup-seek/`.

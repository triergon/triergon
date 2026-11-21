# playback-worker

Playback measurement worker for the harness (`publish = false`, not part of the
library). Each mode family runs one operation of the
[triergon-playback](../playback/README.md) engine or a container reader on one input;
the FFmpeg reference worker in `harness/` performs the same operation through public
FFmpeg APIs. `playback-worker` carries every mode family below;
`playback-matroska-worker` carries the Matroska families `feed-` and `mkv-audio-` alone,
for the reference build whose only demuxer is Matroska.

## Command line

```text
playback-worker PREFIXfold   FILE ARGS
playback-worker PREFIXbench  FILE ITERATIONS ARGS
playback-worker PREFIXmemory FILE ARGS
playback-worker PREFIXtrace  FILE ARGS
```

The modes and the result line are those of
[triergon-harness-runner](../harness-runner/README.md); `trace`, where a family has
it, prints what the operation folds before the result.

| Prefix | ARGS | Operation | Contract |
|---|---|---|---|
| `feed-` | none; FILE is a capsule | Matroska discovery with a 4 KiB read window; the first video track's AVC or HEVC packets, from startup or after a backward seek | `matroska-startup-seek` |
| `hls-` | VIDEO (`avc`, `hevc`) and AUDIO (`aac`, `ac3`, `eac3`) | one TS188 segment through `contiguous_feed` | `hls-segments` |
| `mp4-range-` | an optional backward seek target in video ticks and optional `opus-roll` policy | regular or fragmented MP4 through the range reader | `mp4-range` |
| `progressive-` | CLOCK (`pts-only`, `pts-dts`) and PROGRESS (`before-eof`, `at-eof`) | a forward-only AVC/AAC TS188 session in 4 KiB windows; `bench` runs 50 sessions | `progressive-ts188` |
| `mkv-audio-` | `aac`, `dolby` or `all` | Matroska discovery and the AAC-LC, AC-3 and E-AC-3 track clocks | `matroska-audio-clocks` |
| `mp4-forward-` | `regular` or `progressive` | `Mp4Open` and `Mp4IndexJob` in the native MOV view, through `Engine::mp4` or a front-`moov` native plan | `mp4-forward` |
| `ts-program-` | `ts188:PID=CARRIAGE,...` or `m2ts192:PID=CARRIAGE,...` | explicit PIDs of program 1 in TS188 or M2TS192; native 90 kHz clocks | `ts-program` |
| `raw-video-` | `avc` or `hevc` | `RawVideoProbe`, `RawVideoSource` and `RawVideoFeed` over forward-only Annex B | `raw-annex-b` |

A capsule is `SMVCL001`, one operation byte (0 startup, 1 seek after discovery,
2 seek after reading to the end), the seek target in milliseconds as a big-endian
`i64`, then the Matroska file. `cargo xtask fixtures` writes the capsules and the
MP4 range inputs under `.research/`. In the `mkv-audio-`, `mp4-forward-`, `ts-program-`
and `raw-video-` families `trace` prints one line per packet.

The [transport worker](../transport-worker/README.md) runs the MPEG-TS PES and
access-unit comparisons.

# playback-worker

Playback measurement worker for the harness. Each mode family runs one operation of
the [triergon-playback](../playback/README.md) engine on a preloaded input; the FFmpeg
reference worker in `harness/` performs the same operation through public FFmpeg APIs.
It is tooling (`publish = false`), not part of the library.

## Command line

```text
playback-worker feed-bench  CAPSULE ITERATIONS
playback-worker feed-fold   CAPSULE
playback-worker feed-memory CAPSULE
playback-worker hls-fold    FILE VIDEO AUDIO
playback-worker hls-bench   FILE ITERATIONS VIDEO AUDIO
playback-worker hls-memory  FILE VIDEO AUDIO
playback-worker {mkv-audio-,mp4-forward-,ts-program-,raw-video-}fold   FILE ARG
playback-worker {mkv-audio-,mp4-forward-,ts-program-,raw-video-}bench  FILE ITERATIONS ARG
playback-worker {mkv-audio-,mp4-forward-,ts-program-,raw-video-}memory FILE ARG
playback-worker {mkv-audio-,mp4-forward-,ts-program-,raw-video-}trace  FILE ARG
```

The `feed-` modes open a Matroska file with a 4 KiB read window, discover the streams,
select the first video track and produce its AVC or HEVC packets with native PTS, DTS
and duration, either from startup or after a backward seek.

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

The profile runners (`src/packet_fold.rs`) fold every selected packet as track, PTS,
DTS, duration, flags, size and every payload byte; an unknown time folds as
`i64::MIN` and an unknown duration as zero. `bench` adds the elapsed time of the timed
loop, `memory` brackets the operation with the allocation-window markers and `trace`
prints one line per packet before the result. The reference side is
`harness/reference_packet_fold.h`.

| Prefix | ARG | Operation | Contract |
|---|---|---|---|
| `mkv-audio-` | `aac`, `dolby` or `all` | Matroska stream discovery in 4 KiB windows; AAC-LC, AC-3 and E-AC-3 tracks through `native_aac_clock_probe` or `native_dolby_clock_probe` over the discovery prefix, then the forward clock; times in nanoseconds | `matroska-audio-clocks` |
| `mp4-forward-` | `regular` or `progressive` | `Mp4Open` and `Mp4IndexJob` in the native MOV view; a seekable file through `Engine::mp4`, or a forward-only front-`moov` file through its native front plan and `Engine::mp4_native_terminal_v1`; every track, times in the track time base | `mp4-forward` |
| `ts-program-` | `ts188:PID=CARRIAGE,...` or `m2ts192:PID=CARRIAGE` | explicit PIDs of program 1 through `Engine::ts_native_program_v1`, or one M2TS192 PID through `NativeStartup::new_m2ts_prefix_v2`; native 90 kHz clocks | `ts-program` |
| `raw-video-` | `avc` or `hevc` | `RawVideoProbe`, `RawVideoSource` and `RawVideoFeed` over forward-only Annex B; no container clock, the duration is the active SPS picture period in 1/1,200,000 s | `raw-annex-b` |

The contracts are in `harness/features/<contract>/`; their corpora name inputs relative
to the repository root, which they record as their `directory`.

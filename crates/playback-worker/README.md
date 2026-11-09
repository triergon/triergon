# playback-worker

Playback measurement worker for the harness. Each mode family runs one operation of
the [triergon-playback](../playback/README.md) engine or a container reader on one input; the FFmpeg
reference worker in `harness/` performs the same operation through public FFmpeg APIs.
It is tooling (`publish = false`), not part of the library.

## Command line

Every mode family has one prefix and the same command line (`src/runner.rs`):

```text
playback-worker PREFIXfold   FILE ARGS
playback-worker PREFIXbench  FILE ITERATIONS ARGS
playback-worker PREFIXmemory FILE ARGS
playback-worker PREFIXtrace  FILE ARGS
```

Every mode prints one JSON line with the packet and byte counts and the ordered fold
of each operation's digest. `bench` runs the operation ITERATIONS times and adds the
elapsed time of that loop, `memory` brackets one operation with the allocation-window
markers, and `trace`, where a family has it, prints what the operation folds before
the result. Reading the input, process start-up and the result line are outside the
timed loop and the allocation window. An operation on a preloaded input fails when it
leaves any of that input's pool charged. An operation that ends early stops the loop:
the line then counts the operations that ran, says `"eof":false` and names the refusal
and the input event it is attributed to.

| Prefix | ARGS | Operation | Contract |
|---|---|---|---|
| `feed-` | none; FILE is a capsule | Matroska discovery with a 4 KiB read window; the first video track's AVC or HEVC packets with native PTS, DTS and duration, from startup or after a backward seek | `matroska-startup-seek` |
| `hls-` | VIDEO (`avc`, `hevc`) and AUDIO (`aac`, `ac3`, `eac3`) | one TS188 segment: every access unit retained through segment EOF, then `contiguous_feed` for every video unit, folding its payload, the audio bytes, timing and both stream tails | `hls-segments` |
| `mp4-range-` | an optional backward seek target in video ticks and optional `opus-roll` policy | regular or complete fragmented AVC/AAC, regular AVC/Opus, and regular HEVC/AAC or AV1 with dedicated Dolby Vision sample entries. Seek cases read one packet before seeking backward. The digest covers order, source extents, clocks, flags, trim, bytes and the tag and decoder configuration of Opus and dedicated video entries. The source-roll policy also folds the video landing, audio ordinals and preroll | `mp4-range` |
| `progressive-` | CLOCK (`pts-only`, `pts-dts`) and PROGRESS (`before-eof`, `at-eof`) | a forward-only AVC/AAC TS188 session read from the file in 4 KiB windows, two retained units per stream; `bench` runs 50 sessions, `bench` and `memory` only on Linux AArch64, and `trace` prints every unit with the stream facts on stderr | `progressive-ts188` |
| `mkv-audio-` | `aac`, `dolby` or `all` | Matroska stream discovery in 4 KiB windows; the AAC-LC, AC-3 and E-AC-3 tracks the reader identifies through `native_aac_clock_probe` or `native_dolby_clock_probe` over the discovery prefix, then the forward clock; times in nanoseconds | `matroska-audio-clocks` |
| `mp4-forward-` | `regular` or `progressive` | `Mp4Open` and `Mp4IndexJob` in the native MOV view; a seekable file through the checked range index and `Engine::mp4`, with encoded plans for edits over composition offsets, or a forward-only front-`moov` file through its native front plan and `Engine::mp4_native_terminal_v1`; every track, times in the track time base | `mp4-forward` |
| `ts-program-` | `ts188:PID=CARRIAGE,...` or `m2ts192:PID=CARRIAGE` | explicit PIDs of program 1 through `Engine::ts_native_program_v1`, or one M2TS192 PID through `NativeStartup::new_m2ts_prefix_v2`; native 90 kHz clocks | `ts-program` |
| `raw-video-` | `avc` or `hevc` | `RawVideoProbe`, `RawVideoSource` and `RawVideoFeed` over forward-only Annex B; no container clock, the duration is the active SPS picture period in 1/1,200,000 s | `raw-annex-b` |
| `ts-au-` | SIZE (`188`, `192`) and PROGRAM, optionally `--dvb` or `--atsc` | the MPEG-TS source reader through `AnyReader`: complete AVC/HEVC, ADTS AAC-LC and explicitly signalled DVB or ATSC AC-3 and E-AC-3 units, plus separately reported video-header groups, incomplete audio and unframed video at logical EOF; the packet comparison uses original PES anchors through `compat::pes::DemuxClock`; empty completion requires a confirmed control-only input | `mpegts-access-units` |
| `ts-` | SIZE (`188`, `192`) and PROGRAM, optionally `--dvb` and further media entities (`[--discontinuity] FILE`) | the FFmpeg-compatible packets of one program through the public API: `TransportStream` selects the program, its first active PMT binds a `compat::pes::DemuxObserver` to the one elementary stream, that PID's transport packets feed it and `DemuxClock` places the timestamps; a context the observer alone cannot take (more streams, a later PMT, clock or format boundaries, transport, PES-time and active PSI faults, rejected carriers, a second entity) and an operation without a packet end with their cause; the strict source view and its native clock are not compared | `mpegts-pes` |

A capsule is `SMVCL001`, one operation byte (0 startup, 1 seek after discovery,
2 seek after reading to the end), the seek target in milliseconds as a big-endian
`i64`, then the Matroska file. `cargo xtask fixtures` writes the capsules and the
MP4 range inputs under `.research/`.

The `mkv-audio-`, `mp4-forward-`, `ts-program-` and `raw-video-` families share one
packet fold (`src/packet_fold.rs`): track, PTS, DTS, duration, flags, size and every
payload byte, with an unknown time folded as `i64::MIN` and an unknown duration as
zero; `trace` prints one line per packet. The reference side is
`harness/reference_packet_fold.h`. `ts-` folds stream index, PID, PTS, DTS, duration,
position, flags, payload and the stream-ID side data, as `harness/reference_ts.h` does.
The separate `ts-au-` operation enables reference codec parsing with `NOFILLIN`
and no stream discovery. It preserves the original `ts-` operation and its corpus.
Derived source sample durations remain available on the packet. The comparison
keeps zero packet durations for video and AAC; AC-3 duration is confirmed by a
second frame header, retaining one packet until that lookahead or EOF. This
context and retention remain inside the measured operation. The bounded input bootstrap
supplies the comparison's M2TS carrier or sync position. Incomplete audio is
reported separately from complete units, with its original bytes and refusal
reason; it never enters the neutral packet stream.
The Annex B EOF policy also reports unframed video as an opaque source event.
The comparison folds its original payload, clock anchor and PES position.
The LATM packet policy folds each validated nonempty PES in source order, with
zero packet flags and duration. Separate reader tests exercise LOAS framing.
DVB subtitle segment groups keep their source PES positions. The prefix
policy covers an overlong final segment. The private-audio EOF policy keeps
one unframed PES as an opaque observation. A discarded startup PSI pointer
can be followed by a validated map and media in the same entity.
If a single entity ends without an access unit or stops at a PSI diagnostic,
the comparison can complete empty after the control-only proof classifies the
original carriers through physical end and logical EOF.
Its generic refusal context records the last input-window offset; source
diagnostics carry their exact source location.

The contracts are in `harness/features/<contract>/`. Packet-fold corpora record
the repository root as their `directory` and name inputs relative to it.

For ordered MPEG-TS entities, the access-unit reference carries parser state
across contiguous input and initialization maps. An explicit discontinuity
flushes the parser before the next entity. The candidate supplies each entity
through the same access-unit reader and preserves its physical source ranges.
The comparison uses logical packet positions across the ordered bytes. Only
the final EOF drains the parser.

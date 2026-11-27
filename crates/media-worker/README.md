# media-worker

Matroska measurement worker for the harness. It runs one operation of
[triergon-matroska](../matroska/README.md) on one input, as the FFmpeg reference
worker in `harness/` does with FFmpeg's Matroska demuxer, and is the candidate of the
Matroska features that name no other. It is tooling (`publish = false`), not part of
the library.

## Command line

```text
media-worker [VIEW-]trace        FILE
media-worker [VIEW-]trace-short  FILE CHUNK
media-worker [VIEW-]bench        FILE ITERATIONS
media-worker [VIEW-]memory       FILE
media-worker trace-source        FILE
media-worker trace-source-short  FILE CHUNK
media-worker metadata-source     FILE
media-worker catalogue-source    FILE
media-worker seek                FILE NS
media-worker seek-trace          FILE NS [after-eof]
media-worker playback-seek-trace FILE NS [after-eof]
media-worker seek-bench          FILE ITERATIONS NS
media-worker seek-memory         FILE NS
media-worker inflate             FILE [OUTPUT_BYTES [EXPANSION_RATIO [WORK]]]
media-worker baseline            FILE
media-worker memory-baseline     FILE
```

`VIEW` changes what a demux trace carries: `codec` keeps only the packets a decoder
receives, `metadata` adds each track's colour fields and `catalogue` gives the
catalogue in place of track languages and names. The `-short` modes read the input at
most `CHUNK` bytes per call. The `*-source` modes trace the source-frame views. `seek`
reads to the end, seeks the first video track to the key frame before `NS`
nanoseconds and prints the packet there; `seek-trace` and `playback-seek-trace` trace
every packet after a block-start or playback seek of track 0, from the end with
`after-eof`. `inflate` decompresses FILE as one zlib stream under the given limits. The
`baseline` modes read the input and parse nothing. Inputs are read whole before the
operation, at most 64 MiB.

The trace modes print the harness protocol's records; the bench and memory modes print
its one JSON line, the bench mode with the elapsed time and the memory mode around one
operation's allocation-window markers
([triergon-harness-runner](../harness-runner/README.md)).

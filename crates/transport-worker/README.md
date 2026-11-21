# transport-worker

Harness worker for the public MPEG-TS PES and access-unit readers. It reads the original
entities before measurement and emits their ordered packet counts, byte counts and
digests; the FFmpeg reference worker uses the same input and operation.

```text
transport-worker PREFIXfold   FILE SIZE PROGRAM [--dvb|--atsc] [[--discontinuity] ENTITY]...
transport-worker PREFIXbench  FILE ITERATIONS SIZE PROGRAM [--dvb|--atsc] [[--discontinuity] ENTITY]...
transport-worker PREFIXmemory FILE SIZE PROGRAM [--dvb|--atsc] [[--discontinuity] ENTITY]...
```

The `ts-` modes also accept `188 PROGRAM --discover-ts188` for a single seekable
entity. Each run discovers the TS188 grid and active program map, then replays
registered HEVC and ADTS AAC tracks in source order. Other track types, changes to
the selected program map or clock interval, and unsupported source faults are refused.

SIZE is 188 or 192. Use `ts-` for PES packets
([contract](../../harness/features/mpegts-pes/contract.json)) and `ts-au-` for access
units ([contract](../../harness/features/mpegts-access-units/contract.json)). `ts-au-`
also takes a trailing `--dts-core=PID`, which reads that PID of a single entity as an
unlabelled DTS core, and then `--ignore-pcr-reserved-bits`. A contiguous entity carries
parser state across its boundary; an explicit discontinuity resets it and the final EOF
drains it. Input loading and result serialization are outside the timed loop and the
memory window.

| Prefix | Arguments after the mode's FILE [ITERATIONS] | Operation |
|---|---|---|
| `ts-seek-` | `SIZE PROGRAM PID PTS90 POLICY` (`previous` or `preroll`), then optional `REPEAT_PTS90` and `split=BYTES`, or `epoch=CLOCK:CONFIG epoch-start=BYTES` | a seek, replaying the landing and every following packet through EOF ([picture contract](../../harness/features/mpegts-previous-picture/contract.json)) |
| `ts-audio-seek-` | as `ts-seek-` without the epoch options, with an optional `--dvb` or `--atsc` after PROGRAM | a seek on an ADTS, LOAS, AC-3, MPEG audio or DTS PID ([contract](../../harness/features/mpegts-previous-audio/contract.json)) |
| `ts-free-audio-` | `SIZE PROGRAM PID [layer1-padding]` | free-format MPEG audio frames from the start ([contract](../../harness/features/mpegts-free-audio/contract.json)) |
| `ts-eac3-six-block-` (also `trace`) | `188 PROGRAM --atsc` | six-block ATSC E-AC-3 packet projection |
| `ts-dts-hd-` (also `trace`) | `SIZE PROGRAM --dvb` | grouped single-extension DTS-HD against an explicit DTS parser applied to each aligned finite PES |

`cargo xtask fixture transport-dts-hd-grouped` generates the DTS-HD inputs into
`.research/`; add `--record REFERENCE` to record their folds from the pinned reference
worker.

`ts-au-trace` prints the projected packet fields, original payload and side data
before the fold summary. Trace printing is selected once and absent from the
ordinary per-packet fold.

The [six-block ATSC E-AC-3 comparison](../../harness/eac3-proof.md) checks the
complete grouped operation with a separate public-parser adapter. All ten
authored inputs match every packet field and payload through EOF. The ordinary
parser operation differs on all ten inputs. Both comparisons are available
through `cargo xtask worker-tests transport::eac3-experiment`. The separate
[six-block report](../../harness/features/mpegts-atsc-eac3-six-block/contract.json)
pins all ten folds. Native timing and memory qualification remain open.

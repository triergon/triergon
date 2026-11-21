# transport-worker

The harness runs the public MPEG-TS PES and access-unit readers in a standalone
worker. It reads the original entities before measurement and emits their ordered
packet counts, byte counts and digests. The FFmpeg reference worker uses the same
input and operation.

```text
transport-worker PREFIXfold   FILE SIZE PROGRAM [--dvb|--atsc] [[--discontinuity] ENTITY]...
transport-worker PREFIXbench  FILE ITERATIONS SIZE PROGRAM [--dvb|--atsc] [[--discontinuity] ENTITY]...
transport-worker PREFIXmemory FILE SIZE PROGRAM [--dvb|--atsc] [[--discontinuity] ENTITY]...
```

SIZE is 188 or 192. Use `ts-` for PES packets and `ts-au-` for access units.
`ts-au-` also takes a trailing `--dts-core=PID`, which reads that PID of a single
entity as an unlabelled DTS core, and then `--ignore-pcr-reserved-bits`.
Input loading and result serialization are outside the timed loop and the memory window.

The [PES contract](../../harness/features/mpegts-pes/contract.json) follows the
selected program's PES and compatibility clock. The
[access-unit contract](../../harness/features/mpegts-access-units/contract.json) uses
`compat::annex_b::ContinuityReader` and covers complete access units and bounded EOF
observations. The digest covers order, stream index, PID, clocks, duration, position,
flags, payload and side data. A contiguous entity carries parser state across its
boundary; an explicit discontinuity resets it and the final EOF drains it.

`ts-seek-{fold,bench,memory}` takes `FILE [ITERATIONS] SIZE PROGRAM PID PTS90
POLICY`, where POLICY is `previous` or `preroll`. Optional `REPEAT_PTS90` and
`split=BYTES` select repeated indexed seeks and contiguous entity parts, and
`epoch=CLOCK:CONFIG epoch-start=BYTES` one explicit source epoch, which the candidate
passes to `Reader::request_seek_in_epoch` (or its entity and index forms); the
physical offset identifies the reference control only. Both replay the landing and
every following packet through EOF.

`ts-audio-seek-{fold,bench,memory}` takes the same command line without the epoch
options and with an optional `--dvb` or `--atsc` after PROGRAM, and seeks an ADTS,
LOAS, AC-3, MPEG audio or DTS PID
([contract](../../harness/features/mpegts-previous-audio/contract.json)).
`ts-free-audio-{fold,bench,memory}` takes `FILE [ITERATIONS] SIZE PROGRAM PID
[layer1-padding]` and reads free-format MPEG audio frames from the start
([contract](../../harness/features/mpegts-free-audio/contract.json)); the fold adds
each frame's DTS, elementary offset, sample rate and samples per frame.

The [picture comparison](../../harness/features/mpegts-previous-picture/contract.json)
includes authored AVC clock and PMT changes in TS188 and M2TS192, one picture per
PES. The public FFmpeg packet position selects reference candidates at or beyond
the supplied carrier boundary. The fold covers requested, landing and discard
times and every suffix packet's PTS, effective DTS, random-access flag and bytes.

`ts-dts-hd-{fold,bench,memory,trace}` takes `FILE [ITERATIONS] SIZE PROGRAM --dvb` and
compares grouped single-extension DTS-HD against an explicit DTS parser applied to
each aligned finite PES, since the pinned demuxer reports the DVB DTS-HD descriptor
as `bin_data`. Its `trace` mode prints every folded field (`src/ts_dts_hd.rs`); the
original header and every AU byte also enter the fold. Multiple extensions without a
core use the `ts-au-` comparison, where each aligned finite PES contains exactly one
complete access unit. Generate both sets of inputs with
`cargo xtask fixture transport-dts-hd-grouped`; add `--record REFERENCE` to record
their folds from the pinned reference worker. Generated inputs stay in `.research/`.

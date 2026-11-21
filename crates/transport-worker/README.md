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
`bench` measures the repeated operation. `memory` measures one operation.
Input loading and result serialization are outside both measurement windows.

The [PES contract](../../harness/features/mpegts-pes/contract.json) follows the
selected program's PES and compatibility clock. The [access-unit contract](../../harness/features/mpegts-access-units/contract.json)
uses `compat::annex_b::ContinuityReader` and covers complete access units and
bounded EOF observations. It inspects ordinary carrier events by reference.
The digest covers order, stream index, PID, clocks,
duration, position, flags, payload and side data. A contiguous entity carries
parser state across its boundary. An explicit discontinuity resets it and the
final EOF drains it.

`ts-seek-{fold,bench,memory}` takes `FILE [ITERATIONS] SIZE PROGRAM PID PTS90
POLICY`, where POLICY is `previous` or `preroll`. Optional `REPEAT_PTS90` and
`split=BYTES` select repeated indexed seeks and contiguous entity parts.
For one explicit source epoch, use `epoch=CLOCK:CONFIG epoch-start=BYTES`
instead. The candidate passes the complete pair to `Reader::request_seek_in_epoch`;
the physical offset identifies the reference control only. Both scan the full
stable input and replay the landing and every following packet through EOF.

The [picture comparison](../../harness/features/mpegts-previous-picture/contract.json)
includes authored AVC clock and PMT changes in TS188 and M2TS192, one picture per
PES. The public FFmpeg packet position selects reference candidates at or beyond
the supplied carrier boundary. The fold covers requested, landing and discard
times and every suffix packet's PTS, effective DTS, random-access flag and bytes.

`ts-dts-hd-` compares grouped single-extension DTS-HD against an explicit DTS
parser applied to each aligned finite PES. Its `trace` mode prints the folded
fields: stream index, PID, source AU PTS/DTS, raw PES PTS/DTS, physical PES header
and carrier offsets, declared PES length, PES payload size and elementary offset,
AU payload range, AU elementary range, exact duration numerator/denominator,
original header size and whether the AU claims the PES clock. The original
header and every AU byte also enter the fold. Unknown clocks use `i64::MIN`;
unknown duration is `(0, 0)`. Static timing is read independently from the
lowest-index extension's fixed prefix on the reference side.

The pinned demuxer reports the DVB DTS-HD descriptor as `bin_data` and preserves
its PES boundaries. The explicit parser supplies the grouped single-extension
boundaries. Multiple extensions without a core use the separate `ts-au-`
comparison, where each aligned finite PES contains exactly one complete access
unit. Generate both sets of inputs with
`cargo xtask fixture transport-dts-hd-grouped`; add `--record REFERENCE` to record
their folds from the pinned reference worker. Generated inputs stay in `.research/`.

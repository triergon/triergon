# ATSC E-AC-3 six-block reference adapter

The separately compiled `reference_ts_eac3_worker.c` accepts `ts-eac3-proof FILE` and
`ts-eac3-proof-trace FILE`. These opt-in commands use the existing formats
reference link inputs through the existing xtask formats profile as
`ffmpeg-ts-eac3-worker`. The adapter also accepts `ts-eac3-six-block-{fold,bench,memory}` with
`FILE [ITERATIONS] 188 PROGRAM --atsc`. Fold and proof modes verify parser-output byte identity and digest the original source headers; bench and memory modes omit those reference-only passes. The required [report contract](features/mpegts-atsc-eac3-six-block/contract.json) pins its ten folds. The ordinary reference workers' `ts-au-fold` continues to return ordinary FFmpeg results.

The operation starts with the same preloaded original entity as the existing
transport harness, creates fresh demuxer/parser state, and includes demuxing,
framing, grouping, source clock validation, side data, duration derivation and
folding. All modes check parser interval ownership and complete coverage. Bench and
memory parse fixed syncframe fields and the BSI of the opening short primary;
fold and proof additionally inspect every syncframe BSI. Input loading is outside the operation. There is no second demux scan,
whole-input validation pass, serialized intermediate representation or separate
whole-input copy. All temporary state is released before the summary is printed.

The public MPEG-TS demuxer runs with `NOPARSE | NOFILLIN`, custom forward AVIO,
no stream discovery and no decoder. Every selected PES is retained by a public
packet reference until parser coverage and its complete AUs are consumed.
The public E-AC-3 parser is fed incrementally and drained at actual demux EOF.
Every parser interval must fit the next unclaimed bytes of one PES; fold and proof also compare those bytes for identity.
The adapter retains at most 32 PES owners and 1 MiB of PES payload. These limits
do not bound FFmpeg's own internal allocations or its read-ahead.

Grouping follows [ATSC A/52:2018](https://www.atsc.org/wp-content/uploads/2021/04/A52-2018.pdf),
Annex E Table E1.2 and Annex G §3.3. A primary independent substream 0 and its
following dependents accumulate six primary blocks entirely within a PES.
Each dependent matches its primary's rate and block count, has sequential IDs starting at zero in each primary interval, and has the same
count in every interval of the group. Completed groups publish
before inspection of the next primary's BSI. The next fixed header identifies
that boundary. Later faults preserve already published groups.

The summary's `digest` is the existing ordered packet fold: stream, PID, public
PES PTS/DTS, derived 90 kHz duration, position, flags, payload size and all bytes,
then every side-data type, size and byte. Only the first AU in a PES claims its
clocks and position. All AUs retain its side data. The packet clock projection
preserves the public demuxer's DTS synthesis; it is distinct from raw source
clocks. `source_digest` additionally folds each consumed PES position, original
raw PTS/DTS (absent as `INT64_MIN`), header length and every original PES header
byte. Recovering these facts inspects only the original header's carrier, inside
the operation. Trace mode prints every folded packet field and payload byte.
The summary reports the prefix even on refusal and returns status 1; EOF returns
status 0. Malformed command lines return 2.

The supported proof boundary is deliberately narrow: one TS188 entity, program
7, one direct `0x87` E-AC-3 stream in finite `0xbd` PES packets, `bsid=16`, primary substream 0, independent or dependent
stream types, integral AU duration, and an optional PES header contained in its
first carrier. Fold and proof refuse mixing metadata when inspecting BSI. All modes refuse
converted streams, other independent services, multiple selected streams, split
optional headers and nonintegral durations. AU-start `convsync=1` for short frames is the adapter's expectation;
interior `convsync=1` is accepted. This is a framing check, not CRC, conversion
or audio decoding certification. General metadata syntax and broader transport validation still need dedicated proof
coverage before broadening this boundary.

The 24 retained functional controls agree on full expected packet records and
ordered folds, including 13 refusal outcomes. Ten original controls emit
30 AUs / 5,760 bytes. Fourteen mutation controls emit 6 AUs / 1,152 bytes:
`early-convsync` accepts 3 / 576; `incomplete-au-at-pes-end`,
`missing-next-au-convsync` and `truncated-syncframe` each refuse after 1 / 192;
the other ten refuse with empty prefixes. Retained expectations have independent
payload, timing, position, stream/flags and original public PES side-data checks.
The transport worker's `ts-au-trace FILE 188 7 --atsc` exposes the live candidate's
packet projection. Diagnostic printing uses a separate const-generic operation;
ordinary folds select the non-printing operation before measurement and carry no
runtime per-packet trace flag. All ten original controls agree with this adapter on every
ordered packet field, payload byte and side-data element, the exact combined
fold, EOF and successful process exit. Every case emits three access units.
No group-field mismatch occurs. The fourteen mutation controls above retain
their separate expected-record scope; live candidate refusal equality is not
established by the ten accepting cases. Refusal strings are adapter causes,
not claims of identical candidate error text.

| Primary blocks per frame | Dependents per interval | Bytes | Packet digest |
|---|---:|---:|---:|
| mixed | 0 | 288 | 4393127636328960007 |
| mixed | 2 | 864 | 7620691429583559969 |
| 1 | 0 | 576 | 4931118181656872344 |
| 1 | 2 | 1728 | 9010230201963547148 |
| 6 | 0 | 96 | 6814399669389160617 |
| 6 | 2 | 288 | 10729622621970055899 |
| 3 | 0 | 192 | 10056975916167440644 |
| 3 | 2 | 576 | 9836296303524275752 |
| 2 | 0 | 288 | 9341172979077238791 |
| 2 | 2 | 864 | 8335105489591770401 |

These results are independent of the ordinary FFmpeg comparison, which remains
0/10 exact folds. They do not replace its pins, operation or accepted scopes.

The [shared quality gates](../docs/quality.md) apply separately on Linux AArch64
and 32-bit ARMv7. [Status](../docs/status.md) records native timing and memory
results.

Reproduce the functional comparison from the repository root, after preparing
its pinned formats reference (`cargo xtask prepare-ffmpeg --profile formats`):

```sh
export CARGO_INCREMENTAL=0
cargo build --release -p transport-worker
cargo xtask worker-tests transport::eac3-experiment
```

This opt-in worker test reuses `fixture transport-eac3-probe` in a temporary
directory and compiles both the isolated adapter and stock public worker against
`.research/reference-formats/install`. It compares all three ordered packet
records, including payload and side data, the literal packet folds above, real
EOF and successful exits, and checks the ordinary candidate fold against its
trace summary. It displays each of the ten adapter outcomes and each of the ten
stock `ts-au-fold` outcomes separately. The expected functional result is 10/10
adapter matches and 0/10 stock matches. A stock difference remains a difference;
it is not counted as six-block agreement. This experiment is excluded from the default worker tests. The separate required report covers the ten accepting controls; it does not replace the ordinary comparison.
Generated inputs and worker output are not stored in git.

# MPEG-TS original-input observations

[transport-coverage.json](transport-coverage.json) classifies all currently unmapped
TS identities in both tracked indexes, including TS-fed remux and subtitle cases.
It preserves each index's original row, including configured status, command hashes
and unresolved sample references. The formats index resolves the three ordinary
probe inputs that the older index leaves empty. Only the PMT-merge identity is
configured in the recorded formats build; the program and LATM identities are
not. No authored packet control is an exact equivalent of these upstream
operations. The observation command does not change that build configuration.
`exact_input_cases` identifies existing comparisons on the identical original
sample, separately from equivalence of an upstream operation. In particular,
`subtitle-dvb:fate-dvbsubtest_filter` already pins the original DVB input
(SHA-256 `93ad6d0be649bb29697275ff522a983d475a1e58ab070271f912b86799e04a86`)
and a decode fold of 46 packets, 1,714,944 bytes and digest 5167161527884782263.
That establishes existing subtitle-decode coverage, not TS-fed remux output or
execution of the indexed upstream subtitle command.

```sh
CARGO_INCREMENTAL=0 cargo xtask transport-fate check
CARGO_INCREMENTAL=0 cargo xtask transport-fate inventory harness/fate/transport-coverage.json
CARGO_INCREMENTAL=0 cargo xtask transport-fate observe .research/transport-spec.json .research/transport-observations
```

The last command follows [the observation contract](transport-observations-contract.json).
Its specification is JSON with these fields:

| Field | Value |
| --- | --- |
| `id` | One of `fate-mpegts-probe-pmt-merge`, `fate-mpegts-probe-program`, `fate-mpegts-probe-latm` |
| `reference_revision` | `d32b387f2b0a484599d4587d651891f0c63c4238` |
| `program` | A nonzero decimal program number as a string, obtained from the original input's program catalogue |
| `input` | `{ "file": ".research/fate-samples/mpegts/pmtchange.ts", "sample": "mpegts/pmtchange.ts", "sha256": "<verified original sample SHA-256>" }`; program/LATM probes use `mpegts/loewe.ts` |
| `reference_build` | `{ "file": "<reference build.json>", "sha256": "<verified build manifest SHA-256>" }`; its revision must match |
| `ffprobe` | `{ "file": "<prepared reference ffprobe>", "sha256": "<verified executable SHA-256>" }` |
| `reference_worker` | `{ "file": "<built formats reference worker>", "sha256": "<verified executable SHA-256>" }` |
| `candidate_worker` | `{ "file": "<built transport-worker>", "sha256": "<verified executable SHA-256>" }` |

Paths are relative to the repository or absolute. Replace the angle-bracket values
with independently verified build and sample pins. The tool does not manufacture
pins from an unverified download. A missing sample is fetched with the existing
SHA-256-checked download tooling into the ignored sample cache. Changed inputs are
refused. Executable hashes and the build manifest identify the chosen observation;
they do not independently certify that an executable was built from that manifest.
Use the existing worker build identity for that provenance before qualification.

Each run retains stdout, stderr, arguments, input/binary pins and output hashes.
The reference discovery JSON is an observation, not candidate metadata parity or
an upstream FATE execution. The original input also runs through the unchanged PES
and AU operations, including their existing unsupported-context refusals. Packet
comparisons require matching full folds and complete EOF; failures exit nonzero
and stay in the observation result. An original multi-program input can therefore produce
useful reference metadata and still fail one or both packet operations.

Strict source facts remain distinct: original PCR/OPCR, PES headers, raw 33-bit
PTS/DTS, ranges and epochs need their own independent oracle. The existing
`clock_views` tests check source timestamps separately from compatibility clocks.
The signalling tests preserve unsupported stream types 0x33 and 0x36 and opaque
descriptors as unknown tracks. This is a mapping boundary, not VVC framing or LCEVC
enhancement support. Base AVC/HEVC coverage does not establish enhancement parity.

The program inventory also lists absent playback profiles: original PMT merging,
original multi-program selection, LATM discovery, automatic-selection refusal,
program removal/PMT PID changes, complete M2TS multi-PID playback and strict
original clock/PES facts. The existing `ts-program` corpus explicitly selects PIDs
of program 1; its M2TS cases each select one PID.

The original PMT-merge probe reports program 4. For that input the reference
emits seven PES packets and eight access units. The program and LATM probes use
program 769 and the reference emits 23 PES packets and 36 access units. The
candidate PES operation requires a program-level reader on both sources. The
candidate access-unit operation reports a PSI diagnostic for `pmtchange.ts` and
a transport diagnostic for `loewe.ts`. These comparisons remain open.

Run the source-only program reader controls with the hash-pinned originals in
`.research/fate-samples/mpegts`:

```sh
CARGO_INCREMENTAL=0 cargo test -p triergon-mpegts --test original_program -- --ignored
```

The controls assert the same source events for one-byte and 32 KiB delivery.
They independently check original carrier bytes, PES payload ranges, encoded
PTS/DTS, PCR/OPCR and accepted-section CRCs. With explicit program 4 or automatic
sole-program selection, `pmtchange.ts` yields PMT versions 29 and 30 and five
replacement routes in each version. It yields four complete video PES packets
(138,514 payload bytes) and diagnostics for five CRC-valid table-ID `0xc0`
sections on PMT PID 73 and one truncated audio PES. The regular sync grid of
`loewe.ts` starts at offset one. The aligned reader reports 532 transport faults
through EOF and discovers no tracks. The original inputs also cover open
compatibility work on PMT merging, foreign-table filtering, incomplete audio and
unaligned startup.

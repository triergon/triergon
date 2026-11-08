# MP4 terminal-media FFprobe records

`../native_terminal_golds.rs` holds what FFprobe 9.0.2 printed for 39 MP4
terminal-media inputs in this directory tree: 31 original inputs under
`open-ended/`, `progressive/` and `mp4/`, and 8 under
`native-terminal-controls/`. `tests/native_terminal_matrix.rs` and
`tests/native_terminal_controls.rs` read it through
`tests/terminal_golds/mod.rs`.

FFprobe read every input twice, from the file and from a nonseekable pipe
(`-i pipe:0`), with

```sh
ffprobe -fflags +noparse+nofillin -show_streams -show_packets -show_format \
  -show_data -of json -i INPUT
```

The records keep each packet object's 12 keys with FFprobe's spelling and
key presence; `data` holds the bytes of its hex dump. A read FFprobe refused
(exit 1 after `error reading header` with `Invalid data found when processing
input`, and no sections) is `Probe::InvalidHeader`; any other exit status or
message fails the conversion. The matrix compares the pipe record of every
input and the file record of the 31 original inputs; the file records of the
other 8 are checked and listed in the manifest but not compared.

## Source

The captures are in the retained evidence set
`playback-terminal-matrix-ffprobe` (`harness/evidence/index.json`):

| | |
|---|---|
| Key | `evidence/playback-terminal-matrix-ffprobe/5eb191f3826461ffcc4c8fe47132fd03e060fb930f10b92e26b3031d9a3a465e.tar.zst` |
| SHA-256 | `5eb191f3826461ffcc4c8fe47132fd03e060fb930f10b92e26b3031d9a3a465e` |
| Size | 209,168,659 bytes, a zstd-compressed tar |
| FFprobe binary SHA-256 | `0cb2ccb439f0177fa302007885a4919b13954ac6d581a1c146cdee134beec8cc` |

[`captures.json`](captures.json) lists the three capture directories under
the set's one top-level directory with the SHA-256 of each one's row list,
and for every input its
repository path and, for each read, the input SHA-256, FFprobe's exit status
and the SHA-256 of its stdout and stderr. It ends with the SHA-256 of the
generated records.

## Regenerating

```sh
# TRIERGON_EVIDENCE_ARCHIVE names the archive (docs/quality.md#evidence)
cargo xtask evidence fetch playback-terminal-matrix-ffprobe DIR
cargo xtask playback-golds DIR --check
```

`evidence fetch` downloads the set, checks its SHA-256 and unpacks it through
the system `zstd`. `playback-golds DIR` (`xtask/src/playback_golds.rs`)
checks every capture's stdout and stderr against the SHA-256 its capture
recorded, every input against the repository file, the FFprobe arguments, the
packet keys and their order, and each hex dump against its size and ASCII
column, then writes `../native_terminal_golds.rs` and `captures.json`.
`--check` compares them with the committed files instead, so a regeneration
that differs fails.

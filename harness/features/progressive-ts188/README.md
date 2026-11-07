# Progressive AVC/AAC TS188 feeds

`cargo xtask progressive` compares three forward-only AVC/AAC packet folds and
checks ten literal per-engine outcomes across five functional inputs. It validates
all outcomes before collecting timing or memory samples.

Add `--benchmark` or `--memory` for paired collection on native AArch64 Linux.
Each of the three comparable inputs must satisfy the 1.20 runtime ceiling and
strictly lower peak heap and total resident RSS at the upper paired 95% bound.
The runtime optimization target is 0.80. Timing uses 50 complete sessions per
sample and at least 30 pairs.

Both numeric workers use the harness's exact grouped byte fold. Every payload
byte contributes to the same ordered checksum as the scalar trace path.

| Input | Rust functional outcome | Reference functional outcome | Timed |
|---|---|---|---|
| `continuous-44100` | positive output | identical output | yes |
| `reordered-48000` | positive output | identical output | yes |
| `later-entry-48000` | positive output | identical output | yes |
| `duplicate-control-44100` | positive output | identical output | no |
| `legal-duplicate-44100` | positive normative output | recorded `AVERROR_INVALIDDATA` refusal | no |

Each session opens the same file and reads it as an unknown-length, forward-only
source in windows of at most 4096 bytes. It selects AVC and ADTS AAC, retains two
complete units per stream, folds a unit when it is replaced or at EOF, and releases
the source and session. File opening, reads and release are inside the operation.
The three timing inputs require output and retained-unit retirement before EOF.
The two short controls check the same work with progress checked at EOF.

The fold includes AVC PTS, DTS, raw33 source clocks, IDR, raster and every Annex B
byte. AAC contributes source clocks, resolved presentation, duration, rate,
channels, sample count and every ADTS byte. Complete session folds accumulate in
iteration order, combining the video stream before audio.

`corpus.json` pins the three comparable inputs and their expected work.
`controls.json` pins all five input hashes, normative Rust work and both engines'
literal stdout hashes and exit codes. The legal-duplicate reference outcome also
requires its recorded error stage and code. Its Rust result must match the
positive output. Input and observation origins are recorded in `provenance.json`.

The command saves all ten raw outcomes and a source/binary-bound report under
`OUTPUT/controls/`, linked from the three-case report. Literal traces include
arrival and retirement order. Worker tests also check all five Rust traces.

Both workers expose `progressive-trace`, `progressive-fold`, `progressive-bench`
and `progressive-memory`. Arguments after the input select the source video clock
(`pts-dts` or `pts-only`) and progress requirement (`before-eof` or `at-eof`).

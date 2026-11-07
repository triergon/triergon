# Quality: reference, gates and harness

Every module is measured against a pinned FFmpeg build doing the same work. A module
is complete only when it passes all three gates on every case in its corpus.

## Gates

| Gate | Requirement |
|---|---|
| Parity | Every declared feature matches the reference on every required case. Missing inputs, crashes, unsupported declared cases and unexplained differences are failures, never omitted rows. |
| Runtime | `T_triergon / T_ffmpeg` at most **1.20** per case, using the upper bound of a paired 95% confidence interval. The optimisation target is **0.80**. |
| Memory | Peak live heap and total resident peak RSS both strictly lower than the reference, each at the upper bound of a paired 95% interval. |

The harness reads these limits from `harness/policy.json`. Each feature's contract and
corpus manifest live in `harness/features/<feature>/`.
RSS growth from the start of the operation is reported separately when every pair
has positive growth bounds.

The work being compared must be identical: the same selection, the same packets, the
same output. A gate is never passed by narrowing the work, dropping slow cases or
weakening a comparison. A failed result stays recorded against the exact source and
binaries that produced it.

FFmpeg is the compatibility reference, not the specification. Disagreements are
investigated against the standard. A reference bug or a deliberately stricter
resource limit becomes an explicit, reviewed rule for that case.

## Reference and corpus

- **FFmpeg pins.** `cargo xtask prepare-ffmpeg --profile P` builds FFmpeg from a pinned,
  hash-checked source with recorded configure flags, compiler and binary hashes, and
  `cargo xtask build-workers` builds the reference worker of every prepared profile
  and records the baseline version from the prepared installation. Version
  discovery for an unpacked archive stops at the reference source boundary.
  Containers are measured against git revision `d32b387f` on the 9.0 branch: `matroska`
  for the container harness, `formats` for MOV, TS, subtitles and the HLS segment
  reference, and `video` (the Matroska build plus the H.264 and HEVC decoders that
  stream discovery needs) for the playback startup and seek reference. Decoders and
  capture are measured against the FFmpeg 9.0.2 release tarball: `decode` (the default
  configuration) for the codec comparison, whose `ffmpeg` and `ffprobe` also decode
  capture output independently, and `capture` (gating) and `capture-small` for the
  capture comparison. The two capture profiles link FFmpeg's component libraries
  into the worker so the allocation observer can audit the code doing the work.
- **Authored corpus.** Small canonical streams generated from synthetic signals and
  authored structures, so inputs stay identical across encoder versions
  (`cargo xtask fixtures`, `harness/features/<feature>/{contract,corpus}.json`).
- **FATE.** Upstream regression identities are indexed as data, without running
  Make (`cargo xtask fate-inventory`, `harness/fate/index.json`), and mapped case by
  case. A FATE name is not a test until its input, operation and expected result are
  mapped and run. Samples are fetched into an ignored cache with
  recorded hashes; nothing is redistributed without reviewed rights.
- **Required vectors.** Larger external vectors are fetched and hash-verified by
  `cargo xtask fetch-vectors` and kept outside version control.

## Correctness by module family

| Family | Compared |
|---|---|
| Time | Exact rational arithmetic, rounding and overflow; signed extremes; 32-bit narrowing |
| Demux | Track identity and configuration; payload hashes in per-track decode order; signed PTS, DTS, duration and trim; random-access facts; side data, metadata, attachments and chapters |
| Seek | Landing point, preceding random-access point, preroll and the packets that follow; fresh, repeated, backward and after-EOF seeks; missing indexes; changing input windows; cancellation |
| Packet framing | Equivalent access units after documented framing normalisation; configuration and payload preserved |
| Subtitles | Exact cue timing, text and flags; normalised bitmap rectangles, palettes and alpha |
| Decoders | Exact output where the standard guarantees it; otherwise predeclared per-format numeric tolerances |
| Encoders | Standards compliance, independent decode, quality at equal bitrate, throughput and memory |

## Harness

The harness runs the same operation through a Triergon worker and an FFmpeg worker and
compares canonical output. Workers speak one line protocol with `trace`, `bench` and
`memory` modes, framed by begin/end markers and a named digest.

```sh
cargo xtask prepare-check   # build the FFmpeg reference, fixtures and workers
cargo xtask check           # format, test, clippy, rustdoc, release build, worker tests, design checks
cargo xtask report --fate-cache .research/fate-samples
                            # correctness report against the reference
cargo xtask report --benchmark --memory --fate-cache .research/fate-samples --require-acceptance
                            # correctness, runtime and memory with acceptance
```

`cargo xtask report` runs the correctness comparison of one feature contract
(`--contract`, `--manifest`, `--corpus`) and grades it. A contract may name its own
candidate worker and reference, as `matroska-startup-seek` and `hls-segments` do with
`playback-worker` and the `video` and `formats` references. `--benchmark` adds paired
runtime and `--memory` the heap and RSS measurement under the native observers
(`harness/allocations*.c`), including the Linux link-closure and active-window guard
certificates. The report records the build identity, source provenance digest and input
hashes it was measured with; when a result is cited, its source revision and raw-sample
SHA-256 go in the [measured revisions](status.md#measured-revisions) table. `cargo xtask compare` compares a single input, `cargo xtask external-vector`
qualifies the mandatory external vector and `cargo xtask fate-run` runs one supported
upstream FATE case. `cargo xtask build-workers` builds the workers and records the build
identity every collection is checked against. `cargo xtask trace-identities` records the
digest of every candidate trace, so a change can show its traces are byte-identical
(`--compare` an earlier record). `cargo xtask worker-tests` runs the integration tests
of the built workers: work digests, catalogue, colour, content encoding, seek, timing
and inflate behaviour against the reference workers and the oracles. The independent
oracles stay in Python (`harness/*_oracle.py`, run through `python3 -m harness.oracles`),
as do the numpy table-recovery chains under `harness/codecs/` and the capture input
generator `harness/capture/generate_inputs.py`, whose numpy PCG64 and float draws the
cited capture inputs depend on. `cargo xtask codecs` grades, times and records the
identities of the codec crates against the pinned FFmpeg decoders
(`harness/codecs/STATUS.md`); `cargo xtask capture` does the same for the capture
pipeline ([harness/capture](../harness/capture/README.md)).
`cargo xtask fuzz` runs a deterministic, replayable byte-mutation campaign against
debug, release and reference workers from a pinned manifest.

On native AArch64 Linux, `cargo xtask memory-admission CANDIDATE REFERENCE`
checks the worker libraries using the probe and observer in `target/harness`.
It prints the calibration certificate and exits nonzero if coverage cannot
be established. Use built workers you trust.

## Measuring performance

- Time optimised builds doing equivalent work. For demuxing, the timed operation is
  open, probe, the required reads and packet production for the same selection, with
  process startup and serialisation outside the timing.
- Use 30 or more randomised A/B pairs per case after warm-up, keep every raw sample,
  and report p50, p95 and the confidence bound per case. An aggregate never hides a
  failing case.
- Measure on an isolated machine. Never time while compiling, fuzzing or running
  another benchmark; the runner rejects contaminated pairs.
- Allocation tracking and timing are separate runs, because instrumentation changes
  timing.
- To compare two revisions, build both on the same machine and run `cargo xtask report
  --benchmark --baseline-worker W` from the changed tree, with `W` the same worker from
  the base tree. Correctness is still compared with the reference; the pairs and ratios
  are against `W`. Such a report is never an acceptance run.
- Qualification runs on native AArch64 Linux. Results on one platform do not establish
  another; 32-bit ARMv7 needs its own runs.

## Robustness

Malformed input must never panic, hang or escape its budget. Tests and fuzzing cover
truncation at every structural boundary, corrupted lengths, counts and timestamps,
deep nesting, decompression bombs, duplicate identifiers and conflicting indexes.
A transport simulator covers short reads, delayed ranges, changing length, exact end of
file, reconnect failure and cancellation. Results are reproducible from a saved seed
and edit recipe.

## Evidence

A result names its exact source revision, worker binaries, reference build, corpus and
input hashes, platform and all per-case outcomes. Summaries and identities are kept
in the pull request that changes the result and in [status](status.md), whose
[measured revisions](status.md#measured-revisions) table names the revision and
raw-sample SHA-256 behind each cited result. Raw samples are archived by content hash,
outside the repository. [harness/evidence/index.json](../harness/evidence/index.json)
lists every archived set:

- its key and SHA-256;
- whether it is kept (cited results) or expires after 90 days;
- each file it holds, by path and SHA-256.

The index also records how identifiers were renamed. With `TRIERGON_EVIDENCE_ARCHIVE` set
to the archive location, `cargo xtask evidence fetch SET` downloads a set, verifies it
against the index and unpacks it into `.research/evidence/SET/`. `cargo xtask evidence
pack DIR OUT` writes the deterministic archive a new set is uploaded as.

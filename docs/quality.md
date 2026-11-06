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

The work being compared must be identical: the same selection, the same packets, the
same output. A gate is never passed by narrowing the work, dropping slow cases or
weakening a comparison. A failed result stays recorded against the exact source and
binaries that produced it.

FFmpeg is the compatibility reference, not the specification. Disagreements are
investigated against the standard. A reference bug or a deliberately stricter
resource limit becomes an explicit, reviewed rule for that case.

## Reference and corpus

- **FFmpeg pin.** `cargo xtask prepare-ffmpeg` builds FFmpeg from a pinned source
  revision with recorded configure flags, compiler and binary hashes.
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
make prepare-check   # build the FFmpeg reference, fixtures and adapters
cargo xtask check    # format, test, clippy, release build, harness tests, design checks
make report          # correctness report against the reference
make qualify         # correctness, runtime and memory with acceptance
```

`cargo xtask report` runs the correctness comparison of one feature contract
(`--contract`, `--manifest`, `--corpus`) and grades it; `--benchmark` adds paired
runtime and `--memory` the heap and RSS measurement under the native observers
(`harness/allocations*.c`), including the Linux link-closure and active-window guard
certificates. The report records the build identity, source provenance digest and input
hashes it was measured with; when a result is cited, its source revision and raw-sample
SHA-256 go in the [measured revisions](status.md#measured-revisions) table. `cargo xtask compare` compares a single input, `cargo xtask external-vector`
qualifies the mandatory external vector and `cargo xtask fate-run` runs one supported
upstream FATE case. `cargo xtask build-workers` builds the workers and records the build
identity every collection is checked against. `cargo xtask trace-identities` records the
digest of every candidate trace, so a change can show its traces are byte-identical
(`--compare` an earlier record). The independent oracles stay in Python
(`harness/*_oracle.py`, run through `python3 -m harness.oracles`).
`cargo xtask fuzz` runs a deterministic, replayable byte-mutation campaign against
debug, release and reference workers from a pinned manifest.

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
outside the repository.

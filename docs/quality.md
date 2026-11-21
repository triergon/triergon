# Quality: reference, gates and harness

Every module is measured against a pinned reference implementation doing the same
work: FFmpeg for containers, framing, subtitle decoding, codecs and capture, FreeType
for font tables and outlines, HarfBuzz and FriBidi for shaping and bidi, and libass
for rasterising and subtitle rendering. The gates and measurement rules below are the
same for every reference. A module is complete only when it passes all three gates on
every case in its corpus. Results are in [status](status.md).

## Gates

| Gate | Requirement |
|---|---|
| Parity | Every declared feature matches the reference on every required case. Missing inputs, crashes, unsupported declared cases and unexplained differences are failures, never omitted rows. |
| Runtime | `T_triergon / T_reference` at most **1.00** per case, using the upper bound of a paired 95% confidence interval ([early stop](#early-stop)). The optimisation target is **0.80**. Both values are defined in [`harness/policy.json`](../harness/policy.json) and nowhere else; `cargo xtask check` keeps this line and every status row in step with it. |
| Memory | Peak live heap and total resident peak RSS both strictly lower than the reference, each at the upper bound of a paired 95% interval ([early stop](#early-stop)). |

The harness reads these limits from `harness/policy.json`. Each feature's contract and
corpus manifest live in `harness/features/<feature>/`. RSS growth from the start of the
operation is reported separately when every pair has positive growth bounds.

The work compared must be identical: the same selection, the same packets, the same
output. A gate is never passed by narrowing the work, dropping slow cases or weakening a
comparison. A failed result stays recorded against the exact source and binaries that
produced it.

The reference is the compatibility reference, not the specification. Disagreements are
investigated against the standard. A reference bug or a deliberately stricter resource
limit becomes an explicit, reviewed rule for that case.

## Reference and corpus

- **Pinned downloads.** Reference sources and corpus files are fixed by their SHA-256.
  Each may list mirrors, tried in order; bytes that don't match the pin are refused.
  With `TRIERGON_DOWNLOAD_CACHE` set, verified downloads are kept there under their
  hash and reused before any network access. CI keeps this cache between runs.
- **Reference builds.** Each prepare command keys its build by the SHA-256 of what it
  depends on (its xtask code with pinned hashes, flags and link steps, the link policy,
  its directory, the builds it links, the compilers, the installed packages and the
  compiler environment), recorded in `reference-key.json`. A prepare whose directory
  holds its key does nothing, one whose key is packed in
  `.research/reference-cache/NAME/KEY.tar.zst` unpacks it, and only a miss builds.
  `cargo xtask references keys|pack|fetch|upload` lists keys and moves packs to and
  from `qualification/reference-builds/` (with `TRIERGON_EVIDENCE_ARCHIVE`). A pack is
  not evidence and expires 90 days after upload.
- **Reference pins.** Each prepare command builds its reference from a hash-checked
  source and records the configure flags, compiler and library hashes in its
  `build.json`; `cargo xtask build-workers` then links the reference workers
  (`harness/reference*.h`) of every prepared build.

  | Reference | Prepare | Builds | Features |
  |---|---|---|---|
  | FFmpeg git `d32b387f` (9.0 branch) | `prepare-ffmpeg --profile P` | `matroska` (container harness), `formats` (MOV, TS, HLS segments, playback; 12 demuxers), `video` (Matroska with the H.264 and HEVC decoders), `transport-dts` (MPEG-TS demuxer and DTS parser), `subtitles` (PGS, DVB, DVD, closed-caption and teletext decoders, with libzvbi 0.2.46) | containers, framing, playback, [DTS transport](../harness/features/mpegts-dts-core/contract.json), subtitles |
  | FFmpeg 9.0.2 release | `prepare-ffmpeg --profile P` | `decode` (its `ffmpeg` and `ffprobe` also decode capture output), `capture` (gating), `capture-small`; the capture builds link FFmpeg's libraries into the worker so the observer audits them | codecs, capture |
  | mpg123 1.33.7 | `prepare-mpg123` | static | MPEG-TS free-format audio |
  | FreeType 2.15.0 | `fonts prepare` | static, no optional dependencies | `font-outlines`, `font-hinting-none`, `font-hinting-default`; `font-hinting-modes` on demand |
  | FriBidi 1.0.17, HarfBuzz 14.6.0 | `shape prepare` | static, HarfBuzz from its amalgamation, `-O2 -ffp-contract=off` | `shape-libass`, `shape-text`, `shape-bidi`, `shape-simple`, `shape-unicode`, `shape-extents`; `shape-text-all` on demand |
  | libass 0.17.5 | `raster prepare` (`ass prepare` is the same) | static, against the builds above, `--disable-fontconfig --disable-coretext --disable-directwrite --disable-libunibreak --disable-require-system-font-provider`, `-O2 -ffp-contract=off -ftrivial-auto-var-init=zero` | `raster-glyphs`, `raster-outlines`, `raster-bitmaps`, `ass-parse-scripts`, `ass-parse-chunks`, `ass-parse-events`, `ass-render`, `ass-parse-host`, `ass-render-host`; `raster-sweep` on demand; `raster-simd` diagnostic |

  `cargo xtask fonts|shape|raster|ass compare` reports a family's required features,
  `--all` adds those on demand and `--diagnostic` the diagnostic ones. Acceptance of the
  shaper rests on `shape-libass`, `shape-bidi`, `shape-simple` and `shape-unicode`. A
  candidate worker carries only the runner families its reference can demux; a
  contract's `candidate` names its binary.
- **Instrumented references are never measured.** The `subtitles` worker wraps
  (`--wrap`) the C calls whose result can differ between targets for the same arguments
  (`strtol`, `strtoul`, `sscanf` integer conversions, out-of-range rounding), and the
  ASS reference (`harness/reference_ass.h`) compiles libass's rendering, drawing and
  parsing sources into itself with checks of its own. Their features name an
  uninstrumented `measurement_reference` (`ffmpeg-subtitles-plain-native`,
  `libass-player-native`); `report`, `grade` and the case evidence refuse runtime or
  memory against an instrumented reference, and `cargo xtask check-design` requires a
  `measurement_reference` to be an uninstrumented build of the same pinned libraries.
- **Results C leaves to the target.** The instrumented references flag a case that
  reached such a result (`harness/reference_subtitles_host.h`; for ASS an out-of-range
  or NaN `double` to `int` conversion or a drawing coordinate at `INT_MIN`). A flagged
  case fails every feature not pinned per target and belongs in one that is
  (`subtitle-dvd-host`, `ass-parse-host`, `ass-render-host`), where the expected result
  follows the reference built with GCC for 32-bit ARM hard-float.
- **libass configuration.** The rasterising and rendering references use libass's plain
  C bitmap engine (`ass_bitmap_engine_init(ASS_CPU_FLAG_NONE)`) on every host, as libass
  runs on 32-bit ARMv7, checked at startup; `raster-simd` compares the host's own SIMD
  engine with those pins. Zeroed automatic variables give the value both targets agree
  with where the pinned source reads one uninitialised; the reference workers' own code
  fails the build on such a read (`-Wall -Werror -Werror=uninitialized`). The ASS
  reference includes `ass_render.c`, `ass_drawing.c` and a copy of `ass_parse.c` that
  `ass prepare` writes, and restates, citing them, the text loop of `parse_events` and
  `ass_read_styles` from memory. `ass-render` renders `harness/fixtures/ass-render` as a
  player embeds libass (attached fonts, no system font provider, no hinting, caches of
  1000 glyphs and 24 MiB), timed from the configured renderer to the last frame. When
  the rasterising features are timed, timing uses a build without
  `-ftrivial-auto-var-init` and keeps libass's per-run cache reset outside the clock.
- **Font probes.** TrueType behaviour no specification settles is established by
  probes: `harness/fonts/probes/*.probe` describe small faces, `cargo xtask fonts probe`
  assembles them and loads them through both workers, and each `.dump` beside a probe
  records the reference's load on 32-bit ARMv7 (`--record`). `cargo xtask fonts fetch`
  fetches the font corpus of `harness/fonts/corpus.json` (sources, licences, hashes).
- **Generated manifests and pins.** The font, shaping, rasterising and ASS features
  commit no case rows: `cargo xtask fonts|shape|raster|ass manifest` generate them
  (`xtask/src/generators.rs`) and take each case's expected work from
  `harness/features/<feature>/pins.json`, the reference's folds. Re-record pins with
  `COMMAND manifest --record` after `build-workers`, never by hand; `cargo xtask
  check-design` fails when case ids and pins differ. A diagnostic feature that compares
  another reference with these pins names them in `cases_from`.
- **Contract fields.** `gate` is `"required"` (run by `cargo xtask check`),
  `"on-demand"` or `"diagnostic"`. `timeout_seconds` (1 to 3600, default 30) limits one
  worker run; `parity_timeout_seconds` limits `report`'s untimed parity runs alone.
  `required_targets` names the targets every case must pass on: every gated feature
  names Linux AArch64 and 32-bit ARMv7, and accepting a module needs both targets'
  evidence. With `"pinned_per_target": true`, `pins.json` holds a fold per target
  (`unpinned_targets` lists exceptions with a reason), and `COMMAND manifest --record
  --target TRIPLE --reference-worker PATH` records another target's. A written manifest
  pins per target in `expected_by_target`. `cargo xtask subtitle-fixtures
  verify|record FEATURE REFERENCE` checks or writes a subtitle manifest against the
  independent FFmpeg worker (see the [subtitle worker
  guide](../crates/subtitle-worker/README.md#recording-and-verifying-subtitle-fixtures)).
- **Open cases.** A contract may list `open_cases`, each tied to one of its
  `pending_module_requirements`. `report` and `grade` still run them, pass only while
  every listed case still fails and every other case passes, and refuse a listed case
  that passes. A module stays unaccepted while any case of a gating feature is listed;
  additional-coverage features (`shape-text`, `shape-text-all`) do not block it.
- **Bounds.** `grade` fails on a case whose runtime bound is above the ceiling, whose
  heap or total RSS bound is not below 1, or whose measurement is not valid. A contract
  may list `open_bounds` (`bound`, `cases`, measured `cause`, optional `targets`); such
  a bound fails as `open as listed` without failing the step, and one that holds is
  printed for removal. Suites without a grade apply the same verdict to their own
  collections.
- **Input rights.** A case is measured only when its `rights` are `authored-synthetic`,
  `reviewed-licence` with a `licence` under which a reviewed input manifest
  (`harness/fonts/corpus.json`) records the file's hash, or `fetched-not-redistributed`
  for an upstream regression sample (or a local remux of one) whose hash
  `harness/fate/samples.json` records: fetched into the ignored cache, compared locally,
  never redistributed. Any other case fails as `rights-unreviewed`.
- **Authored corpus.** Small canonical streams from synthetic signals and authored
  structures, identical across encoder versions (`cargo xtask fixtures`,
  `harness/features/<feature>/{contract,corpus}.json`).
- **FATE.** Upstream regression identities are indexed as data without running Make
  (`cargo xtask fate-inventory`, `harness/fate/index.json`) and mapped case by case. A
  FATE name is not a test until its input, operation and expected result are mapped and
  run.
- **Required vectors.** Larger external vectors are fetched and hash-verified by `cargo
  xtask fetch-vectors` and kept outside version control.

## Correctness by module family

| Family | Compared |
|---|---|
| Time | `cargo xtask core-time` compares exact rational arithmetic, rounding, overflow, signed extremes and wide arithmetic against independent Python integer and Fraction results in debug and release builds. Workspace tests also cover 32-bit narrowing. |
| Demux | Track identity and configuration; payload hashes in per-track decode order; signed PTS, DTS, duration and trim; random-access facts; side data, metadata, attachments and chapters |
| Seek | Landing point, preceding random-access point, preroll and the packets that follow; fresh, repeated, backward and after-EOF seeks; missing indexes; changing input windows; cancellation |
| Packet framing | Equivalent access units after documented framing normalisation; configuration and payload preserved |
| Subtitles | Exact cue timing, text and flags; normalised bitmap rectangles, palettes and alpha |
| Decoders | Exact output where the standard guarantees it; otherwise predeclared per-format numeric tolerances |
| Encoders | Standards compliance, independent decode, quality at equal bitrate, throughput and memory |

## Harness

The harness runs the same operation through a Triergon worker and a reference worker and
compares canonical output. Workers speak one line protocol with `trace`, `bench` and
`memory` modes, framed by begin/end markers and a named digest.

```sh
cargo xtask prepare-check   # build the pinned references, fixtures and workers
cargo xtask check           # every part below
cargo xtask report --fate-cache .research/fate-samples
                            # correctness report against the reference
cargo xtask report --benchmark --memory --fate-cache .research/fate-samples --require-acceptance
                            # correctness, runtime and memory with acceptance
```

`cargo xtask check PART` runs one part, after `cargo xtask prepare-check PART` prepares
what it reads: `lint` (format, lints, rustdoc and the repository checks), `test` (the
workspace tests), `variants` (the reference-trace tests, the release build, the nightly
SIMD tests and the core time oracle) and `parity` (the worker tests, the required
feature reports, the fuzz smoke rounds and the link orders). CI
([check.yml](../.github/workflows/check.yml)) runs the four parts as separate jobs and an
`armv7 tests` job: the `checked`-profile tests of `triergon-framing`, `triergon-isobmff`
and `triergon-mpegts` on `thumbv7neon-unknown-linux-gnueabihf` under user-mode QEMU,
whose budget tests pin storage sizes. A pull request that changes only files the
documentation checks read ([tools/ci-scope.py](../tools/ci-scope.py): files under
`docs/` or `harness/evidence/sets/`, or Markdown edited in place outside `harness/`,
`tests/` and `fixtures/` that no Rust source includes) runs `cargo xtask check docs`
instead (format, check-design, check-links, check-revisions and the evidence index
tests). The required `check` job passes when the jobs of its scope passed.

Commands: `report` compares one feature contract (`--contract`, `--manifest`,
`--corpus`) and grades it, `--benchmark` adding paired runtime and `--memory` heap and
RSS; a contract may name its own `candidate` and reference, whose workers take one input
per operation through its `worker_operation` prefix (`PREFIXfold`, `PREFIXbench`,
`PREFIXmemory`). `isobmff-bench`, `isobmff-seek`, `framing` and `progressive` run their
suites; `compare` one input; `external-vector` the mandatory external vector; `fate-run`
one FATE case; `trace-identities` records every candidate trace's digest; `worker-tests`
runs the built workers against the reference workers and the independent Python oracles
(`harness/*_oracle.py`). `codecs` and `capture` grade, time and measure the codec crates
and the capture pipeline ([harness/codecs/STATUS.md](../harness/codecs/STATUS.md),
[harness/capture](../harness/capture/README.md)); codec timing and memory require passing
parity. `fuzz` runs a deterministic, replayable byte-mutation campaign against debug,
release and reference workers (`--contract` for a feature's cases; `codecs fuzz --lane
L` for a codec lane's FATE vectors). `build-workers` builds the workers and observers
and records the build identity every collection is checked against (`--only WORKER`
rebuilds `media-worker` and that worker alone); `memory-admission CANDIDATE REFERENCE`
checks a pair's libraries against the Linux observer.

Memory is measured under the native observers (`harness/allocations*.c`), which count
`malloc`, `calloc`, `realloc`, `free`, `posix_memalign`, `aligned_alloc`, `strdup` and
`strndup`. On Linux a worker is measured only if every allocation entry point it and its
libraries import is counted, guarded, an operator `new` or `delete` form the probe ran
from the one library that defines them all, or a weak import nothing defines. Inside
the operation the observer admits read-only file mappings and their exact `munmap`,
counted in total RSS and reported as `file_mappings` and `file_mapped_bytes`; any other
mapping voids the measurement. A report records the build identity, source provenance
digest and input hashes; a cited result's source revision and raw-sample SHA-256 go in
the [measured revisions](status.md#measured-revisions) table.

### Separate timing and memory collections

`cargo xtask grade TIMING MEMORY --contract F --manifest F --output DIR` joins complete
reports and recomputes the bounds from their raw paired samples; it reruns no workers.
Both collections must hold every case with identical parity, inputs, contract, workers,
source graphs, compiler settings and instrumentation; only the CPUs and a kernel RSS
proof pin may differ. Pass `collection.json` with its `measurements/` directory to grade
large collections one case at a time, and use a new `--output` directory for each run.
ISO BMFF collects `--benchmark` and `--memory` separately and grades parts with `grade
--isobmff-part I/N`. `--additional-report PATH` supplies an external grade;
`--require-slice-acceptance` requires the feature gates, `--require-acceptance` also the
FATE coverage and module requirements.

Timing requires the RSS kernel module unloaded throughout. Kernel-backed memory follows
the [kernel proof workflow](../harness/rss-bound/README.md) after timing, with the
module loaded through collection and grading; the workers rebuilt with the proof must
have the same hashes.

## Measuring performance

- **Gate build.** Gate results come from workers compiled by the toolchain in
  `rust-toolchain.toml` with no extra compiler configuration. One exception is pinned:
  `codec-aac-worker` (the AAC decode operations alone, through `--candidate-for
  audio-aac=` and `--codec-worker-for audio-aac=`) and the AAC decode example are built
  by the dated nightly (`NIGHTLY`, as `cargo xtask check-simd` uses) with the decoder's
  `simd` feature, because on 32-bit ARM its SBR QMF banks need explicit `std::simd`.
  Every other worker stays on the stable pin. `build-workers` refuses another `rustc`,
  `RUSTFLAGS`, `CARGO_ENCODED_RUSTFLAGS`, `RUSTC`, `RUSTC_WRAPPER`, target or profile
  overrides and equivalent Cargo configuration, and the grader refuses a report that
  records any of them or doesn't say. `--comparison-build` (and the opt-in HEVC `simd`
  lane) builds workers for comparisons only, never a gate result.
- **Link policy** (`static-toolchain-runtime-trained-order-v1`). On GNU/Linux each side
  links its toolchain's runtime support libraries statically: `libgcc_eh` for the Rust
  workers; libgcc, libstdc++ and libatomic for the C and C++ references. libc with libm
  and third-party libraries such as zlib stay shared on both sides. Elsewhere both
  sides link as their toolchains do.
- **Entry point.** Every Rust worker enters through C's `main`, as the C references do,
  so a measurement compares the library's work and not the Rust runtime's start-up.
  Libraries forbid unsafe code; worker packages deny it, and each worker binary's C
  entry point, `#[allow(unsafe_code)] #[unsafe(no_mangle)] extern "C" fn main`, calling
  the safe [`triergon_worker_entry::enter`](../crates/worker-entry), is its one allowed
  exception, which `cargo xtask check` admits once per worker binary. A worker gives up
  the standard start-up's stack-overflow message, `SIGPIPE` handling and reopening of
  closed standard streams; a panic still ends with status 101.
- **Trained link order.** On GNU/Linux a Rust worker with a trained order for the target
  is linked by the pinned toolchain's `lld` with the functions its order lists first,
  then every other function in module order, so its result reflects the linker, the
  module order and the trained order together; other workers and every reference link
  as above. `harness/link-order/training.json` lists the training files by SHA-256 (kept
  as an evidence set unpacked at `.research/link-order-training`, made by `cargo xtask
  link-order corpus OUT`) and the operations that train each worker on each target.
  `cargo xtask link-order train` records `harness/link-order/TARGET/WORKER.txt` on a
  prepared gate host from the functions each operation first runs under QEMU user mode
  (`qemu-arm -cpu cortex-a15`, `qemu-aarch64 -cpu neoverse-n1`, `-d page,in_asm`), after
  `link-order separate` checks that no training file shares a SHA-256 with a gate input
  or committed file. `build-workers` fails when committed orders disagree with the
  manifest or a worker holds fewer than 95% of its order's names, and records each
  order in `identity.json`; `cargo xtask link-order BINARY [OUT] --trained FILE` writes
  one binary's ordering file. The linker arguments, after the policy's own, are

  ```text
  -C link-arg=-B$(rustc --print sysroot)/lib/rustlib/HOST/bin/gcc-ld
  -C link-arg=-fuse-ld=lld
  -C link-arg=-Wl,--symbol-ordering-file=ORDER
  -C link-arg=-Wl,--no-warn-symbol-ordering
  ```

  where `HOST` is the host triple of `rustc -vV`. `report` and `grade` refuse a build
  that records another policy or none, or whose workers disagree with the committed
  orders.
- **Release profile.** Gate results assume the workspace release profile, `lto = "fat"`
  and `codegen-units = 1`. A program that depends on these crates matches them only
  with the same settings and, on GNU/Linux, linked in the playback worker's trained
  order (the [README](../README.md#getting-started) gives the commands). `panic =
  "abort"` is safe: no library code catches or relies on unwinding.
- **Paired runner.** `report`, `external-vector`, `codecs time` and `capture qualify`
  time through one runner: it holds the host-wide benchmark lock, checks one warm-up of
  each worker, proves the RSS kernel module absent, and retries a contaminated session
  up to twice on the next seeds, keeping the rejected attempts. One validator checks
  every session's pairs, hashes, clean CPU records, equal work and reproducible interval.
- **Samples.** A sample is one worker run of the declared iterations, timed in-process
  from the first iteration to the last (for capture, `--iterations` complete sessions).
  Use 30 or more randomised A/B pairs per case after warm-up, keep every raw sample, and
  report p50, p95 and the confidence bound per case; an aggregate never hides a failing
  case. `--iterations` overrides the contract's count for diagnostic runs only.
- **Equivalent work.** Time optimised builds doing equivalent work. For demuxing the
  timed operation is open, probe, the required reads and packet production for the same
  selection; process startup and serialisation are outside it.
- **Isolation.** Measure on an isolated machine. Never time while compiling, fuzzing or
  running another benchmark; the runner rejects a run in which a tracked process uses
  CPU or steal time grows. On Linux a timing session refuses to start unless the process
  may use exactly one CPU (`taskset -c CPU`). Allocation tracking and timing are
  separate runs.
- **Executables.** Timing and memory measure freshly written copies of both workers at
  paths of equal length (one pair per [memory slot](#memory-in-parallel)). Before every
  Linux memory run both copies are written back and their cached pages dropped (`sync
  FILE`, then `dd iflag=nocache`); a failed eviction fails the run. Keep the measured
  executables with the raw memory samples.
- **Comparing revisions.** Build both on the same machine and run `cargo xtask report
  --benchmark --baseline-worker W` from the changed tree, `W` being the same worker from
  the base tree. Correctness is still compared with the reference; pairs and ratios are
  against `W`. Such a report is never an acceptance run.
- **Instruction count first.** `cargo xtask count CANDIDATE REFERENCE ARGUMENT...` runs
  both workers on one workload under `perf stat`, five runs each, prints the least
  user-mode instructions, cycles and branch misses, and fails when instructions rise
  more than 0.25%. Passing it says nothing about speed; the timing pass decides. On a
  stock host run `sudo sysctl kernel.perf_event_paranoid=1` first.
- **Quick check.** A change not meant to affect speed runs `cargo xtask same-code` on
  both workers, then one round of 30 pairs against a main build for every contract the
  change reaches, graded by `cargo xtask screen`, which flags a case above 1.5% or a
  group median above 0.75%. A flagged case with unchanged functions and instruction
  counts is function placement. A screen never replaces a recorded result.
- **Targets and hosts.** Qualification runs natively on AArch64 Linux and on 32-bit
  ARMv7 Linux in an armhf user space ([below](#32-bit-armv7)); results on one platform
  do not establish another. Both targets qualify on one host class, the AWS c6g.4xlarge
  (Graviton2, 16 vCPUs, 32 GiB). Runtime from another class is not comparable, because
  another core moves the ratio of the same instructions. Every row of
  [status](status.md) names the class it was measured on.

### Running the gates

`cargo xtask gate` holds the full gate as one plan of items, each the work for one
measured thing (a feature's timing, memory and grade, a suite, a codec workload list's
timing or memory part, a capture case, the `checked` and release tests, a comparison
family). A contract with `"runner": "cargo xtask report"` adds its feature; other
runners and exclusions are listed in `xtask/src/gate.rs`, and the planner rejects a
feature with neither. Both targets run every item. `cargo xtask gate plan --shards N`
splits the items into N shards of about equal planning weight, one per host.

`--items NAME,...`, given to every gate command of a run, narrows the plan; `gate
summary` then writes `"partial": true` with the selection. A result cited as the full
gate is a summary whose `partial` is false. A change that reaches a few items gates them
alone, one host per target, from a checkout at its head:

```sh
tools/gate-aws.sh run aarch64 1 HEAD .research/gate-aarch64 --items matroska-startup-seek,matroska-audio-clocks
tools/gate-aws.sh run armv7 1 HEAD .research/gate-armv7 --items matroska-startup-seek,matroska-audio-clocks
```

`cargo xtask gate plan --target TARGET --items NAME,...` prints what a selection runs.
A change reaches the items whose contract, corpus or inputs it changes and those whose
workers link a crate it changes (`cargo tree -i CRATE`). For later runs to reuse a run's
results, add each `OUT/shard-I/entry.json` to the evidence index and promote it in the
pull request that cites the run.

A step that exits 0 with a case it did not measure fails, and `gate run` records its
verdict in the step's `gate-result.json`. On native Linux with glibc, the gate build and
a gate target, the codec gates fail on a workload whose bound does not hold unless the
list's `open_workloads` names it with its `cause`; elsewhere those bounds are
diagnostics. `memory_timeout_seconds` gives `codecs memory` its per-target run limit
(30 s when absent).

`cargo xtask gate check --shards N` runs on any machine before a host exists and fails,
naming the item and path, on any input a prepared host would not have; `tools/gate-aws.sh
run` runs it first.

Prepare every reference first, in this order (libass links the FreeType, FriBidi and
HarfBuzz builds); `build-workers --all-references` exits 1 without writing a build
identity when a profile is not prepared:

```sh
for profile in matroska formats transport-dts video subtitles decode capture capture-small; do
    cargo xtask prepare-ffmpeg --jobs 8 --profile "$profile"
done
cargo xtask prepare-mpg123 --jobs 8
cargo xtask fonts prepare --jobs 8
cargo xtask shape prepare --jobs 8
cargo xtask ass prepare --jobs 8      # the libass build, as raster prepare makes it
cargo xtask build-workers --all-references
```

On one host, a shard's run is:

1. `tools/gate-host.sh ready I/N`: the prepare commands, `build-workers
   --all-references`, and `cargo xtask gate preflight`, which runs `cargo xtask
   preflight` over everything the shard measures with the [committed RSS module
   review](../harness/rss-bound/README.md#proof) for the running kernel (`--review
   FILE`, `--reference NAME`, `--standard-memory`; see `cargo xtask preflight --help`).
   It checks the workers and references against the build identity, every case's
   reference and pinned inputs, numpy for the capture generator, the review and that
   the module is unloaded, printing every failure at once; `prove.sh` then runs once.
2. `tools/gate-host.sh run I/N OUT` runs the parity steps, then times the shard alone on
   one CPU with the module unloaded (`taskset -c 1`, `gate run --stage timing`).
3. It [proves](../harness/rss-bound/README.md) the kernel bound, rebuilds the workers
   with the proof, checks them with `gate preflight --kernel`, and collects memory on
   the remaining CPUs (`taskset -c 1-15`, `gate run --stage memory`); the capture
   memory steps run as one `capture qualify` batch (`memory-capture`).
4. It grades with the module loaded, several grades at once.

Never time while memory, a build or anything else runs on the host. `cargo xtask gate
summary --output FILE DIR...` joins the shards into one result; it fails when a planned
step did not run exactly once, a step failed, or the shards measured different sources
or timing builds. To grade one feature by hand:

```sh
cargo xtask grade OUT/benchmark/mp4-forward/report.json OUT/memory/mp4-forward/report.json \
    --contract harness/features/mp4-forward/contract.json \
    --manifest harness/features/mp4-forward/corpus.json \
    --output OUT/graded/mp4-forward
```

#### Early stop

A gate timing session (`report`, `isobmff-bench`, `isobmff-seek`, `external-vector`,
`codecs time`, `capture qualify`) may end after 20 of its 30 pairs, taken as two blocks
of ten, when every ratio `T_triergon / T_reference` of the second block and at least
eight of the first lie strictly below the ceiling and the lag-1 autocorrelation of the
20 ratios is at most 0.42. A gate memory collection stops only when its peak heap and
total resident ratios each meet the same conditions under 1. Otherwise all 30 pairs run
and the paired bootstrap grades them. A stopped session records the rule (`early_stop`)
and, as its interval, the median of its 20 ratios bounded by their 3rd and 18th
smallest. The validator and memory evaluator replay the rule on the recorded pairs, and
the record must name the ceiling of [`harness/policy.json`](../harness/policy.json).
Packet framing, comparisons with another build (`--baseline-worker`, quick checks) and
any gate command given `--no-early-stop` run every pair. The rule is a sign test that
keeps the one-sided 5% false-pass guarantee of a 95% bound for independent pairs; its
replays, simulations and runs are evidence sets `gate-early-stop-two-blocks`,
`gate-early-stop-runs-aarch64` and `gate-early-stop-runs-armv7`, over the archived
sessions `armv7-thumb2-3df7ae6d-linux-armv7`, `runners-gate-build-linux-arm64`,
`mpegts-pes-gate-build-linux-arm64` and `isobmff-armv7-runtime-regate-linux-arm64`.

#### Memory in parallel

Memory collections run side by side, one slot per CPU the process may use (at most 32,
the sessions the kernel RSS bound admits), each slot's thread pinned to its CPU and
measuring one case at a time from its own copies of the worker executables; only those
copies are evicted before each run. Cases that read a common file (input, ordered
entity or companion, or workers measured from their own paths) run in the same slot one
after another. `capture qualify --stage timing` runs correctness and timing alone on one
CPU, and `--stage memory --timing DIR` then collects every case's memory in one
collection from those records, each case reading its own checked copy of its generated
input. The reference's memory is measured afresh in every pair. Parallel and serial
collections of the same worker bytes give the same verdicts (evidence sets
`gate-reference-memory-cross-host`,
`gate-parallel-memory-matroska`, `gate-parallel-memory-matroska-head`,
`gate-parallel-memory-isobmff-aarch64`, `gate-parallel-memory-isobmff-armv7`,
`gate-parallel-memory-capture-codecs-aarch64`,
`gate-parallel-memory-capture-codecs-armv7`,
`gate-run-aarch64-c82b4452-20261118-143205-shard-1`,
`gate-run-armv7-c82b4452-20261118-143302-shard-1`,
`gate-run-aarch64-f7b1d8eb-20261118-104434-shard-1` to `-5` and
`gate-run-armv7-f7b1d8eb-20261118-104533-shard-1` to `-5`).

Each collection records its slots (`memory_slots`) and the host's memory over it
(`host_memory`, from `/proc/meminfo` and `/proc/vmstat`). Run kernel-bound memory gates
on the qualified class; `preflight --kernel` refuses a host with less than 30 GiB of
`MemTotal`, because on smaller hosts another task's update of a worker's resident
counters can void pairs, which a collection measures again within the limit the [kernel
bound](../harness/rss-bound/README.md#what-it-measures) sets.

#### Reuse

Every gate command (`report --benchmark`, `report --memory`, `grade`, `isobmff-bench`,
`isobmff-seek`, `framing`, `progressive`, `external-vector`, `codecs time`, `codecs
memory`, `capture qualify`) derives a key before it measures and writes it, with its
exit status, to `gate-result.json`. The key covers the command and settings, the case
documents, every worker and shared library byte, the observer, every shared input of
the build identity, `xtask/src`, `harness/*.py`, the RSS module, `Cargo.lock`,
`rust-toolchain.toml`, `harness/policy.json`, the tool versions, the build environment,
the gate target and the host class; a `grade` key also binds both collections. When a
kept [evidence](#evidence) set lists a result with the same key, the command takes it
instead of measuring, writes a `gate-result.json` naming it as `reused`, and exits with
the recorded status; `--full` measures anyway. Gate hosts download sets through the
addresses `tools/gate-aws.sh` gives them (`cargo xtask evidence restorable`). A reused
collection beside a measured one cannot be graded; measure both with `--full`.

`cargo xtask gate test --profile checked|release` runs the workspace tests for the gate
target and is reused the same way, keyed by the profile, every tracked and unignored
file but the evidence index and `docs/`, and the test inputs. The `checked` profile is
the release optimisation with every overflow check and debug assertion.

### 32-bit ARMv7

The harness runs natively in a 32-bit ARM Linux user space on an AWS Graviton2 host
(Neoverse N1), whose AArch64 kernel runs 32-bit user spaces; later Graviton generations
cannot. The gate target is `thumbv7neon-unknown-linux-gnueabihf` (Thumb-2 with NEON, as
the armhf GCC that builds the references compiles by default), so its workers do not run
on an ARMv7 core without NEON. rustup's host toolchain there,
`armv7-unknown-linux-gnueabihf`, builds xtask only; `build-workers`, `check` and the
comparison commands build the workers and tests for the gate target (`rustup target add
thumbv7neon-unknown-linux-gnueabihf`).

The user space is the Ubuntu 24.04 armhf image archived as evidence set
`armv7-userland`. `tools/gate-host.sh setup armv7` fetches and hash-checks it, unpacks
it at `/srv/armhf`, binds `/proc`, `/sys`, `/dev` and debugfs into it at every boot, and
installs Rust with the gate target and the packages the `subtitles` reference's libzvbi
build needs (`autoconf automake libtool autopoint gettext`); every command then runs
inside it under `setarch linux32` as user `ubuntu`. The image was made with `debootstrap
--arch=armhf --variant=buildd --components=main,universe noble` from
`http://ports.ubuntu.com/ubuntu-ports`, with `noble-updates` and `noble-security`
enabled, then `apt-get install build-essential pkg-config zlib1g-dev python3
python3-numpy make git zstd curl ca-certificates sudo jq time`. Compare a later image's
`dpkg-query -W` list before treating its results as comparable, and record the CPU,
kernel and package list with results.

The kernel RSS bound works for a 32-bit process through the module's 32-bit ioctl entry
point. Build and load the module on the host, bind debugfs into the user space, and run
`prove.sh` on the host from the repository inside the user space, naming its root:

```sh
sudo mount --bind /sys/kernel/debug /srv/armhf/sys/kernel/debug
cd /srv/armhf/home/ubuntu/triergon
harness/rss-bound/prove.sh .research/rss-bound /srv/armhf
```

Then build the workers with `TRIERGON_RSS_KERNEL_PROOF` and collect memory with
`TRIERGON_RSS_KERNEL_BOUND=1` inside the user space, as the [kernel proof
workflow](../harness/rss-bound/README.md) describes.

### Gate hosts

`tools/gate-aws.sh` runs gate hosts on AWS from a workstation with the `aws` CLI; the
hosts hold no AWS credentials. It needs bash 4 or later (on macOS, Homebrew's bash
first on `PATH`, or `/opt/homebrew/bin/bash tools/gate-aws.sh`).

`tools/gate-aws.sh image aarch64|armv7 REVISION INPUT...` builds a target's gate image
on a c6g.4xlarge: Ubuntu 24.04 with the build packages and Rust, held on the kernel the
committed RSS module review covers, for ARMv7 the armhf user space, and the repository
at REVISION with every reference build, fixture, vector, manifest and worker. Each INPUT
is a media-input archive (`s3:evidence/SET/SHA.tar.gz`, keys in the
[index](../harness/evidence/sets/)): evidence sets `gate-media-inputs` (FATE samples,
codec workload inputs, subtitle corpus), `codec-file-workload-inputs`
([file-workloads.json](../harness/codecs/file-workloads.json)), `ogg-reader-inputs`,
`subtitle-cc-inputs` and `codec-derived-streams` (made by `cargo xtask codecs derive`).
Rebuild the image when reference keys or media inputs change.

`tools/gate-aws.sh run TARGET N REVISION OUT`, in a clean checkout at REVISION, runs
`gate check`, launches N hosts from the image, unpacks each `--input s3:KEY`, and runs
shard I on host I. Each shard's outputs are archived under
`qualification/gate-runs/RUN/shard-I/` (expiring after 90 days unless promoted),
`OUT/shard-I/entry.json` holds the index entry, logs and step records are copied to OUT,
and the host is terminated. The command exits non-zero when a shard did not finish or
the summary fails. SSH is open only to `TRIERGON_SSH_CIDR`, the workstation's current
public address as a /32; hosts power off 72 hours after boot.

## Robustness

Malformed input must never panic, hang or escape its budget. Tests and fuzzing cover
truncation at every structural boundary, corrupted lengths, counts and timestamps,
deep nesting, decompression bombs, duplicate identifiers and conflicting indexes.
A transport simulator covers short reads, delayed ranges, changing length, exact end of
file, reconnect failure and cancellation. Results are reproducible from a saved seed
and edit recipe.

## Evidence

A result names its exact source revision, worker binaries, reference build, corpus and
input hashes, platform and all per-case outcomes. Summaries go in the pull request that
changes the result and in [status](status.md), whose [measured
revisions](status.md#measured-revisions) table names the revision and raw-sample SHA-256
behind each cited result. Raw samples are archived by content hash outside the
repository; [harness/evidence/sets/SET.json](../harness/evidence/sets/) indexes each set
(key, SHA-256, kept or expiring, each file's path and SHA-256). Sets under
`qualification/` expire 90 days after upload; sets under `evidence/` are kept. With
`TRIERGON_EVIDENCE_ARCHIVE` set:

1. `cargo xtask evidence pack DIR OUT` writes the deterministic archive of a run
   (`.tar.gz`, or `.tar.zst` through the system `zstd`).
2. `cargo xtask evidence upload SET OUT` uploads it under `qualification/SET/` and
   prints its index entry, listing the [gate results](#reuse) it holds; with its reason
   filled in, it is `harness/evidence/sets/SET.json`. `cargo xtask evidence entry SET
   OUT` prints the entry without uploading.
3. `cargo xtask evidence promote SET` moves a set under `evidence/` and records it as
   kept. Promote a set in the pull request that first cites it; an uncited upload
   expires.
4. `cargo xtask evidence fetch SET` downloads, verifies and unpacks a set into
   `.research/evidence/SET/`. Packing and fetching stream, so a set larger than memory
   unpacks on a 32-bit host.

[harness/evidence/index.json](../harness/evidence/index.json) records where keys lie
and how identifiers were renamed. A result key is listed by one kept set at most.

Every revision status cites stays reachable from `main`: `cargo xtask check-revisions`,
part of `check`, fails when `HEAD` does not reach a cited commit. A pull request that
cites its own commits merges with a merge commit; a cited commit off `main` joins it
through a merge that keeps `main`'s tree (`git merge -s ours`). An abbreviated revision
must identify one commit in the checkout history. Keep a bundle of the measured source
with the archived result.

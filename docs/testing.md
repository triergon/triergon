# Shared correctness, FATE, performance and memory framework

Status: the frozen shared-core K0–K5 Matroska snapshot passed all 116 native Mac ARM64 parity/time/memory gates; see [commands and current evidence](implementation.md). Later source or dependency changes require affected qualification. The earlier candidate-comparison spike remains separate. No complete production module has passed all gates.

## One registry for every selected module

Each module registers its immutable feature contract, reference operation, supported profile matrix, corpus manifest, canonical result schema, comparison rules, fuzz targets, absolute resource limits, benchmark workloads and release targets. `research/module-gates.json` captures the current gate constants, qualified slices and remaining module work. A missing adapter, missing input, missing rights evidence, crashed worker or unsupported declared case is a visible non-pass, never an omitted row.

Run the same operations against our implementation and an external pinned FFmpeg reference worker. Container/codec/processing crates share the harness protocol; module-specific adapters produce a canonical, versioned event stream. Keep reference code and any C shim in separate tooling processes, outside the pure-safe production graph. The Python control plane may launch tools, but may not synthesize outputs for a failed candidate.

## FFmpeg pin and test reuse

FFmpeg's regression system is FATE. Its documentation describes configuring a sample directory, `make fate-rsync`, `make fate-list` and `make fate`; individual cases can be selected. These operate on a configured FFmpeg build, not directly on a Rust library. Sources: [FATE guide](https://www.ffmpeg.org/fate.html), [upstream FATE documentation](https://github.com/FFmpeg/FFmpeg/blob/master/doc/fate.texi).

Use an immutable FFmpeg source revision and recorded configure flags/compiler/binary hashes. The initial definition reference is tag n9.0, resolved in `research/source-pins.json`. The local exploratory spike used the installed FFmpeg/ffprobe **9.0.2**; it is not a controlled n9.0 performance baseline. Both the bundled Streem reference configuration and a full-feature reference build may be needed; each test states which binary supplies the required operation.

The initial `inventory-fate.py` indexed 177 identities from four n9.0 `.mak` files: Matroska, MOV, MPEG-TS and subtitles. This is a lexical inventory, not the configure-expanded runnable suite, not all of FATE, and not 177 tests executed. Some definitions are aliases/templates or need capability expansion. All entries remain `unmapped` with rights `unreviewed`.

The separate [formats inventory](../research/formats-fate-inventory.json) reconciles
those four definition files with the formats reference's configured list. Seven
lexical identities are configured in that build; none was executed by this inventory.
The index now follows case-local `SRC` references, retains unresolved Make paths and
deduplicates literal samples. It never evaluates Make or shell expressions. Historical
Matroska inventories and qualified receipts are preserved.

H0 must:

1. Obtain the configured upstream `fate-list` and reconcile its selected module cases with the lexical index, including conditionally enabled and generated names.
2. Map each applicable test's exact input, operation, expected result and comparison semantics to both adapters. Keep the upstream case ID and source location as provenance. Do not execute arbitrary downloaded make/shell strings in our runner.
3. Hash each sample and expected-output artifact; record acquisition location and distribution rights. Unknown rights block redistribution; use an external content-addressed cache rather than assuming that public samples share FFmpeg's source license. Do not mirror the full sample collection by default.
4. Reproduce the expected upstream result with the pinned reference before grading Rust. Classify cases as required/mapped, explicitly outside the frozen feature scope, or blocked. Unmapped cases inside scope prevent acceptance.
5. Keep a stable compatibility baseline plus a separately reported latest-upstream lane. A new FFmpeg revision cannot silently replace expectations used in a release claim.

Selected FATE examples to map include Matroska lacing, compressed subtitle tracks and unknown-length clusters; MOV edit lists, signed composition offsets, fragment overlap, initial non-keyframes and subtitle timing. Some FATE tests run whole decode/remux pipelines: reuse their vectors and semantics, then isolate the module under test. Test names alone are not equivalent coverage.

FATE is a valuable compatibility corpus, not a complete performance workload or proof that every valid file works. Add licensed codec conformance vectors, independently authored fixtures, recovered regressions, mutation/fuzz cases and streaming/network scenarios.

The [deterministic mutation runner](mutation-testing.md) provides pinned workers,
replayable byte edits, debug/release/native differential observations and bounded
process/output capture. Its findings are separate from qualification; stateful
scheduling, absolute resource and sustained target coverage remain open.

## Canonical correctness by operation

| Module family | Required comparison |
|---|---|
| Core/time | Exact rational arithmetic and rounding/overflow policy; unknown values; bounds and cancellation. Test signed extremes and 32-bit narrowing. |
| Demux | Track identities/configuration; payload hashes **in per-track decode order**; separate signed PTS/DTS/duration/trim; key/random-access facts; side data, metadata, flags, attachments and chapters. Do not sort packets by PTS or erase initial offsets to make a test pass. |
| Seek | Request, actual byte/track landing, preceding random-access point, required preroll, next packets, decoder recovery and output time. Include fresh seeks, after EOF, backward/forward repetition, missing indexes, changing input windows and abort. Compare decoded output from a common reference decoder after packet equivalence where relevant. |
| Packet framing | Compare equivalent semantic access units after a documented framing normalization; preserve configuration and payload. Normalizing legal 3-/4-byte Annex-B start codes is different from discarding NAL units or timing. |
| Subtitle | Exact cue timing/text/configuration and flags; normalized bitmap rectangle/palette/alpha outputs. Rendering outside the selected module is not part of a packet-extraction test. |
| Mux | Re-demux both outputs and compare packets, timestamps and metadata; file-byte identity only when the format and deterministic options make it meaningful. |
| Lossless codec | Exact decoded samples/pixels plus format, dimensions, channel mapping, delay and padding. |
| Lossy decoder | Normative conformance plus exact outputs where the standard/reference guarantees them; otherwise predeclared, justified per-format numeric tolerances. No perceptual-only escape for a wrong decoder. |
| Encoder | Standards compliance and independent decode; quality at equal bitrate, throughput and memory. Compare bitrate/quality curves and latency under matched features/presets; byte equality is generally not meaningful. |
| Filter / scale / resample | Match dimensions, precision, range, transfer, chroma siting, rounding, boundary handling and delay. Exact integer results when specified; explicit tolerances and signal metrics otherwise. |

FFmpeg is the benchmark and compatibility reference, not infallible specification truth. Investigate disagreements using standards and independent implementations. A documented reference bug or deliberate stricter resource rejection is an explicit case disposition; it cannot silently shrink the valid-input parity contract.

## Approved, bounded comparison rules

The [two-case decision](../research/decisions/2026-10-31-zlib-comparisons.md) keeps
complete source frames, bytes/private data and exact source timestamps as mandatory
independent assertions. The optional empty-PCM decoder view and the explicit
FFmpeg-plus-native-restoration reference apply only to their hash-bound K2 cases.
Native raw output and the exact expected disagreement remain recorded. A changed
input, source pin, comparison view, unexpected disagreement or selected-worker hash
fails the gate. The combined case's performance result names the augmented native
pipeline; other cases use native FFmpeg. All original numerical limits and required
profiles remain. These rules do not qualify a complete module or other inputs.

## Fuzzing, malformed input and streaming

Use parser-only and stateful open/read/seek/reset targets; truncate every structural boundary; mutate lengths/counts/timestamps; exercise nesting, overflow, decompression bombs, duplicate IDs and conflicting indexes. Metamorphic tests cover equivalent legal encodings, segmentation of the same byte stream, track interleaving, mux/demux round trips and irrelevant metadata insertion. Unknown extension data must stay opaque and bounded rather than disappearing.

A transport simulator injects short reads, delayed ranges, changing length, exact EOF, HTTP 416 at a known end, reconnect failure, cancellation and progressive input starvation. Never reinterpret a failed reopen as EOF unless the source really is at its declared end. Sequential and random-access profiles are separate capabilities; a pipe is not falsely made seekable by buffering the whole file.

Require no crash/panic/hang for in-policy input, bounded work between yields, no stale results after seek/reset, and no budget escape through retained packet/metadata clones. Run targeted integer/32-bit tests, mutation regressions and sustained fuzzing; Miri is supplementary, not a proof that a dependency is pure safe Rust.

## Matched speed gate

The owner originally clarified the optimization target as **at least 20% less
execution time**, not an 80% reduction. On 2026-10-31 the owner authorized a
separate [initial-delivery ceiling](../research/decisions/initial-delivery-runtime.md).

`R_time = T_rust / T_ffmpeg`; initial delivery requires the upper paired 95%
confidence bound to be at most **1.25** for every required benchmark
class/module/target. The optimization target remains **0.80**, corresponding to
at least 1.25× throughput at equal work. Report both classifications, p50, p95,
dispersion and the full per-case results; an aggregate average cannot hide a
failing required class. Freeze workload classes before optimization so selecting
only easy cases cannot manufacture success.

First establish correctness using independent canonical outputs. Then time equivalent work in optimized builds. For parsing/demuxing, primary operation timing includes module open/probe, the required reads/metadata work and packet production/teardown for the same selection. Do not compare a Rust parser with a CLI that decodes or serializes every frame. Keep process startup and JSON/hash serialization outside the core-operation timing, but report separate cold/end-to-end latency. For very small operations use persistent workers or batched calls so process overhead cannot dominate.

Pin source, binary/configuration, CPU features, optimization level, compiler, input hashes, warm/cold cache mode, thread count and worker topology. Test each implementation's best allowed configuration under the same machine-resource budget; do not disable FFmpeg optimizations merely to win. Randomize paired A/B order, warm up, collect enough repetitions for the confidence test, and preserve raw samples. Serialize measurements machine-wide, separate building/fuzzing from timing, record load/thermal/power state and reject contaminated runs. Do not infer TV performance from a Mac.

Checkpoints distinguish source comparisons processed in the current attempt from
unvisited cached observations. Both survive interruption. Replaying an early part
of the corpus must not erase a later clean result or permit a failed case to be
timed again unchanged. Final receipts contain fresh source comparisons; cached
timings still require identical workers, operation, contract and input, and memory
also requires identical calibration/observer binaries. Partial checkpoints always
remain unaccepted.

New build identities bind the selected worker's Cargo dependency closure, resolved
features, source/assets/test fixtures in its owned packages, shared build settings
and the explicit operation/harness inputs. Nested sources and binary assets are
included; generated build/cache files and Finder metadata are excluded. Workspace
membership and the whole lockfile remain separately recorded provenance: adding an
unrelated format does not change an existing worker's dependency closure. Selected
core/source changes still invalidate affected identities. The collector rechecks
source identity before accepting a final report. Historical schema-1 collections
retain their original whole-workspace identity rule and immutable receipts.

Timing and memory collection both reject active compiler/reference processes
before starting workers. Memory receipts also retain CPU snapshots around
calibration and measurement. CPU advancement invalidates the result while
preserving its raw observations; schema-2 acceptance requires this isolation
evidence. Interrupted or contaminated measurements may be retried. A clean
numerical failure remains evidence for that exact source and worker identity.

Both collectors retain terminal worker, unequal-work, observer and RSS-attribution
findings on an unchanged operation. Changing collection settings cannot replace a
complete clean measurement. A shorter diagnostic screen may advance to the required
pair count; changed observer/probe identities invalidate memory evidence separately
from timing. Reuse requires a different output directory and records the previous
report hash/path so earlier observations remain available.

Read/copy/allocation counters diagnose results, not replace elapsed-time evidence. Every required case must meet the current delivery upper runtime ratio of 1.25; a case above 0.80 still misses the optimization target. I/O-bound work remains in the equivalent operation and its timing evidence. Distinguish module wins from total player speed—hardware decoding and network waits remain outside a container parser's control.

## Lower-memory gate

Measure equivalent work and caller retention with the same live-packet/output window. Initial delivery requires lower peak live heap and lower total resident peak RSS in isolated, matched workers, each at an upper paired 95% ratio strictly below 1.0. Under the [owner-approved v2 policy](../research/decisions/2026-11-03-total-resident-rss.md), each RSS pair uses the proven candidate resident peak ceiling plus two candidate pages over the observed reference peak minus two reference pages. Missing observations, unproved peaks and a nonpositive reference lower bound remain unqualified. Retain operation-growth RSS and its original verdicts as optimization diagnostics; no historical measurement or claim is rewritten. No arbitrary memory reduction percentage is implied by the owner's request.

Record peak and steady resident memory, live/peak heap, allocations, input buffering, codec/index/metadata storage, retained output and target stack limits. Allocation tracking and RSS require different measurement lanes: instrumenting allocation changes performance, so do not use that run for the speed claim. Use the same allocator/instrumentation where possible; explain limitations otherwise. Synthetic allocation counters alone cannot prove whole-process memory use.

Absolute hard limits are mandatory in addition to relative improvement. H0 must choose and freeze host/32-bit-TV budget profiles from actual input sizes and device headroom; no default unlimited metadata, decompression or index scan is permitted. A lower memory footprint achieved by dropping required packets/metadata fails parity.

## Targets, reporting and acceptance

The owner moved new development qualification to native Linux on 2026-10-31;
the active target is AArch64 Linux. See the
[target decision](../research/decisions/native-linux-qualification.md). The earlier
Mac receipts retain their exact source/platform scope. Parity, runtime and memory
thresholds are unchanged. Future Streem shipping proof still requires its actual
32-bit hardware and integration. A compiler check is not a device performance
run. Optional WASM support is a separate capability until demonstrated; no
network/OS assumptions enter the core API.

Every report includes feature-manifest hash, module/source revision, dependency graph/features, FFmpeg pin/config/hash, corpus hashes, all outcomes, environment, correctness diff, timing samples, confidence calculation and memory observations. Outcomes distinguish pass, mismatch, unsupported, limit rejection, crash, timeout, missing sample, rights-blocked and unmeasured. Release acceptance requires:

1. All required selected-feature cases passing and no unexplained missing coverage.
2. Strict-safe dependency gate passing on each enabled target/feature graph.
3. The authorized 1.25 initial-delivery runtime ceiling and lower-memory gates
   passing at matched work, with the 0.80 optimization classification retained.
4. Absolute limits/fuzz/regression gates passing.
5. Streem integration and actual hardware acceptance for any claim involving its decoder/presentation.

The historical candidate spike measures correctness only. It has **no** timing samples, memory metrics, FATE executions, full conformance result or accepted module. The newer shared harness qualifies the frozen K0–K5 slices (116 workloads) on native Mac, executes two upstream reference FATE cases (Xiph lacing and zlib) and separately compares three FATE vectors at the packet-demux level; see `implementation.md`. Neither study qualifies a complete module.

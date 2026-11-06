# Shared core development (M00-C1)

The shared core is being implemented. Its dependent
[K0–K5 Matroska snapshot](../research/qualification/shared-core-mac-arm64/README.md)
passes all 116 frozen selected workloads through the unchanged native Mac ARM64
parity, runtime and both memory gates. This qualifies the recorded operations and
source graph; complete M00/M01 and later source/dependency changes remain open.

Current work adds charged shared byte storage and arenas, wider exact timestamps,
incremental input/generation/failure states and bounded work. Format lanes prepare
independent specifications, authored fixtures and source oracles against this
boundary. Core, workspace/build integration and numerical qualification are owned
by the main lane. No third-party production dependency is admitted.

Correctness tests precede the implementation. Retained-capacity accounting,
backpressure and lifetime must remain valid through cloning, reader teardown,
seek/reset and cross-thread release. Starvation, explicit EOF and transport failure
are separate states. Complete format operations and new selected work classes
still require full parity, initial-delivery runtime upper paired95 ratio <=1.20 and both lower-memory
gates. Host tests alone do not close M00, integration or shipping-target work.

The prototype currently exposes:

- `Budget`, linear reservations, immutable shared `Bytes` spans and `BudgetVec`
  construction/arena storage. Growth charges the old and replacement allocations
  concurrently. Rejected pushes/freezes return the original value or builder.
  Immutable bytes expose their complete tracked owner charge and fixed header
  charge; the pool exposes its fixed control charge. An opaque retained input
  window handle permits allocation deduplication with packet/source spans across
  consumption, reset and cancellation. These are capacity-accounting facts, not
  RSS measurements or observations of other threads' live usage.
- Reduced `Time` in exact seconds (`i128` numerator, `u128` denominator), positive
  `u64` timebases and explicit rounded views. A fixed, allocation-free 256-bit
  scratch calculation handles products that reduce or round to a representable
  result. Unknown timestamps remain `Option`, without a sentinel.
- `InputCursor` with checked `u64` source offsets, bounded windows, separate
  pool/parser-generation/range-request identity, explicit entity completion and
  sticky transport/cancellation/truncation errors. Internal range changes preserve
  the logical generation; resets invalidate it. Moving to the next completed
  source entity preserves the logical generation. A format owns packet boundaries,
  configuration changes, transport clock epochs and logical stream completion.
- `WorkBudget` and input reads that preserve their position when yielding. Each
  format must also charge its parsing, decompression, indexing and pixel work.

The Matroska development reader now exposes typed incremental configuration,
packet, metadata, lacing and seek jobs on the shared pool and input cursor. Its
blocking adapter drives the same retained jobs, including resumable inflater
growth and allocation-pressure recovery. The selected worker uses that adapter.
A later frozen checkpoint passes all 141 Rust tests, five worker tests, Clippy,
a release build and the no-default-feature library check, plus all 116 unchanged
selected source/native comparisons. A separate 224-case seek replay agrees
strictly with the native worker after correcting equal-time references across
Clusters to start at the earliest physical Cluster. The old unstable-permutation
diagnostics and 136 observed replay mismatches remain preserved. Same-Cluster
ambiguity and direct plain-seek native coverage remain open. These checks do not
establish complete module coverage or speed/memory qualification of the new graph.
The earlier ownership migration and its clone/pool/input regressions remain
preserved.

Source metadata now exposes opaque shared document handles. Their private Arc
prevents callers from retaining weak aliases after the document's accounting has
been released. The original weak-allocation escape and permissive API both failed
regressions before the fix; the opaque API and cross-thread final-release checks
pass afterward. The new K0–K5 receipt includes this source change and its fresh
numerical proof.

The first combined workspace run passes 117 Rust tests (the historical 50 plus
23 shared-core, 12 MP4, 21 TS and 11 DVB regressions). An independent Python
`Fraction`/integer corpus also agrees on 34,502 exact-time and 15,024 private wide
arithmetic cases. This is sampled arithmetic correctness, not an exhaustive domain
proof or an FFmpeg performance result. `make core-oracle` generates the independent
corpus and checks it in both debug and release profiles with source, executable
and compiler identity. An additional versioned corpus adds 1,280 successful wide
rational reductions and 429 dense-division/construction cases; all **51,235**
unique cases pass in both profiles. Every run preserves source, executables and
raw outputs. This also runs in `make check`. The initial format implementations and the
core are not numerically qualified; every selected operation keeps full parity,
initial-delivery runtime upper paired95 <=1.20 and both lower-memory gates;0.80 remains the optimization target.

The make10 snapshot passes 306 Rust and 51 Python tests, Clippy, no-default
features, release and safety/design checks. Fresh selected Matroska source/native
comparisons pass all 116 workloads, including the original HDR vector. That
first shared-core numerical baseline passes both memory gates on all 116 cases
and the speed gate on 98. Its 18 clean speed failures remain evidence; that
historical baseline remains a failed snapshot. Later storage/inflater
optimizations have separate qualification below. The seventh, AVC/HEVC framing
package and complete modules still need their own full qualification.

The later fused inflater/borrowed-lace snapshot passes 90 media-library tests,
all 116 selected source/native comparisons, the selected Xiph/zlib FATE cases and
zero-length-block demux comparison. Its three-pair diagnostic retains four
fixed/laced speed warnings and an empty-stream outlier among the 18 earlier failing
cases; all diagnostic memory observations are lower. These short observations remain explicitly unqualified. The minimum
30-pair confidence and lower-memory gates are unchanged. Subsequent inflater work
requires another source identity and fresh checks.

The make10 compact-table/core-history/opaque-document snapshot passes 92 media
library regressions plus the negative ownership API test, all 116 selected
source/native workloads and the selected FATE checks. Its numerical screening
was diagnostic only. Other formats have separate source/host evidence: 59 MP4 integration
regressions and the extent example guard, 53 TS regressions plus 70 actual source
comparisons, 39 framing regressions, and 34 DVB regressions. The DVB boundary tests
reproduced four failures before their fix. These are implementation checks and
source observations; complete modules and their numerical gates remain open.

The make12 snapshot passes 315 Rust and 51 Python tests, including 92 media-library
regressions and the negative ownership API guard. Its final compact inflater,
lazy error checks, charged copies and matched checksum workers pass full parity
and fresh 30-pair time/live-heap/attributable-RSS gates on all 116 frozen K0–K5
workloads. The worst upper ratios are 0.6721 time, 0.8525 live heap and 0.7188 RSS
(rounded upward). First clean measurements and all earlier failures are retained.
This source-specific receipt covers the recorded three-package Matroska worker
graph. The later typed adapter and subsequent format changes need fresh affected
qualification; complete core/format contracts remain open.

## Current typed-reader optimization checkpoint

New qualification uses native Linux ARM64. The earlier recovery checkpoint's core passes 50 host tests with
non-owning, non-reused input-token pool identities and checked owned-window
handoff. The recovery implementation passes231 all-target media tests,
Clippy and the no-default library check. Its earlier unprofiled worker `eb12b3dac9d3c089`
passes all116 frozen selected comparisons, all224 strict cross-Cluster Cues
replay pairs, ten additional boundary traces and78 complete operations over26
fixed independent inputs. Playback seek recovers real earlier subtitle packets
without inventing source ends or changing previous-RAP seeking. Read/work limits,
cancellation, source identity, retained owners and configuration ordering have
regression guards. The additional scan is real operation work; this source graph
has not completed numerical qualification. Those 50 core tests also cover consuming byte views: moving a view
retains the complete hidden backing capacity and its existing charge.

The later discard checkpoint consumes ignored Matroska bodies without a copy or
temporary byte owner. Collected bodies and their direct owner views are unchanged.
Its frozen core41/media graph passes65 core,238 media and five worker tests in
debug/release, all116 frozen packet comparisons,224 Cues pairs and78 complete
operations over26 independent inputs. The combined working tree also includes
direct metadata/packet appends and the checked-width clock changes described by
their separate source receipts. Its build identity and numerical qualification
must be established independently; earlier workers do not qualify changed source.

The reader now carries owned BlockAdditions/mapping evidence and track-local
CodecState changes, including configuration events before affected packets and
configuration restoration at seek boundaries. These additions have separate
host regressions; they do not establish complete native M01 feature parity.
Ordinary header read-ahead stays on the canonical parser. Compressed frames
charge their inflater workspace only when needed; uncompressed identity views
retain the existing work, cancellation and full-owner accounting rules.
The current changes move final packet/configuration owners rather than cloning
them, use bounded ASCII/zero-suffix scans with the original scalar error path,
and skip UID namespace walks proven empty by the completed count. UID counting
now happens during the existing scalar visit; scalar nodes skip master-only
validation dispatch. Complete duplicate checks and workspace admission remain.
The canonical ordinary header refill is bounded to 4096 bytes by the caller,
source window, scan allowance and parent ends. Its explicit I/O-policy guards
use the same limit. Release builds use fat LTO with the existing panic policy.
The optional buffer cache, streamed seek and contiguous-header experiments
remain private.

Complete 30-pair Linux timing and memory qualification remains separate from
correctness. The preceding copy-removal graph passed 9 of 116 runtime gates and
115 lower-heap/RSS gates; one startup calibration was unqualified. Its original
failure remains preserved. The new probe waits for the existing single-thread
precondition before BEGIN, with a bounded timeout; observer rejections and
numeric thresholds are unchanged. The earlier lazy-workspace worker `780c0dc2509e95ec`
has 116/116 qualified lower-heap/RSS results and 10/116 runtime results at the
0.80 optimization target. The later authorized 1.25 delivery ceiling classifies
92 of those unchanged first timings as passing. Memory was collected separately
after a loader-provenance metadata mismatch refused automatic timing reuse.

The separate layout-only worker `a86128ceb9950c30` passes parity and both memory
gates on all 116 workloads; 96 pass delivery runtime and 10 meet the optimization
target. Header, zlib and colour slices pass their delivery gates for that exact
checkpoint. The earlier header/seek/owned-handoff worker `af47af1450357093`
has its own complete first-clean 30-pair Linux receipt: all 116 pass selected
parity and strictly lower live heap and attributable RSS; 98 pass delivery runtime
and 10 meet the optimization target. The remaining 18 runtime failures are retained.
The offline raw audit recomputes equal-work, isolation and confidence bounds;
versioned live kernel admission evidence was captured on each runner before
unloading instrumentation. These results apply to that worker, not later private
candidates. The earlier minimal worker `ea8c4e0a912c38ce` has fresh 30-pair
results for all 116: selected parity and both strict memory gates pass everywhere;
102 pass delivery runtime, 14 fail, and 10 meet the optimization target. Its
versioned live memory admission and offline raw-pair audit are separate from the
earlier receipts. The preceding combined worker `b5e47713f2cabe587` has a separate
first-clean 30-pair audit: all 116 selected parity, lower live heap and attributable
RSS gates pass; 103 pass delivery runtime, 13 fail and 11 meet the optimization
target. Main AVC/HEVC upper runtime ratios are 1.643–1.730, nested chapters 1.314
and the eight seek cases 1.696–2.478. Original inputs, native workers, comparator
rules and numeric gates are unchanged. Capture-time Linux admission and the
offline raw-pair audit are retained separately. A separate private compiler-PGO
worker `b2ff0a1a3a099af1` passes104/116 delivery runtime gates and15 optimization
targets, with all116 frozen parity and memory gates passing. It predates the
subtitle recovery fix and does not qualify the current source. Balanced training
on all26 fixed independent inputs after that fix passes correctness; its five-pair
screen is diagnostic and closes no new delivery case. Complete modules and Streem
adoption remain open.

The historical `d28848c115eb7932ea858be94d7476d85aa6d57a59ba310bded787801ac38da1`
Mac collector completed all 116 cases: 106 runtime failures and one RSS failure.
It is unqualified and no longer running. Its earlier 115-case partial baseline,
contaminated observation and all FATE receipts remain preserved. The sealed
shared-core receipt qualifies only its original source and platform.

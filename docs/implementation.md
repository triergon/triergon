# Shared harness and Matroska development slices

The sealed historical shared-core snapshot of all six frozen K0–K5 development slices passes selected parity, runtime and
both lower-memory gates on native Mac ARM64. The
[historical receipt](../research/qualification/shared-core-mac-arm64/README.md)
retains 116 workloads: 115 authored cases and the mandatory intact original HDR
vector. Every case has 30 raw paired timing/memory observations. K0–K4 contracts
and all numeric thresholds are unchanged. Complete modules and Streem offboarding
remain open; the earlier source snapshots remain immutable historical receipts.

The earlier experimental review tree adopted the frozen 5,095-file Root3477 composition.
Native Linux ARM64 debug and release each recorded 3,477 passed, zero failed and
one ignored test, plus 15 passing doctests per profile. All 13 root checks
passed, including shipping and ARM32. The successor retains every Root3474
test name and adds one capture guard and two HEVC storage guards. Root3474's
historical 177-name delta covers 22 playback, 35 framing, 16 native PGS and
104 native mov_text tests. Earlier counts retain their own source scope.

- The explicit [terminal-MDAT operation](../crates/playback/README.md) matched
  120 whole native observations across five required fixtures in its standalone
  functional run. Source completion still requires the matching actual EOF.
  The default native refusal and the wider matrix's 252 native nonpasses remain.
- The [compact framing views](../crates/framing/README.md) now include the
  packed HEVC descriptor. The packed worker and Root3477 have separate
  functional receipts. The Framing44 RSS campaign did not pass its gate.
- The standalone [native PGS first-event operation](../crates/subtitles/src/pgs/README.md)
  passed runtime, heap and total peak RSS for all 229 memberships on the
  exact b545 candidate. That qualification applies to the approved receiver
  operation and its pinned workers. The integrated root has functional evidence.
- The explicit [mov_text receiver](../crates/subtitles/src/text/native_mov_text/README.md)
  matched 82 selected cases at each of three work quanta per profile. The
  retained Streem revision routes mov_text through its existing packet-text path.

D1–D6 remain open. Root3477 includes capture85. The standalone capture85
quality run passed all 15 selected cases under the six-factor contract.
The exact standalone packed-framing worker passed runtime, heap and
total peak RSS delivery on all 125 selected cases. The standalone
capture85 worker passed runtime delivery on 15/15 cases; its memory
qualification remains open. These runs are outside the root receipt.
Actual consumer integration remains open.

The current review tree adds regular front-moov MP4 playback across multiple
finite MDATs. Deferred samples need containment in an observed MDAT before
payload delivery. Backward layouts return an unsupported-layout error. The
[desktop session composition](../crates/sim/README.md) uses the same playback
crate to exercise real MP4 packet delivery, decode and seek reset.

The numerically selected graph has three owned Rust packages, no third-party packages, and forbids
unsafe production code. Native FFmpeg, allocation/RSS observers and Python remain
isolated test tools. The frozen whole-workspace host snapshot passes 315 Rust and
51 Python tests, formatting, Clippy, no-default features, release build and
strict-safe/design checks across seven owned packages. All 51,235 independent
arithmetic cases pass in debug and release, and the seven cached specification
hashes are verified. New development performance runs use
[native Linux ARM64](../research/decisions/native-linux-qualification.md).
Later source graphs require fresh qualification; shipping still requires 32-bit
webOS hardware and integration evidence.

K5 adds bounded source Info, chapters/editions, tags, attachments, track flags and
legacy/BCP47 languages. Full source facts and the native catalogue view pass
independently. Dense chapter/tag memory failures and the initial large-attachment
runtime failure were retained and resolved. Both workers now use the same faster,
scalar-verified ordered checksum. Interrupted checkpoints preserve unvisited clean
observations, including failures; final evidence retains all original clean results.

The two owner-approved K2 comparison rules remain input/hash bound. Only the mixed
encoding case uses the explicitly augmented native restoration reference; the
other selected cases use pinned native FFmpeg. No input is stripped to pass.

The [shared core and parallel format lanes](core-development.md) expand the
workspace beyond the earlier two-package snapshot. The first shared-core numerical
baseline passed memory on all 116 cases and speed on 98; its 18 clean speed failures
remain retained. Subsequent charged-copy/inflater/error-check optimizations and
matched checksum workers now pass every selected gate with fresh 30-pair evidence.
Worst upper time/live-heap/attributable-RSS ratios are 0.6721/0.8525/0.7188, rounded
upward. Later source changes still need affected qualification. MP4 indexing,
TS/PES primitives and DVB pixel rasterization are implemented prototypes; their
complete source/native operations and numerical gates remain open. The seventh
owned package includes bounded AVC/HEVC NAL framing,
AV1 OBU/configuration inspection, VP9 configuration facts and bounded AC-3/E-AC-3
header/frame/configuration parsing. Later DTS-core work passes 62 host regressions,
Clippy and a release build at its recorded checkpoint. The later selected AVC
wire parser and HCRC clarification pass165 framing regressions on their frozen
Linux source; acquired ISO14496-15:2010/incorporated2006FRExt grammar is bounded.
At that recorded checkpoint, later AVC profiles, hvcC, remaining packet semantics
and full native/numerical qualification were open. This historical feature list
is not the current integration inventory. The metadata40/core41/packet42 snapshots have separate
functional and code-generation receipts; they do not qualify the combined public
tree or close complete M03/source/native/numerical gates.
The fused-inflater snapshot's explicitly unqualified three-pair screen retains
four fixed/laced speed warnings plus an empty-stream outlier among the 18 earlier
failing classes. It does not replace any 30-pair gate.
The adaptive, fused and compact inflater snapshots pass all 116 source/native
comparisons and the selected Xiph/zlib FATE cases. Further optimization needs another source identity
and fresh checks; these results name only their tested snapshots.

## Commands

```sh
python3 tools/fetch-specs.py
python3 tools/prepare-ffmpeg.py
python3 tools/generate-corpus.py
python3 tools/generate-header-corpus.py
python3 tools/generate-zlib-corpus.py
python3 tools/generate-seek-corpus.py
python3 tools/generate-colour-corpus.py
python3 tools/generate-catalogue-corpus.py
python3 tools/build-harness.py
make check
python3 tools/run-fate.py --case fate-matroska-xiph-lacing
python3 tools/run-fate.py --case fate-matroska-zlib-decompression
python3 tools/run-fate.py --case fate-matroska-zero-length-block --demux-only
python3 tools/test-media.py --benchmark --memory --pairs 30 --iterations 50 \
  --output .research/qualification-k0 --fate-cache .research/fate-samples \
  --require-slice-acceptance
python3 tools/test-media.py --benchmark --memory --pairs 30 --iterations 50 \
  --manifest harness/header-corpus.json --contract harness/header-slice.json \
  --corpus .research/header-fixtures --output .research/qualification-header \
  --require-slice-acceptance
python3 tools/test-media.py --benchmark --memory --pairs 30 --iterations 50 \
  --manifest harness/zlib-corpus.json --contract harness/zlib-slice.json \
  --corpus .research/zlib-fixtures --output .research/qualification-zlib \
  --require-slice-acceptance
python3 tools/test-media.py --benchmark --memory --pairs 30 --iterations 50 \
  --manifest harness/seek-corpus.json --contract harness/seek-slice.json \
  --corpus .research/seek-fixtures --output .research/qualification-seek \
  --require-slice-acceptance
```

The colour lane additionally runs:

```sh
python3 tools/test-media.py --benchmark --memory --pairs 30 --iterations 50 \
  --manifest harness/colour-corpus.json --contract harness/colour-slice.json \
  --corpus .research/colour-fixtures --output .research/qualification-colour \
  --require-slice-acceptance
```

K5 additionally requires the intact, hash-pinned external HDR vector in the local
FATE cache. Missing or changed input fails; its redistribution rights are still
unreviewed. Qualify that vector before passing its exact-source receipt to the
48-profile authored lane:

```sh
python3 tools/test-catalogue-vector.py --benchmark --memory --pairs 30 --iterations 50 \
  --output .research/qualification-catalogue-hdr
python3 tools/test-media.py --benchmark --memory --pairs 30 --iterations 50 \
  --manifest harness/catalogue-corpus.json --contract harness/catalogue-slice.json \
  --corpus .research/catalogue-fixtures --output .research/qualification-catalogue \
  --additional-report .research/qualification-catalogue-hdr/report.json \
  --require-slice-acceptance
```

These are setup/generation commands for a new corpus. Preserve recorded corpus
files when reproducing an existing receipt; fresh encoder/container generation can
change input hashes, including mux metadata. The two approved rules require their
recorded input hashes. Run performance commands only in an isolated window; this
session's camera pause/resume authorization is specific to the identified process.

Requires Python 3.12+, Rust nightly, a C compiler, Make and pkg-config. Initial
synthetic movie fixture generation additionally uses a full FFmpeg CLI with the
selected encoders; it is separate from the pinned comparison build. The pinned
FFmpeg builder verifies the source archive and every upstream source file. Source/build inputs, static reference archives, binaries and corpus are hash-pinned;
compiler identities and raw measurements are retained. System-library paths, runtime
zlib version and OS build are recorded; no standalone OS shared-cache hash is claimed.
The collector rejects stale binaries/source and changed inputs. Synthetic generation
can change across encoder versions; each actual materialized input is hash-pinned.

`--reuse-report` accepts evidence only for identical workers, feature contract and
input hashes. Memory reuse additionally requires the same calibration probe and observer
binaries. Raw timing and memory statistics are recomputed at acceptance. Invalid
windows remain recorded. The first complete clean measurement is retained; a valid
speed failure requires a code change before requalification.

## Scope and gates

`slice_accepted` requires every declared current profile to pass strict packet and
source-time comparisons, all short-read/seek requirements, a runtime ratio with
paired 95% upper confidence bound at most 1.20 (1.25 before 2026-11-05), and lower measured RSS/live allocations.
The separately reported optimization target remains a runtime upper bound of 0.80;
the [delivery policy](../research/decisions/initial-delivery-runtime.md) preserves historical receipts.
It requires all 30 raw pairs and rejects missing evidence, unequal work, corruption,
invalid instrumentation, CPU contamination and non-finite numbers.

`accepted` additionally includes broader module/FATE requirements. It remains false
for unfinished M01/M02 work. `--require-acceptance` therefore still exits 2. The
separate `--require-slice-acceptance` reports the frozen K0–K5 contracts while
preserving the stricter complete-module gate. The stage split does not clear or
remove pending module requirements or change numeric thresholds.

The earlier sealed [segment-metadata K0–K5 snapshot](../research/qualification/segment-metadata-mac-arm64/README.md) has a worst upper paired 95% runtime ratio of
0.7265 across all 116 workloads (rounded upward). Conservative attributable RSS
and live-allocation upper ratios are at most
0.6286 and 0.8288. Every case has 30 paired timing and memory observations.
These describe the exact source, workers and native Mac corpus in the receipt;
they do not establish TV behavior. The mixed-encoding case uses its explicitly
augmented reference. Each receipt pins source, binaries, instrumentation and inputs.

## Exact timing and FFmpeg compatibility

The five initial differences came from distinct timing representations. The parser
preserves exact rational source nanoseconds, including fractional laced durations and
negative PTS. An explicit `BlockTiming::ffmpeg9()` adapter exposes the pinned FFmpeg
packet-clock behavior. No source precision is discarded and no epsilon comparison
is used. The independent Python/Fraction RFC oracle checks the exact source values;
the native FFmpeg worker checks the compatibility timeline, bytes and metadata.
Both must pass. Seven additional boundary fixtures test fractional/absent durations,
half-tick delays, explicit zero durations and pre-zero timestamps.

The initial performance failure on AC3/EAC3 was measured, retained and fixed in the
archived reader: ordinary packets own a view into their block buffer, avoiding two
allocations and a payload copy. That snapshot used `Read::read_to_end` with reserved
capacity. The current charged reader initializes its `BudgetVec` destination,
reads directly into that mutable slice, then freezes shared storage. Checksums consume all packet
bytes, selected metadata, timestamps, flags and trims identically in both workers.
The benchmark checksum operation was preserved.

## Measurement isolation and memory

Timing uses optimized matched open/read-all/teardown workers, warmups, seeded random
A/B order, raw pairs and confidence intervals. Process startup/trace serialization
are outside operation timing. A host-wide lock and compiler/encoder CPU counters
reject competing work; merely existing idle processes are not active CPU work.
The camera capture was paused only with explicit owner authorization, with cleanup
and an independent resume watchdog. Every pause/resume event is recorded.

Heap and RSS use separate external observer runs with explicit operation boundaries;
no observer is loaded for speed measurements. The known-allocation probe reproduces
its independently calculated live peak exactly and returns to its starting live
allocation count. Imported allocator entry points of both selected workers are
covered. Certificates identify the probe, observer and worker binaries.

RSS uses the candidate's OS high-water upper bound plus two pages of uncertainty,
and the reference's observed operation peak less two pages, measured within each
process. An unresolved/zero reference lower bound fails. The conservative ratios,
raw observations, work checksums and calibration receipts are retained. This certifies
these worker paths, not arbitrary system-library allocation behavior.

## Subsequent component: content encoding and late metadata

Header removal is implemented for frame and CodecPrivate scopes, including their
combination. Header restoration occurs per laced frame. Empty encoded payloads,
ordered headers, duplicate-order rejection and decoded-size limits are tested.
Bounded zlib frame/CodecPrivate decompression and ordered header/zlib chains are now
implemented. bzip2/lzo, encryption and next-encoding scope remain unsupported.

Seekable opening can resolve bounded, chained SeekHeads to load Tracks after the
first Cluster. Target IDs/bounds, cycles, read budgets and replay of already loaded
Tracks are guarded. Sequential opening keeps its declared metadata-first restriction;
late metadata does not cause full-file buffering.

The real FATE zero-length-block vector matches both packets/21 payload bytes and
source timing through the independent adapters. It requires seekable input. The
upstream full remux test is not claimed as executed. The Xiph-lacing and zlib upstream cases
and their separate packet-demux comparisons also pass. External vectors remain outside git and
redistribution rights remain unreviewed.

Validated Cues lookup and all-track preroll are now implemented, with a bounded
missing-index fallback. The K2/K3 contracts retain every required profile and the approved reference
rules; see [the feature contract and evidence](zlib-cues.md).
Archived timing/memory reports preserve each exact source separately. K0/K1 passed
requalification together with K2/K3 on the final archived source. The final audit
recomputes every verdict and verifies current calibration/worker/input hashes.
Earlier binary receipts never qualify a changed tree.

The archived K4 snapshot qualifies static container colour/HDR; exact source IEEE values stay
separate from native rational/side-data representation. See [the contract](colour-hdr.md).

Shared retained-capacity accounting is implemented for track, packet and catalogue
storage. The recovery implementation passes all 116 frozen source/native
comparisons, 231 all-target media tests, five worker tests and 224 strict
fresh/after-EOF Cues replay pairs. Ten additional boundary traces and all78
operations over26 fixed independent inputs agree with the pinned native worker.
Playback seek can replay real earlier subtitle packets without inferring a
missing source end; previous-RAP source seeking keeps its original boundary.
The extra scan uses the same remaining work/read limits and source generation.
The recovery graph requires fresh runtime and memory qualification. Packet side data and track-local codec-state restoration are implemented
with new host guards; their complete native feature qualification remains open.
The historical Mac d288 numerical collector completed and failed qualification.
Later Linux optimization graphs retain their own first measurements and failures;
see the [current checkpoint](core-development.md#current-typed-reader-optimization-checkpoint).
Remaining complete-module work includes broader growth/work validation, native
side-data/state coverage, packet adapters, full numerical qualification and
sustained coverage-guided fuzzing.
No full Matroska, codec-decoder or
shipping-TV qualification is claimed.

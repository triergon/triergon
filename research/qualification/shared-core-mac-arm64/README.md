# Qualified shared-core Matroska slices: native Mac ARM64

All **116 frozen selected workloads** pass source/native parity, the unchanged
30-pair speed gate and both lower-memory gates on 2026-10-31. The selected graph
contains `media-worker`, `safe-media` and `safe-media-core`, with no third-party
production package. This receipt qualifies K0–K5 at the recorded source and worker
identities. Complete core/Matroska modules, later format prototypes, application
integration and TV performance remain unqualified.

The corpus contains 115 authored cases and the mandatory intact external
`mkv/hdr10tags-both.mkv`. Every case has 30 paired timing observations, with 50
complete operations in each worker observation, and 30 paired memory observations.
The table rounds each worst per-case upper paired 95% ratio upward.

| Slice | Parity | Time (<=0.80) | Attributable RSS (<1) | Live heap (<1) |
|---|---:|---:|---:|---:|
| K0 packets | 12/12 | 0.5590 | 0.5556 | 0.1950 |
| K1 header removal | 4/4 | 0.3772 | 0.5556 | 0.1239 |
| K2 bounded zlib | 10/10 | 0.6701 | 0.7188 | 0.8525 |
| K3 Cues/preroll | 8/8 | 0.5148 | 0.4667 | 0.1846 |
| K4 colour/static HDR | 33/33 | 0.4423 | 0.5173 | 0.2918 |
| K5 segment metadata | 48/48 | 0.6721 | 0.7143 | 0.8419 |
| K5 intact external HDR | 1/1 | 0.3408 | 0.4616 | 0.0731 |

[Exact summary](summary.json) and raw-pair reports: [K0](k0/report.json),
[K1](header/report.json), [K2](zlib/report.json), [K3](seek/report.json),
[K4](colour/report.json), [K5](catalogue/report.json) and
[intact HDR](external/report.json). All six contracts and corpus manifests retain
their previously frozen hashes. No required input, source frame, feature, minimum
pair count or numeric threshold was removed or relaxed.

## What changed and what was compared

The Matroska implementation uses the shared charged pool for tracks, packets and
source catalogues. Opaque document handles prevent weak aliases from outliving
their allocation accounting. Inflater tables, charged copies, laced-frame spans
and successful error checks avoid unnecessary copying or destructor work while
preserving exact source data, bounds, cancellation and work policy.

Both worker adapters use the same mathematically equivalent bounded checksum
calculation; every selected byte/field still contributes in its original order.
The pinned FFmpeg revision, configuration and static archive hashes are unchanged.
The changed worker identities are qualified by fresh measurements; historical
timings are not reused across workers. K2 retains only its
[two owner-approved, input-bound comparison rules](../../decisions/2026-10-31-zlib-comparisons.md).
The combined-encoding case explicitly uses pinned FFmpeg plus the native
restoration adapter for the stock-unsupported operation. Other cases retain the
stock pinned native reference.

## Retained and audited evidence

[The audit](audit.py) recomputes all per-case verdicts from raw observations,
requires timing pair IDs 0–29 and raw worker iteration counts of 50, verifies the
manifest's operation/view/seek target, and checks the full collection history.
Every first clean timing and memory observation must remain unchanged. Only
missing/incomplete or explicitly contaminated measurements can be collected in
a later window; terminal non-contamination failures cannot be replaced.
[Collection evidence](collection-evidence.json) retains all window/report/log
hashes, first-clean origins and bounded camera pause/resume receipts. Competing
build attempts and interrupted collections remain recorded.

[Source mapping](source-manifest.json), [source hashes](source-sha256.json),
[worker identity](identity.json), [binaries](binary-evidence.json),
[inputs](input-evidence.json) and [qualification state](qualification-state.json)
bind the exact evidence. Content-addressed input and binary copies live outside
mutable build output. The 589-file source export contains the unchanged 588-file
whole-workspace host snapshot plus the selected graph's FATE index. Historical
Markdown uses `.md.txt`; the source map retains its original filename and hash.
Only the three selected packages receive numerical qualification from this run.
The [completion audit](completion-audit.json), [claim review](claim-audit.md) and
[archive hashes](archive-sha256.json) record the final checks. The archived audit
and controller are provenance copies of scripts executed from `.research/`;
they are not entrypoints for starting a new collection inside this archive.

[Host checks](checks.log) pass **315 Rust tests and 51 Python tests**, formatting,
Clippy, no-default-feature and release checks, and strict safety/design checks.
The independent **51,235-case** exact-arithmetic corpus passes in both debug and
release. These results describe the frozen host source, not later prototype edits.

[Xiph](fate-xiph.json) and [zlib](fate-zlib.json) record freshly executed upstream
FATE targets and candidate demux comparisons. The
[zero-length-block receipt](fate-zero-length.json) covers demux only; its full
upstream remux was not run. Each receipt binds the exact sample, reference build,
revision and upstream FFmpeg binary. The diagnostic `ffprobe` hash recorded by the
harness comes from PATH; it is archived separately from the pinned build's
auxiliary `ffprobe`. Neither CLI replaces the native timing/reference worker.
External sample redistribution rights remain unreviewed; payloads are not vendored.

The [earlier sealed qualification](../segment-metadata-mac-arm64/README.md) and
[preserved shared-core failures](preserved-development-evidence.json) remain
separate immutable evidence. The initial shared-core run's 18 clean speed failures
were resolved by source changes, not rerun with unchanged code until they passed.

Complete O10–O70, typed incremental Matroska/seek integration, packet side data and
codec-state handling, remaining formats/subtitles, simulator/capture replacements,
Streem integration, sustained fuzzing and 32-bit webOS/TV qualification remain
open. No Streem source, TV state, commit, push or publication is part of this receipt.

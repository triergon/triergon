# Qualified bounded segment metadata and K0–K4: native Mac ARM64

All 116 workloads pass their frozen selected parity, runtime and both lower-memory
gates on 2026-10-31. This covers 115 authored cases plus the mandatory intact
external `mkv/hdr10tags-both.mkv`. Every case retains 30 paired timing observations
(50 complete operations per worker observation) and 30 paired memory observations.
The table gives the worst per-case upper paired 95% ratios, rounded upward.

| Slice | Parity | Time (<=0.80) | Attributable RSS (<1) | Live heap (<1) |
|---|---:|---:|---:|---:|
| K0 packets | 12/12 | 0.5854 | 0.4075 | 0.1993 |
| K1 header removal | 4/4 | 0.2632 | 0.4075 | 0.0766 |
| K2 bounded zlib | 10/10 | 0.6893 | 0.5000 | 0.6403 |
| K3 Cues/preroll | 8/8 | 0.5413 | 0.3667 | 0.1503 |
| K4 colour/static HDR | 33/33 | 0.4929 | 0.3861 | 0.2645 |
| K5 segment metadata | 48/48 | 0.7265 | 0.6286 | 0.8288 |
| K5 intact external HDR | 1/1 | 0.3459 | 0.3778 | 0.0653 |

[Summary](summary.json); full raw-sample reports: [K0](k0/report.json),
[K1](header/report.json), [K2](zlib/report.json), [K3](seek/report.json),
[K4](colour/report.json), [K5](catalogue/report.json) and
[original HDR vector](external/report.json). All verdicts were recomputed from raw
pairs. No original K0–K4 contract/profile changed; no numeric threshold changed.
Complete-module acceptance remains false.

## Source metadata and native catalogue

K5 retains bounded Segment Info, complete source edition/chapter trees, displays
and languages, recursive text/binary tags with every target UID, attachments and
track flags/legacy/BCP47 languages. Dates retain signed nanoseconds; duration and
static HDR retain exact source IEEE values. One owned raw buffer and a compact
immutable arena back borrowed views; attachment handles share it across teardown.
Byte/node/depth/storage limits and late-metadata/repeated-seek/fault tests apply.

The independent Python source oracle checks original document bytes, every node,
scalar and track field. A separate schema-2 native view compares packets,
configuration, static colour/HDR, dictionaries, flat chapters and static attachment
bytes against public pinned FFmpeg APIs. Native omissions/collapses do not erase
source facts. Both proofs are mandatory on seekable input; 1/7/4096-byte sequential
checks additionally verify native catalogue output and exact source packet timing.
Every canonical payload/metadata byte contributes to the same ordered checksum.

The original HDR input is required by the K5 gate and was neither stripped nor
substituted. Its earlier refusal remains in the immutable
[K4 receipt](../colour-hdr-mac-arm64/additional-hdr-input.json). Its full upstream
decoded-frame metadata-priority tests were not run. Sample redistribution rights
remain unreviewed; no external payload is vendored.

K5 and K4 use stock pinned FFmpeg. K2 retains the
[two owner-approved comparison rules](../../decisions/2026-10-31-zlib-comparisons.md).
Only the mixed-encoding case uses **pinned FFmpeg plus native scope-1 restoration**,
explicitly labelled in its report. Complete source data/time proof remains required.

## Preserved failures and harness integrity

The initial dense metadata implementation failed 512 chapters (heap upper ratio
4.2881, RSS 1.4572, rounded upward) and 512 tags (heap 1.3888). Compact twelve-byte
source nodes and borrowed/inline adapter dictionaries resolved those failures;
see [the preserved evidence](preserved-memory-failures.json).

A clean 1 MiB attachment run initially failed runtime at 0.8474820869645205.
Separate sampling found the full checksum dominating both workers. The same
ordered polynomial now uses four independent lanes in both adapters, consuming
every byte; scalar tests independently verify the identical result. The original
failure/source/workers remain [preserved](preserved-runtime-failure.json).

Interrupted replay also exposed a checkpoint defect that could lose an unvisited
later observation. The [actual reproduction](checkpoint-reproduction.json) and
[before](checkpoint-before.log)/[after](checkpoint-after.log) tests remain recorded.
Partial reports now retain unvisited observations separately and reject duplicate
cache identities. All 26 recovered original clean timings were retained exactly,
including passes and any failures, as verified by
[the recovery audit](recovered-observation-evidence.json). Partial reports never
become acceptance receipts.

## Exact evidence and remaining scope

[Identity](identity.json), [inputs/frozen paths](input-evidence.json),
[binaries/frozen paths](binary-evidence.json), [completion audit](completion-audit.json),
[source snapshot map](source-manifest.json), [claim audit](claim-audit.md) and
[archive hashes](archive-sha256.json) retain exact provenance. Raw trace hashes
point to ignored original observations; content-addressed binaries and input files
are separate from mutable build output. Markdown source copies use `.md.txt`.

[Host checks](checks.log) pass 50 Rust tests (1 worker, 19 library, 30 integration),
39 Python tests, formatting, Clippy, no-default features, release build and
strict-safe/design checks. The [direct library check](lib-no-default-features.log),
[seven specification hashes](specifications.log) and
[live primary errata review](specification-errata-review.json) also pass. The graph
contains two owned packages and no third-party production packages.

[FATE evidence](fate-evidence.json) records two freshly executed upstream reference
targets (Xiph lacing and zlib) and three packet-demux comparisons (also zero-length
block). No full zero-length remux or upstream HDR frame-priority test is claimed.
The broader FATE inventory and sample distribution rights remain open.

[Isolation receipts](camera-isolation.json), [current windows](qualification-windows.log)
and [earlier windows](previous-qualification-windows.log) retain bounded camera
pause/resume and rejected competing-build attempts. Speed and memory use separate
workers; final calibration/observer/worker/input identities match every memory row.

Complete M00–M05, packet side data/configuration changes, resumable input/work,
shared retained-capacity budgets, sustained fuzzing, later formats, simulator
decoding/capture, Streem integration, Linux and 32-bit webOS/TV qualification remain
open. All twelve overarching offboarding milestones remain open. No commit,
publication, Streem change or TV operation is claimed by this receipt. A later
source/binary/input/operation change needs its own affected qualification.

# Qualified colour/static-HDR and K0–K3 slices: native Mac ARM64

All 67 frozen cases pass selected parity, runtime and both lower-memory gates on
2026-10-31. K4 covers all thirteen Colour integer elements and ten mastering
floats, including exact binary32/binary64 source values, partial/zero/default
metadata, all nine siting combinations and 64 tracks. It retains 33 required
profiles. No original K0–K3 profile or contract changed.

Each case retains 30 paired timing observations (50 complete operations per
worker observation) and 30 paired memory observations. Values below are the
worst per-case upper paired 95% ratios, rounded upward. Lower is better.

| Slice | Parity | Time (<=0.80) | Attributable RSS (<1.0) | Live heap (<1.0) |
|---|---:|---:|---:|---:|
| K0 packets | 12/12 | 0.7031 | 0.2963 | 0.1894 |
| K1 header removal | 4/4 | 0.2035 | 0.2858 | 0.0750 |
| K2 zlib | 10/10 | 0.7922 | 0.4375 | 0.6397 |
| K3 Cues/preroll | 8/8 | 0.6828 | 0.2667 | 0.1491 |
| K4 colour/static HDR | 33/33 | 0.6601 | 0.3215 | 0.2643 |

[Summary](summary.json); full raw-sample reports: [K0](k0/report.json),
[K1](header/report.json), [K2](zlib/report.json), [K3](seek/report.json),
[K4](colour/report.json). Every numerical verdict is recomputed from its raw
pairs and exact selected-worker/input identities. All slices pass; complete
module acceptance remains false.

## Source and native views

The production API retains original colour codes, bit depth, subsampling/siting,
optional light/mastering fields and full raw TrackEntry bytes. A separate Python
EBML oracle checks every source field, its IEEE bits and the raw metadata. The
optional FFmpeg 9 view compares all 32 public colour/side-data rational fields
without an epsilon. The native no-parser operation omits some incomplete/zero
HDR side data and exposes no BitsPerChannel through bits_per_raw_sample; these
omissions never erase the source values. Both source and native proofs are required.

A supplementary probe found the rounded half-minimum rational boundary. Its
[original failure](boundary-before.json) remains, and the exact boundary became
the 33rd required case. Source floats never changed. No case, output byte or
numerical gate was removed. Original pre-feature failures remain in
[regression-before.log](regression-before.log).

K4 compares stock pinned FFmpeg for its complete selected operation. K2 retains
its [two owner-approved rules](../../decisions/2026-10-31-zlib-comparisons.md).
Only zlib-mixed uses **pinned FFmpeg plus native scope-1 combined-encoding
restoration**; its result is against that augmented pipeline, not stock FFmpeg
for a feature stock FFmpeg cannot supply. The source API keeps empty frames;
only the approved optional empty-PCM decoder view projects its decoder packets.

## Additional real HDR input remains unaccepted

The upstream `mkv/hdr10tags-both.mkv` vector contains chapters and an attachment.
Our reader currently rejects those features before packet reading. Its original
input/hash and full failure remain in the ignored cache; the
[public receipt](additional-hdr-input.json) records the unsupported result and
raw trace hashes without exporting the external payload. The file was not stripped
or substituted. The full upstream codec/frame-side-data-priority tests were not
executed. Bounded chapters/tags/attachments are the next M01 block. This known
whole-file failure keeps broader M01 qualification open; the 33 declared K4 profiles
are a qualified slice, not a claim of arbitrary Matroska playback.

[FATE evidence](fate-evidence.json) separately records two actually executed
upstream targets (Xiph lacing and zlib) and three passing packet-demux comparisons
(the same two plus zero-length block). The zero-length full upstream remux test
is not claimed. Sample redistribution remains unreviewed and no external payload
is vendored. The broader lexical FATE inventory remains unmapped/unqualified.

## Integrity, checks and remaining scope

[Identity](identity.json), [input hashes/frozen paths](input-evidence.json),
[binary hashes/frozen paths](binary-evidence.json), [completion audit](completion-audit.json),
[claim audit](claim-audit.md) and [archive hashes](archive-sha256.json) retain exact
provenance. Owned source/build/doc snapshots are under `source/`, with
[original-to-stored mapping](source-manifest.json); Markdown snapshots use `.md.txt`
to retain literal original links. Frozen binaries/owned inputs remain under ignored
content-addressed paths, separate from mutable build output.

[Host checks](checks.log): 3 groups totaling 40 Rust tests (1 worker, 12 library,
27 integration), 33 Python tests, formatting, Clippy, no-default-feature checks,
release build and strict-safe/design checks pass.
[The direct library check](lib-no-default-features.log) and
[seven primary specification snapshots](specifications.log) pass too. The graph
has two owned packages, no third-party packages or unsafe production code.

[Isolation windows](qualification-windows.log) retain rejected competing-build
attempts. The first clean complete measurements were kept across checkpoints;
only contaminated/missing observations were retried. The owner-authorized capture
process resumed after every bounded pause, with a separate watchdog. Memory and
speed use separate processes; no allocation observer is loaded for speed.
Calibration probe, observer, worker and input hashes match every final memory case.

Dynamic codec HDR/BlockAdditions, chapters/tags/attachments, remaining track fields,
packet adapters, resumable work/input, shared retained-capacity budgets, complete
M01/M02/FATE and later formats remain open. Linux x86_64, 32-bit webOS, actual TV
and Streem integration are not qualified. Nothing was committed, pushed, published
or changed in Streem. Later source/binary/input/operation changes need their own
qualification; earlier receipts cannot qualify a changed tree.

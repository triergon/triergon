# Qualified K0–K3 development slices: native Mac ARM64

All 34 cases pass their frozen selected parity, runtime and lower-memory gates on
2026-10-31. Every case retains 30 paired timing observations (50 full operations
per worker observation) and 30 paired memory observations. Values below are the
worst per-case upper paired 95% ratios, rounded upward. Lower is better.

| Slice | Parity | Time (required <=0.80) | Attributable RSS (<1.0) | Live heap (<1.0) |
|---|---:|---:|---:|---:|
| K0 packet demux | 12/12 | 0.7055 | 0.3936 | 0.1724 |
| K1 header removal | 4/4 | 0.1992 | 0.2667 | 0.0504 |
| K2 bounded zlib | 10/10 | 0.7902 | 0.4118 | 0.6301 |
| K3 Cues/preroll | 8/8 | 0.6889 | 0.2500 | 0.1291 |

These are qualified development slices, not complete M00/M01/M02 modules or a
Streem replacement. Native Mac ARM64 is the currently authorized performance
target; Linux x86_64, 32-bit webOS and actual TV/integration qualification remain
open. No Streem changes, commits, pushes or publication were performed.

## Approved reference boundary

The owner approved the [two explicit zlib rules](../../decisions/2026-10-31-zlib-comparisons.md).
`zlib-empty` retains its complete empty source event. Its optional PCM decoder-input
view matches native FFmpeg's zero decoder packets, after full decompression and
checksum work. Source bytes/private data and exact source timestamps are separately
checked for all ten K2 cases.

Only `zlib-mixed` is compared against **pinned FFmpeg plus native scope-1
combined-encoding restoration**. Stock FFmpeg does not supply that feature. This
case's timing is against that augmented native pipeline and must not be quoted as
a stock-FFmpeg speed comparison. Every other case retains the native FFmpeg
reference. Raw native differences, their full output fingerprints and independent
source proofs remain recorded. No input, required profile or numerical threshold
was dropped.

All eight K3 fixtures match complete packet sequences across six fresh and six
after-EOF seek requests (96 sequences), preserving recovery audio, preroll and
one-time initial trimming. This is packet-level evidence, not decoded A/V device
recovery proof.

## Reproducible evidence

- [Summary](summary.json), full reports: [K0](k0/report.json), [K1](header/report.json),
  [K2](zlib/report.json), [K3](seek/report.json). Acceptance recomputes statistics from
  raw samples; every report has `slice_accepted: true`, `accepted: false` for the
  remaining broader module/FATE requirements.
- [Exact source/build/compiler identity](identity.json), [input hashes and frozen
  paths](input-evidence.json), [binary hashes and frozen paths](binary-evidence.json).
  Owned source/build/doc inputs are copied under `source/`; Markdown snapshots
  use `.md.txt` to preserve their literal original links outside their original
  tree. [The source manifest](source-manifest.json) maps original to stored paths.
  No dependency is admitted.
- [Completion audit](completion-audit.json), [claim audit](claim-audit.md),
  [host checks](checks.log), [library shipping-feature check](lib-no-default-features.log),
  [seven primary specification hashes](specification-verification.log).
- [FATE evidence](fate-evidence.json): upstream Xiph-lacing and zlib targets execute
  and pass. Their separate demux comparisons and the zero-length-block demux
  comparison pass. The zero-length full upstream remux test is not claimed.
  External vectors/complete raw payloads remain in the ignored cache; sample
  redistribution rights remain unreviewed. The broader 177-identity FATE inventory
  is not a claim of tests executed.

The final integrity pass found cached memory certificates naming an earlier
calibration-probe binary. Cache validation and acceptance now bind both probe and
observer identities. Memory evidence was recollected for all 34 cases under the
current probe; valid timing pairs were retained only for identical selected
workers, input hashes, contracts and operations. All old reports remain intact.
No observer is loaded into speed workers. Certificates qualify the selected
allocator paths, not every possible system-library allocation.

The host suite reports 1 worker + 8 library + 25 integration tests passing, zero
failures, plus 29 Python tests, strict-safe/dependency checks, formatting, Clippy,
no-default-feature checking and release builds. The production graph has two owned
packages, no third-party packages, and forbids unsafe code.

[Archive hashes](archive-sha256.json) seal the receipt after prose/source review.
Earlier failed/partial measurements remain separate historical evidence. A changed
source, worker, input, operation or instrument must be qualified under its own
identity; this receipt cannot qualify later edits automatically.

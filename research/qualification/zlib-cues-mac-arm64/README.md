# Native Mac ARM64: bounded zlib and Cues/preroll

**K2 and K3 remain unaccepted.** The current source implements both features, but
all selected cases must satisfy exact parity and unchanged numeric gates before
qualification. No failed case was removed or normalized into a pass.

| Slice | Exact parity | Maximum runtime upper ratio on matching cases | Maximum RSS upper ratio | Maximum live-heap upper ratio | Accepted |
|---|---:|---:|---:|---:|---|
| K0 packets | 12/12 | 0.780205 | 0.296297 | 0.172378 | Yes |
| K1 header removal | 4/4 | 0.202837 | 0.285715 | 0.050385 | Yes |
| K2 bounded zlib | 8/10 | 0.867840 | 0.460182 | 0.630082 | No |
| K3 Cues/preroll | 6/8 | 0.751547 | 0.266667 | 0.128982 | No |

Ratios in the table are rounded upward. The hard runtime bound is **0.80**;
both memory upper bounds must be strictly below **1.0**. Every matching case has
30 raw timing pairs and 30 pairs in each memory lane. Nonmatching cases receive
no performance claim. The 0.8004435808978564 result for `zlib-dynamic` is a failure.
Seven matching K2 cases still fail the time gate; all matching cases pass memory.
The byte checksum, timed operation boundaries, case matrix, randomization and
confidence calculation were preserved through the decoder/parser optimizations.

Read [the summary](summary.json), [K0](k0/report.json), [K1](header/report.json),
[K2](zlib/report.json) and [K3](seek/report.json). Reports preserve exact failures,
raw CPU-isolation evidence, work checksums, time samples, memory observations and
calibration certificates. Per-case checkpoints permit resuming an interrupted
isolation window without discarding complete clean cases. A complete clean time
failure is retained; it is not repeatedly measured unchanged in search of a pass.

Four exact mismatches remain: empty decoded frames, combined header/zlib encodings,
codec-delay audio preceding a seek RAP, and fresh seeks into unknown-length
Clusters. The specification-facing source data is retained. See
[feature behavior and blockers](../../../docs/zlib-cues.md). These observations do
not establish a blanket FFmpeg bug claim or waive the exact-compatibility gate.

The actual upstream Xiph and zlib FATE targets pass, as do their separate demux
and independent source-time comparisons. The zero-length-block vector passes its
demux/source comparisons; its full upstream remux target is not claimed as run.
See [Xiph](fate-xiph.json), [zlib](fate-zlib.json) and
[zero-length](fate-zero-length.json). External media and external raw payloads remain
in the ignored cache; redistribution rights are unreviewed.

The [source snapshot](source/) contains the owned code, tests, Makefile and every
hashed build/harness input. It has no external Rust dependencies. Reference FFmpeg
is pinned n9.0 with zlib enabled; static reference archives and worker binaries are
hashed. System zlib 1.2.12 is identified by dynamic-link path, runtime version and
macOS build 24G90, rather than a claimed standalone shared-cache content hash.
Complete authored canonical traces remain in the ignored working evidence; their
hashes are retained per slice. Input hashes and generators permit reproduction.
The earlier failed optimization reports/source snapshots are kept alongside this
receipt. Regression logs show the observed failures before each relevant fix.

The camera process was briefly paused only in authorized windows with an
independent resume watchdog and finally cleanup. Each window receipt records its
resume; the process was resumed after the measurements. The controller is archival
evidence for this specific capture session, not a general machine-control tool.

[Host checks](checks.log) passed: 31 Rust and 15 Python tests, formatting, Clippy,
no-default-features, strict-safety and design checks. See the
[manual behavior-claim audit](claim-audit.md). Complete M01/M02, Linux x86_64,
webOS/ARM32 and Streem integration remain unqualified. No commit, push or deployment
was performed.

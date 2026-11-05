# D5 / O62 capture replacement: current source and historical evidence

Goal and ledger: [GOAL.md](GOAL.md). Frozen contract: [contract.json](contract.json).
[Required cases](cases.json).
The [archived collector summaries](#archived-native-collector-reports-superseded-verdicts-linux-arm64)
cover v3 (recorded 15/15, superseded by strict R2), v2 (archived reported
verdicts) and v1 (stopped with the failure retained). The raw collector
reports are kept outside the public checkout.

**Root3477 contains capture85. Standalone quality and runtime delivery are both 15/15. Memory and D5 qualification remain open.**

On 2026-11-03 the owner approved appending only the 4.5× equal-bit comparison
point, giving six factors: 1.25, 1.6, 2.0, 2.6, 3.4 and 4.5. The separate standalone
capture84 quality-only campaign completed on Builder20 at **23:22:35 UTC** with
**15/15 passing cases**. Independent offline review rehashes the 357-file compact,
checks all 15 reports and retained command streams, and recalculates the comparison
predicates and log-linear interpolation. There was no Rust compilation or new
runtime, heap or RSS collection in this campaign.

This result belongs to the existing capture84 worker
`c933a2364157ccf723d61edeb09ca0c659cd7cd474a82e94185add3c606181aa`,
compiled-source manifest
`b1184b27f3d58ebb5d56fdc9042a2f5785ab8bc0cb6a50581d5eb66de3635bd9`.
The approved comparator is
`b0af8672c050e3d992e7dccb7f5d8478d8da15df5d2c9c740c22a35d1743cf08`
and the contract is
`6f924d055cc78f410681d641faa3091d7adff6013524f43db5047c56612c50f1`.
All 15 reports retain 90 candidate frames, 89 reference frames and 89 common
comparison frames. Thirteen complete report objects are unchanged from the prior
five-point run. For c05, equal-bit PSNR-Y is 17.49609271718157 dB versus reference
17.50358164901822 dB; for c11 it is 37.75927572328192 dB versus
37.45712159011508 dB. Both pass the unchanged 0.25 dB tolerance. The reference
worker remains `400a3d4d0282acf210be04c54006b72fa57de4c2a4caedfa0eb81d3fb0dd88af`.

The quality compact is
`.research/d5-quality45-builder20-prep-v1/D5-QUALITY45-ACTUAL-BUILDER20-COMPACT-v1.tar.gz`
(SHA256 `57e48d69dd696ff4f644aa88178aff73da45fc77cc07eaeab256fd65ba05bca3`).
The independent review and per-case matrix are in
`.research/d5-quality45-independent-review-v1/REVIEW.json` and
`QUALITY15-MATRIX.json`. Large input/media outputs are omitted from the compact;
their literal hashes and native-controller byte/FNV checks remain recorded.

**Current capture85 source and evidence.** Capture85 sums each coded 4×4
luma block directly into the coarse plane, removing the intermediate scratch
row. Repeated full coded luma can reuse the retained previous plane. Equal
coded luma proves zero source SSE at the same integer origin. The existing
compensated comparisons still apply to other offsets when needed.

The standalone capture85 worker
`d16abf45e67a8f3cc7d6f7162922cabd41de0224a0c1855d1d83007f5c097c29`
on source map
`2b0172f72da535c090a10715ef9c6b94e5d8292abf3109d0f2a5d8ddcfa1951c`
passed all 15 approved six-factor quality cases. Nominal and auxiliary TS
outputs are byte-identical to the approved capture84 run. The recorded
standalone profiles each passed 85 tests, including all 84 prior names.
The evidence is in
`.research/d5-capture85-time15-prep-v1/ACCEPTED-QUALITY-REVIEW.json`
and `SOURCE-REVIEW.json` in the same folder.

The separate native Linux ARM64 runtime run passed all 15 cases at the
1.25× ceiling. The median case ratio was 0.9900× and the worst paired
upper bound was 1.1107×. Three cases reached the 0.80× optimization target.
The static 480 case's upper bound fell from 1.2779× in capture84 to
1.1107× in capture85. See
`.research/d5-capture85-time15-independent-review-v1/REVIEW.json`.
The exact capture85 memory observer refused the reference allocator closure
before running the selected cases. That refusal is retained.

Root3477 includes both capture85 production files. It passed all 13 root
functional gates with 3,477 passed, zero failed and one ignored test per
profile. The prior Root3474 functional snapshot contained capture84 and
passed 3,474 tests per profile with zero failures and one ignored test.
Both root design gates verified the 1,451 archived evidence files outside
the compiled source maps. Standalone quality, integrated functional
evidence and numerical qualification have separate receipts.

The earlier Root3297 composition contains the scoped capture84
and Source10 PGS Rust deltas alongside D3 catalogue v4. The frozen tested source
is recorded in manifest
`f26b354b3582dd8f9015482b06ab924decb9a21c559344a0b43e566ee4103489`;
its native Linux ARM64 full-workspace debug and release gates each record
3,297 passed, zero failed and one ignored test, with the remaining root checks
passing. The original pre-format disk-floor refusal remains preserved. This
adoption establishes functional gates for the composed source. The 15/15 quality
result above retains its standalone capture84 worker and source scope. No runtime,
live heap or RSS qualification is established here for capture84 or the composed
root; historical numerical passes do not transfer. Capture consumer integration,
target/TV proof and complete D5 qualification remain open.

The prior five-point runs remain immutable. Capture76's Builder19 quality-only
continuation passed 13/15, with c05-noise-960 and c11-motion-960-rate1 classified
`reference-bits-unreachable`; conversion and mux checks passed. Its source manifest
is `43aac08b17f0dd13b885304624b74d908a2dd14f8c9db153932e5f57d796a0ee`,
worker `63e6b7b3c5c02ea6bd92ade13682748060b4e88abc2faa2195b90fdf01642ecb`.
Standalone capture84's five-point campaign also completed 13/15 with the same two
nonpasses. None of those reports, failure labels or archived tables is rewritten.

The previous next19 root contained D3 catalogue v4, PGS8 with nine new guards
and five scoped PGS paths, and capture76. Root3297 adds 15 PGS and eight capture
guards while preserving all 3,274 earlier passing test names. The independent raw
review is `.research/combined-root3297-independent-audit-v1/REVIEW.json`.
The existing M3 observations (4,994 source and 872 native outcomes) retain their
original Root3274 execution scope; 674 dependency files are byte-identical and
these observations were not rerun in Root3297. No Streem integration, TV or
complete-module numerical qualification is claimed.

**Archived report scopes:** v3 reports recorded 15/15 under the older collector,
which tried additional rate points beyond the then-written five-point contract.
The later five-point checks record their separate 13/15 results. The newly
approved six-point standalone capture84 result above changes neither historical
experiment nor its numerical scope. Original tables and labels below remain
archived collector reports. D1–D3 and R2 stay owner-approved contract decisions;
that approval does not retroactively qualify the old collector. The archived v3
report marked four cases at the 0.80 target. All implementation, test, numerical
and platform observations below retain their original source/run scope.

## Archived implementation and test observations (v3 source)

| Component | Code | Contract item |
|---|---|---|
| RGBA → YUV 4:2:0 | `crates/capture/src/convert.rs` | BT.601 limited range, alpha ignored, pair-mean horizontal + center-sited [1,3,3,1]/8 vertical chroma, top-down and bottom-up rows, both geometries |
| MPEG-1 video encoder | `crates/capture/src/mpeg1/` | I/P pictures, GOP 30, sequence+GOP header per I, one slice per picture with macroblock quantiser changes, half-sample forward MC (hierarchical + diamond search), default intra / flat non-intra matrices, mismatch-controlled reconstruction, scene-change I insertion, TM5-derived VBV-constrained rate control (maxrate = b·7/5, VBV = b/7) |
| MPEG-TS muxer | `crates/capture/src/ts.rs` | PAT/PMT/SDT bytes, PIDs and CRC, PCR = DTS·300 on every PES start, RAI on key pictures, 100 ms PAT/PMT and 500 ms SDT cadence plus key-picture PAT, adaptation-field stuffing, per-PID continuity |
| Session | `crates/capture/src/session.rs` | fresh tables + sequence header per session, PTS = 3000(k+1), DTS = 3000k, keepalive re-encode advances timing, sink failure poisons the session |
| Worker | `crates/capture/src/bin/capture-worker.rs` | same CLI as the FFmpeg reference worker (convert, pipeline, recon, time, memory) |

`#![forbid(unsafe_code)]`, no dependencies, no build script
(`tools/check-safety.py` passes); workspace fmt, clippy `-D warnings`, check
and 33 crate tests pass on macOS and natively on Linux ARM64.

32-bit portability (not TV evidence; re-checked on the v3 source): the crate checks and is clippy-clean
for `armv7-unknown-linux-gnueabihf`; all 33 tests pass on `wasm32-wasip1`
(32-bit `usize`, run under Node's WASI), and the wasm32 worker's TS output is
byte-identical to the AArch64 worker's for a 30-frame 480×270 bottom-up
motion session and a 12-frame 960×540 scroll session. Actual 32-bit ARM
execution and TV timing remain O60 work.

Additional robustness (exploratory, run on the v2 source only; local FFmpeg 9.0.2 decoder): 2×2, 18×18,
34×50, 962×542, 4094×16 and 16×4094 inputs, a 400-frame session and
0.1 Mbps at 960×540, each with a forced scene cut, decode without errors and
with the exact frame count.

## Evidence tooling (research only, never production)

- `harness/capture/reference_capture.c`: Streem's exact FFmpeg call sequence
  (`Venc::open`/`encode`, upstream 3bc30f4e `rust-modules/media/src/ff.rs:998-1400`).
- `harness/capture/compare.py`: predeclared correctness rules (conversion ±1,
  own TS walker + FFmpeg re-demux, independent FFmpeg decode, picture types,
  VBV simulation, quality rule v2 with v1 retained).
- `harness/capture/qualify.py`: native gates, reusing the shared harness's
  bootstrap median ratio interval, CPU-contention guard and memory observer.
- `harness/capture/generate_inputs.py`: deterministic, self-authored inputs.
- `harness/capture/summarize.py`: renders a run's per-case table.
- `research/capture/fate-map.json`: FATE identities near this scope, classified
  before execution; none maps to the selected operation, none was run.
- `harness/capture/runner-setup.sh`: pinned FFmpeg 9.0.2 builds (exact Streem
  configuration; same without `--enable-small` as the gating reference; full
  oracle for decoding), observer and workers.

## Declared differences (owner-approved 2026-11-02)

The owner approved D1–D3 and R2 including clause (b): "I'm fine with all of
your decision." (recorded in `contract.json` `owner_review`).

| ID | Difference | Why |
|---|---|---|
| D1 | Frame k's bytes are emitted at call k; FFmpeg emits them one call later and never emits the last frame | Lower latency; per-frame PTS/DTS/PCR identical to FFmpeg's |
| D2 | PMT stream_type 0x01 (FFmpeg 0x02); 0x02 selectable | H.222.0 Table 2-34 reserves 0x02 for constrained-parameter 11172-2 video; these streams are not |
| D3 | SDT provider "Triergon" (FFmpeg "FFmpeg"); selectable | Do not claim another implementation's identity |
| R2 | Encoder comparison rule v2 (contract `encoder_revision_reason`) | FFmpeg's short-session rate control misses the requested rate in both directions; v1 results are kept in every report |

## Archived native collector reports (superseded verdicts, Linux ARM64)

### Run v3 (archived report; verdict superseded)

Run v3, 2026-11-02 23:16 – 2026-11-03 00:11 UTC, second c8g.xlarge runner,
identical OS/toolchain/reference builds and method as v2 below. Source digest
`b2ac546418e79881ae779d223cdb757668df5526f969485415df66a3510550d4`,
candidate `5498f92d…`. Changes from v2: output-neutral kernel work (fused
pair-wise conversion pass, early-exit full-sample SAD, overlapping-load
half-sample SAD) plus three output-changing search/coding changes, each
quality-checked on all 15 cases before the run: the first macroblock of every
row now gets the wide (coarse) search, which a saturating threshold had
silently disabled; the four-diagonal pass runs only after a wide search, else
one directed diagonal; and inter blocks with Σ|residual| < 12·q skip the
transform (65–94% of transformed blocks had quantised to zero; the reference
uses a similar SAD < 20·q skip; 16·q and 20·q were measured and rejected,
20·q failing the scroll case).

| Case | Conversion | Mux | Encoder (mode, bits vs FFmpeg, PSNR-Y ours/FFmpeg) | Runtime ratio median / upper95 | vs Streem build (median) | Heap peak MB ours / FFmpeg | RSS bound upper95 | All gates |
|---|---|---|---|---|---|---|---|---|
| c01-ui-static-960 | pass (±1, 99.45% exact) | pass | pass (within-requested-budget, 1.276×, 54.33/53.20 dB) | 0.843 / **0.844** | 0.806 | 3.18 / 6.63 ✓ | 0.572 ✓ | **pass** |
| c02-ui-scroll-960 | pass (±1, 99.43% exact) | pass | pass (within-requested-budget, 1.0662×, 52.12/52.22 dB) | 0.820 / **0.821** | 0.755 | 3.19 / 6.64 ✓ | 0.572 ✓ | **pass** |
| c03-ui-transition-960 | pass (±1, 99.41% exact) | pass | pass (within-requested-budget, 1.1447×, 53.75/53.08 dB) | 0.833 / **0.834** | 0.783 | 3.18 / 6.63 ✓ | 0.572 ✓ | **pass** |
| c04-motion-960 | pass (±1, 99.23% exact) | pass | pass (equal-or-fewer-bits, 0.9072×, 42.70/42.59 dB) | 1.039 / **1.040** | 0.887 | 3.18 / 6.63 ✓ | 0.572 ✓ | **pass** |
| c05-noise-960 | pass (±1, 99.44% exact) | pass | pass (equal-bits-interpolated, 0.2866×, 17.50/17.50 dB) | 0.084 / **0.084** ✓0.80 | 0.073 | 3.17 / 7.54 ✓ | 0.523 ✓ | **pass** |
| c06-ui-scroll-960-bottomup | pass (±1, 99.43% exact) | pass | pass (within-requested-budget, 1.0399×, 51.35/51.28 dB) | 0.787 / **0.788** ✓0.80 | 0.724 | 3.19 / 6.63 ✓ | 0.572 ✓ | **pass** |
| c07-ui-static-480-bottomup | pass (±1, 99.63% exact) | pass | pass (within-requested-budget, 1.3403×, 52.15/51.60 dB) | 0.928 / **0.929** | 0.870 | 0.87 / 2.15 ✓ | 0.634 ✓ | **pass** |
| c08-ui-scroll-480-bottomup | pass (±1, 99.39% exact) | pass | pass (within-requested-budget, 1.0578×, 51.70/50.54 dB) | 0.855 / **0.856** | 0.776 | 0.88 / 2.16 ✓ | 0.627 ✓ | **pass** |
| c09-motion-480-bottomup | pass (±1, 99.22% exact) | pass | pass (within-requested-budget, 1.031×, 43.90/43.79 dB) | 0.933 / **0.933** | 0.816 | 0.85 / 2.15 ✓ | 0.603 ✓ | **pass** |
| c10-ui-transition-480-topdown | pass (±1, 99.57% exact) | pass | pass (within-requested-budget, 1.1867×, 51.82/50.94 dB) | 0.894 / **0.894** | 0.830 | 0.85 / 2.15 ✓ | 0.601 ✓ | **pass** |
| c11-motion-960-rate1 | pass (±1, 99.23% exact) | pass | pass (equal-bits-interpolated, 0.7317×, 37.76/37.46 dB) | 0.796 / **0.797** ✓0.80 | 0.703 | 3.09 / 6.61 ✓ | 0.555 ✓ | **pass** |
| c12-motion-960-1m | pass (±1, 99.23% exact) | pass | pass (equal-bits-interpolated, 0.8252×, 41.70/41.25 dB) | 0.901 / **0.902** | 0.773 | 3.11 / 6.61 ✓ | 0.570 ✓ | **pass** |
| c13-motion-960-6m | pass (±1, 99.23% exact) | pass | pass (equal-or-fewer-bits, 0.9678×, 43.36/43.33 dB) | 1.019 / **1.020** | 0.882 | 3.29 / 6.64 ✓ | 0.583 ✓ | **pass** |
| c14-motion-960-rate255 | pass (±1, 99.23% exact) | pass | pass (within-requested-budget, 1.1914×, 44.12/43.78 dB) | 0.997 / **0.998** | 0.867 | 3.64 / 6.64 ✓ | 0.621 ✓ | **pass** |
| c15-noise-480-bottomup | pass (±1, 99.44% exact) | pass | pass (equal-or-fewer-bits, 0.9441×, 17.68/17.86 dB) | 0.150 / **0.150** ✓0.80 | 0.132 | 0.90 / 2.27 ✓ | 0.621 ✓ | **pass** |

The archived v3 report recorded all 15 cases as passing under its collector;
parent strict R2 checks supersede that verdict. The report listed runtime upper
95% of 0.08–1.04 (v2:
0.09–1.13); UI cases fell from 0.95–1.13 to 0.79–0.93, motion at 2.5–25.5 Mbps
stays at 1.00–1.04. The 0.80 target is met by c05, c06, c11 and c15. Encoder
verdict modes and the 3 rule-v1 passes (c04, c13, c15) are unchanged from v2.

### Run v2 (archived preceding report)

Run v2, 2026-11-02 21:48–22:41 UTC, AWS c8g.xlarge (Graviton4, Neoverse-V2,
4 vCPU), Ubuntu 24.04, kernel 7.0.0-1014-aws, glibc 2.39, gcc 13.3.0, rustc
1.101.0-nightly (3e92e1a2d 2026-11-01), release profile (fat LTO, 1 codegen
unit), no `target-cpu` flag. Source digest
`e8b02714af66b0f644c2549de7068c9b92ab27bea0cb6acc31bc59a4be0ff153` over
`crates/capture`, `harness/capture`, `research/capture`, `Cargo.toml`,
`Cargo.lock` and the shared harness files used (manifest in `identity.json`).
Binaries (SHA-256): candidate `8791770d…0fe0c1f`, gating reference
(FFmpeg 9.0.2, Streem component set without `--enable-small`)
`400a3d4d…0dd88af`, exact Streem-configuration reference `c5742773…64119b`,
observer `d70fd3ec…6ad60`; full hashes and configure argv in
`setup-receipt.json` and `identity.json`. Runtime: 30 randomised pairs per
case, each sample the median of 3 sessions after 1 warm-up; ratio = shared
bootstrap median-ratio interval. Memory: 10 heap and 10 RSS observer pairs per
case; RSS uses the approved conservative bound (candidate VmPeak ceiling −
start + 2 pages vs reference observed peak − start − 2 pages).

| Case | Conversion | Mux | Encoder (mode, bits vs FFmpeg, PSNR-Y ours/FFmpeg) | Runtime ratio median / upper95 | vs Streem build (median) | Heap peak MB ours / FFmpeg | RSS bound upper95 | All gates |
|---|---|---|---|---|---|---|---|---|
| c01-ui-static-960 | pass (±1, 99.45% exact) | pass | pass (within-requested-budget, 1.276×, 54.33/53.20 dB) | 1.016 / **1.017** | 0.969 | 3.18 / 6.63 ✓ | 0.571 ✓ | **pass** |
| c02-ui-scroll-960 | pass (±1, 99.43% exact) | pass | pass (within-requested-budget, 1.072×, 52.10/52.22 dB) | 0.985 / **0.986** | 0.910 | 3.19 / 6.64 ✓ | 0.572 ✓ | **pass** |
| c03-ui-transition-960 | pass (±1, 99.41% exact) | pass | pass (within-requested-budget, 1.1447×, 53.75/53.08 dB) | 0.977 / **0.978** | 0.918 | 3.18 / 6.63 ✓ | 0.573 ✓ | **pass** |
| c04-motion-960 | pass (±1, 99.23% exact) | pass | pass (equal-or-fewer-bits, 0.9074×, 42.70/42.59 dB) | 1.045 / **1.046** | 0.891 | 3.18 / 6.63 ✓ | 0.571 ✓ | **pass** |
| c05-noise-960 | pass (±1, 99.44% exact) | pass | pass (equal-bits-interpolated, 0.2866×, 17.50/17.50 dB) | 0.085 / **0.086** ✓0.80 | 0.074 | 3.17 / 7.54 ✓ | 0.522 ✓ | **pass** |
| c06-ui-scroll-960-bottomup | pass (±1, 99.43% exact) | pass | pass (within-requested-budget, 1.0401×, 51.36/51.28 dB) | 0.946 / **0.947** | 0.873 | 3.19 / 6.63 ✓ | 0.572 ✓ | **pass** |
| c07-ui-static-480-bottomup | pass (±1, 99.63% exact) | pass | pass (within-requested-budget, 1.3403×, 52.15/51.60 dB) | 1.125 / **1.126** | 1.053 | 0.87 / 2.15 ✓ | 0.636 ✓ | **pass** |
| c08-ui-scroll-480-bottomup | pass (±1, 99.39% exact) | pass | pass (within-requested-budget, 1.0578×, 51.70/50.54 dB) | 0.981 / **0.982** | 0.887 | 0.88 / 2.16 ✓ | 0.628 ✓ | **pass** |
| c09-motion-480-bottomup | pass (±1, 99.22% exact) | pass | pass (within-requested-budget, 1.0306×, 43.90/43.79 dB) | 0.917 / **0.918** | 0.804 | 0.85 / 2.15 ✓ | 0.601 ✓ | **pass** |
| c10-ui-transition-480-topdown | pass (±1, 99.57% exact) | pass | pass (within-requested-budget, 1.1867×, 51.82/50.94 dB) | 1.063 / **1.064** | 0.987 | 0.85 / 2.15 ✓ | 0.603 ✓ | **pass** |
| c11-motion-960-rate1 | pass (±1, 99.23% exact) | pass | pass (equal-bits-interpolated, 0.7313×, 37.81/37.46 dB) | 0.780 / **0.782** ✓0.80 | 0.690 | 3.09 / 6.61 ✓ | 0.560 ✓ | **pass** |
| c12-motion-960-1m | pass (±1, 99.23% exact) | pass | pass (equal-bits-interpolated, 0.8252×, 41.69/41.25 dB) | 1.006 / **1.006** | 0.864 | 3.11 / 6.61 ✓ | 0.570 ✓ | **pass** |
| c13-motion-960-6m | pass (±1, 99.23% exact) | pass | pass (equal-or-fewer-bits, 0.9677×, 43.36/43.33 dB) | 1.023 / **1.024** | 0.885 | 3.29 / 6.64 ✓ | 0.583 ✓ | **pass** |
| c14-motion-960-rate255 | pass (±1, 99.23% exact) | pass | pass (within-requested-budget, 1.1921×, 44.12/43.78 dB) | 0.997 / **0.998** | 0.868 | 3.64 / 6.64 ✓ | 0.634 ✓ | **pass** |
| c15-noise-480-bottomup | pass (±1, 99.44% exact) | pass | pass (equal-or-fewer-bits, 0.9441×, 17.68/17.86 dB) | 0.151 / **0.151** ✓0.80 | 0.133 | 0.90 / 2.27 ✓ | 0.649 ✓ | **pass** |

The archived v2 report recorded these collector outcomes, without establishing
strict R2 acceptance:

All 15: conversion within ±1 (99.2–99.6% exact), mux rule (own TS walker +
FFmpeg re-demux timing) pass, independent FFmpeg decode error-free with the
reference's picture-type sequence, VBV underflow ≤ the reference everywhere
(0 except c11 at 0.1 Mbps where both underflow, ours less), runtime upper
95% ≤ 1.126 (gate 1.25), heap and RSS strictly lower (heap 41–48% of FFmpeg,
RSS bound ratio ≤ 0.65).

Archived collector encoder classifications: 3 cases pass rule v1 outright on equal-or-fewer bits (c04,
c13, c15). 3 pass on equal-bits interpolation because FFmpeg overspent the
request (c05: 3.5× the request and 26 Mbit VBV underflow; c11; c12). 9 pass
as "within requested budget": on UI content FFmpeg stops spending at qmin
while this encoder keeps refining up to the requested rate (1.03–1.34× FFmpeg
bits, −0.12…+1.16 dB); all 9 fail rule v1. In c02 (−0.12 dB at 1.07× bits)
the extra bits do not buy equal-or-better quality. FFmpeg exceeded the
requested rate or VBV in 6 cases (c04, c05, c11, c12, c13, c15).

Run history (all retained): v1 (`qualification/native-linux-arm64-v1`)
measured c01–c04 with an earlier candidate; c02 failed the encoder rule
(−0.71 dB at 1.11× bits) because a speed shortcut skipped half-sample
refinement for SAD ≤ 256. The shortcut was reduced to exact matches, v1 was
stopped while c05 was in progress (marker `STOPPED`) and v2 re-ran every case
with the fixed source. Exploratory Mac and native screening numbers used
during development are not evidence.

## Historical hardware-acceleration evaluation (separate platform results)

[hw/README.md](hw/README.md) records the full evaluation. It is not
qualification evidence. That evaluation changed neither its frozen source nor
its gates; it does not qualify capture76.

- **NVIDIA T4G and Apple M3 Pro, each against a separately pinned,
  hardware-enabled FFmpeg 9.0.2 on the same device:** neither device has
  MPEG-1 encoder hardware (VideoToolbox: no mp1v encoder; NVENC: H.264/HEVC
  only), so encoder acceleration is not proposed.
- **FFmpeg's GPU conversion paths:** `scale_vulkan` cannot produce 4:2:0 and
  does not run on the T4G. `libplacebo` runs on both devices. Both fail the
  ±1 conversion rule, are slower than the CPU path once transfers are
  included, and use far more heap, RSS and device memory.
- **Archived pure-Rust platform observations:** the report recorded correctness
  and memory outcomes for its frozen source and comparator.
- **Spend:** the evaluation runner (g5g.xlarge, $0.69) is torn down; the
  lane total is $1.30.

## Historical optimization observations and remaining integration gaps

- 0.80 runtime optimisation target (historical run v3): met by c05, c06, c11 and c15;
  UI cases 0.82–0.93, motion 0.90–1.04 (against the exact Streem build:
  0.72–0.89). Remaining motion cost is spread evenly over motion search,
  half-sample refinement, DCT and conversion (10–17% each); our worker
  executes ~32% more instructions than FFmpeg at higher IPC, so further gains
  need fewer instructions per macroblock (e.g. a 16-bit-lane DCT).
- Streem adapter (consumer side: hello/rate mapping, socket `send_all`,
  geometry/gap rebuild, JPEG mode) and its integration tests (O50); 32-bit ARM
  build and TV-side behaviour (O60); FFmpeg capture library removal (O70).
- swscale's 32-bit ARM NEON `rgbx_to_nv12` path used on the TV may round
  differently from the AArch64 general path measured in the archived reports.
- Broader corpus: real Streem UI captures (rights are the owner's) would
  strengthen the content classes beyond the authored inputs.
- Fuzz/limit coverage of the public API (odd sizes, extreme rates, long
  sessions) beyond the unit and session tests.

## Archived collector recipe

This recipe records the historical method; it does not authorize new runs or
transfer prior results to modified source. On a native Linux ARM64 host with
Rust nightly, Python 3 + NumPy, a C compiler, make and zlib headers:

```sh
CARGO_INCREMENTAL=0 bash harness/capture/runner-setup.sh
B=~/capture-d5/bin
python3 harness/capture/qualify.py --candidate $B/capture-worker \
  --reference $B/reference_capture-speed --reference-streem $B/reference_capture-streem \
  --oracle-bin ~/capture-d5/ffmpeg/oracle/bin --observer $B/allocations.so \
  --inputs ~/capture-d5/inputs --out ~/capture-d5/qual --pairs 30 --iterations 3 --memory-pairs 10
```

The host must be otherwise idle during the run.

## Shared-core requests

None required. The crate is standalone (`Vec`-backed planes and output); if
the core later wants capture output to flow through `triergon_core::Budget`
or `Bytes`, a retained-capacity charge for the session's planes (~3 pictures)
and per-frame TS buffer is the only owner.

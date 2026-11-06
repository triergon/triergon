# D4 / O61 desktop decoding replacement — handoff

State: **authored code + native Linux ARM64 qualification evidence**, uncommitted. The
replacement decodes the simulator's selected lanes in-process (no ffmpeg child, no native
library, pure safe Rust, no dependencies) and produces the simulator's two outputs (1920x1080
RGBA letterbox, stereo s16). Streem integration, the TV/target proof and removal of the
external ffmpeg belong to the parent (adapter contract: `ADAPTER-CONTRACT.md`). Mac numbers in
the lane STATUS files are development evidence only.

## Identities (final candidate D2)

| Item | Identity |
|---|---|
| Source map | `research/d4/evidence/D2-SOURCE-MAP.sha256` (Cargo.toml, Cargo.lock, crates/, harness/d4); the final tree matches it except 27 generated `harness/d4/__pycache__` files removed afterwards and the later report script `harness/d4/report_tables.py` |
| Candidate | `d4-worker` sha256 `80cb4b7dcaf7e2e1fb02bad8b3132d57863806c08b49c26c65940dea96a35212` (crates/sim, feature `codecs`), release, fat LTO, `-C target-cpu=neoverse-v2` |
| Lane examples (decoder-level grading) | h264 `a5fbe84b…`, hevc `e7cf82d1…`, ac3 `1a6e2e9c…`, aac `c79713fe…`, dca `f1ffe374…` (`bin/D2/BINARIES.sha256`) |
| Reference worker | `harness/d4/reference_decode.c` → `85798460da7235acf177bd76dc3fa6a45188e26a5f8fbffa2912545de5579654` (D1: `e47b5657…`), static against the pinned FFmpeg 9.0.2 libraries on the runner |
| FFmpeg oracle | `ffmpeg` `a3753a1a…`, `ffprobe` `653cebfa…` (FFmpeg 9.0.2, archive `8c385028…`, see `DOWNMIX-CONTRACT.json`) |
| Compiler | rustc 1.101.0-nightly `92b5b8c87c61b20adc9001b41baf587634a74734` (runner default `nightly-2026-10-30`) |
| H0 | `runner.py` `3bbbfc5d…`, memory engine/observer/probe as in M1 (`8415b1d9…`, `202e41c5…`) |
| Policy | `MEASUREMENT-POLICY.md` (declared before D1, two amendments, both dated) |
| Runner | own c8g.xlarge (Graviton4 / Neoverse-V2), never the parent's |

## Scope

Selected/refused matrix: `STATUS.md`. Per lane: `h264/`, `hevc/`, `aac/`, `ac3/`, `dca/STATUS.md`.
Crates: `crates/codec-h264`, `codec-hevc`, `codec-aac`, `codec-ac3`, `codec-dca`, `crates/sim`
(sessions, queues, clock gating, generations, flush/stop/EOS, explicit failure, letterbox RGBA
`scale.rs`, stereo downmix `mix.rs`, Load-payload selection `load.rs`, adapters, `d4-worker`).
Frozen rules: `NUMERIC-RULES.json`, `DOWNMIX-CONTRACT.json` (+ `DOWNMIX-ADDENDUM-heights.json`),
`RGBA-CONTRACT.json`. Workloads: `WORKLOADS.json` (17, frozen before grading).

## Gates (native Linux ARM64)

### Functional (combined M1 + D4 tree, unchanged M1 gate script `functional_gate.py` `126e1c58…`)

D2 tree (`evidence/D2/functional-combined-D2-summary`; full output in `.research/m1d4/evidence/d4-results/functional-combined-D2`): all 20 steps exit 0 — compiler, fmt-check,
test-debug, doc-test-debug, test-release, doc-test-release, clippy (`-D warnings`, workspace,
all targets), shipping, arm32-shipping, the M1 trace guards, worker-release. Real footers:
debug and release each 185 suites, 2354 passed, 0 failed,
10 ignored; doc tests 5 passed per profile. Replays: 1973/1973 equal.
The M1 worker rebuilt from this tree is byte-identical to the qualified C2 worker
(`3476d14808e8939168e9273af7ddd423be9dfc5541af0aac9bb0a2740e3dcbf4`), so the M1 TIME/MEM evidence applies unchanged. The D1 tree gave the same
result (`functional-combined-D1-2`).

### Correctness (`native_correctness.py`, 454 vectors, `evidence/D2/correct-D2.jsonl`, summary `CORRECTNESS-D2-SUMMARY.json`)

Decoder level: our lane decoder vs the pinned FFmpeg decoder (video bit-exact per picture, audio
per-channel RMS <= 8.8e-6 and max <= 6.1035e-5, exact counts). Output path: `d4-worker` vs
`reference_decode` (RGBA bit-exact, s16 <= 1 LSB). Out of set = typed `Unsupported` naming a
feature the lane matrix declares refused (or no core substream for DTS).

| lane | pass (both levels) | out of set (typed refusal) | refused by both | FAIL |
|---|---|---|---|---|
| aac | 35 | 0 | 0 | 1 |
| ac3 | 1 | 0 | 0 | 5 |
| dts | 22 | 1 | 0 | 0 |
| eac3 | 1 | 0 | 0 | 5 |
| h264 | 170 | 21 | 3 | 1 |
| hevc | 174 | 10 | 2 | 2 |

Every failure (none is excluded; dispositions are explanations, not exemptions):

| lane | vector | decoder | output | decoder numbers | output numbers | recorded disposition |
|---|---|---|---|---|---|---|
| h264 | test8b43.264 | pass | fail | 43/43 pictures equal | max 1 levels, 10 pictures | RGBA near-tie in the reference horizontal filter coefficients (research/d4/RGBA-CONTRACT.json) |
| hevc | DELTAQP_A_BRCM_4.bit | pass | fail | 96/96 pictures equal | max 2 levels, 71 pictures | RGBA near-tie in the reference horizontal filter coefficients (research/d4/RGBA-CONTRACT.json) |
| hevc | PICSIZE_A_Bossen_1.bit | pass | fail | 10/10 pictures equal | max 1 levels, 1 pictures | RGBA near-tie in the odd-width output stage (research/d4/RGBA-CONTRACT.json) |
| ac3 | millers_crossing_4.0.ac3 | fail | fail | RMS 4.90e-04, max 5.80e-03 | max 61 LSB | AC-3/E-AC-3 dither and SPX/AHT noise: FFmpeg random generator (owner decision 1 in research/d4/STATUS.md) |
| ac3 | monsters_inc_2.0_192_small.ac3 | fail | fail | RMS 2.14e-04, max 2.08e-03 | max 68 LSB | AC-3/E-AC-3 dither and SPX/AHT noise: FFmpeg random generator (owner decision 1 in research/d4/STATUS.md) |
| ac3 | monsters_inc_5.1_448_small.ac3 | fail | fail | RMS 8.83e-06, max 6.45e-05 | max 2 LSB | AC-3/E-AC-3 dither and SPX/AHT noise: FFmpeg random generator (owner decision 1 in research/d4/STATUS.md) |
| ac3 | mp3ac325_a0.ac3 | fail | fail | RMS 4.80e-05, max 2.45e-04 | max 3 LSB | AC-3/E-AC-3 dither and SPX/AHT noise: FFmpeg random generator (owner decision 1 in research/d4/STATUS.md) |
| ac3 | mp3ac325_a1.ac3 | fail | fail | RMS 1.19e-04, max 5.91e-04 | max 19 LSB | AC-3/E-AC-3 dither and SPX/AHT noise: FFmpeg random generator (owner decision 1 in research/d4/STATUS.md) |
| eac3 | csi_miami_5.1_256_spx_small.eac3 | fail | fail | RMS 1.43e-03, max 2.61e-02 | max 142 LSB | AC-3/E-AC-3 dither and SPX/AHT noise: FFmpeg random generator (owner decision 1 in research/d4/STATUS.md) |
| eac3 | csi_miami_stereo_128_spx.eac3 | fail | fail | RMS 5.00e-04, max 1.03e-02 | max 338 LSB | AC-3/E-AC-3 dither and SPX/AHT noise: FFmpeg random generator (owner decision 1 in research/d4/STATUS.md) |
| eac3 | csi_miami_stereo_128_spx_small.eac3 | fail | fail | RMS 4.46e-04, max 7.48e-03 | max 245 LSB | AC-3/E-AC-3 dither and SPX/AHT noise: FFmpeg random generator (owner decision 1 in research/d4/STATUS.md) |
| eac3 | matrix2_commentary1_stereo_192_small.eac3 | fail | fail | RMS 2.62e-04, max 5.42e-03 | max 178 LSB | AC-3/E-AC-3 dither and SPX/AHT noise: FFmpeg random generator (owner decision 1 in research/d4/STATUS.md) |
| eac3 | the_great_wall_7.1.eac3 | fail | fail | RMS 1.99e-04, max 2.81e-03 | max 32 LSB | AC-3/E-AC-3 dither and SPX/AHT noise: FFmpeg random generator (owner decision 1 in research/d4/STATUS.md) |
| aac | aac-sce-in-stereo.adts | fail | pass | RMS 8.34e-02, max 1.29e-01 | max 1 LSB | FFmpeg leaves the FR plane of SCE-only frames unwritten (stale pooled buffer); ours is silent there (research/d4/aac/STATUS.md) |

Dither-stripped AC-3/E-AC-3 (evidence for owner decision 1; D1 binaries — the ac3 decoder is
identical in D2 `1a6e2e9c…`; streams rewritten by `codec-ac3-strip-dither` `d2625558…`,
`evidence/correct-D1-ac3-nodither.jsonl`):

| vector | decoder | worst RMS | output | max LSB |
|---|---|---|---|---|
| diatonis.ac3 | pass | 7.16e-11 | pass | 1 |
| millers_crossing_4.0.ac3 | pass | 4.00e-09 | pass | 1 |
| monsters_inc_2.0_192_small.ac3 | pass | 1.64e-09 | pass | 1 |
| monsters_inc_5.1_448_small.ac3 | pass | 2.80e-12 | pass | 1 |
| mp3ac325_a0.ac3 | pass | 3.85e-09 | pass | 1 |
| mp3ac325_a1.ac3 | pass | 4.23e-09 | pass | 1 |
| csi_miami_5.1_256_spx_small.eac3 | fail | 1.40e-03 | fail | 133 |
| csi_miami_stereo_128_spx.eac3 | fail | 4.24e-04 | fail | 317 |
| csi_miami_stereo_128_spx_small.eac3 | fail | 3.67e-04 | fail | 258 |
| matrix2_commentary1_stereo_192_small.eac3 | pass | 3.73e-09 | pass | 1 |
| serenity_english_5.1_1536_small.eac3 | pass | 1.64e-12 | pass | 1 |
| the_great_wall_7.1.eac3 | pass | 4.60e-09 | pass | 1 |

### TIME and MEM (30 pairs, seed 280; TIME delivery upper95 <= 1.25, target <= 0.80; MEM heap and attributable RSS upper95 < 1)

D1 is first-clean and kept (3 MEM failures); D2 is the final candidate. Raw records:
`.research/m1d4/evidence/d4-results/time-D*`, `mem-D*` (hash lists `evidence/D*/*.sha256`), ledgers `D4-TIME-LEDGER.json`, `D4-MEM-LEDGER.json`. MEM
lifecycle per run: module absent → negative control → unload → register → positive control →
17 cases with the 28-record guard certificate → unload → module absent (`evidence/D2/mem-D2-summary/commands.json`).
No contamination, no retry, no recollection was needed in D1 or D2.

### D1

| workload | TIME upper95 | median | delivery <=1.25 | target <=0.80 | digest equal | heap upper95 | RSS upper95 | MEM pass | all gates |
|---|---|---|---|---|---|---|---|---|---|
| h264-high-1080p | 0.820 | 0.818 | True | False | True | 0.925 | 0.970 | True | PASS |
| h264-main-1920x800 | 0.736 | 0.734 | True | True | True | 0.919 | 0.958 | True | PASS |
| h264-high-720p | 0.557 | 0.557 | True | True | True | 0.902 | 0.926 | True | PASS |
| hevc-main-1080p | 0.685 | 0.684 | True | True | True | 0.770 | 0.911 | True | PASS |
| hevc-main10-1080p | 0.517 | 0.516 | True | True | True | 0.839 | 0.939 | True | PASS |
| hevc-main10-2160p | 0.850 | 0.849 | True | False | True | 0.821 | 0.847 | True | PASS |
| aac-lc-5.1 | 0.931 | 0.930 | True | False | False | 0.284 | 0.453 | True | PASS |
| aac-lc-stereo | 0.784 | 0.784 | True | True | False | 0.730 | 1.063 | False | FAIL |
| dts-core-es-6.1 | 0.885 | 0.884 | True | False | False | 0.958 | 1.532 | False | FAIL |
| dts-core-7.1-master | 1.002 | 1.001 | True | False | False | 1.094 | 1.359 | False | FAIL |
| ac3-5.1-448 | 1.044 | 1.043 | True | False | False | 0.297 | 0.652 | True | PASS |
| ac3-2.0-192 | 0.949 | 0.948 | True | False | False | 0.404 | 0.723 | True | PASS |
| ac3-4.0 | 0.825 | 0.825 | True | False | False | 0.309 | 0.732 | True | PASS |
| eac3-7.1 | 1.022 | 1.021 | True | False | False | 0.422 | 0.850 | True | PASS |
| eac3-5.1-spx | 0.937 | 0.936 | True | False | False | 0.297 | 0.701 | True | PASS |
| eac3-stereo-spx | 0.972 | 0.972 | True | False | False | 0.438 | 0.802 | True | PASS |
| eac3-5.1-1536 | 0.835 | 0.834 | True | False | False | 0.328 | 0.763 | True | PASS |

D1: TIME delivery 17/17, target 5/17, MEM 14/17, all gates 14/17

### D2

| workload | TIME upper95 | median | delivery <=1.25 | target <=0.80 | digest equal | heap upper95 | RSS upper95 | MEM pass | all gates |
|---|---|---|---|---|---|---|---|---|---|
| h264-high-1080p | 0.823 | 0.821 | True | False | True | 0.925 | 0.972 | True | PASS |
| h264-main-1920x800 | 0.734 | 0.733 | True | True | True | 0.919 | 0.960 | True | PASS |
| h264-high-720p | 0.559 | 0.559 | True | True | True | 0.902 | 0.939 | True | PASS |
| hevc-main-1080p | 0.688 | 0.687 | True | True | True | 0.770 | 0.913 | True | PASS |
| hevc-main10-1080p | 0.521 | 0.520 | True | True | True | 0.839 | 0.943 | True | PASS |
| hevc-main10-2160p | 0.848 | 0.848 | True | False | True | 0.821 | 0.848 | True | PASS |
| aac-lc-5.1 | 0.926 | 0.925 | True | False | False | 0.280 | 0.199 | True | PASS |
| aac-lc-stereo | 0.774 | 0.773 | True | True | False | 0.719 | 0.240 | True | PASS |
| dts-core-es-6.1 | 0.877 | 0.876 | True | False | False | 0.367 | 0.176 | True | PASS |
| dts-core-7.1-master | 1.004 | 1.003 | True | False | False | 0.325 | 0.172 | True | PASS |
| ac3-5.1-448 | 1.039 | 1.039 | True | False | False | 0.294 | 0.161 | True | PASS |
| ac3-2.0-192 | 0.939 | 0.938 | True | False | False | 0.390 | 0.165 | True | PASS |
| ac3-4.0 | 0.839 | 0.838 | True | False | False | 0.305 | 0.164 | True | PASS |
| eac3-7.1 | 1.019 | 1.019 | True | False | False | 0.414 | 0.169 | True | PASS |
| eac3-5.1-spx | 0.933 | 0.932 | True | False | False | 0.294 | 0.167 | True | PASS |
| eac3-stereo-spx | 0.963 | 0.962 | True | False | False | 0.389 | 0.169 | True | PASS |
| eac3-5.1-1536 | 0.825 | 0.824 | True | False | False | 0.321 | 0.161 | True | PASS |

D2: TIME delivery 17/17, target 5/17, MEM 17/17, all gates 17/17


## Preserved failures, incidents and harness defects (chronological)

1. Development runs (never evidence): shakedown/dev4 timings and dev MEM checks under
   `~/d4/results-dev` (copied to `.research/m1d4/evidence/d4-dev`).
2. Bench digest replaced before any qualification run (the serial byte fold was 9 ms per 1080p
   RGBA picture on both sides); reference filter graph pinned to one thread (it had
   slice-threaded scaling); memory markers written with write(2) (stdio bypassed the observer);
   memory-mode work JSON without digest (audio may differ by 1 LSB). Policy file.
3. Reference defect found by the dev4 correctness run: without the CLI's probe the reference
   dropped 2–9 out-of-order pictures on 28 H.264 PAFF/MBAFF/B-pyramid vectors; fixed
   (amendment 1). The dev4 rows are kept.
4. `al22_chCfg0PCE_44` (5.1.2 PCE heights) failed the output path by 1795 LSB in dev4: the
   downmix had no rule for height channels. Rule observed natively and added
   (`DOWNMIX-ADDENDUM-heights.json`).
5. First combined functional gate of D1 invalid: `cargo +nightly` auto-installed another nightly
   (`1c3a65069`); kept as `gate-D1-INVALID-compiler-drift`, toolchain removed, gate rerun with the
   unchanged M1 gate script on the pinned toolchain. One further start failed before any step
   (rustc not on PATH under nohup), kept as `functional-combined-D1-STARTUP-NO-RUSTC`.
6. **D1 MEM failed 3/17** (first-clean, kept): aac-lc-stereo RSS 1.063; dts-core-es-6.1 RSS
   1.532; dts-core-7.1-master heap 1.094 and RSS 1.359. Causes: (a) the reference's load-time
   probe pre-warmed its decoder outside the window for audio (amendment 2, decided after
   seeing D1, disclosed); (b) the DTS decoder allocated worst-case subband/synthesis buffers
   (8 slots x 32 bands x 132 floats) and `d4-worker` indexed every access unit before decoding.
   (b) is fixed in the candidate (D2). D1 is never retimed.

## Owner decisions requested

1. **AC-3/E-AC-3 dither and SPX/AHT noise.** FFmpeg's pseudo-random generator makes the frozen
   float rule unreachable for an independent decoder (FFmpeg against itself with another seed
   fails the same way). Native evidence for the alternative: with dither stripped
   (`codec-ac3-strip-dither`, `evidence/correct-D1-ac3-nodither.jsonl`) all 6 AC-3 and 3/6
   E-AC-3 vectors pass both levels (RMS <= 4.6e-9, s16 <= 1 LSB); the 3 SPX streams still differ
   (RMS up to 1.4e-3) because SPX noise blending is not affected by dither flags. Options:
   authorise an FFmpeg-compatible generator (requires knowing it; black-box recovery not done),
   or accept dither-stripped exactness plus a statistical noise check.
2. **Table provenance.** DTS ADPCM/HF VQ codebooks and the perfect-reconstruction filter were
   recovered by black-box observation of FFmpeg output (ETSI TS 102 114 names them but prints
   none). Several AAC SBR/PS tables (SBR noise and Huffman tables, PS Huffman tables, 12/8/4-band
   hybrid prototypes, k→i maps) are from recall and trusted only because every vector passes;
   ISO/IEC 14496-3 was not available. Acceptance or a licensed source is the owner's call.
3. **aac-sce-in-stereo**: FFmpeg leaves FR of SCE-only frames as stale pooled-buffer data
   (oracle artefact); ours is silent there. Decoder-level fails (RMS 6.4e-2), output path passes.
4. **Toolchain**: `codec-hevc`'s default `simd` feature uses `std::simd` (nightly); scalar
   `--no-default-features` builds on stable and passes all vectors, slower.
5. **Single-core comparison**: the reference decoder and filter graph run one thread; the
   shipped CLI would thread decoding and scaling. Declared in the policy.
6. **RGBA near-ties**: 3 vectors differ by 1–2 levels in a few pixels (reference filter
   coefficient rounding not fully pinned, `RGBA-CONTRACT.json`).

## Remaining gaps

- **Optimization target**: 5/17 workloads reach <= 0.80. Audio lanes sit at 0.77–1.04
  (AC-3 5.1 1.039, E-AC-3 7.1 1.019, DTS 7.1 1.004 are the slowest); H.264 1080p 0.82 and HEVC
  2160p 0.85 miss it narrowly. Same-size 8-bit RGBA conversion is still ~2x swscale's NEON
  routine on the Mac (no explicit SIMD in `scale.rs`).
- **AC-3/E-AC-3 noise** (owner decision 1): 10/12 vectors fail the frozen rule with dither; 3
  SPX streams fail even dither-stripped.
- **RGBA near-ties**: test8b43 (max 1), DELTAQP_A (max 2), PICSIZE_A (max 1).
- **aac-sce-in-stereo**: FFmpeg oracle artefact (decision 3).
- **Unverified paths** (no vector exercises them): AAC SBR Huffman tails, fine-IID PS books, PS
  mixing B, 34/5-band IPD/OPD maps; DTS XXCh (refused), X96+XCh together, X96 noise fill
  (ABITS96 = 0) and residual overload at scale >= 61 (synthetic only, failing); AC-3 bsid 9/10
  rest on crafted streams (no real DolbyNet stream); E-AC-3 reduced-rate path with
  `ffmpeg_compat = false`.
- **DTS-HD XLL-only streams**: out of the declared feed; our decoder yields nothing without a
  typed error (should become an explicit `Unsupported`).
- **Error concealment**: H.264/HEVC drop the damaged remainder of a picture where FFmpeg
  conceals; conformance vectors are intact, the fuzz runs found no panics.
- **Workspace hygiene**: examples named `decode` exist in five crates, so a workspace-wide
  `cargo build --examples` warns about an output-filename collision (cargo #6313).
- **Not measured**: the parent's Streem integration, device output, TV/target performance,
  other hosts. Only the 17 frozen workloads carry TIME/MEM; the conformance vectors carry
  correctness only.
- **Height-channel downmix** observed only with default mix levels (enough for the AAC lane).

## Shared-core requests

None from D4: the D4 crates use only `std`. The M1 core requests are in `research/m1/HANDOFF.md`.

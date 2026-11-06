# D4 AAC decoder lane — status

Crate: `crates/codec-aac` (`triergon-codec-aac`), pure safe Rust (`#![forbid(unsafe_code)]`),
no dependencies, no build script. Development evidence only (macOS arm64, Homebrew FFmpeg
9.0.2 as black-box oracle). Nothing here is a qualification, parity or performance claim.

**State:** working prototype. AAC Main/LC/LTP core incl. CCE, PCE, PNS, IS, M/S, TNS,
pulse, Main prediction, LTP: all 16 selected core vectors pass the frozen numeric rules,
and one more (`aac-sce-in-stereo`) matches on FL; its FR failure has a stated cause.
**SBR and PS (2026-11-03, second lane): all 18 selected SBR/PS vectors now pass** the
frozen rules against FFmpeg 9.0.2 (worst max |d| 2.3e-5 on `al_sbr_ps_04_new`, all others
≤ 5.9e-6), and the four with a conformance `.s16` at the same rate agree with it to ≤ 1 LSB.
The earlier failures and their root causes are kept below as history. Table provenance is
not complete: several SBR/PS tables are still recalled and only validated by these vectors
(see provenance). The LC decode path was also sped up 2.7x (see timing).

## API (semantics)

```rust
let mut dec = Decoder::new();
dec.decode(adts_frame, &mut frame)?;   // one ADTS frame incl. header (7 or 9+ bytes)
dec.reset();                            // drop overlap / predictor / SBR / PS history
```

- `AudioFrame { sample_rate, layout, samples_per_channel, planes }`. **Deviation from
  the brief:** `decode` *replaces* the frame's contents (capacity reused). It does not
  append, because the output format can change between access units (implicit SBR/PS,
  PCE). One ADTS frame with N raw_data_blocks yields N × 1024 (or N × 2048 with SBR)
  samples.
- `ChannelLayout::speakers()` gives each plane's position as a `Speaker`. The
  discriminant is FFmpeg's `AV_CHAN_*` id. Planes are in FFmpeg native order, so plane i
  equals FFmpeg output channel i. The exception is PCE layouts that FFmpeg reports as
  "unknown": those keep PCE element order, as FFmpeg does.
- Errors: `Unsupported(Feature)` (SSR / gain control, sample-rate index 13–15),
  `InvalidData(&str)` naming the syntax element, `ResourceLimit`, `Truncated`. A failed
  AU leaves `out` empty. FFmpeg also drops failed packets; see `foo.aac`.
- `examples/decode.rs IN OUT.f32`: splits ADTS with its own sync search (skips ID3v2,
  stops at ID3v1 `TAG`). It writes interleaved f32le in plane order and prints a single
  JSON line `{sample_rate, channels, layout, frames, samples_per_channel, errors,
  format_changes}` to stderr. `examples/compare.rs CAND REF CH` applies the numeric
  rules. Development aids: `examples/stats.rs` (per-frame parse statistics, enables
  `Decoder::debug_enable_stats`), `examples/strip_sbr.rs` (core-only stream),
  `examples/sbr_mute.rs` (SBR signalled but never started: pure-QMF oracle probe),
  `examples/bench.rs IN [PASSES]` (in-process decode timing).

## Feature matrix

| Feature | State |
|---|---|
| ADTS header, CRC-protected header, 1..4 raw_data_blocks/frame (+ per-block CRC fields) | implemented; CRC is parsed, not verified (FFmpeg does not verify it either). Synthetic tests. FFmpeg warns "More than one AAC RDB per ADTS frame is not implemented" but gives the same sample count |
| Profiles: Main (1), LC (2), LTP (4) | implemented, vectors pass |
| SSR (3) / gain control | typed refusal `Unsupported(SsrGainControl)` |
| ER / LD / ELD / USAC / 960-frame | ADTS cannot carry them (FFmpeg's ADTS muxer refuses AOT 23/39/42 and 960/120 windows), so they never reach this decoder. Listed as contract gaps below |
| SCE, CPE (common window, ms_mask 0/1/2), LFE, DSE, FIL (fill / SBR / DRC skipped), PCE incl. height extension, END | implemented |
| CCE: dependent (before / after TNS) and independent (time domain) coupling, gain lists, sign | implemented; al07 (dependent), al15/am05 (independent) pass |
| Section / scalefactor / spectral Huffman (11 books + escape), pulse data | implemented; all 11 books verified symbol by symbol (see provenance) |
| M/S, intensity (incl. FFmpeg's inversion with ms_mask_present 2), PNS (FFmpeg LCG noise) | implemented; PNS exact vs FFmpeg (al18, 2.2e-8 max) |
| TNS long/short (fused multiply-add like the reference build) | implemented |
| Main prediction (16-bit rounding / truncation / even rounding, group reset, PNS reset) | implemented; am00/am05 pass |
| LTP (forward MDCT, TNS analysis on the estimate) | implemented; ap05 passes |
| Windows sine/KBD, long/start/short/stop, IMDCT 2048/256 (own FFT), overlap-add | implemented |
| Channel mapping chcfg 1..7 (5/6 → side, 7 → 7.1) and PCE mapping incl. FFmpeg's chcfg pattern rule and "unknown" order | implemented. Probed against FFmpeg with `tools/probe_layout.py` |
| Stream quirks: SCE in chcfg 2 (FL then FR), CPE in chcfg 1 (→ stereo), `libfaac` FIL signature (skip 1024 samples) | implemented, matching FFmpeg |
| SBR: header/reset, grid (all classes), dt/df, invf, envelope + noise (coupled / balance), sinusoids, freq tables, patches (≤ 6, needed for CT streams), limiter, gain/boost, smoothing, noise, QMF 32/64 | implemented; all SBR vectors pass (cm_48_2, cm_48_5.1, fsaac48, 13 CT streams) |
| Implicit SBR: output at twice the core rate from the first SBR frame | implemented (`fsaac48`: 48 kHz ADTS core → 96 kHz output, same as FFmpeg). SBR payloads on core rates above 48 kHz are ignored (assumption, no vector exercises it) |
| Implicit PS: mono stream with SBR in the first frame → stereo output (copy until the first PS data) | implemented, matching FFmpeg's frame and sample counts |
| PS: header/frames, iid (coarse/fine), icc, ipd/opd (incl. smoothing), 20- and 34-band hybrid, 20/34 switching, decorrelation + transient attenuation, mixing A/B, envelope interpolation | implemented; 20-band (ps_04, CT) and 34-band (ps_06, incl. per-frame 10/34 resolution changes and a 34 → 20 → 34 switch) pass. Not exercised by any vector: mixing B (icc_mode > 2), IPD/OPD in the 34-band configuration or at 5-band resolution, fine IID |
| DRC | parsed and ignored. FFmpeg does not apply it either, by default |

## Table provenance (read this)

The brief's allowed sources were 3GPP TS 26.401–26.405 and the implementer's knowledge of
ISO/IEC 14496-3. TS 26.402 does not contain the SBR/PS tables: it covers error concealment,
downmix and resampling. Sources actually used:

Documents (kept outside git under `target/agent-aacsbr/spec/`; spec text and printed
tables/figures only, no reference code opened):

| document | version | file | SHA-256 | used for |
|---|---|---|---|---|
| ETSI TS 101 980 (DRM) | V1.1.1 (2001-09) | `drm_v111.pdf` (etsi.org deliver ts_101980v010101p.pdf) | `0469110f2565361d7caf2980a03386131673169baa4b48c7553b6b23a13a56d9` | QMF window values (Annex I Table I.1) |
| 3GPP TS 26.404 | V19.0.0 (2025-10) | `26404.zip` (3gpp.org archive 26_series/26.404/26404-j00.zip; docx inside `1d78f3ab…`) | `4b03ff8eefca04184830b8357e486374a562dffffbbadb373e46ac3eed9f2336` | QMF analysis structure (Figure 6, text recovered from the figure's WMF) |
| 3GPP TS 26.405 | V19.0.0 (2025-10) | `26405.zip` (26_series/26.405/26405-j00.zip; docx inside `4154a582…`) | `4a88e9121d00671c9f139873183c884d24ad9a232dda72385c2f8bb5551f55b4` | QMF synthesis structure (Figure 4), 8-band hybrid prototype (Table 1) |

Searched and not usable here: ISO/IEC 14496-3 (not freely available); ETSI TS 101 980's own
SBR bank (Figure 14) is an older design (its printed analysis/synthesis does not reproduce
the MPEG bank: 4.2e-3 max at best scale/delay); ETSI TS 103 190-1 (AC-4) has a different
window and noise table; the AES/academic papers found that print the equations were not
retrievable (bot-protected repository). Everything below that is "from knowledge" has no
document behind it and is only as good as its validation.

- **AAC scalefactor + spectral books 2–11** (14496-3 Tables 4.A.1, 4.A.3–4.A.12): written
  from knowledge. Unit tests check Kraft equality and prefix freedom.
  `tools/verify_cb.py` then probes the FFmpeg binary (black box) with one crafted ADTS
  frame per codeword: all symbols of books 1–11 decode to the tuple this table assigns.
- **Spectral book 1** (Table 4.A.2): recall was wrong after entry 18 (failed Kraft). The
  codeword set was rebuilt from the book's structure. The symbol assignment for entries
  19–80 was then **determined from FFmpeg black-box probes** (`tools/probe_cb.py`), and it
  agrees with the partial recall. Flagged for re-verification against the normative text.
- **SBR Huffman tables** (4.A.78–4.A.87): t/f env 1.5/3 dB, t env-bal 1.5/3 dB, noise and
  noise-bal from knowledge (Kraft-checked). `f_huffman_env_bal_1_5dB` is from knowledge;
  its tail beyond ±8 is a completion. `f_huffman_env_bal_3_0dB`: the core order (0, −1, +1,
  −2, +2, +3, −3, −4, +4) was **corrected from oracle evidence** by the first lane; the tail
  beyond ±4 is a canonical completion. *Second lane:* the coupled-stereo `cm` vectors now
  match FFmpeg to 5.9e-6 with these tables, so every codeword those streams use decodes to
  the right value; codewords they do not use (the completed tails) remain **unverified**.
  No document for these tables is available here.
- **PS Huffman tables** (Annex 8.B): from knowledge, Kraft-checked, not individually probed.
  Validated by ps_04 (IID coarse 20, IPD/OPD), ps_06 (IID coarse 10/34, ICC 10/34) and the
  CT streams; the fine-IID books are not exercised by any vector (**unverified**).
- **QMF window** (Table 4.A.89): ETSI TS 101 980 V1.1.1 (DRM, 2001) Annex I Table I.1, a
  public ETSI specification with the same 640 magnitudes. That table mislabels entries
  297/305; values are taken in table order. *Second lane, corrected:* the banks use the
  table **reversed**, c[n] = c_DRM[639 − n] (c[0] = 0). Reversal is not a one-sample shift:
  the DRM order has its sign changes one position later at the segment boundaries 128, 256,
  384, 512. The first lane used the DRM order as-is with tuned phases ((2n − 0.5)/64 and
  (2n − 251)/128 plus a 2-sample delay); that bank equals the right one except at 2 of every
  128 `v` positions, which gave the ~4 LSB16 residual of the old diagnosis 2. Evidence:
  `examples/sbr_mute.rs` zeroes every SBR payload (SBR signalled, never started), so FFmpeg's
  output is the pure analysis → synthesis of the core signal; `tools/qmffit.py` searches
  window offset/orientation, conjugation and phase offsets against it: reversed window,
  (2n − 1/2)/64 and (2n − 255)/128 reproduce FFmpeg to **9.5e-8** max (next candidate
  family 1.0e-4); the crate's banks reproduce it to 1.8e-7 (CT sbr_i-ps_i, cm_48_2).
- **QMF equations** (4.6.18.4.1/2): the exponential forms in `sbr/qmf.rs` are recalled (ISO
  text not available) and confirmed by that probe. The buffer layout, window and summation
  steps follow the flowcharts printed in **3GPP TS 26.404 V19.0.0 Figure 6** (64-band
  analysis: x shift, Z = x·c, u = Σ_j Z[n + 128j]) and **TS 26.405 V19.0.0 Figure 4** (32-band
  synthesis: v shift, g[64n + k] = v[128n + k], g[64n + 32 + k] = v[128n + 96 + k], w = g·c,
  out = Σ w[32n + k]), at the decoder's band counts. The phase terms of those encoder figures
  are not the decoder's and were not used.
- **SBR noise table V** (4.A.90, 512 entries): from knowledge. *Second lane:* validated: the
  CT streams, fsaac48 and cm_48_2 (noise floors in every frame) match FFmpeg to ≤ 5.9e-7.
- **SBR constants** (limiter gains, h_smooth, boost limit 10^(2/10), chirp tables, t_HFGen /
  t_HFAdj): from knowledge; validated by the same vectors.
- **PS constants**: IID/ICC quantisers, all-pass constants, the 8-band hybrid prototype
  (3GPP TS 26.405 V19.0.0 Table 1 column 1, matches recall), and the 2/12/8/4-band
  prototypes, k→i maps and f_center tables from knowledge. *Second lane:* the 34-band hybrid
  band order was wrong (QMF band 0's 12 sub-bands were permuted like the 20-band
  configuration's 8); the order is now filter order q = 0..11, and it is the only one for
  which the recalled F_CENTER_34 equals each filter's centre frequency (QMF band p, filter q
  of Q: c = (q + 1/2)/Q cycles per slot, frequency p + 2c (even p) / p + 1 + 2c (odd p),
  modulo 2 QMF bands). The 10 → 34 parameter map was wrong (recalled); it is now derived as
  10 → 20 (b → 2b, 2b + 1) composed with 20 → 34, and 20 → 34 / 34 → 20 are consistent with
  the k→i layouts. 12/8/4-band prototypes: recalled, validated by ps_06 (3.1e-7 max). The
  26.405 Table 1 column 2 (4-band) prototype is the encoder's 77-band design, not used.
- **PS IPD/OPD band maps** (5 → 11, 5/11 → 17, 17 → 11): derived from the band layouts
  (17 → 11 recalled); no vector exercises them: **unverified**.
- **PS 20/34 switching** (no document): when the configuration changes, the decorrelator
  state (delay lines, transient detector) is cleared, the hybrid input history is kept, and
  the mixing-matrix interpolation start values are mapped to the new parameter-band
  resolution with the 20 → 34 / 34 → 20 maps. Determined from ps_06 frame 191/192 (one PS
  frame with IID disabled switches to 20 bands): without the remapping 5.0e-2, with it
  3.1e-7.
- **SBR coupled-grid bookkeeping** (no table): the second channel of a coupled CPE copies the
  first channel's grid; its l_A of the previous frame must be evaluated before the copy
  (as `parse_grid` does). The old code evaluated it after `num_env` was overwritten, which
  produced the 1e-3…1e-2 errors in VARFIX frames after FIXVAR transients (old diagnosis 3).
- FFmpeg sources under `.research/` were not opened. Some behaviours came from my own
  knowledge of FFmpeg's documented or observable behaviour, and each was then confirmed
  as black-box output: the PNS LCG (constants 1664525 / 1013904223, seed 0x1f2e3d4c, it
  matches to 2e-8), the libfaac skip, the implicit-PS stereo upgrade, and the 6-patch
  allowance. These are listed here so they can be reviewed.

## Per-vector results (FFmpeg 9.0.2 `-f aac … -f f32le`, every sample, no alignment search)

Inputs: FATE `aac/` suite, mp4/3gp extracted with `ffmpeg -c:a copy -f adts`. Raw `.aac`
files are used as-is. Current state (2026-11-03, second lane; `tools/run_all.sh |
tools/summarize.py`; worst channel shown):

| vector (ADTS) | ref rate,layout | result | worst-ch RMS | worst max\|d\| | samples cand/ref |
|---|---|---|---|---|---|
| al04_44 (LC mono PCE, pulse, TNS) | 44100,mono | pass | 6.962e-09 | 7.451e-08 | 363520 / 363520 |
| al05_44 (IS) | 44100,stereo | pass | 2.287e-08 | 2.384e-07 | 327680 / 327680 |
| al06_44 (PCE 3.0, IS, TNS) | 44100,3.0 | pass | 1.354e-08 | 1.490e-07 | 363520 / 363520 |
| al07_96 (PCE 5.1, dependent CCE) | 96000,5.1 | pass | 2.564e-08 | 2.980e-07 | 798720 / 798720 |
| al15_44 (PCE 6ch FLC/FRC, independent CCE, strong TNS) | 44100,FL+FR+FC+LFE+FLC+FRC | pass | 9.215e-07 | 2.947e-05 | 133120 / 133120 |
| al17_44 (two front SCEs, short windows) | 44100,stereo | pass | 9.023e-09 | 7.451e-08 | 330752 / 330752 |
| al18_44 (all-PNS) | 44100,mono | pass | 2.861e-09 | 2.235e-08 | 2880512 / 2880512 |
| al22_chCfg0PCE_44 (PCE height → 5.1.2(back)) | 44100,5.1.2(back) | pass | 2.539e-08 | 1.267e-07 | 53248 / 53248 |
| am00_88 (Main prediction, 88.2 kHz) | 88200,mono | pass | 2.999e-08 | 2.086e-07 | 5781504 / 5781504 |
| am05_44 (Main 5.1, independent CCE) | 44100,5.1 | pass | 8.127e-07 | 2.638e-05 | 133120 / 133120 |
| ap05_48 (LTP) | 48000,stereo | pass | 3.510e-08 | 7.153e-07 | 912384 / 912384 |
| ct_faac-adts (raw, libfaac skip) | 44100,stereo | pass | 3.041e-08 | 3.576e-07 | 880640 / 880640 |
| foo (raw, truncated last frame → 1 error, like FFmpeg) | 44100,stereo | pass | 2.380e-08 | 1.788e-07 | 87040 / 87040 |
| id3v1 (CRC-protected, ID3v1 trailer) | 48000,stereo | pass | 1.216e-08 | 7.451e-08 | 5120 / 5120 |
| id3v2 | 48000,mono | pass | 1.089e-07 | 5.364e-07 | 239616 / 239616 |
| id3v2_two_tags | 48000,mono | pass | 0.000e+00 | 0.000e+00 | 480256 / 480256 |
| aac-sce-in-stereo (chcfg 2 carrying SCEs) | 44100,stereo | **FAIL** (FR only) | 7.266e-02 | 1.296e-01 | 222208 / 222208 |
| al_sbr_cm_48_2 (SBR, coupling, 3 dB/1.5 dB switching, sinusoids) | 48000,stereo | pass | 1.840e-08 | 5.831e-07 | 1544192 / 1544192 |
| al_sbr_cm_48_5.1 (SBR 5.1) | 48000,FL+FR+FC+LFE+FLC+FRC | pass | 1.094e-07 | 5.906e-06 | 1544192 / 1544192 |
| al_sbr_sr_48_2_fsaac48 (SBR, 48 kHz core → 96 kHz) | 96000,stereo | pass | 1.927e-08 | 2.384e-07 | 2351104 / 2351104 |
| al_sbr_ps_04_new (PS 20-band, ipd/opd; SBR from frame 0, PS data from frame 53) | 32000,stereo | pass | 1.090e-06 | 2.317e-05 | 217088 / 217088 |
| al_sbr_ps_06_new (PS 34-band) | 32000,stereo | pass | 1.925e-08 | 3.129e-07 | 434176 / 434176 |
| CT_DecoderCheck (13 files: File1..7, sbr_*-ps_*; mp4/3gp → ADTS + 3 raw .aac; same bitstream) | 44100,stereo | pass (all 13) | 1.993e-08 | 2.980e-07 | 346112 / 346112 |

Independent check against the conformance `.s16` files next to the FATE streams
(`tools/s16cmp.py`, candidate scaled to 16 bit and rounded, best lag searched):

| vector | conformance reference | lag | max | RMS |
|---|---|---|---|---|
| al_sbr_cm_48_2 | al_sbr_hq_cm_48_2.s16 | 0 | 1 LSB | 0.576 LSB |
| al_sbr_cm_48_5.1 | al_sbr_hq_cm_48_5.1_reorder.s16 | 0 | 1 LSB | 0.574 LSB |
| al_sbr_ps_04_new | al_sbr_ps_04_ur.s16 | 0 | 1 LSB | 0.075 LSB |
| al_sbr_ps_06_new | al_sbr_ps_06_ur.s16 | 0 | 1 LSB | 0.571 LSB |
| al_sbr_sr_48_2_fsaac48 | al_sbr_hq_sr_48_2_fsaac48.s16 | – | not comparable: the reference is the downsampled-SBR output at 48 kHz (half the samples); FFmpeg and this decoder output 96 kHz | |

### History: results before the second lane (first lane, 2026-11-03)

Kept as recorded; every SBR/PS row failed.

| vector (ADTS) | ref rate,layout | result | worst-ch RMS | worst max\|d\| | samples cand/ref |
|---|---|---|---|---|---|
| al04_44 (LC mono PCE, pulse, TNS) | 44100,mono | pass | 7.10e-09 | 8.94e-08 | 363520 / 363520 |
| al05_44 (IS) | 44100,stereo | pass | 2.39e-08 | 2.38e-07 | 327680 / 327680 |
| al06_44 (PCE 3.0, IS, TNS) | 44100,3.0 | pass | 1.41e-08 | 1.79e-07 | 363520 / 363520 |
| al07_96 (PCE 5.1, dependent CCE) | 96000,5.1 | pass | 2.67e-08 | 2.98e-07 | 798720 / 798720 |
| al15_44 (PCE 6ch FLC/FRC, independent CCE, strong TNS) | 44100,FL+FR+FC+LFE+FLC+FRC | pass | 9.22e-07 | 2.95e-05 | 133120 / 133120 |
| al17_44 (two front SCEs, short windows) | 44100,stereo | pass | 9.26e-09 | 8.20e-08 | 330752 / 330752 |
| al18_44 (all-PNS) | 44100,mono | pass | 2.95e-09 | 2.24e-08 | 2880512 / 2880512 |
| al22_chCfg0PCE_44 (PCE height → 5.1.2(back)) | 44100,5.1.2(back) | pass | 2.59e-08 | 1.49e-07 | 53248 / 53248 |
| am00_88 (Main prediction, 88.2 kHz) | 88200,mono | pass | 3.10e-08 | 2.09e-07 | 5781504 / 5781504 |
| am05_44 (Main 5.1, independent CCE) | 44100,5.1 | pass | 8.13e-07 | 2.64e-05 | 133120 / 133120 |
| ap05_48 (LTP) | 48000,stereo | pass | 4.37e-08 | 8.94e-07 | 912384 / 912384 |
| ct_faac-adts (raw, libfaac skip) | 44100,stereo | pass | 3.16e-08 | 3.58e-07 | 880640 / 880640 |
| foo (raw, truncated last frame → 1 error, like FFmpeg) | 44100,stereo | pass | 2.46e-08 | 1.79e-07 | 87040 / 87040 |
| id3v1 (CRC-protected, ID3v1 trailer) | 48000,stereo | pass | 1.25e-08 | 7.45e-08 | 5120 / 5120 |
| id3v2 | 48000,mono | pass | 1.09e-07 | 5.36e-07 | 239616 / 239616 |
| id3v2_two_tags | 48000,mono | pass | 0 | 0 | 480256 / 480256 |
| aac-sce-in-stereo (chcfg 2 carrying SCEs) | 44100,stereo | **FAIL** (FR only) | 7.27e-02 | 1.30e-01 | 222208 / 222208 |
| al_sbr_cm_48_2 (SBR, coupling, 3 dB/1.5 dB switching, sinusoids) | 48000,stereo | **FAIL** | 5.48e-05 | 9.71e-03 | 1544192 / 1544192 |
| al_sbr_cm_48_5.1 (SBR 5.1) | 48000,FL+FR+FC+LFE+FLC+FRC | **FAIL** | 4.90e-04 | 6.72e-02 | 1544192 / 1544192 |
| al_sbr_sr_48_2_fsaac48 (SBR, 48 kHz core → 96 kHz) | 96000,stereo | **FAIL** | 3.90e-05 | 1.74e-03 | 2351104 / 2351104 |
| al_sbr_ps_04_new (PS 20-band, ipd/opd; SBR from frame 0, PS data from frame 53) | 32000,stereo | **FAIL** | 1.13e-05 | 3.13e-04 | 217088 / 217088 |
| al_sbr_ps_06_new (PS 34-band) | 32000,stereo | **FAIL** | 2.30e-03 | 4.84e-02 | 434176 / 434176 |
| CT_DecoderCheck (13 files: File1..7, sbr_*-ps_*; mp4/3gp → ADTS + 3 raw .aac; same bitstream) | 44100,stereo | **FAIL** (all 13) | 2.74e-04 | 2.96e-03 | 346112 / 346112 |

### Resolved failures (root causes found in the second lane)

1. **QMF bank (old diagnosis 2, all SBR/PS vectors).** The window was used in the DRM
   table's order; the right orientation is reversed (see provenance). With the reversed
   window and the standard phases the pure-QMF probe matches FFmpeg to 9.5e-8, and fsaac48
   and all 13 CT streams pass at once (2.4e-7). The old "variable-border PS frame" error in
   the CT streams (old diagnosis 4) was this residual growing in loud frames.
2. **Coupled CPE grid (old diagnosis 3, cm_48_2 / cm_48_5.1).** l_A of the previous frame
   was computed for the second channel after its `num_env` was replaced; only VARFIX frames
   after a FIXVAR/VARVAR transient at the frame end were affected (4 frames in cm_48_2).
   The balance-table tails were not the cause.
3. **PS 34-band (old diagnosis 5, ps_06).** Three faults: (a) the 12 hybrid sub-bands of
   QMF band 0 were permuted; (b) the recalled 10 → 34 parameter map was wrong (only visible
   once ICC at 10-band resolution varies across bands, from frame 164); (c) a 34 → 20 → 34
   configuration switch (frames 191/192) must remap the mixing interpolation start values.

### Remaining failures

1. **aac-sce-in-stereo, FR** (unchanged). FL is decoded correctly (5e-8). FFmpeg never writes
   FR for SCE-only frames. Its FR plane holds stale pooled-buffer contents: FR of frame 20
   equals FL of frame 18 exactly, 119/120/125 equal FL of 118, and so on, while it is 0 in
   unwritten-but-fresh frames. This decoder outputs silence on FR. Reproducing it would mean
   emulating FFmpeg's frame pool, so this is treated as an oracle artefact.
2. **al_sbr_ps_04_new** passes, but its max |d| (2.3e-5) is ~100x the other SBR/PS vectors.
   The excess is in the IPD/OPD frames (58–104); the likely cause is the precision of the
   phase-smoothing table (computed in f64 here); not isolated.

### History: diagnosis recorded by the first lane

1. **aac-sce-in-stereo, FR.** FL is decoded correctly (5e-8). FFmpeg never writes FR for
   SCE-only frames. Its FR plane holds stale pooled-buffer contents: FR of frame 20 equals
   FL of frame 18 exactly, 119/120/125 equal FL of 118, and so on, while it is 0 in
   unwritten-but-fresh frames. This decoder outputs silence on FR. Reproducing it would
   mean emulating FFmpeg's frame pool, so this is treated as an oracle artefact.
2. **SBR QMF bank residual (affects every SBR/PS vector).** The low-band path's linear
   time-invariant response matches FFmpeg to 1e-5 (estimated transfer function). The
   aliasing terms do not: in frames without SBR data (pure 2× QMF upsampling) the peak
   error is about 1.2e-4 absolute (4.7e-4 of peak, ~4 LSB16). FFmpeg is within 1.5 LSB of
   the conformance `.s16` reference; this bank is 4 LSB off. So the bank's alignment is
   not yet the standard's. The implemented bank uses analysis phase (2n − 0.5)/64, which
   is the only choice that makes patched HF match, and synthesis phase (2n − 251)/128 plus
   a 2-sample delay. That synthesis is the oracle-best equivalent of the standard's
   (2n − 255) bank. A search over phase offsets, window shifts, buffer orientations,
   conjugation and boundary-tap signs (`tools/` + scratch scripts) found nothing better.
   The standard's normative 4.6.18.4 text is needed to resolve it. Alone, this residual
   already exceeds the 6.1e-5 max rule on loud frames.
3. **SBR beyond the QMF residual (al_sbr_cm_48_2 / 5.1 / fsaac48).** Frames ≥ ~500 of the
   `cm` material (coupling switching between 1.5 and 3 dB, sinusoids, FIXVAR/VARVAR)
   still show 1e-3…1e-2 errors in the coupled CPE channels. Suspects: the unverified
   balance-table tails, and amp-res switching with time-delta balance decoding. Fixing
   the stop-frequency rule (stopMin from 2 × 4/8/10 kHz) and the `f_env_bal_3dB` core cut
   the HF error from >100 % to 2.6e-5 relative over the file. Simple frames now match to
   7e-5 relative.
4. **PS 20-band (CT_DecoderCheck, ps_04).** ps_04 reaches RMS 9e-6 / 1.1e-5, right at the
   QMF residual floor. CT: frames up to 98 match to ~5e-6 RMS. From frame 99 (first
   variable-border PS frame, with an SBR transient frame next) the error grows to ~5e-4
   RMS and stays. The cause is not isolated yet; the ±1-slot envelope-range variant was
   tested and made it worse.
5. **PS 34-band (ps_06).** RMS 2.3e-3. The 34-band hybrid order, k→i map and 34→20/10
   remaps are from recall and are the likely error. Swapping the 4-band prototype for the
   26.405 one changes nothing; that table is the encoder's 77-band design.

## Vectors outside the selected (ADTS) set

| vector | reason | handling |
|---|---|---|
| usac/* (8), Fd_2_c1_Ms_0x01/0x04 | USAC / xHE-AAC, AOT 42 | ADTS can't carry it (FFmpeg muxer refuses). Contract gap: no USAC decoder |
| er_ad6000np_44_ep0 | ER AAC LD, AOT 23 | ADTS can't carry it. Contract gap |
| er_eld1001np/2000np/2100np | ER AAC ELD, AOT 39 | ADTS can't carry it. Contract gap |
| al04sf_48 | 960-sample frames (frameLengthFlag) | ADTS can't carry it. Contract gap |
| latm_000000001180bc60.mpg, latm_stereo_to_51.ts | LATM/LOAS transport, not ADTS | transport out of scope. The AAC inside is LC / HE-AAC; a LATM → raw-AU path would need an AudioSpecificConfig API (not implemented) |
| CT_DecoderCheck/aacPlusv2.wav | reference PCM only | – |

## Development timing (macOS arm64, non-evidence)

Second lane (2026-11-03), Apple M3 Pro, load average 6–8 from other agents' jobs (so all
numbers are noisy; cycles from `/usr/bin/time -l` were used to judge changes). Ours:
`examples/bench FILE 20`, in-process decode only, fastest pass. FFmpeg: `tools/ffbench.sh
FILE 19`, single-threaded `ffmpeg -stream_loop 19 … -f null -` utime minus the same run with
`-c:a copy`, per pass (best of 5). Neither side includes resampling or s16 conversion.

| input | ours before (ms) | ours now (ms) | FFmpeg (ms) | ratio now |
|---|---|---|---|---|
| d4-inputs/aac-lc-5.1.aac (470 frames) | 20.9 | 7.7 | 14.1–15.0 | 0.52–0.55 |
| d4-inputs/aac-lc-stereo.aac (432 frames) | 7.0 | 2.25 | 5.7–7.8 | 0.29–0.39 |
| am00_88 | – | 19.3 | 64.3 | 0.30 |
| al07_96 | – | 11.2 | 18.9 | 0.59 |
| al_sbr_cm_48_2 | 32.9 (after the LC work, before the QMF work) | 27.6 | 33.0 | 0.83 |
| al_sbr_cm_48_5.1 | 95.9 (same) | 88.8 | 77.8 | 1.14 |
| al_sbr_sr_48_2_fsaac48 | 42.8 (same) | 40.6 | 43.9 | 0.92 |
| ct_sbr_i-ps_i | 8.4 (same) | 7.8 | 7.2 | 1.08 |
| al_sbr_ps_06_new | 9.4 (same) | 8.4 | 10.2 | 0.82 |

Instructions retired for 20 passes of aac-lc-5.1: 4.31e9 → 2.34e9 (cycles 886e6 → 574e6).
The coordinator's Linux ARM64 measurement (decoder 1.9x FFmpeg on aac-lc-5.1, 1.77x on
aac-lc-stereo, both including the d4-worker mix) predates these changes; it has not been
re-measured, and Mac ratios do not establish Linux or 32-bit TV performance.

LC changes (all outputs unchanged within float rounding, max change 7e-7 vs the previous
build; every passing vector still passes):
- spectral decoding: joint codeword + sign-bit lookup (10-bit table per book, values
  pre-inverse-quantised), branch-free sign handling, no per-symbol copies;
- bit reader: 64-bit cached window, a peek is a shift instead of a load;
- FFT: split real/imaginary arrays, radix-4 stages (twiddle-free first stage, final radix-2
  for odd log2), the butterfly loop in a non-inlined function with non-aliasing slices so it
  vectorises (27.5k → 8.6k instructions for 512 points);
- DCT-IV: natural-order pre-twiddle, bit-reversal gather fused with the first stage, last
  stage fused with the post-twiddle, output split into even / reversed-odd halves;
- long-window IMDCT: windowing and overlap-add in one pass straight from the DCT-IV halves;
- no per-frame text formatting (debug statistics are opt-in), no struct copies of
  `IcsInfo`/`Tns`, planes appended instead of zero-filled and copied.
SBR: the 64-band synthesis now uses two 64-point DCT-IVs instead of a zero-padded 128-point
complex FFT. Remaining SBR work (to reach the 0.80 target on 5.1 and PS): the 32-band
analysis (64-point FFT with a bit-reversal gather), LPC/HF generation and HF adjustment
(per-slot smoothing history copies) are now the largest SBR costs.

Peak RSS (first lane): al_sbr_cm_48_5.1 4.9 MB (FFmpeg 13.8 MB), al07_96 2.9 MB (FFmpeg
14.9 MB); not re-measured, no buffers were added.

### History: first-lane timing (process wall time incl. startup and output writing)

| vector | ours ms | ffmpeg ms | ratio |
|---|---|---|---|
| al18_44 | 70.6 | 77.8 | 0.91 |
| am00_88 | 76.4 | 130.9 | 0.58 |
| ap05_48 | 39.1 | 49.9 | 0.78 |
| ct_faac-adts | 33.7 | 54.1 | 0.62 |
| al07_96 | 57.1 | 59.4 | 0.96 |
| al_sbr_cm_48_2 | 91.1 | 78.3 | 1.16 |
| al_sbr_sr_48_2_fsaac48 | 106.2 | 84.2 | 1.26 |
| ct_sbr_i-ps_i | 26.5 | 39.5 | 0.67 |

## Reproduce

```sh
# from the worktree
export CARGO_INCREMENTAL=0 CARGO_TARGET_DIR=target/agent-aacsbr
cargo +nightly test --release -p triergon-codec-aac
cargo +nightly clippy -p triergon-codec-aac --all-targets -- -D warnings
rustfmt --edition 2024 --check crates/codec-aac/src/lib.rs crates/codec-aac/examples/*.rs crates/codec-aac/tests/*.rs
cargo +nightly build --release -p triergon-codec-aac --examples
# vectors: target/agent-aac/vec/*.adts, refs vec/ref/*.f32 (ffmpeg -f aac -i X -f f32le -c:a pcm_f32le)
TGT=$PWD/target/agent-aacsbr research/d4/aac/tools/run_all.sh | python3.14 -I research/d4/aac/tools/summarize.py
AAC_REF_DIR=<dir with name.adts + name.f32> cargo test --release -p triergon-codec-aac --test fate_vectors
python3.14 -I research/d4/aac/tools/s16cmp.py OUT.f32 .research/m1d4/fate-suite/aac/REF.s16 CH 2100
# pure-QMF oracle probe (CT stream shown)
target/agent-aacsbr/release/examples/sbr_mute  target/agent-aac/vec/ct_sbr_i-ps_i.adts X.mute.adts
target/agent-aacsbr/release/examples/strip_sbr target/agent-aac/vec/ct_sbr_i-ps_i.adts X.core.adts
target/agent-aacsbr/release/examples/decode X.core.adts X.core.f32
ffmpeg -f aac -i X.mute.adts -f f32le -c:a pcm_f32le X.mute.ref.f32
python3.14 -I research/d4/aac/tools/qmffit.py X.core.f32 1 X.mute.ref.f32 2 WINDOW.npy
#   WINDOW.npy: np.save of the 640 values of crates/codec-aac/src/sbr/tables.rs QMF_WINDOW
# timing
target/agent-aacsbr/release/examples/bench FILE 20; research/d4/aac/tools/ffbench.sh FILE 19
python3 research/d4/aac/tools/verify_cb.py 1 2 3 4 5 6 7 8 9 10 11   # codebook oracle probe (needs numpy)
python3 research/d4/aac/tools/probe_layout.py 0 S0,C0,C1,L0 'f=S0,C0;b=C1;l=0'
```

Other dev aids in `tools/`: `frameerr.py` (per-frame max |d|), `errspec.py` (error spectrum
per QMF band), `lagscan.py` (integer lag scan), `bitdiff.py` (output equality between two
builds).

Source spec downloads (first lane, `target/agent-aac/spec/`, not committed): 3GPP 26401-j00
(sha256 ee1e24c3…), 26402-j00 (806cd277…), 26403-j00 (035d44f4…), 26404-j00 (4b03ff8e…),
26405-j00 (4a88e912…), ETSI TS 101 980 V1.1.1 PDF (0469110f…). The documents the second lane
relies on are copied to `target/agent-aacsbr/spec/` (table in the provenance section).
ETSI TS 103 190-1 was checked and **not** used: its QMF window and noise table differ.

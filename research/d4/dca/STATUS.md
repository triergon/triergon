# D4 DTS core decoder (`crates/codec-dca`, package `triergon-codec-dca`) — status

State: working prototype. This is development evidence from macOS only. It does not
establish qualification, parity or performance gates. The coordinator's native Linux ARM64
runs decide those.

## Sources

| Source | Use | Identity |
|---|---|---|
| ETSI TS 102 114 V1.6.1 (2019-08) | primary spec: syntax, decoding process, Annex D tables | https://www.etsi.org/deliver/etsi_ts/102100_102199/102114/01.06.01_60/ts_102114v010601p.pdf, sha256 `29f9e50575e48bfe55222c5f004088af379bc9a6c65274ed975a81dca10647ba` |
| ETSI TS 102 114 V1.3.1 (2011-08) | Table D65 only (erratum, see below) | https://www.etsi.org/deliver/etsi_ts/102100_102199/102114/01.03.01_60/ts_102114v010301p.pdf, sha256 `63ec849a0311d83dbbb5625167b33daa56d99c915ec18630f759962c94c3efac` |
| FFmpeg 9.0.2 binary (`/opt/homebrew/bin/ffmpeg`) | black-box oracle only; no FFmpeg/libdca/GPL source was opened | Homebrew build on this Mac |

The PDFs and their `pdftotext -layout` text live in `target/agent-dca/spec/` (scratch, not committed).

## Input and output contract

- Input: one DTS core frame per `Decoder::decode` call. The decoder accepts all four packings that FFmpeg's raw `dts`
  demuxer accepts: 16-bit BE `7FFE8001`, 16-bit LE `FE7F0180`, 14-bit BE `1FFFE800 07Fx` and 14-bit LE `FF1F00E8 Fx07`.
  Each packing is normalized to the canonical 16-bit BE bitstream in a reused scratch buffer.
  - In 14-bit streams, FSIZE counts bytes of the 16-bit-equivalent bitstream. FFmpeg reads it the same way: repacked
    BE16, LE16, BE14 and LE14 copies of every vector give FFmpeg output that is bit-identical to BE16.
  - The core-embedded XCh and X96 extensions inside the frame are decoded. DTS-HD extension substreams are not part of
    the contract: the example splitter and the tests skip them.
- Output: `AudioFrame { sample_rate, layout, samples_per_channel, planes }`. Planes are `f32` at full scale 1.0, in
  FFmpeg's native order (ascending `AV_CHAN_*` bit). `ChannelLayout::speakers()` names every plane; for example,
  5.1(side) is FL FR FC LFE SL SR and 6.1 is FL FR FC LFE BC SL SR. Mapping from AMODE (Table 5-4): 0 → FC; 1–4 → FL
  FR; 5 → FL FR FC; 6 → FL FR BC; 7 → FL FR FC BC; 8 → FL FR SL SR; 9 → FL FR FC SL SR. LFF adds LFE, XCh adds BC, and
  X96 doubles the rate.
- API: `Decoder::new()`, `decode(&mut self, au, &mut AudioFrame) -> Result<(), DecodeError>`, `reset()`.
  `DecodeError` is `Unsupported(name)`, `InvalidData(name)` or `ResourceLimit(name)`. No panics on any input (fuzz
  test). Like FFmpeg, a failed frame is dropped without disturbing any history.
- `examples/decode.rs IN.dts [OUT.f32|--null]` splits the input with its own sync and FSIZE parsing, skipping
  extension substreams. It writes interleaved f32le in plane order and prints one JSON line to stderr:
  `{"sample_rate","channels","layout","speakers","frames","samples_per_channel","errors"}`.

## Feature matrix (spec clause → status → evidence)

"Oracle" means the feature was compared with FFmpeg 9.0.2 at the D4 numeric rules: every sample, per-channel RMS
≤ 8.8e-6 and max ≤ 6.1035e-5. The comparison used real vectors (V) and/or self-authored synthetic streams (S, written
by `tests/support/synth.rs` / `scenarios.rs`).

| Feature | Status | Evidence |
|---|---|---|
| Sync, 4 packings (5.3) | implemented | V (all 22, repacked dts_es/x96/xch/dts in LE16, BE14, LE14), S `pack_*` |
| Frame header, all fields, CPF/HCRC (5.4.2) | implemented | V, S `cpf`, `cpf_dynf` |
| AMODE 0–9, LFE, all 8 core rates (Tables 5-4, 5-5) | implemented | S `amode0..9`, `sfreq*`, `nolfe` |
| AMODE 10–15 | typed refusal `Unsupported` (FFmpeg refuses too) | S `amode10/12/15` |
| NBLKS 8–128 blocks, 1–16 subframes, nSSC 1–4 | implemented | S `blocks128`, `blocks256s`, `huff` |
| Primary audio coding header: SUBS, VQSUB, JOINX, THUFF, SHUFF, BHUFF, SEL, ADJ (5.4.3) | implemented | V, S `huff` |
| Side info: PMODE/PVQ, ABITS (A12–E12, 4/5-bit linear), TMODE (A4–D4), SCALES (SA129–SE129, 6/7-bit linear), JOIN_SHUFF/JOIN_SCALES (5.5.1) | implemented | S `huff`, `scalebooks` (every scale difference −62..62 in every book), `joint*` |
| Quantizers: Huffman A3…G129, block codes V3–V25, NFE up to ABITS 26, lossy and lossless step tables (5.6, D.2, D.5, D.6) | implemented | S `huff`, `block`, `bigabits`, `lossless` |
| Transient mode scale switching | implemented | S `huff`, `block` (transients on) |
| Inverse ADPCM, history across subframes and frames, HFLAG (C.3.3) | implemented | V, S `adpcm`, `adpcm_h0`, `size_changes_adpcm` |
| High-frequency VQ (5.6, D.10.2) | implemented | S `hfvq`, `hfvq_short`, `hfvq_loud` |
| Joint intensity (C.3.4), including XCh channels joined to core channels | implemented | S `joint`, `joint_huff`, `joint_jlin`; V `xch_61` (XCh JOINX=4) |
| Sum/difference SUMF, SUMS, AMODE 3 (C.3.5) | implemented | S `sumf`, `sums`, `sumf_stereo`, `amode3` |
| 32-band QMF, non-perfect (FILTS=0) and perfect (FILTS=1) (C.3.6, D.8) | implemented, FFT DCT-IV | V (FILTS=0), S `perfect`, `imp_f1_b3` |
| LFE 64x and 128x interpolation (C.3.7, D.8) | implemented | V (64x), S `lfe128` |
| DSYNC, ASPF | checked; a mismatch is `InvalidData`, like FFmpeg's "DSYNC check failed" | S `huff_aspf`, `bad_dsync5` |
| Optional info: TIMES, AUX, OCRC (5.7/5.8) | skipped, non-fatal, as in FFmpeg | V (AUXF=1 vectors), S `timef_auxf` |
| DRC (RANGE), dialog normalization | parsed, **not applied**, matching FFmpeg's default | S `dynf`, `vernum6` |
| PSC (partial subsubframe) | read and ignored (FFmpeg does the same) | S `psc` |
| Deficit samples (SHORT ≠ 31) | typed refusal (FFmpeg: "Deficit samples are not supported") | S `short5` |
| VERNUM > 7 | decoded (FFmpeg decodes too; the spec says mute) | S `vernum8` |
| XCh 6.1 (6.4), including removal of the embedded Ls/Rs = Ls/Rs + 0.7071·Cs downmix | implemented (one channel) | V `dts_es`, `xch_61`, `xbr_xch_61`, `x96_xch_61`, `xll_xch_61` |
| X96 in core, DTS_BCCORE_X96 (6.2.4): REVNO < 8 and = 8, HIGHRES 0/1, all quantizers, VQ16, ADPCM, joint, 64-band QMF (D.9), 2x LFE | implemented | V `x96_51`, `xll_x96_51`; S `x96_basic/lin/hi/hi_lin/rev8/vq/adpcm/joint/aspf/lfe128/s58/s60` |
| X96 REVNO 0 or > 8 | ignored; decodes core at the core rate (as FFmpeg) | S `x96_rev9`, `x96_rev0` |
| X96 ABITS96 = 0 noise fill | **gap**: outputs zero (see Known gaps) | S `x96_zero` FAIL |
| XXCh inside the core substream (6.5) | typed refusal `Unsupported("XXCH channel extension in the core substream")` | no selected vector carries it; the 7.1 vectors put XXCh in the extension substream |
| XBR, XLL, LBR, X96/XXCh in the extension substream | out of contract (core-only input) | n/a |

### Behaviour that follows FFmpeg (the reference) rather than the spec text

Each item was found with the oracle and is commented at its code site.

1. Synthesis gain. C.3.6 leaves `rScale` unspecified. The calibrated gain is 0.25·2^7.5 for the 32-band bank and
   0.25·2^8.5 for the 64-band bank, with PCM/2^23 = 1.0 full scale.
2. Perfect-reconstruction prototype. FFmpeg's FILTS=1 output does not match the Annex D.8 table: it deviates by up
   to 5.9e-6 absolute (peak 1.9e-2), which gives up to 6e-5 output error on an impulse. FILTS=0 matches D.8 to float
   precision. The effective prototype was measured from impulse responses (`recover_pr.py`, resynthesis error
   1.25e-7) and is used for FILTS=1. Annex D.8 stays in `tables.rs`.
3. X96 2x LFE. FFmpeg uses `z[2n] = 0.75·x[n−1] + 0.25·x[n]` and `z[2n+1] = 0.25·x[n−1] + 0.75·x[n]`, not the
   Table 6-11 5-tap filter. This was found by least-squares fit on `x96_51_24_96_1509` (residual 6e-8).
4. 24-bit saturation to [−2^23, 2^23−1] of dequantized subband samples, ADPCM output, HF VQ samples, joint-intensity
   copies, decimated LFE samples and X96 residual samples. X96 core+residual recombination is *not* saturated.
   Evidence: `scalehigh`, `adpcm_ovf`, `hfvq_loud`, `joint_huff`, `xll_51_24_48_768` LFE, `x96_s58/s60`.
5. LFE history in 8-block frames, which have fewer than 8 decimated LFE samples. The history is refilled by
   repeating the frame's samples cyclically, end aligned (`blocks256s`, `lfe128_short`).
6. ADPCM and LFE histories are dropped whenever a frame needs more subband samples than any frame since the last
   reset (`lfe_mixed`, `size_changes_adpcm`).
7. A failed frame is dropped without resetting any state (`bad_dsync5`; test
   `failed_frame_is_dropped_without_disturbing_history`).
8. XCh JOINX indexes channels globally (core first). XCh is located by sync at distance XChFSIZE+1 or XChFSIZE from
   the frame end, and X96 by sync at distance FSIZE96.

## Table provenance (owner review requested)

- **Transcribed mechanically from the V1.6.1 text** by `research/d4/dca/gen_tables.py`, which writes
  `src/tables.rs`: all 62 Huffman books (each validated by level set, exact Kraft sum and prefix freedom),
  D.1.1/D.1.2 scale factors, D.2 step sizes, D.3 joint scales, the D.4 DRC table (unused), D.8 QMF and LFE FIRs, and
  the D.9 X96 FIR. Two spec errata are handled explicitly and documented in the script:
  - V1.6.1/V1.3.1 D.5.12 **Table D129** prints level `37` twice (codes 1582 and 1581, length 11) and omits −37. The
    pair pattern shows that code 1581 is −37.
  - V1.6.1/V1.4.1 D.5.11 **Table D65** drops levels ±31/±32 at a page break (Kraft sum 0.99988). The complete table
    is taken from V1.3.1, after checking that every row V1.6.1 prints is identical.
- **Not printed in any ETSI edition** (V1.2.1–V1.6.1 say "Due to its extensive size, this table is not included
  here"): the D.10.1 ADPCM codebook (4096×4) and the D.10.2 HF VQ codebook (1024×32). Both are normative. They were
  **recovered black-box from FFmpeg's output** on synthetic probe streams (`examples/synth adpcm_probe|hfvq_probe` →
  `recover_vq.py` → `gen_vq.py` → `src/vq.rs`). The method solves impulse-response least squares per band, then
  inverts the C.3.3 recursion or the VQ scaling. Integrality: worst fractional deviation is 0.041 for ADPCM and 3e-5
  for HF VQ. Entry 0 of D.10.1 equals the spec's printed example, 9928.
  - Three independent ADPCM runs (q = 40, 100, 200) agree on all 4096 entries. The exception is entry 3916, where
    the q = 200 probe saturated at 2^23; that saturation is how finding 4 above was discovered.
  - No decoder source was read. Whether values obtained this way are acceptable to the project is a
    licensing/provenance decision for the owner. The same applies to the measured perfect-reconstruction prototype
    in `src/pr_table.rs`.

## Selected vectors (all 22 core-carrying FATE `dts/` vectors)

Input: `ffmpeg -i X -c:a copy -bsf:a dca_core -f dts` (core-only bytes). Reference: `ffmpeg -f dts -i core.dts -f f32le`.
The values below are the maximum over channels. Every result is a pass with the exact rate, channel count, order and
sample count, from sample 0 with no delay offset. `xll_51_24_48_none` has no core substream, so it is outside the
contract (`dca_core` returns XLL-only frames).

| Vector | Rate | Ch | Samples/ch | max RMS | max abs | Result |
|---|---|---|---|---|---|---|
| dts (dts.ts) | 48000 | 6 | 129536 | 1.74e-07 | 9.44e-07 | pass |
| dts_es | 48000 | 7 | 128000 | 2.09e-07 | 1.20e-06 | pass |
| dtsx_7.1.4_24bit | 48000 | 6 | 1024 | 1.97e-07 | 9.19e-07 | pass |
| master_audio_7.1_24bit | 48000 | 6 | 600064 | 2.58e-07 | 2.21e-06 | pass |
| core_51_24_48_768_0 | 48000 | 6 | 3072 | 1.34e-07 | 5.96e-07 | pass |
| core_51_24_48_768_1 | 48000 | 6 | 3072 | 1.47e-07 | 7.75e-07 | pass |
| x96_51_24_96_1509 | 96000 | 6 | 6144 | 2.32e-07 | 1.43e-06 | pass |
| x96_xch_61_24_96_3840 | 48000 | 7 | 3072 | 1.71e-07 | 8.35e-07 | pass |
| x96_xxch_71_24_96_3840 | 48000 | 6 | 3072 | 1.63e-07 | 7.75e-07 | pass |
| xbr_51_24_48_3840 | 48000 | 6 | 3072 | 1.70e-07 | 1.04e-06 | pass |
| xbr_xch_61_24_48_3840 | 48000 | 7 | 3072 | 1.72e-07 | 7.75e-07 | pass |
| xbr_xxch_71_24_48_3840 | 48000 | 6 | 3072 | 1.53e-07 | 6.56e-07 | pass |
| xch_61_24_48_768 | 48000 | 7 | 3072 | 2.40e-07 | 1.46e-06 | pass |
| xll_51_16_192_768_0 | 48000 | 6 | 3072 | 1.34e-07 | 6.56e-07 | pass |
| xll_51_16_192_768_1 | 48000 | 6 | 3072 | 1.33e-07 | 7.75e-07 | pass |
| xll_51_24_48_768 | 48000 | 6 | 3072 | 1.47e-07 | 7.75e-07 | pass |
| xll_71_24_48_768_0 | 48000 | 6 | 3072 | 1.32e-07 | 5.96e-07 | pass |
| xll_71_24_48_768_1 | 48000 | 6 | 3072 | 1.31e-07 | 6.86e-07 | pass |
| xll_71_24_96_768 | 48000 | 6 | 3072 | 1.25e-07 | 6.84e-07 | pass |
| xll_x96_51_24_96_1509 | 96000 | 6 | 6144 | 2.32e-07 | 1.43e-06 | pass |
| xll_xch_61_24_48_768 | 48000 | 7 | 3072 | 2.52e-07 | 1.40e-06 | pass |
| xxch_71_24_48_2046 | 48000 | 6 | 3072 | 1.53e-07 | 6.56e-07 | pass |

The vectors exercise AMODE 9, LFF=2, FILTS=0, NBLKS=15, 48 kHz, XCh (5 vectors) and core X96 (2 vectors). The DRC,
sum/difference, FILTS=1, VQ, other AMODEs and rates appear only in the synthetic set.

## Synthetic oracle sweep (`research/d4/dca/run_all.sh synth`)

- **Passing (70):** `basic perfect lfe128 nolfe huff huff_aspf block bigabits blocks128 blocks256s lfe128_short
  lfe_mixed size_changes_adpcm adpcm adpcm_h0 adpcm_ovf hfvq hfvq_short hfvq_loud joint joint_huff joint_jlin sumf
  sums sumf_stereo scalebooks scalehigh dynf cpf cpf_dynf timef_auxf lossless psc vernum6 vernum8 es_flag amode0..9
  sfreq1/2/3/6/7/8/11/12 pack_le16/be14/le14 imp_f1_b3 x96_basic x96_lin x96_hi x96_hi_lin x96_rev8 x96_vq x96_adpcm
  x96_joint x96_aspf x96_lfe128 x96_s58 x96_s60`.
  - The largest error is in `joint_huff`: RMS 3.0e-6, max 2.3e-5. FFmpeg holds subband samples as integers, so
    joint copies of saturated sources are rounded there. All other scenarios are ≤ 5.4e-7 RMS.
- **Refused by both decoders, typed on our side:** `short5`, `amode10/12/15`.
- **Failing** (diagnosis under Known gaps): `x96_zero`, `x96_s62`/`x96_s61`, `term31`.

## Known gaps (each precisely diagnosed; none occurs in a selected vector)

1. **X96 ABITS96 = 0 noise fill** (`x96_zero`). FFmpeg fills these bands with deterministic integer noise, uniform in
   ±scale/2: the integers are recovered exactly by least squares. The generator was not identified. The tests covered
   common LCGs (ANSI C, Numerical Recipes, MSVC, Borland and others) with 5 output mappings, rounding and strides
   1–32. This decoder outputs zero for such bands, so the error is the noise level (2.3e-4 RMS in the test). The real
   X96 vectors code silent high bands with VQ (ABITS96 = 1), not 0.
2. **X96 residual overload at scale index ≥ 61** (`x96_s61`, `x96_s62`). With scale indices 61/62 and quantized
   residuals far beyond 2^23, FFmpeg's result is neither 2^23 saturation nor plain float. Scale ≤ 60 matches,
   including the saturating `x96_s58`/`x96_s60`, and small-q probes at 61/62 match exactly. The case is pathological:
   outputs peak near 19× full scale.
3. **Termination frames, FTYPE = 0** (`term31`). The frames decode fine in this decoder. FFmpeg's raw-`dts`
   *parser* does not start a packet at FTYPE=0 frames, so a run of them is merged and only the first decodes. That is
   demuxer behaviour, not decoder output. Frames with SHORT ≠ 31 are refused by both.
4. **XXCh inside the core substream** is refused, typed, not decoded (see the feature matrix). Decoding it needs the
   6.5 channel-set syntax, the downmix-coefficient removal (D.11) and 7.1 layout mapping, which could only be
   validated with synthetic streams.
5. XCh with more than one channel is refused, typed. X96 and XCh in the same core frame: no vector and no test. X96
   with XCh would search X96 by distance-to-frame-end and miss it.
6. Sum/difference combined with X96 is applied to the 64-band vectors without oracle confirmation.

## Performance (development timing on this Mac, Apple M-series arm64 — **non-evidence**)

Commands: `decode IN --null` (single thread, includes our own frame splitting and file I/O) versus
`ffmpeg -benchmark -threads 1 -f dts -i IN -f null -`. Each figure is the median of 5 runs.

| Workload | Frames | ours real | ffmpeg rtime (utime) | ratio |
|---|---|---|---|---|
| 5.1 core 48 kHz (master_audio core ×32, 400 s) | 37 504 | 0.55 s | 0.73 s (1.13 s) | ≈0.76 |
| 5.1 core (dts.ts core ×60) | 15 180 | 0.19 s | 0.27 s (0.40 s) | ≈0.72 |
| 6.1 XCh (dts_es ×60) | 15 000 | 0.24 s | 0.30 s (0.45 s) | ≈0.80 |
| 5.1 X96 96 kHz (x96_51 ×2400) | 14 400 | 0.44 s | 0.69 s (0.85 s) | ≈0.64 |

- Peak RSS of the whole example process is about 2 MB, against about 12 MB for the ffmpeg process.
- Decoder heap is about 0.2 MB for a core decoder. About 0.25 MB more is allocated lazily on the first X96 frame.
- There is no steady-state allocation: output planes, normalization scratch and all histories are reused.
- QMF synthesis uses an N/2-point FFT DCT-IV plus a 16-tap polyphase window written with fixed-size arrays for
  auto-vectorization: 104 ns per 32-sample block and 203 ns per 64-sample block (`qmf_speed` probe).
- None of this establishes the Linux ARM64 or 32-bit TV gate.

## Tests (`CARGO_INCREMENTAL=0 CARGO_TARGET_DIR=target/agent-dca cargo +nightly test -p triergon-codec-dca`)

- Unit tests:
  - DCT-IV against the direct definition.
  - `qmf_speed`, an ignored timing probe.
- `tests/synthetic.rs` (self-authored streams, always run):
  - 64 scenarios against FFmpeg fingerprints in `tests/data/synthetic.fp`: per-sample spot checks at 6.1035e-5 and
    4096-sample block sums at 4096·8.8e-6.
  - Packing equivalence.
  - Layouts for AMODE 0–9.
  - Silence decodes to zeros.
  - Typed errors.
  - Corruption fuzzing with no panics (2 400 corrupted frames).
  - `reset()` restores a fresh state.
  - Failed-frame drop.
  - X96 revision fallback.
- `tests/vectors.rs`: the 22 FATE vectors, plus the XLL-only one, taken from their containers by the test's own
  raw / DTS-HD-chunk / MPEG-TS parsing, against `tests/data/vectors.fp`. Each test skips with a note when its file is
  absent.
- The fingerprints are regenerated with `research/d4/dca/make_fingerprints.sh`, which requires ffmpeg.

## Files

- Crate (`crates/codec-dca/`):
  - `src/lib.rs`: the API, frame flow, extension location and synthesis orchestration.
  - `src/header.rs`, `src/subframe.rs`: core and XCh syntax and dequantization.
  - `src/x96.rs`, `src/qmf.rs`, `src/dct.rs`, `src/lfe.rs`, `src/bits.rs`, `src/pack.rs`, `src/books.rs`.
  - Generated tables: `src/tables.rs`, `src/vq.rs`, `src/pr_table.rs`.
  - `examples/decode.rs`, plus `examples/synth.rs` (the scenario generator).
  - `tests/`, with `tests/support/{synth,scenarios,harness}.rs`.
- Tools (`research/d4/dca/`):
  - `gen_tables.py`, `recover_vq.py`, `gen_vq.py`, `recover_pr.py`: table generation and recovery.
  - `compare.py`: numeric-rules comparison.
  - `fingerprint.py`, `make_fingerprints.sh`, `run_all.sh`: fingerprints and the oracle sweep.
- Scratch, gitignored: `target/agent-dca/` holds the spec, extracted core streams, references, synthetic streams and
  benchmark inputs. No media is in the crate.

## Shared-code requests

None required. Two notes for other lanes:

- The framing crate (`crates/framing/src/dts14_*`) lists the 14-bit FSIZE domain as unresolved. FFmpeg reads FSIZE
  in the 16-bit-equivalent domain (see Input and output contract).
- If the session layer feeds termination frames (FTYPE = 0), the decoder handles them. The reference pipeline's
  parser merges them.

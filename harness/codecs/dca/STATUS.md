# DTS core derivation notes

How the tables and FFmpeg-compatible behaviour of
[crates/codec-dca](../../../crates/codec-dca/README.md) were obtained. Sources: ETSI
TS 102 114 V1.6.1 (2019-08) and V1.3.1 (2011-08, Table D65 only). No FFmpeg, libdca or
other decoder source was read; FFmpeg 9.0.2 was used only as a black-box binary.
Synthetic stream names below are scenarios of `examples/synth.rs`.

## Table provenance

- `gen_tables.py` writes `src/tables.rs` from the V1.6.1 text: 62 Huffman books (level
  set, exact Kraft sum, prefix freedom checked), scale factors, step sizes, joint scales,
  and the D.8/D.9 FIRs. Errata: Table D129 prints level 37 twice (code 1581 is −37);
  Table D65 drops ±31/±32 at a page break and is taken from V1.3.1 after checking every
  row V1.6.1 prints. Neither the D.4 DRC table nor the D.8 perfect-reconstruction
  prototype is emitted; the decoder uses neither.
- D.10.1 ADPCM (4096×4) and D.10.2 HF VQ (1024×32) codebooks: recovered from FFmpeg's
  output on probe streams (`synth adpcm_probe|hfvq_probe` → `recover_vq.py` →
  `gen_vq.py` → `src/vq.rs`) by per-band impulse-response least squares and inversion of
  the C.3.3 recursion or the VQ scaling. Worst fractional deviation: 0.041 (ADPCM), 3e-5
  (HF VQ). Entry 0 equals the spec's printed example, 9928. Three ADPCM runs (q = 40,
  100, 200) agree on all entries except one where q = 200 saturates.
- Perfect-reconstruction prototype (`src/pr_table.rs`): measured from FFmpeg's FILTS = 1
  impulse responses (`recover_pr.py`, resynthesis error 1.25e-7). It differs from Annex
  D.8 (read from the V1.6.1 text) by up to 5.9e-6; FILTS = 0 matches D.8.
- `regenerate_tables.sh` reruns the whole chain and compares both files byte for byte.

## Calibrations

- Synthesis gain 2^7.5 (32-band) and 2^8.5 (64-band) times 0.25: fitted to FFmpeg's float
  output.
- X96 LFE doubling `(0.75, 0.25)` taps: least-squares fit on `x96_51_24_96_1509`
  (residual 6e-8), replacing Table 6-11.
- 24-bit saturation, including LFE saturation of decimated samples: `scalehigh`,
  `adpcm_ovf`, `hfvq_loud`, `joint_huff`, `x96_s58`, `x96_s60`.
- LFE history in 8-block frames: `blocks256s`, `lfe128_short`. History reset on growth:
  `lfe_mixed`, `size_changes_adpcm`. Failed frame dropped without state change:
  `bad_dsync5`.

Other tools: `compare.py` (numeric rules), `fingerprint.py` and `make_fingerprints.sh`
(`tests/data/*.fp`), `run_all.sh` (vector and synthetic sweep).

# AAC derivation notes

How the tables and FFmpeg-compatible behaviour of
[crates/codec-aac](../../../crates/codec-aac/README.md) were obtained. ISO/IEC 14496-3 was
not available; no FFmpeg source was read. FFmpeg 9.0.2 was used only as a black-box
binary.

## Documents

| Document | Used for |
|---|---|
| ETSI TS 101 980 V1.1.1 (DRM), Annex I Table I.1 | The 640 QMF window values (the table mislabels entries 297/305; values are taken in table order) |
| 3GPP TS 26.404 V19.0.0, Figure 6 | 64-band QMF analysis structure |
| 3GPP TS 26.405 V19.0.0, Figure 4, Tables 1 and 2 | 32-band QMF synthesis structure; 8-band hybrid prototype; 20-band parameter map |
| H. Purnhagen, "Low complexity parametric stereo coding in MPEG-4", DAFx 2004 | PS mixing (eq. 4-8), hybrid filterbank layout, decorrelator structure, transient reduction |
| J. Breebaart et al., "Parametric coding of stereo audio", EURASIP JASP 2005:9 | PS mixing for ICC modes 3..5 (rotation and scale) |

## Tables and how they are validated

- Scalefactor and spectral books 2–11: from knowledge of the standard; unit tests check
  Kraft equality and prefix freedom. `tools/verify_cb.py` probes FFmpeg with one crafted
  ADTS frame per codeword: every symbol of books 1–11 decodes to this table's tuple.
- Spectral book 1: codeword set rebuilt from the book's structure; symbols 19–80 assigned
  from FFmpeg probes (`tools/probe_cb.py`).
- QMF banks: the window is used reversed, `c[n] = c_DRM[639 − n]`, with phases
  `(2n − 1/2)/64` and `(2n − 255)/128`. `examples/sbr_mute.rs` makes FFmpeg output the pure
  analysis/synthesis of the core; `tools/qmffit.py` fits orientation and phases against it
  (9.5e-8 max).
- SBR Huffman books (except SBR_F_ENVBAL30), SBR noise table and SBR constants: from
  knowledge, validated only by the conformance vectors. Tails and books no vector reaches
  are unverified.
- Parametric Stereo: the structure follows Purnhagen 2004, Breebaart 2005 and TS 26.405
  (Documents above). The `ps_data()` syntax, the ten PS books, SBR_F_ENVBAL30 and every PS
  signal table and rule (hybrid prototypes and band maps, decorrelator and ducking, both
  mixing procedures, IPD/OPD, behaviour over frames, 20/34-band changes) are recovered by
  scripted probes of the FFmpeg binary ([ps/README.md](ps/README.md)).
  `cargo xtask codecs aac-ps-tables` reruns the recovery and writes
  `crates/codec-aac/src/huffman_probed.rs` and `crates/codec-aac/src/sbr/ps/tables.rs`;
  `--check` compares them. Not recovered: the three open cases of
  [ps-open-manifest.json](ps-open-manifest.json), graded with `codecs grade` like the
  other manifests (see the crate's known differences).

Confirmed black-box: PNS noise LCG (1664525, 1013904223, seed `0x1f2e3d4c`), PCE speaker
mapping (`tools/probe_layout.py`), the libfaac fill-element skip, the implicit PS stereo
upgrade, six SBR patches.

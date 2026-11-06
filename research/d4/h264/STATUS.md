# triergon-codec-h264 — STATUS

Lane: D4 H.264 decoder (`crates/codec-h264`, package `triergon-codec-h264`).
Status class: **working prototype** (authored code, bit-exact against the development oracle on
every selected conformance vector). Not parity-qualified, not release-qualified; Mac runs are
development evidence only.

## Specification

- Rec. ITU-T H.264 (V15) (08/2024), "Advanced video coding for generic audiovisual services".
- Downloaded from `https://www.itu.int/rec/dologin_pub.asp?lang=e&id=T-REC-H.264-202408-S!!PDF-E&type=items`
  (854 pages), SHA-256 `647d6bb2f7a62d221cd5e31da9a948e2dde32f9ac008a548f98348c838bd389d`.
  Kept outside git (`target/agent-h264/spec/`).
- Tables transcribed / regenerated from the spec text (scripts kept outside git, outputs committed in
  the crate with the table name in a comment): CABAC m/n initialisation Tables 9-12..9-33
  (`tables_cabac_init.rs`, one printed typo in Table 9-26 — ctxIdx "641" in the 484..571 table is 541 —
  corrected and noted), coeff_token / total_zeros / run_before Tables 9-5, 9-7, 9-8, 9-9(a), 9-10,
  rangeTabLPS / transIdx Tables 9-44 / 9-45 (`tables_vlc.rs`), Table 9-43, scans Tables 8-13 / 8-14,
  default scaling lists Tables 7-3 / 7-4, normAdjust (8-315 / 8-318), QPc Table 8-15, alpha / beta /
  tC0 Tables 8-16 / 8-17, CBP mapping Table 9-4, mb_type / sub_mb_type Tables 7-14 / 7-18, SAR Table E-1.
- No FFmpeg / libavcodec / x264 / openh264 / GPL decoder source was read or translated. FFmpeg 9.0.2
  (Homebrew) was used only as a black-box oracle (framemd5 / rawvideo / ffprobe).

## Implemented (against the spec)

- Annex B byte stream, NAL header, emulation prevention removal (7.3.1, 7.4.1, B.1); cached 64-bit
  RBSP bit reader, ue/se/te (9.1).
- SPS incl. VUI (SAR, colour description, timing, HRD skip, bitstream_restriction), PPS incl.
  transform_8x8_mode, scaling matrices with fall-back rules A/B (7.3.2, 7.4.2, Annex E).
- Slice header, ref_pic_list_modification, pred_weight_table, dec_ref_pic_marking (7.3.3).
- Slice data loop for CAVLC and CABAC incl. MBAFF skip/field-flag inference and CABAC look-ahead of the
  bottom MB's skip/field flags (7.3.4, 7.4.4).
- Macroblock layer: all I/P/B mb_types and sub_mb_types, I_PCM (CAVLC and CABAC engine restart),
  transform_size_8x8_flag, mb_qp_delta, CBP (7.3.5).
- CAVLC residual (9.2) with two-level lookup tables; CABAC engine (9.3.1.2, 9.3.3.2) with a 64-bit
  left-aligned offset register and branchless interval selection; all CABAC binarizations and
  ctxIdxInc derivations incl. MBAFF special cases (9.3.2, 9.3.3.1).
- Neighbour derivation for frames, fields and MBAFF (6.4.9 .. 6.4.13, Table 6-4).
- Intra 4x4 / 8x8 (with reference filtering) / 16x16 / chroma prediction incl. constrained_intra_pred
  with per-row availability in MBAFF (8.3).
- Inter prediction: mv prediction incl. 16x8/8x16 rules and MBAFF scaling (8.4.1.3), P_Skip, spatial and
  temporal direct with all co-located cases of Tables 8-6 / 8-8 and direct_8x8_inference, 6-tap luma and
  bilinear chroma interpolation with field chroma offset (Table 8-10), default / explicit / implicit
  weighted prediction (8.4.2).
- Scaling and inverse transforms 4x4, 8x8, luma DC Hadamard, chroma DC (8.5), lossless bypass refused.
- Deblocking for frames, field pictures and MBAFF (mixed edges, field-mode top edges of frame MBs over
  field pairs, bS rules for fields), per-slice filter control (8.7).
- POC types 0/1/2, mmco5 handling (8.2.1); picture numbers, list init for P/B frames and fields with
  alternating parity, list modification (8.2.4); sliding window, all MMCO ops, long-term refs, IDR
  long_term_reference_flag (8.2.5); frame_num gaps with "non-existing" frames (8.2.5.2).
- DPB: frame stores holding frames or complementary field pairs, second-field pairing, output bumping
  by DPB fullness (level MaxDpbMbs or max_dec_frame_buffering) and max_num_reorder_frames,
  no_output_of_prior_pics, flush at end of stream; PTS travels with the picture of its access unit.
- Output: cropped 4:2:0 8-bit planes shared with the frame buffer (no copy, `Arc`), SAR, colour
  description; monochrome (4:0:0) output with 128-filled chroma like FFmpeg's yuv420p.

## Selected matrix and what is refused

Selected: Constrained Baseline, Main, High; 8-bit 4:2:0 and 4:0:0; frames, PAFF, MBAFF; CAVLC and
CABAC; I/P/B; multiple slices; spatial/temporal direct; explicit and implicit WP; 8x8 transform and
scaling matrices; long-term refs and MMCO; RPLM; POC types 0/1/2; DPB output; cropping; VUI SAR /
colour. Extended-profile streams that use only these tools also decode (BA3_SVA_C, MR6/7/8_BT_B).

Refused with `DecodeError::Unsupported(<feature>)` (never decoded wrongly):
- bit depth > 8 (High 10 / High 4:2:2 / High 4:4:4), chroma 4:2:2 / 4:4:4, lossless transform bypass;
- flexible macroblock ordering (num_slice_groups > 1; FFmpeg also rejects these PPSs);
- SP/SI slices and data partitioning (Extended profile);
- MVC/SVC slice extensions (NAL 20/21) are skipped; prefix / subset SPS NALs (14/15) ignored, so a base
  view decodes.
- Redundant slices (redundant_pic_cnt > 0) are dropped (primary pictures are always intact in the
  vectors).

## Conformance vectors

Corpus: FFmpeg FATE `h264-conformance` (195 files incl. FRext/, rights unreviewed, gitignored at
`.research/m1d4/fate-suite/h264-conformance/`). Reference: FFmpeg 9.0.2 `-threads 1 -f framemd5
-pix_fmt yuv420p -fps_mode passthrough`. Comparison: every output frame's MD5, output count and order.
`tests/conformance.rs` re-checks all of them (digests in `tests/ffmpeg_framemd5_digests.txt`) and that
out-of-set vectors are refused; it skips when the corpus is absent.

Summary: **167/167 selected vectors bit-exact** (all frames, order and count), 4/4 Extended-profile
streams with in-set tools bit-exact, 24 out-of-set vectors refused (11 4:2:2, 7 10-bit, 4 FMO,
2 SP). No selected vector fails.

| vector | profile | chroma/bit depth | entropy | slices | tools | class | result |
|---|---|---|---|---|---|---|---|
| AUD_MW_E.264 | Baseline | 1/8 | CAVLC | IP |  | selected | bit-exact 100 frames |
| BA1_FT_C.264 | Baseline | 1/8 | CAVLC | IP | multi-slice | selected | bit-exact 299 frames |
| BA1_Sony_D.jsv | Baseline | 1/8 | CAVLC | I |  | selected | bit-exact 17 frames |
| BA2_Sony_F.jsv | Baseline | 1/8 | CAVLC | IP |  | selected | bit-exact 300 frames |
| BA_MW_D.264 | Baseline | 1/8 | CAVLC | IP |  | selected | bit-exact 100 frames |
| BAMQ1_JVC_C.264 | Baseline | 1/8 | CAVLC | I |  | selected | bit-exact 30 frames |
| BAMQ2_JVC_C.264 | Baseline | 1/8 | CAVLC | IP |  | selected | bit-exact 30 frames |
| BANM_MW_D.264 | Baseline | 1/8 | CAVLC | IP |  | selected | bit-exact 100 frames |
| BASQP1_Sony_C.jsv | Baseline | 1/8 | CAVLC | I | multi-slice | selected | bit-exact 4 frames |
| CABA1_Sony_D.jsv | Main | 1/8 | CABAC | I |  | selected | bit-exact 50 frames |
| CABA1_SVA_B.264 | Main | 1/8 | CABAC | I |  | selected | bit-exact 17 frames |
| CABA2_Sony_E.jsv | Main | 1/8 | CABAC | IP |  | selected | bit-exact 300 frames |
| CABA2_SVA_B.264 | Main | 1/8 | CABAC | IP |  | selected | bit-exact 17 frames |
| CABA3_Sony_C.jsv | Main | 1/8 | CABAC | BIP | temporal-direct | selected | bit-exact 300 frames |
| CABA3_SVA_B.264 | Main | 1/8 | CABAC | BIP | temporal-direct | selected | bit-exact 33 frames |
| CABA3_TOSHIBA_E.264 | Main | 1/8 | CABAC | IP |  | selected | bit-exact 300 frames |
| CABACI3_Sony_B.jsv | Main | 1/8 | CABAC | BIP | temporal-direct multi-slice | selected | bit-exact 300 frames |
| CABAST3_Sony_E.jsv | Main | 1/8 | CABAC | BIP | temporal-direct multi-slice | selected | bit-exact 25 frames |
| CABASTBR3_Sony_B.jsv | Main | 1/8 | CABAC | BIP | temporal-direct multi-slice | selected | bit-exact 25 frames |
| CABREF3_Sand_D.264 | Main | 1/8 | CABAC | BIP | PAFF/field temporal-direct | selected | bit-exact 50 frames |
| CACQP3_Sony_D.jsv | Main | 1/8 | CABAC | BIP | temporal-direct | selected | bit-exact 50 frames |
| CAFI1_SVA_C.264 | Main | 1/8 | CABAC | BIP | PAFF/field spatial-direct | selected | bit-exact 33 frames |
| CAMA1_Sony_C.jsv | Main | 1/8 | CABAC | I | MBAFF | selected | bit-exact 5 frames |
| CAMA1_TOSHIBA_B.264 | Main | 1/8 | CABAC | BIP | MBAFF spatial-direct | selected | bit-exact 90 frames |
| cama1_vtc_c.avc | Main | 1/8 | CABAC | BIP | MBAFF PAFF/field spatial-direct | selected | bit-exact 4 frames |
| cama2_vtc_b.avc | Main | 1/8 | CABAC | BIP | MBAFF PAFF/field spatial-direct | selected | bit-exact 4 frames |
| CAMA3_Sand_E.264 | Main | 1/8 | CABAC | BIP | MBAFF temporal-direct | selected | bit-exact 50 frames |
| cama3_vtc_b.avc | Main | 1/8 | CABAC | BIP | MBAFF PAFF/field spatial-direct | selected | bit-exact 4 frames |
| CAMACI3_Sony_C.jsv | Main | 1/8 | CABAC | BIP | MBAFF CIP temporal-direct | selected | bit-exact 17 frames |
| CAMANL1_TOSHIBA_B.264 | Main | 1/8 | CABAC | BIP | MBAFF spatial-direct | selected | bit-exact 90 frames |
| CAMANL2_TOSHIBA_B.264 | Main | 1/8 | CABAC | BIP | MBAFF temporal-direct | selected | bit-exact 90 frames |
| CAMANL3_Sand_E.264 | Main | 1/8 | CABAC | BIP | MBAFF temporal-direct | selected | bit-exact 50 frames |
| CAMASL3_Sony_B.jsv | Main | 1/8 | CABAC | BIP | MBAFF temporal-direct multi-slice | selected | bit-exact 17 frames |
| camp_mot_fld0_full.26l | Main | 1/8 | CABAC | BIP | PAFF/field temporal-direct | selected | bit-exact 30 frames |
| camp_mot_frm0_full.26l | Main | 1/8 | CABAC | BIP | temporal-direct | selected | bit-exact 30 frames |
| camp_mot_mbaff0_full.26l | Main | 1/8 | CABAC | BIP | MBAFF temporal-direct | selected | bit-exact 30 frames |
| CAMP_MOT_MBAFF_L30.26l | Main | 1/8 | CABAC | BIP | MBAFF temporal-direct | selected | bit-exact 30 frames |
| CAMP_MOT_MBAFF_L31.26l | Main | 1/8 | CABAC | BIP | MBAFF temporal-direct | selected | bit-exact 30 frames |
| camp_mot_picaff0_full.26l | Main | 1/8 | CABAC | BIP | PAFF/field temporal-direct | selected | bit-exact 30 frames |
| CANL1_Sony_E.jsv | Main | 1/8 | CABAC | I |  | selected | bit-exact 50 frames |
| CANL1_SVA_B.264 | Main | 1/8 | CABAC | I |  | selected | bit-exact 17 frames |
| CANL1_TOSHIBA_G.264 | Main | 1/8 | CABAC | I |  | selected | bit-exact 300 frames |
| CANL2_Sony_E.jsv | Main | 1/8 | CABAC | IP |  | selected | bit-exact 300 frames |
| CANL2_SVA_B.264 | Main | 1/8 | CABAC | I |  | selected | bit-exact 17 frames |
| CANL3_Sony_C.jsv | Main | 1/8 | CABAC | BIP | temporal-direct | selected | bit-exact 300 frames |
| CANL3_SVA_B.264 | Main | 1/8 | CABAC | IP |  | selected | bit-exact 17 frames |
| CANL4_SVA_B.264 | Main | 1/8 | CABAC | BIP | spatial-direct | selected | bit-exact 33 frames |
| CANLMA2_Sony_C.jsv | Main | 1/8 | CABAC | IP | MBAFF | selected | bit-exact 17 frames |
| CANLMA3_Sony_C.jsv | Main | 1/8 | CABAC | IP | MBAFF | selected | bit-exact 17 frames |
| CAPA1_TOSHIBA_B.264 | Main | 1/8 | CABAC | BIP | PAFF/field temporal-direct | selected | bit-exact 90 frames |
| CAPAMA3_Sand_F.264 | Main | 1/8 | CABAC | BIP | MBAFF PAFF/field temporal-direct | selected | bit-exact 50 frames |
| CAPCM1_Sand_E.264 | Main | 1/8 | CABAC | I |  | selected | bit-exact 30 frames |
| CAPCMNL1_Sand_E.264 | Main | 1/8 | CABAC | I |  | selected | bit-exact 30 frames |
| CAPM3_Sony_D.jsv | Main | 1/8 | CABAC | BIP | temporal-direct | selected | bit-exact 300 frames |
| CAQP1_Sony_B.jsv | Main | 1/8 | CABAC | I |  | selected | bit-exact 50 frames |
| CAWP1_TOSHIBA_E.264 | Main | 1/8 | CABAC | IP | WP-explicit | selected | bit-exact 90 frames |
| CAWP5_TOSHIBA_E.264 | Main | 1/8 | CABAC | IP | WP-explicit RPLM | selected | bit-exact 90 frames |
| CI1_FT_B.264 | Baseline | 1/8 | CAVLC | IP | CIP multi-slice | selected | bit-exact 291 frames |
| CI_MW_D.264 | Baseline | 1/8 | CAVLC | IP | CIP | selected | bit-exact 100 frames |
| CVBS3_Sony_C.jsv | Main | 1/8 | CAVLC | BIP | temporal-direct | selected | bit-exact 300 frames |
| CVCANLMA2_Sony_C.jsv | Main | 1/8 | CAVLC+CABAC | IP | MBAFF | selected | bit-exact 17 frames |
| CVFC1_Sony_C.jsv | Baseline | 1/8 | CAVLC | IP | multi-slice crop | selected | bit-exact 50 frames |
| CVFI1_Sony_D.jsv | Main | 1/8 | CAVLC | IP | PAFF/field multi-slice | selected | bit-exact 17 frames |
| CVFI1_SVA_C.264 | Main | 1/8 | CAVLC | IP | PAFF/field | selected | bit-exact 7 frames |
| CVFI2_Sony_H.jsv | Main | 1/8 | CAVLC | BIP | PAFF/field temporal-direct multi-slice | selected | bit-exact 17 frames |
| CVFI2_SVA_C.264 | Main | 1/8 | CAVLC | BIP | PAFF/field spatial-direct | selected | bit-exact 13 frames |
| CVMA1_Sony_D.jsv | Main | 1/8 | CAVLC | I | MBAFF | selected | bit-exact 5 frames |
| CVMA1_TOSHIBA_B.264 | Main | 1/8 | CAVLC | BIP | MBAFF spatial-direct | selected | bit-exact 90 frames |
| CVMANL1_TOSHIBA_B.264 | Main | 1/8 | CAVLC | BIP | MBAFF spatial-direct | selected | bit-exact 90 frames |
| CVMANL2_TOSHIBA_B.264 | Main | 1/8 | CAVLC | BIP | MBAFF temporal-direct | selected | bit-exact 90 frames |
| CVMAPAQP3_Sony_E.jsv | Main | 1/8 | CAVLC | BIP | MBAFF PAFF/field temporal-direct | selected | bit-exact 8 frames |
| CVMAQP2_Sony_G.jsv | Main | 1/8 | CAVLC | IP | MBAFF | selected | bit-exact 17 frames |
| CVMAQP3_Sony_D.jsv | Main | 1/8 | CAVLC | BIP | MBAFF temporal-direct | selected | bit-exact 17 frames |
| cvmp_mot_fld0_full_B.26l | Main | 1/8 | CAVLC | BIP | PAFF/field spatial-direct | selected | bit-exact 30 frames |
| CVMP_MOT_FLD_L30_B.26l | Main | 1/8 | CAVLC | BIP | PAFF/field temporal-direct | selected | bit-exact 30 frames |
| cvmp_mot_frm0_full_B.26l | Main | 1/8 | CAVLC | BIP | spatial-direct | selected | bit-exact 30 frames |
| CVMP_MOT_FRM_L31_B.26l | Main | 1/8 | CAVLC | BIP | PAFF/field temporal-direct | selected | bit-exact 30 frames |
| cvmp_mot_mbaff0_full_B.26l | Main | 1/8 | CAVLC | BIP | MBAFF spatial-direct | selected | bit-exact 30 frames |
| cvmp_mot_picaff0_full_B.26l | Main | 1/8 | CAVLC | BIP | PAFF/field spatial-direct | selected | bit-exact 30 frames |
| CVNLFI1_Sony_C.jsv | Main | 1/8 | CAVLC | IP | PAFF/field multi-slice | selected | bit-exact 17 frames |
| CVNLFI2_Sony_H.jsv | Main | 1/8 | CAVLC | BIP | PAFF/field temporal-direct multi-slice | selected | bit-exact 17 frames |
| CVPA1_TOSHIBA_B.264 | Main | 1/8 | CAVLC | BIP | PAFF/field temporal-direct | selected | bit-exact 90 frames |
| CVPCMNL1_SVA_C.264 | Main | 1/8 | CAVLC | I |  | selected | bit-exact 30 frames |
| CVPCMNL2_SVA_C.264 | Main | 1/8 | CAVLC | I |  | selected | bit-exact 2 frames |
| CVSE2_Sony_B.jsv | Baseline | 1/8 | CAVLC | IP |  | selected | bit-exact 278 frames |
| CVSE3_Sony_H.jsv | Main | 1/8 | CAVLC | BIP | temporal-direct | selected | bit-exact 278 frames |
| CVSEFDFT3_Sony_E.jsv | Main | 1/8 | CAVLC | BIP | temporal-direct | selected | bit-exact 200 frames |
| CVWP1_TOSHIBA_E.264 | Main | 1/8 | CAVLC | IP | WP-explicit | selected | bit-exact 90 frames |
| CVWP2_TOSHIBA_E.264 | Main | 1/8 | CAVLC | BIP | WP-explicit spatial-direct | selected | bit-exact 90 frames |
| CVWP3_TOSHIBA_E.264 | Main | 1/8 | CAVLC | BIP | WP-implicit spatial-direct | selected | bit-exact 90 frames |
| CVWP5_TOSHIBA_E.264 | Main | 1/8 | CAVLC | IP | WP-explicit RPLM | selected | bit-exact 90 frames |
| FI1_Sony_E.jsv | Main | 1/8 | CAVLC | I | PAFF/field | selected | bit-exact 17 frames |
| freh10.264 | High | 1/8 | CABAC | BIP | PAFF/field 8x8 scaling temporal-direct | selected | bit-exact 100 frames |
| freh11.264 | High | 1/8 | CABAC | BIP | PAFF/field 8x8 scaling temporal-direct | selected | bit-exact 100 frames |
| Freh12_B.264 | High | 1/8 | CABAC | BIP | 8x8 scaling temporal-direct | selected | bit-exact 100 frames |
| Freh1_B.264 | High | 1/8 | CAVLC | BIP | 8x8 scaling spatial-direct | selected | bit-exact 100 frames |
| Freh2_B.264 | High | 1/8 | CABAC | BIP | 8x8 scaling temporal-direct | selected | bit-exact 100 frames |
| freh3.264 | High | 1/8 | CABAC | BIP | 8x8 scaling spatial-direct | selected | bit-exact 100 frames |
| freh4.264 | High | 1/8 | CABAC | BIP | PAFF/field 8x8 scaling spatial-direct | selected | bit-exact 100 frames |
| freh5.264 | High | 1/8 | CABAC | BIP | MBAFF 8x8 scaling spatial-direct | selected | bit-exact 100 frames |
| freh6.264 | High | 1/8 | CAVLC | BIP | PAFF/field 8x8 scaling temporal-direct | selected | bit-exact 100 frames |
| Freh7_B.264 | High | 1/8 | CABAC | BIP | PAFF/field 8x8 scaling temporal-direct | selected | bit-exact 100 frames |
| freh8.264 | High | 1/8 | CABAC | BIP | 8x8 scaling temporal-direct | selected | bit-exact 100 frames |
| freh9.264 | High | 1/8 | CABAC | BIP | 8x8 scaling temporal-direct | selected | bit-exact 100 frames |
| FREXT01_JVC_D.264 | High | 1/8 | CABAC | BIP | MBAFF 8x8 scaling spatial-direct | selected | bit-exact 16 frames |
| FREXT02_JVC_C.264 | High | 1/8 | CAVLC | BIP | MBAFF 8x8 scaling spatial-direct | selected | bit-exact 16 frames |
| FRExt1_Panasonic.avc | High | 1/8 | CAVLC | BIP | 8x8 scaling spatial-direct | selected | bit-exact 8 frames |
| FRExt2_Panasonic.avc | High | 1/8 | CAVLC | BIP | MBAFF PAFF/field 8x8 scaling temporal-direct RPLM MMCO1 | selected | bit-exact 15 frames |
| FRExt3_Panasonic.avc | High | 1/8 | CABAC | BIP | 8x8 scaling temporal-direct | selected | bit-exact 11 frames |
| FRExt4_Panasonic.avc | High | 1/8 | CABAC | BIP | MBAFF PAFF/field 8x8 scaling spatial-direct | selected | bit-exact 10 frames |
| FRExt_MMCO4_Sony_B.264 | High | 1/8 | CABAC | BIP | 8x8 scaling spatial-direct MMCO1,2,3,4,6 | selected | bit-exact 60 frames |
| HCAFF1_HHI.264 | High | 1/8 | CABAC | BIP | PAFF/field 8x8 scaling spatial-direct | selected | bit-exact 10 frames |
| HCAFR1_HHI.264 | High | 1/8 | CABAC | BIP | 8x8 scaling spatial-direct | selected | bit-exact 10 frames |
| HCAFR2_HHI.264 | High | 1/8 | CABAC | BIP | 8x8 scaling spatial-direct | selected | bit-exact 10 frames |
| HCAFR3_HHI.264 | High | 1/8 | CABAC | BIP | 8x8 scaling spatial-direct | selected | bit-exact 10 frames |
| HCAFR4_HHI.264 | High | 1/8 | CABAC | BIP | 8x8 scaling spatial-direct | selected | bit-exact 10 frames |
| HCAMFF1_HHI.264 | High | 1/8 | CABAC | BIP | MBAFF 8x8 scaling spatial-direct | selected | bit-exact 10 frames |
| HCBP1_HHI_A.264 | Baseline | 1/8 | CAVLC | IP | RPLM frame-num-gaps MMCO1 | selected | bit-exact 250 frames |
| HCBP2_HHI_A.264 | Baseline | 1/8 | CAVLC | IP | RPLM frame-num-gaps MMCO1 | selected | bit-exact 250 frames |
| HCMP1_HHI_A.264 | Main | 1/8 | CAVLC | BIP | spatial-direct RPLM frame-num-gaps MMCO1 | selected | bit-exact 250 frames |
| HPCA_BRCM_C.264 | High | 1/8 | CABAC | BIP | 8x8 temporal-direct | selected | bit-exact 300 frames |
| HPCADQ_BRCM_B.264 | High | 1/8 | CABAC | BIP | 8x8 scaling temporal-direct | selected | bit-exact 300 frames |
| HPCAFL_BRCM_C.264 | High | 1/8 | CABAC | BIP | PAFF/field 8x8 temporal-direct | selected | bit-exact 300 frames |
| HPCAFLNL_BRCM_C.264 | High | 1/8 | CABAC | BIP | PAFF/field 8x8 temporal-direct | selected | bit-exact 300 frames |
| HPCALQ_BRCM_B.264 | High | 1/8 | CABAC | BIP | 8x8 scaling temporal-direct | selected | bit-exact 300 frames |
| HPCAMAPALQ_BRCM_B.264 | High | 1/8 | CABAC | BIP | MBAFF PAFF/field 8x8 scaling temporal-direct | selected | bit-exact 300 frames |
| HPCAMOLQ_BRCM_B.264 | High | 0/8 | CABAC | BIP | 8x8 scaling temporal-direct | selected | bit-exact 100 frames |
| HPCANL_BRCM_C.264 | High | 1/8 | CABAC | BIP | 8x8 temporal-direct | selected | bit-exact 300 frames |
| HPCAQ2LQ_BRCM_B.264 | High | 1/8 | CABAC | BIP | 8x8 scaling temporal-direct | selected | bit-exact 100 frames |
| HPCV_BRCM_A.264 | High | 1/8 | CAVLC | BIP | 8x8 temporal-direct | selected | bit-exact 300 frames |
| HPCVFL_BRCM_A.264 | High | 1/8 | CAVLC | BIP | PAFF/field 8x8 temporal-direct | selected | bit-exact 300 frames |
| HPCVFLNL_BRCM_A.264 | High | 1/8 | CAVLC | BIP | PAFF/field 8x8 temporal-direct | selected | bit-exact 300 frames |
| HPCVMOLQ_BRCM_B.264 | High | 0/8 | CAVLC | BIP | 8x8 scaling temporal-direct | selected | bit-exact 100 frames |
| HPCVNL_BRCM_A.264 | High | 1/8 | CAVLC | BIP | 8x8 temporal-direct | selected | bit-exact 300 frames |
| LS_SVA_D.264 | Baseline | 1/8 | CAVLC | IP |  | selected | bit-exact 1700 frames |
| MIDR_MW_D.264 | Baseline | 1/8 | CAVLC | IP |  | selected | bit-exact 100 frames |
| MPS_MW_A.264 | Baseline | 1/8 | CAVLC | IP |  | selected | bit-exact 150 frames |
| MR1_BT_A.h264 | Baseline | 1/8 | CAVLC | IP | RPLM multi-slice MMCO1,3,4 | selected | bit-exact 62 frames |
| MR1_MW_A.264 | Baseline | 1/8 | CAVLC | IP | RPLM | selected | bit-exact 150 frames |
| MR2_MW_A.264 | Baseline | 1/8 | CAVLC | IP | MMCO1,2,3,4 | selected | bit-exact 300 frames |
| MR2_TANDBERG_E.264 | Baseline | 1/8 | CAVLC | IP | RPLM MMCO1,2,3,4,5,6 | selected | bit-exact 300 frames |
| MR3_TANDBERG_B.264 | Baseline | 1/8 | CAVLC | IP | RPLM frame-num-gaps MMCO1,2,3,4,5 | selected | bit-exact 300 frames |
| MR4_TANDBERG_C.264 | Baseline | 1/8 | CAVLC | IP | RPLM frame-num-gaps MMCO1,2,3,4,5 | selected | bit-exact 300 frames |
| MR5_TANDBERG_C.264 | Baseline | 1/8 | CAVLC | IP | RPLM frame-num-gaps MMCO1,2,3,4,5 | selected | bit-exact 300 frames |
| MR9_BT_B.h264 | Main | 1/8 | CABAC | BIP | MBAFF PAFF/field temporal-direct RPLM MMCO1,3,4 | selected | bit-exact 58 frames |
| NL1_Sony_D.jsv | Baseline | 1/8 | CAVLC | I |  | selected | bit-exact 17 frames |
| NL2_Sony_H.jsv | Baseline | 1/8 | CAVLC | IP |  | selected | bit-exact 300 frames |
| NL3_SVA_E.264 | Main | 1/8 | CAVLC | BIP | spatial-direct | selected | bit-exact 33 frames |
| NLMQ1_JVC_C.264 | Baseline | 1/8 | CAVLC | I |  | selected | bit-exact 30 frames |
| NLMQ2_JVC_C.264 | Baseline | 1/8 | CAVLC | IP |  | selected | bit-exact 30 frames |
| NRF_MW_E.264 | Baseline | 1/8 | CAVLC | IP |  | selected | bit-exact 100 frames |
| Sharp_MP_Field_1_B.jvt | Main | 1/8 | CAVLC | IP | PAFF/field | selected | bit-exact 15 frames |
| Sharp_MP_Field_2_B.jvt | Main | 1/8 | CAVLC | IP | PAFF/field | selected | bit-exact 15 frames |
| Sharp_MP_Field_3_B.jvt | Main | 1/8 | CAVLC | BIP | PAFF/field spatial-direct | selected | bit-exact 15 frames |
| Sharp_MP_PAFF_1r2.jvt | Main | 1/8 | CAVLC | BIP | PAFF/field spatial-direct | selected | bit-exact 15 frames |
| Sharp_MP_PAFF_2.jvt | Main | 1/8 | CABAC | BIP | PAFF/field spatial-direct | selected | bit-exact 15 frames |
| SL1_SVA_B.264 | Main | 1/8 | CAVLC | BIP | temporal-direct multi-slice | selected | bit-exact 33 frames |
| slice2_field_aurora4.264 | High | 1/8 | CABAC | BIP | PAFF/field 8x8 spatial-direct RPLM multi-slice MMCO1 | selected | bit-exact 50 frames |
| src19td.IBP.264 | Main | 1/8 | CABAC | BIP | spatial-direct | selected | bit-exact 257 frames |
| SVA_BA1_B.264 | Baseline | 1/8 | CAVLC | I |  | selected | bit-exact 17 frames |
| SVA_BA2_D.264 | Baseline | 1/8 | CAVLC | IP |  | selected | bit-exact 17 frames |
| SVA_Base_B.264 | Baseline | 1/8 | CAVLC | IP | multi-slice | selected | bit-exact 17 frames |
| SVA_CL1_E.264 | Baseline | 1/8 | CAVLC | IP | multi-slice | selected | bit-exact 50 frames |
| SVA_FM1_E.264 | Baseline | 1/8 | CAVLC | IP | multi-slice | selected | bit-exact 17 frames |
| SVA_NL1_B.264 | Baseline | 1/8 | CAVLC | I |  | selected | bit-exact 17 frames |
| SVA_NL2_E.264 | Baseline | 1/8 | CAVLC | IP |  | selected | bit-exact 17 frames |
| test8b43.264 | Main | 1/8 | CAVLC | IP |  | selected | bit-exact 43 frames |
| BA3_SVA_C.264 | Extended | 1/8 | CAVLC | BIP | spatial-direct | Extended profile_idc, in-set tools only | bit-exact 33 frames |
| FM1_BT_B.h264 | Baseline | 1/8 | CAVLC | IP | FMO RPLM multi-slice | out: FMO | refused (Unsupported) |
| FM1_FT_E.264 | Baseline | 1/8 | CAVLC | IP | FMO multi-slice | out: FMO | refused (Unsupported) |
| FM2_SVA_B.264 | Baseline | 1/8 | CAVLC | IP | FMO multi-slice | out: FMO | refused (Unsupported) |
| FM2_SVA_C.264 | Baseline | 1/8 | CAVLC | IP | FMO multi-slice | out: FMO | refused (Unsupported) |
| Hi422FR10_SONY_B.264 | High422 | 2/8 | CABAC | I | 8x8 | out: 4:2:2 | refused (Unsupported) |
| Hi422FR13_SONY_B.264 | High422 | 2/10 | CABAC | I | 8x8 | out: 4:2:2 | refused (Unsupported) |
| Hi422FR1_SONY_A.jsv | High422 | 2/8 | CAVLC | I |  | out: 4:2:2 | refused (Unsupported) |
| Hi422FR6_SONY_A.jsv | High422 | 2/10 | CAVLC | I |  | out: 4:2:2 | refused (Unsupported) |
| MR6_BT_B.h264 | Extended | 1/8 | CAVLC | IP | PAFF/field RPLM MMCO1,3,4 | Extended profile_idc, in-set tools only | bit-exact 60 frames |
| MR7_BT_B.h264 | Extended | 1/8 | CAVLC | IP | PAFF/field RPLM MMCO1,3,4 | Extended profile_idc, in-set tools only | bit-exact 60 frames |
| MR8_BT_B.h264 | Extended | 1/8 | CAVLC | BIP | PAFF/field temporal-direct RPLM MMCO3,4 | Extended profile_idc, in-set tools only | bit-exact 58 frames |
| PPH10I1_Panasonic_A.264 | High10 | 1/10 | CAVLC | I | 8x8 multi-slice | out: bit depth 10 | refused (Unsupported) |
| PPH10I2_Panasonic_A.264 | High10 | 1/10 | CABAC | I | 8x8 multi-slice | out: bit depth 10 | refused (Unsupported) |
| PPH10I3_Panasonic_A.264 | High10 | 1/10 | CABAC | I | 8x8 multi-slice | out: bit depth 10 | refused (Unsupported) |
| PPH10I4_Panasonic_A.264 | High10 | 1/10 | CAVLC | I | 8x8 multi-slice crop | out: bit depth 10 | refused (Unsupported) |
| PPH10I5_Panasonic_A.264 | High10 | 1/10 | CABAC | I | 8x8 multi-slice crop | out: bit depth 10 | refused (Unsupported) |
| PPH10I6_Panasonic_A.264 | High10 | 1/10 | CAVLC | I | MBAFF 8x8 multi-slice crop | out: bit depth 10 | refused (Unsupported) |
| PPH10I7_Panasonic_A.264 | High10 | 1/10 | CABAC | I | MBAFF 8x8 multi-slice crop | out: bit depth 10 | refused (Unsupported) |
| PPH422I1_Panasonic_A.264 | High422 | 2/10 | CAVLC | I | 8x8 multi-slice | out: 4:2:2 | refused (Unsupported) |
| PPH422I2_Panasonic_A.264 | High422 | 2/10 | CABAC | I | 8x8 multi-slice | out: 4:2:2 | refused (Unsupported) |
| PPH422I3_Panasonic_A.264 | High422 | 2/10 | CABAC | I | 8x8 multi-slice | out: 4:2:2 | refused (Unsupported) |
| PPH422I4_Panasonic_A.264 | High422 | 2/10 | CAVLC | I | 8x8 multi-slice crop | out: 4:2:2 | refused (Unsupported) |
| PPH422I5_Panasonic_A.264 | High422 | 2/10 | CABAC | I | 8x8 multi-slice crop | out: 4:2:2 | refused (Unsupported) |
| PPH422I6_Panasonic_A.264 | High422 | 2/10 | CAVLC | I | MBAFF 8x8 multi-slice crop | out: 4:2:2 | refused (Unsupported) |
| PPH422I7_Panasonic_A.264 | High422 | 2/10 | CABAC | I | MBAFF 8x8 multi-slice crop | out: 4:2:2 | refused (Unsupported) |
| sp1_bt_a.h264 | Extended | 1/8 | CAVLC | IPSP | MMCO5 | out: SP slices (Extended) | refused (Unsupported) |
| sp2_bt_b.h264 | Extended | 1/8 | CAVLC | IPSP | MMCO5 | out: SP slices (Extended) | refused (Unsupported) |

## Performance (development, Mac, NOT evidence)

Machine: Apple M3 Pro, macOS, shared with other agents' builds (heavily loaded; cycle counts vary
by several percent between runs). Metric: minimum over 3 interleaved runs of the counters reported
by `/usr/bin/time -l` (cycles, instructions retired, user+sys CPU, max RSS), `ffmpeg -threads 1 -f
h264 -i X -f null -` versus `examples/decode X /dev/null` (streams the input in 1 MiB chunks and
discards pictures). Every stream below decodes bit-exact against FFmpeg. The `h264-*` inputs are the
coordinator's D4 workloads (`.research/m1d4/d4-inputs/`, all CABAC); the `*_1080*` streams are
libx264 encodes of `testsrc2` + noise (8-10 Mbit/s, kept outside git).

Current build (second optimisation round):

| stream | FFmpeg cycles | ours cycles | ratio | instr ratio | FFmpeg CPU | ours CPU | max RSS MB FFmpeg / ours |
|---|---|---|---|---|---|---|---|
| h264-high-1080p.h264 | 1.16G | 1.34G | 1.16 | 1.61 | 0.30 s | 0.35 s | 42 / 27 |
| h264-high-720p.h264 | 0.60G | 0.64G | 1.07 | 1.50 | 0.15 s | 0.16 s | 22 / 14 |
| h264-main-1920x800.h264 | 0.83G | 0.97G | 1.16 | 1.63 | 0.21 s | 0.25 s | 34 / 21 |
| high_cabac_1080p.264 | 4.84G | 5.22G | 1.08 | 1.67 | 1.30 s | 1.40 s | 51 / 28 |
| high_mbaff_1080i.264 | 5.39G | 5.99G | 1.11 | 1.81 | 1.42 s | 1.59 s | 53 / 28 |
| main_cavlc_1080p.264 | 2.41G | 3.29G | 1.37 | 2.04 | 0.63 s | 0.86 s | 58 / 29 |

Peak memory footprint (`/usr/bin/time -l`, a heap-plus-dirty-pages proxy) on the D4 inputs, FFmpeg /
ours: 1080p 32.4 / 27.3 MB, 720p 17.4 / 14.0 MB, 1920x800 23.4 / 21.2 MB.

Start of this round (same machine and method): D4 cycle ratios 1.44 / 1.28 / 1.42 (instructions
7.40G / 3.41G / 5.26G, now 5.64G / 2.63G / 4.06G), max RSS 49 / 31 / 40 MB (RSS also varied by
+-15 MB between runs because freed frame buffers were not reused).

Coordinator note (Linux ARM64 Neoverse V2, `-C target-cpu=neoverse-v2`, fat LTO, snapshot taken
before both optimisation rounds' later work): h264-high-1080p 731 ms vs FFmpeg 472 ms (1.55x),
h264-high-720p 338 vs 229 ms (1.48x). ARM64 gating must be re-measured by the coordinator.

Changes of this round, each re-verified bit-exact on all 195 vectors and the six streams above:
- Frame buffers no longer reallocated per picture: a store whose samples an output picture still
  shares parks the buffer as a spare and reuses it once the caller drops the picture (was a fresh
  zeroed 3 MB allocation for a third of the pictures). Non-reference pictures keep no motion data;
  stored references of frame-only streams with direct_8x8_inference keep only the corner 4x4
  vector of each 8x8 (the only blocks co-location reads there). This is the RSS / footprint drop.
- Deblocking per MB row as soon as the row below is decoded (1-row delay keeps the unfiltered
  samples intra prediction reads), so the filter works on cache-resident rows instead of a
  whole-picture pass; bS of an edge between two MBs with uniform motion from one motion comparison;
  simple left edges without per-line arrays.
- Whole-MB spatial direct (B_Skip / B_Direct_16x16): refIdx stored once, non-MBAFF neighbour fetch
  without the generic location path, colZeroFlag as a block mask with an inlined frame/frame
  co-located lookup. P_Skip, 16x16 and uniform direct MBs hand their motion straight to MC.
- MC: full-sample blocks copied without entering the interpolation kernels; bi-prediction reads
  full-sample blocks in place and averages / weights both sources straight into the picture (one
  pass instead of copy + copy + average); implicit / small explicit weights in 16-bit lanes;
  six-tap rows from overlapping windows with 16-bit clipping.
- CABAC significance maps: one look-ahead refill per significant / last pair.

Remaining gap: instructions retired are still 1.5-1.7x FFmpeg's (IPC ~4.5 vs ~3.0 here), so a
narrower core is expected to show a larger ratio than this Mac. On the D4 1080p input the time
splits roughly: macroblock parsing ~53 % of which about half is CABAC residual decoding (~20 cycles
per bin, dominated by data-dependent branches), MC ~24 %, deblocking ~17 % (now near FFmpeg's own
deblocking cost when measured with `-skip_loop_filter all`), reconstruction ~6 %. Tried and
dropped (no gain here): software prefetch of reference rows (`std::hint::prefetch_read`), a per-MB
neighbour motion cache, a bS < 4 specialised luma filter, register-held CABAC state for level
prefixes, inlining the MC dispatchers.

Optimisations in place overall: cached 64-bit bit reader and local register copies in hot loops;
CABAC engine with a 64-bit left-aligned offset, packed rangeTabLPS rows, branchless interval
selection and renormalisation, per-category specialised residual decoding; CAVLC two-level VLC
tables, single-window level decoding, direct run_before tables; autovectorised fixed-window MC
kernels with in-place reference reads and edge emulation only at borders; lane-parallel IDCT 8x8,
branch-free dequantisation; deblocking on gathered 16-line lanes with branch-free vectorised
filters and u64 in-register transposes; output pictures share the frame buffer (`Arc`, no copy).
No `unsafe`, no `std::simd` (stable-compatible autovectorisation).

Lint status: `cargo +nightly clippy -p triergon-codec-h264 --all-targets --locked -- -D warnings`
is clean (real fixes; scoped `#[allow]`s with a one-line reason only for indexed hot loops over
parallel fixed-size arrays, explicit-geometry kernels with many arguments, and `.max().min()` where
`Ord::clamp`'s run-time assertion blocks vectorisation); `rustfmt --edition 2024 --check` clean.

## Gaps / not done

- **Performance gate not met on the evidence that exists** (Mac D4 cycle ratios 1.07-1.16 now; the
  only ARM64 numbers, 1.48-1.55x, predate this work). Next steps in expected payoff order: explicit
  `std::simd` kernels (nightly, behind a feature with the scalar path kept) for the avg / weight /
  copy rows and the deblocking filters to cut the per-row bounds checks and lane widening that
  autovectorisation leaves; fewer per-MB instructions on the skip path (deblocking control and the
  MC glue still cost several hundred instructions per skipped MB); CAVLC residual (main_cavlc 1.37).
- Memory: max RSS and footprint are now below FFmpeg's on the D4 inputs; heap was not measured with
  an allocator hook (footprint margin is 2-5 MB).
- Error concealment: none beyond FFmpeg-compatible behaviour on intact conformance streams; damaged
  slices are reported as `InvalidData` and their macroblocks stay undecoded (FFmpeg would conceal).
  Fuzz smoke test (`examples/fuzz.rs`, bit flips / truncation / splices of all 195 vectors, overflow
  checks and debug assertions on): 0 panics in 1560 + 784 mutated decodes in the first round and
  0 in 1568 (8 per file, 195 vectors + one D4 input) after this round's changes.
- Out of the selected set (refused, typed): bit depth > 8, 4:2:2 / 4:4:4, lossless, FMO/ASO slice
  groups, SP/SI, data partitioning; MVC/SVC extension NAL units are skipped.
- Redundant slices are dropped rather than used for concealment.
- Unpaired fields are output as a frame with the other field's lines unset (no conformance vector
  exercises this).
- Mac results are development evidence only; Linux ARM64 qualification is the coordinator's.

## Shared-code requests

None required. Note for the coordinator: the crate builds on stable Rust (no nightly features);
tests need the gitignored FATE corpus and skip otherwise.

## Files

- `crates/codec-h264/src/*.rs` (decoder), `tests/conformance.rs` + `tests/ffmpeg_framemd5_digests.txt`
  (all 195 vectors), `examples/decode.rs` (Annex B -> raw yuv420p + per-picture JSON on stderr;
  `/dev/null` skips writing), `examples/inspect.rs` (feature classification), `examples/fuzz.rs`.


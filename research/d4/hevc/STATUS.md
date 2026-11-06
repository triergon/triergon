# triergon-codec-hevc status (D4 HEVC lane)

**State: working prototype.** Every selected-set conformance vector in the local FATE
download (176 of 188 files) decodes bit-exact against FFmpeg 9.0.2 on macOS arm64, frame
count and order included; the 10 out-of-set RExt vectors are refused with typed
`Unsupported` errors; the 2 remaining files are refused by FFmpeg itself and by us. This is
development evidence from a Mac only — not a parity pass, not release-qualified, and the
Linux ARM64 runtime gate is not demonstrated: the coordinator's Neoverse V2 run of an
earlier snapshot measured 1.27-1.29x FFmpeg on the D4 gate inputs (gate <= 1.25); the
speed round below (Mac: 8-bit gate input 1.09x -> 0.83x, instructions 7.37 G -> 5.17 G vs
FFmpeg's 4.80 G) has not been re-measured there (see Performance).

## Specification and provenance

- Rec. ITU-T H.265 (V11) (01/2026), "High efficiency video coding", edition in force.
  https://www.itu.int/rec/T-REC-H.265-202601-I/en, English PDF (12 246 649 bytes, posted
  2026-03-18), SHA-256 `78b093480e178a2b99d8cb7499934240f359c50c6d09568f8be651bc1151d4d2`.
  Kept in `target/agent-hevc/spec/` only (not committed).
- Written from the specification text. Tables are transcribed (or regenerated from the
  spec formula) with the table / equation number in a comment (`src/tables.rs`,
  `src/cabac.rs`). No FFmpeg / libavcodec / libde265 / HM / GPL decoder source was read.
  FFmpeg was used only as a black-box binary (raw output, log messages, `trace_headers`).
- Crate: `crates/codec-hevc` (`triergon-codec-hevc`), `#![forbid(unsafe_code)]`, no
  dependencies, ~12.4 k lines in `src/` including comments and unit tests. Feature `simd`
  (default) enables `#![feature(portable_simd)]` kernels (motion compensation, deblocking,
  SAO; nightly); `--no-default-features` builds on stable and uses the generic scalar code,
  which is the reference equivalent (both variants pass the full vector set).

## Selected feature matrix — implemented

| Area | Clauses | Status |
|---|---|---|
| Annex B byte stream, NAL header, emulation prevention, AU splitting | B, 7.3.1, 7.4.2.4.4 | done; `nuh_layer_id > 0` NAL units skipped (base layer of MV-HEVC decodes) |
| VPS (id only), SPS incl. VUI/HRD skip, PPS, PTL, sub-layer ordering | 7.3.2, 7.3.3, E.2 | done |
| Scaling lists: default, explicit, prediction from ref matrix / default, DC | 7.3.4, 7.4.5 | done (SPS and PPS level) |
| Short-term RPS incl. inter-RPS prediction, long-term refs (SPS + slice), POC | 7.3.7, 7.4.8, 8.3.1, 8.3.2 | done |
| Slice segment header incl. dependent segments, RPL modification, pred weight table, entry points, header extension | 7.3.6 | done |
| CABAC engine, all context tables, initType, WPP storage/sync, dependent-slice context carry-over | 9.3 | done |
| CTU / quadtree / CU / PU / TU syntax, all slice types, CTB 16/32/64, min CB 8..64 | 7.3.8 | done |
| Intra: planar, DC, 33 angular, reference substitution, [1 2 1] filter, strong intra smoothing, boundary filters, constrained intra pred | 8.4 | done |
| Inter: merge (spatial, temporal, combined bi-pred, zero), parallel merge level, AMVP incl. scaling and long-term rules, TMVP (collocated MV scaling, LongTermRefPic), 8x4/4x8 bi restriction | 8.5.3.2 | done |
| 8-tap luma / 4-tap chroma interpolation, edge clamping, 8 and 10 bit | 8.5.3.3.3 | done |
| Default and explicit weighted prediction | 8.5.3.3.4 | done |
| AMP partitions | 7.3.8.5 | done |
| Dequantization with flat / explicit scaling lists, QP prediction, cu_qp_delta, chroma QP table | 8.6.1-8.6.3 | done |
| Inverse DCT 4..32 (butterfly), DST 4x4, transform skip (4x4), transquant bypass | 8.6.4, 8.6.2 | done |
| Sign data hiding | 7.3.8.11 | done |
| PCM (luma/chroma bit depths) with pcm_loop_filter_disabled | 7.3.8.7 | done |
| Deblocking: TU/PU edges, BS derivation, slice/tile boundary flags, per-slice beta/tc offsets, PCM / bypass exclusion | 8.7.2 | done |
| SAO band / edge, slice / tile boundary rules, 10-bit, PCM / bypass exclusion | 8.7.3 | done |
| Tiles (uniform and explicit spacing), WPP (single-threaded), dependent slices | 6.5.1, 9.3.1 | done |
| CRA / BLA / IDR / RASL / RADL: NoRaslOutputFlag, RASL after CRA at stream start / after EOS not decoded or output, missing-reference generation | 8.1.3, 8.3.3 | done |
| Output-order DPB: bumping with sps_max_num_reorder_pics / latency / dec_pic_buffering, no_output_of_prior_pics (CRA after EOS treated per C.5.2.2), pic_output_flag | C.5.2 | done |
| Conformance window cropping, VUI SAR and colour description reported | 7.4.3.2, E.3 | done (zero-copy output planes) |
| SEI | — | parsed as opaque and ignored (not needed for output) |

FFmpeg-matching behaviours verified on vectors: RASL skipping (RAP_A/B), EOS followed by
CRA (NoOutPrior_A, RAP_B), no_output_of_prior_pics (NoOutPrior_A/B), pic_output_flag
(OPFLAG_A/B/C), MV-HEVC base layer (MVHEVCS_*), PPS extension bits ignored for Main
profiles (PS_A_VIDYO_3, where `pps_extension_4bits` payload would otherwise misparse).

## Out of the selected set (typed `DecodeError::Unsupported`, tool named)

chroma_format_idc 0 / 2 / 3 (monochrome, 4:2:2, 4:4:4) and separate colour planes;
bit depth > 10; unequal luma / chroma bit depth (FFmpeg also refuses); every
`sps_range_extension` flag (transform-skip rotation / context, implicit / explicit RDPCM,
extended precision, intra smoothing disabled, high-precision offsets, persistent Rice
adaptation, CABAC bypass alignment); PPS range extension tools (log2_max_transform_skip
> 2, cross-component prediction, chroma QP offset lists, SAO offset scaling) when the SPS
profile is RExt or higher; SPS SCC extension and PPS SCC extension for SCC profiles.
Multilayer / 3D SPS extension payloads are ignored for the base layer.

## Public API

`Decoder::{new, send(au, pts), send_nal, receive, flush, reset}`; `Picture { width, height
(cropped), bit_depth, chroma, planes: [Plane; 3], pts, poc, sar, colour }`;
`Plane::{row(y), raw() -> (whole backing slice, stride, offset of first cropped sample),
write_raw}`; `Row::{U8, U16}`; `DecodeError::{Unsupported, InvalidData, ResourceLimit,
MissingParameterSet}`; `AccessUnitSplitter`. PTS travels with the picture whose AU carried
it. Pictures share the decoder's frame buffers (Arc); dimensions are capped at level 6.2
(MaxLumaPs 35 651 584, 16888 per side) before allocation; DPB size comes from
sps_max_dec_pic_buffering (<= 16).

`examples/decode.rs`: `decode <in.hevc> <out.yuv|-|null> [--quiet] [--repeat N]` streams
the input, writes planar output in output order (u8, or little-endian u16 for 10-bit) and
one JSON line per picture to stderr (`frame, width, height, bitdepth, pts, poc, sar,
colour`). `null` decodes without writing samples (the counterpart of `ffmpeg -f null`);
writing to `/dev/null` still pays the row copies (8-bit) or the u16 -> little-endian
conversion (10-bit, now a vectorized loop: it had cost ~3 G instructions on the 2160p input).

## Tests

- Unit tests: bit reader / NAL, CABAC init, transform vs. direct matrix product (full and
  sparse extents, 8 / 10 bit), DC shortcut, table symmetry, scan tables, MD5 self-test.
- `tests/conformance.rs`: MD5 of FFmpeg raw output per vector (`tests/ffmpeg_ref_md5.txt`,
  186 entries); quick subset by default, `HEVC_CONFORMANCE_FULL=1` for all; out-of-set
  vectors must be refused with `Unsupported`; `Plane::raw` vs `row` consistency. Skips
  cleanly if `.research/m1d4/fate-suite/hevc-conformance` is absent.
- Last full run (2026-11-03, after the speed round, macOS arm64): all 186 entries pass with
  `simd` (nightly) and with `--no-default-features` (scalar path, built and run on stable);
  `compare.py` over the whole download: 176 PASS, 10 RExt refused, 2 refused by both.
- Lint / format: `cargo +nightly clippy -p triergon-codec-hevc --all-targets -- -D
  warnings` clean with default features and with `--no-default-features`;
  `rustfmt --edition 2024 --check` clean. The only `#[allow]`s are
  `clippy::too_many_arguments` on kernel / filter entry points (hot-path signatures that
  pass slices and strides instead of building structs).
- `research/d4/hevc/compare.py` streams FFmpeg and our output through pipes and compares
  per-frame MD5 (only hash lists are stored); on a mismatch it re-streams to locate the
  first differing frame / plane / sample / CTB. `status_table.py` renders the table below.
- Additional x265-encoded streams (generated locally, not committed): 1080p 8-bit
  (300 frames, CRF 26 + noise), 1080p 10-bit (300 frames), 2160p 10-bit (90 frames):
  bit-exact (per-frame MD5 equal to FFmpeg), as are the three D4 gate inputs.

## Per-vector results (FFmpeg 9.0.2 Homebrew, macOS arm64; development oracle)

176 selected PASS (bit-exact, same frame count), 10 out-of-set refused, 2 refused by both.

| vector | profile_idc | class | result |
|---|---|---|---|
| ADJUST_IPRED_ANGLE_A_RExt_Mitsubishi_1.bit | RExt | out-of-set: RExt 4:2:2 | refused with typed Unsupported (FFmpeg decodes) |
| AMP_A_Samsung_4.bit | Main | selected | PASS frames=60 |
| AMP_A_Samsung_6.bit | 0 (pre-final) | selected | PASS frames=60 |
| AMP_B_Samsung_4.bit | Main | selected | PASS frames=60 |
| AMP_B_Samsung_6.bit | 0 (pre-final) | selected | PASS frames=60 |
| AMP_D_Hisilicon.bit | Main | selected | PASS frames=100 |
| AMP_E_Hisilicon.bit | Main | selected | PASS frames=100 |
| AMP_F_Hisilicon_3.bit | Main | selected | PASS frames=100 |
| AMVP_A_MTK_4.bit | Main | selected | PASS frames=30 |
| AMVP_B_MTK_4.bit | Main | selected | PASS frames=41 |
| AMVP_C_Samsung_4.bit | Main | selected | PASS frames=60 |
| AMVP_C_Samsung_6.bit | 0 (pre-final) | selected | PASS frames=60 |
| BUMPING_A_ericsson_1.bit | Main | selected | PASS frames=49 |
| CAINIT_A_SHARP_4.bit | Main | selected | PASS frames=50 |
| CAINIT_B_SHARP_4.bit | Main | selected | PASS frames=50 |
| CAINIT_C_SHARP_3.bit | Main | selected | PASS frames=50 |
| CAINIT_D_SHARP_3.bit | Main | selected | PASS frames=50 |
| CAINIT_E_SHARP_3.bit | Main | selected | PASS frames=50 |
| CAINIT_F_SHARP_3.bit | Main | selected | PASS frames=50 |
| CAINIT_G_SHARP_3.bit | Main | selected | PASS frames=50 |
| CAINIT_H_SHARP_3.bit | Main | selected | PASS frames=50 |
| CIP_A_Panasonic_3.bit | Main | selected | PASS frames=2 |
| CIP_C_Panasonic_2.bit | Main | selected | PASS frames=2 |
| CONFWIN_A_Sony_1.bit | Main | selected | PASS frames=60 |
| DBLK_A_MAIN10_VIXS_2.bit | Main 10 | selected | PASS frames=8 |
| DBLK_A_MAIN10_VIXS_3.bit | Main 10 | selected | PASS frames=8 |
| DBLK_A_SONY_3.bit | Main | selected | PASS frames=30 |
| DBLK_B_SONY_3.bit | Main | selected | PASS frames=30 |
| DBLK_C_SONY_3.bit | Main | selected | PASS frames=30 |
| DBLK_D_VIXS_1.bit | 0 (pre-final) | selected | PASS frames=8 |
| DBLK_D_VIXS_2.bit | Main | selected | PASS frames=8 |
| DBLK_E_VIXS_1.bit | 0 (pre-final) | selected | PASS frames=8 |
| DBLK_E_VIXS_2.bit | Main | selected | PASS frames=8 |
| DBLK_F_VIXS_1.bit | 0 (pre-final) | selected | PASS frames=8 |
| DBLK_F_VIXS_2.bit | Main | selected | PASS frames=8 |
| DBLK_G_VIXS_1.bit | 0 (pre-final) | selected | PASS frames=8 |
| DBLK_G_VIXS_2.bit | Main | selected | PASS frames=8 |
| DELTAQP_A_BRCM_4.bit | Main | selected | PASS frames=96 |
| DELTAQP_B_SONY_3.bit | Main | selected | PASS frames=30 |
| DELTAQP_C_SONY_3.bit | Main | selected | PASS frames=30 |
| DSLICE_A_HHI_5.bit | Main | selected | PASS frames=50 |
| DSLICE_B_HHI_5.bit | Main | selected | PASS frames=50 |
| DSLICE_C_HHI_5.bit | Main | selected | PASS frames=50 |
| ENTP_A_LG_2.bit | 0 (pre-final) | selected | PASS frames=50 |
| ENTP_A_Qualcomm_1.bit | Main | selected | PASS frames=24 |
| ENTP_B_LG_2.bit | 0 (pre-final) | selected | PASS frames=50 |
| ENTP_B_Qualcomm_1.bit | Main | selected | PASS frames=24 |
| ENTP_C_LG_3.bit | 0 (pre-final) | selected | PASS frames=50 |
| ENTP_C_Qualcomm_1.bit | Main | selected | PASS frames=24 |
| EXT_A_ericsson_3.bit | Main | FFmpeg-refused | no output, matches FFmpeg (SPS uses pre-v2 extension syntax; FFmpeg: "Overread SPS", no output) |
| EXT_A_ericsson_4.bit | Main | selected | PASS frames=3 |
| FILLER_A_Sony_1.bit | Main | selected | PASS frames=60 |
| HRD_A_Fujitsu_2.bit | Main | selected | PASS frames=96 |
| HRD_A_Fujitsu_3.bit | Main | selected | PASS frames=96 |
| INITQP_A_Sony_1.bit | Main | selected | PASS frames=60 |
| INITQP_B_Sony_1.bit | Main 10 | selected | PASS frames=90 |
| IPCM_A_RExt_NEC.bit | RExt | out-of-set: RExt 4:2:2 | refused with typed Unsupported (FFmpeg decodes) |
| IPCM_B_RExt_NEC.bit | RExt | out-of-set: RExt 4:4:4 | refused with typed Unsupported (FFmpeg decodes) |
| IPRED_A_docomo_2.bit | Main | selected | PASS frames=20 |
| IPRED_B_Nokia_3.bit | MSP | selected | PASS frames=1 |
| IPRED_C_Mitsubishi_2.bit | 0 (pre-final) | selected | PASS frames=4 |
| IPRED_C_Mitsubishi_3.bit | Main | selected | PASS frames=4 |
| LS_A_Orange_2.bit | Main | selected | PASS frames=9 |
| LS_B_ORANGE_3.bit | Main | selected | PASS frames=25 |
| LS_B_ORANGE_4.bit | Main | selected | PASS frames=25 |
| LTRPSPS_A_Qualcomm_1.bit | Main | selected | PASS frames=500 |
| MAXBINS_A_TI_4.bit | Main | selected | PASS frames=2 |
| MAXBINS_B_TI_4.bit | Main | selected | PASS frames=3 |
| MAXBINS_C_TI_4.bit | Main | selected | PASS frames=3 |
| MERGE_A_TI_3.bit | Main | selected | PASS frames=8 |
| MERGE_B_TI_3.bit | Main | selected | PASS frames=8 |
| MERGE_C_TI_3.bit | Main | selected | PASS frames=8 |
| MERGE_D_TI_3.bit | Main | selected | PASS frames=8 |
| MERGE_E_TI_3.bit | Main | selected | PASS frames=8 |
| MERGE_F_MTK_4.bit | Main | selected | PASS frames=41 |
| MERGE_G_HHI_4.bit | Main | selected | PASS frames=2 |
| MVCLIP_A_qualcomm_3.bit | Main | selected | PASS frames=5 |
| MVDL1ZERO_A_docomo_3.bit | Main | selected | PASS frames=500 |
| MVEDGE_A_qualcomm_3.bit | Main | selected | PASS frames=17 |
| MVHEVCS_A.bit | Main | selected | PASS frames=48 |
| MVHEVCS_B.bit | Main | selected | PASS frames=64 |
| MVHEVCS_E.bit | Main | selected | PASS frames=48 |
| MVHEVCS_F.bit | Main | selected | PASS frames=48 |
| Main_422_10_A_RExt_Sony_1.bin | RExt | out-of-set: RExt 4:2:2 10-bit | refused with typed Unsupported (FFmpeg decodes) |
| Main_422_10_B_RExt_Sony_1.bin | RExt | out-of-set: RExt 4:2:2 10-bit | refused with typed Unsupported (FFmpeg decodes) |
| NUT_A_ericsson_4.bit | Main | selected | PASS frames=34 |
| NUT_A_ericsson_5.bit | Main | selected | PASS frames=34 |
| NoOutPrior_A_Qualcomm_1.bit | Main | selected | PASS frames=40 |
| NoOutPrior_B_Qualcomm_1.bit | Main | selected | PASS frames=47 |
| OPFLAG_A_Qualcomm_1.bit | Main | selected | PASS frames=500 |
| OPFLAG_B_Qualcomm_1.bit | Main | selected | PASS frames=98 |
| OPFLAG_C_Qualcomm_1.bit | Main | selected | PASS frames=96 |
| PERSIST_RPARAM_A_RExt_Sony_1.bit | RExt | out-of-set: RExt 4:4:4, persistent Rice adaptation | refused with typed Unsupported (FFmpeg decodes) |
| PERSIST_RPARAM_A_RExt_Sony_3.bit | RExt | out-of-set: RExt 4:4:4, persistent Rice adaptation | refused with typed Unsupported (FFmpeg decodes) |
| PICSIZE_A_Bossen_1.bit | Main | selected | PASS frames=10 |
| PICSIZE_B_Bossen_1.bit | Main | selected | PASS frames=10 |
| PICSIZE_C_Bossen_1.bit | Main | selected | PASS frames=10 |
| PICSIZE_D_Bossen_1.bit | Main | selected | PASS frames=10 |
| PMERGE_A_TI_3.bit | Main | selected | PASS frames=8 |
| PMERGE_B_TI_3.bit | Main | selected | PASS frames=8 |
| PMERGE_C_TI_3.bit | Main | selected | PASS frames=8 |
| PMERGE_D_TI_3.bit | Main | selected | PASS frames=8 |
| PMERGE_E_TI_3.bit | Main | selected | PASS frames=8 |
| POC_A_Bossen_3.bit | Main | selected | PASS frames=5 |
| PPS_A_qualcomm_7.bit | Main | selected | PASS frames=81 |
| PS_A_VIDYO_3.bit | Main | selected | PASS frames=25 |
| PS_B_VIDYO_3.bit | Main | selected | PASS frames=25 |
| QMATRIX_A_RExt_Sony_1.bit | RExt | out-of-set: RExt 4:4:4 scaling lists | refused with typed Unsupported (FFmpeg decodes) |
| RAP_A_docomo_4.bit | Main | selected | PASS frames=86 |
| RAP_B_Bossen_1.bit | Main | selected | PASS frames=80 |
| RPLM_A_qualcomm_4.bit | Main | selected | PASS frames=300 |
| RPLM_B_qualcomm_4.bit | Main | selected | PASS frames=300 |
| RPS_A_docomo_4.bit | Main | selected | PASS frames=44 |
| RPS_B_qualcomm_5.bit | Main | selected | PASS frames=300 |
| RPS_C_ericsson_4.bit | Main | selected | PASS frames=40 |
| RPS_C_ericsson_5.bit | Main | selected | PASS frames=40 |
| RPS_D_ericsson_5.bit | Main | selected | PASS frames=68 |
| RPS_D_ericsson_6.bit | Main | selected | PASS frames=68 |
| RPS_E_qualcomm_5.bit | Main | selected | PASS frames=300 |
| RPS_F_docomo_1.bit | Main | selected | PASS frames=50 |
| RQT_A_HHI_4.bit | Main | selected | PASS frames=2 |
| RQT_B_HHI_4.bit | Main | selected | PASS frames=2 |
| RQT_C_HHI_4.bit | Main | selected | PASS frames=2 |
| RQT_D_HHI_4.bit | Main | selected | PASS frames=2 |
| RQT_E_HHI_4.bit | Main | selected | PASS frames=2 |
| RQT_F_HHI_4.bit | Main | selected | PASS frames=2 |
| RQT_G_HHI_4.bit | Main | selected | PASS frames=2 |
| SAO_A_MediaTek_4.bit | Main | selected | PASS frames=60 |
| SAO_A_RExt_MediaTek_1.bit | RExt | out-of-set: RExt 4:4:4 SAO offset scaling | refused with typed Unsupported (FFmpeg decodes) |
| SAO_B_MediaTek_5.bit | Main | selected | PASS frames=60 |
| SAO_C_Samsung_4.bit | Main | selected | PASS frames=60 |
| SAO_C_Samsung_5.bit | Main | selected | PASS frames=5 |
| SAO_D_Samsung_4.bit | Main | selected | PASS frames=60 |
| SAO_D_Samsung_5.bit | Main | selected | PASS frames=5 |
| SAO_E_Canon_4.bit | Main | selected | PASS frames=16 |
| SAO_F_Canon_3.bit | Main | selected | PASS frames=16 |
| SAO_G_Canon_3.bit | Main | selected | PASS frames=16 |
| SDH_A_Orange_3.bit | Main | selected | PASS frames=2 |
| SLICES_A_Rovi_3.bit | Main | selected | PASS frames=9 |
| SLIST_A_Sony_4.bit | Main | selected | PASS frames=65 |
| SLIST_B_Sony_8.bit | Main | selected | PASS frames=65 |
| SLIST_C_Sony_3.bit | Main | selected | PASS frames=65 |
| SLIST_D_Sony_9.bit | Main | selected | PASS frames=65 |
| SLPPLP_A_VIDYO_1.bit | Main | selected | PASS frames=33 |
| SLPPLP_A_VIDYO_2.bit | Main | selected | PASS frames=33 |
| STRUCT_A_Samsung_5.bit | Main | selected | PASS frames=60 |
| STRUCT_B_Samsung_4.bit | Main | selected | PASS frames=60 |
| STRUCT_B_Samsung_6.bit | 0 (pre-final) | selected | PASS frames=60 |
| TILES_A_Cisco_2.bit | Main | selected | PASS frames=100 |
| TILES_B_Cisco_1.bit | Main | selected | PASS frames=100 |
| TMVP_A_MS_2.bit | 0 (pre-final) | selected | PASS frames=17 |
| TMVP_A_MS_3.bit | Main | selected | PASS frames=17 |
| TSCL_A_VIDYO_5.bit | Main | selected | PASS frames=73 |
| TSCL_B_VIDYO_4.bit | Main | selected | PASS frames=73 |
| TSKIP_A_MS_2.bit | 0 (pre-final) | selected | PASS frames=17 |
| TSKIP_A_MS_3.bit | Main | selected | PASS frames=17 |
| TSUNEQBD_A_MAIN10_Technicolor_2.bit | Main 10 | FFmpeg-refused | no output, matches FFmpeg (luma 10 / chroma 9 bit; FFmpeg: "different luma and chroma bit depth unsupported", no output) |
| TUSIZE_A_Samsung_1.bit | Main | selected | PASS frames=5 |
| VPSID_A_VIDYO_1.bit | Main | selected | PASS frames=33 |
| VPSID_A_VIDYO_2.bit | Main | selected | PASS frames=33 |
| WPP_A_ericsson_MAIN10_2.bit | Main 10 | selected | PASS frames=48 |
| WPP_A_ericsson_MAIN_2.bit | Main | selected | PASS frames=48 |
| WPP_B_ericsson_MAIN10_2.bit | Main 10 | selected | PASS frames=48 |
| WPP_B_ericsson_MAIN_2.bit | Main | selected | PASS frames=48 |
| WPP_C_ericsson_MAIN10_2.bit | Main 10 | selected | PASS frames=48 |
| WPP_C_ericsson_MAIN_2.bit | Main | selected | PASS frames=48 |
| WPP_D_ericsson_MAIN10_2.bit | Main 10 | selected | PASS frames=48 |
| WPP_D_ericsson_MAIN_2.bit | Main | selected | PASS frames=48 |
| WPP_E_ericsson_MAIN10_2.bit | Main 10 | selected | PASS frames=48 |
| WPP_E_ericsson_MAIN_2.bit | Main | selected | PASS frames=48 |
| WPP_F_ericsson_MAIN10_2.bit | Main 10 | selected | PASS frames=48 |
| WPP_F_ericsson_MAIN_2.bit | Main | selected | PASS frames=48 |
| WPP_HIGH_TP_444_8BIT_RExt_Apple_2.bit | HT 4:4:4 | out-of-set: RExt High Throughput 4:4:4 tools | refused with typed Unsupported (FFmpeg decodes) |
| WP_A_MAIN10_Toshiba_3.bit | Main 10 | selected | PASS frames=256 |
| WP_A_Toshiba_3.bit | Main | selected | PASS frames=256 |
| WP_B_Toshiba_3.bit | Main | selected | PASS frames=256 |
| WP_MAIN10_B_Toshiba_3.bit | Main 10 | selected | PASS frames=256 |
| cip_B_NEC_2.bit | Main | selected | PASS frames=5 |
| cip_B_NEC_3.bit | Main | selected | PASS frames=5 |
| ipcm_A_NEC_2.bit | Main | selected | PASS frames=1 |
| ipcm_A_NEC_3.bit | Main | selected | PASS frames=1 |
| ipcm_B_NEC_2.bit | Main | selected | PASS frames=1 |
| ipcm_B_NEC_3.bit | Main | selected | PASS frames=1 |
| ipcm_C_NEC_2.bit | Main | selected | PASS frames=1 |
| ipcm_C_NEC_3.bit | Main | selected | PASS frames=1 |
| ipcm_D_NEC_2.bit | Main | selected | PASS frames=1 |
| ipcm_D_NEC_3.bit | Main | selected | PASS frames=1 |
| ipcm_E_NEC_2.bit | Main | selected | PASS frames=1 |

## Performance (development timing on an Apple M3 Pro under concurrent load — NOT evidence)

Mac numbers do not establish Linux ARM64 / Neoverse V2 performance (AGENTS.md); they are
development measurements with other agents' builds running (load average 3.5-9).

### D4 gate inputs, before / after the 8-bit speed round (2026-11-03)

Interleaved runs, minimum of 5, user+sys CPU time; `ffmpeg -v error -threads 1 -i X -f null -`
vs `examples/decode X /dev/null --quiet`; cycles / instructions are the macOS
`/usr/bin/time -l` counters of the same best run. "before" is the binary at the start of
the round (after the clippy fixes), "after" the current crate; "after, null" decodes
without writing samples.

| input | FFmpeg | before | after | after, null | cycles F / before / after | instructions F / before / after |
|---|---|---|---|---|---|---|
| hevc-main-1080p (8-bit, 96 fr.) | 0.408 s | 0.447 s (1.09) | 0.338 s (0.83) | 0.330 s (0.81) | 1.49 / 1.65 / 1.25 G | 4.80 / 7.37 / 5.17 G |
| hevc-main10-1080p (96 fr.) | 0.668 s | 0.618 s (0.92) | 0.428 s (0.64) | 0.401 s (0.60) | 2.50 / 2.32 / 1.60 G | 9.84 / 10.83 / 6.39 G |
| hevc-main10-2160p (24 fr.) | 0.622 s | 0.592 s (0.95) | 0.402 s (0.65) | 0.379 s (0.61) | 2.32 / 2.22 / 1.51 G | 9.13 / 10.29 / 6.03 G |

The 10-bit "after" gain with output includes the example's vectorized u16 output
conversion; decode alone (`null`) is 5.8-6.2 G instructions there.

Other streams, after (min of 3, same method):

| stream | FFmpeg | ours | ratio |
|---|---|---|---|
| AMP_A_Samsung_4 (2560x1600, inter, AMP, no loop filters) | 0.592 s | 0.758 s | 1.28 |
| DELTAQP_A_BRCM_4 (1920x1088) | 0.591 s | 0.581 s | 0.98 |
| ENTP_C_LG_3 (1080p, entry points) | 0.720 s | 0.865 s | 1.20 |
| TILES_A_Cisco_2 (1080p, tiles) | 0.249 s | 0.295 s | 1.18 |
| DSLICE_A_HHI_5 (1080p, WPP + dependent slices) | 0.200 s | 0.219 s | 1.10 |
| PICSIZE_B_Bossen_1 (8440x1056) | 0.209 s | 0.206 s | 0.99 |
| x265 1080p 8-bit, 300 frames (high bitrate) | 3.210 s | 3.262 s | 1.02 |
| x265 1080p 10-bit, 300 frames | 4.516 s | 2.567 s | 0.57 |
| x265 2160p 10-bit, 90 frames | 3.790 s | 2.106 s | 0.56 |

Coordinator's native Linux ARM64 development timing (Neoverse V2, `target-cpu=neoverse-v2`,
fat LTO, `simd` feature) of a snapshot taken ~01:45 UTC, before both this round and the
earlier 10-bit / weighted-prediction work: hevc-main-1080p 692 ms vs FFmpeg 537 ms (1.29x),
hevc-main10-2160p 961 ms vs 755 ms (1.27x). Not re-measured since.

Method notes. Cycle and wall timings on this shared Mac vary by 5-10 %, so changes were
judged mainly by retired-instruction counts (deterministic to ~1 %), with cycles checked
for regressions: one change that cut instructions but raised cycles 6 % (moving the
significance-flag loop into a separate function, which spilled the CABAC state through
memory) was reverted, as was a branch-free CABAC decision (more instructions, no fewer
cycles). Stage costs were measured by "doubling" (running a stage twice and taking the
delta), because removing a pixel-producing stage changes the work of later
content-dependent stages.

What changed in this round (all bit-exact; hevc-main-1080p instruction deltas):
- Boundary strength (-0.30 G, -0.07 G): CU-edge loops compute bS directly for the left /
  top CU edges with the neighbour slice resolved once per edge; interior TU / PU edges are
  scanned only when the CU has them; split borrows instead of per-sample `self` reloads;
  motion equality on packed words.
- Per-CU bookkeeping (-0.23 G): block info (flags, depth, mode, QP) written once per CU
  with a slice fill (QP rewritten only if a cu_qp_delta changed it); motion stored with
  slice fills.
- Merge / AMVP (-0.17 G): candidate derivation stops once the selected candidate is
  known (candidates only depend on earlier ones); AMVP returns mvLXA for mvp flag 0
  without deriving B, and skips the temporal candidate when the list is already long
  enough.
- MC (-0.44 G, -0.11 G, -0.06 G, -0.03 G): 16-lane integer-position bi-prediction in i16
  (saturating sum; a saturated value already clips), const-width row loops without bounds
  checks, const-width row copies for integer uni-prediction, i16 output stages for the
  8-bit fractional kernels, 4-wide last strips in the 8-bit block kernels.
- Deblocking (-0.20 G horizontal luma, -0.05 G vertical luma, -0.10 G chroma): 16-line
  `std::simd` kernels (four 4-line segments per call with per-lane beta / tc / exclusion
  masks; vertical edges through 16x8 <-> 8x16 shuffle transposes), 8 and 10 bit.
- SAO (-0.17 G, -0.11 G, -0.06 G): 16-lane edge / band offset kernels (u8 table lookup
  with saturating add; i16 selects for 10-bit), first / last CTB columns folded into the
  vector run when their neighbours are usable.
- Residual parsing (-0.03 G, plus fewer data-dependent branches): inverse scan tables and
  scan-order significance-context tables, coded-sub-block flags in a bit mask, branch-free
  significance / greater1 bookkeeping, coeff_abs_level_remaining from one 16-bin bypass
  quotient in the common case (prefix <= 3), inline coefficient clearing.
- In-loop filters now run during decoding, one CTB row behind (deblock band r once band
  r+1 is decoded; SAO band r once band r+1 is deblocked), so filtered rows are still in
  cache; finish-of-picture handles the rest. On the Mac (16 MB L2) this is within noise;
  it targets cores with small private L2 such as Neoverse V2. No per-picture scan of block
  flags (no-filter regions are tracked while decoding).

Where the hevc-main-1080p work goes now (approximate, from ablation / doubling; instructions, null output, 5.09 G total):
residual parsing + CU / PU syntax and derivations ~2.0 G; MC ~1.4 G (default bi ~0.7 G,
uni fractional ~0.3 G, integer copies ~0.17 G); deblocking ~0.37 G + bS ~0.25 G; SAO
~0.25 G; inverse transform + add ~0.35 G; intra ~0.27 G.

Design for speed: no per-CU heap allocation (fixed scratch), CABAC with a fixed-position
64-bit window and division-based multi-bin bypass decoding, CTB-local z-order availability
with per-CTB neighbour flags, fused MC output stages (uni writes pixels, bi averages in the
second pass), explicit `std::simd` kernels for 8-bit and 10-bit MC, deblocking and SAO,
butterfly IDCT with zero-extent lane selection and no per-block initialization, in-loop
filtering one CTB row behind decoding, frame pool, zero-copy output planes.

### Memory (2026-11-03, final build)

`/usr/bin/time -l`, same commands as above. macOS "peak memory footprint" is dirty memory
(the closest local proxy for heap); max RSS is reported as well. Both are strictly below
FFmpeg's on the three D4 inputs:

| input | ours RSS | FFmpeg RSS | ours footprint | FFmpeg footprint |
|---|---|---|---|---|
| hevc-main-1080p | 31.6 MB | 51.6 MB | 24.5 MB | 39.3 MB |
| hevc-main10-1080p | 57.0 MB | 76.0 MB | 43.9 MB | 59.3 MB |
| hevc-main10-2160p | 161.2 MB | 238.3 MB | 159.3 MB | 209.9 MB |

Linux heap / RSS were not measured by me.

Robustness: 240 randomly corrupted / truncated streams (bit flips, cuts, random bursts over
conformance vectors) decoded without a panic (release build, re-run after this round);
errors are returned as typed `DecodeError`s.

## Gaps and remaining work (honest list)

1. Runtime gate not demonstrated on Linux ARM64 (last measured snapshot 1.27-1.29x, taken
   before this round) and inter-heavy 8-bit conformance streams with many small fractional
   blocks remain 1.10-1.28x on the Mac. Next levers, by measured share: residual
   coefficient parsing (the CABAC decision chain is now the largest single cost), small
   fractional MC blocks (8x8 luma / 4x4 chroma, per-call overhead), IDCT in i16 lanes,
   horizontal angular intra written column-wise; then real Linux ARM64 runs.
2. No in-loop SEI use (decoded picture hash not checked); SEI ignored.
3. Error resilience is basic: a slice error abandons the current picture (output still
   produced from what was decoded); behaviour on corrupt streams is not compared with
   FFmpeg's concealment.
4. Not exercised by the local vector set: field/interlaced SEI semantics (pictures are
   output as decoded, like FFmpeg), VUI default display window (reported, not applied,
   like FFmpeg's default), streams with SPS changes mid-CVS beyond what the vectors cover.
5. Single-threaded only (WPP / tiles decoded sequentially).

## Shared-code requests

None required. Note for the coordinator: the default `simd` feature needs a nightly
toolchain (`cargo +nightly`, as the brief specifies); `--no-default-features` builds on
stable with the scalar path.

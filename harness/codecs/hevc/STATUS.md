# HEVC comparison tools

Supported features, refusals and the verification method are in
[crates/codec-hevc](../../../crates/codec-hevc/README.md). The decoder is written from
Rec. ITU-T H.265 (V11, 01/2026); FFmpeg 9.0.2 was used only as a black-box binary.

| Tool | Purpose |
|---|---|
| `compare.py` | Streams FFmpeg and decoder output through pipes and compares per-frame MD5; on a mismatch it locates the first differing frame, plane, sample and CTB |

Vectors FFmpeg refuses as well: `EXT_A_ericsson_3.bit` (pre-v2 SPS extension syntax) and
`TSUNEQBD_A_MAIN10_Technicolor_2.bit` (luma 10 / chroma 9 bit).

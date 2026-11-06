# triergon-codec-hevc

H.265/HEVC decoder for Annex B byte streams, written from Rec. ITU-T H.265 (V11, 01/2026).
It is a codec-layer crate whose only dependency is [triergon-codec](../codec/README.md)
(it implements `VideoDecoder`). It decodes the Main,
Main 10 and Main Still Picture profiles (4:2:0, 8 and 10 bit) to cropped pictures in
output order, single-threaded. It does not scale, convert colour or conceal errors.

## Usage

```rust
use triergon_codec_hevc::{AccessUnitSplitter, Decoder, Row};

let mut dec = Decoder::new();
for au in AccessUnitSplitter::new(&stream) {
    dec.send(au, None)?;               // one Annex B access unit, opaque Option<i64> pts
    while let Some(pic) = dec.receive() {
        match pic.planes[0].row(0) {   // Row::U8 for 8-bit, Row::U16 for 10-bit
            Row::U8(r) => { /* ... */ }
            Row::U16(r) => { /* ... */ }
        }
    }
}
dec.flush();                           // end of stream: output the remaining pictures
```

`send_nal` takes one NAL unit without start code; `reset` drops the DPB and any partial
picture. `Plane::raw()` gives zero-copy access to the backing plane and `write_raw`
appends u8 or little-endian u16 samples. `examples/decode_hevc.rs IN.hevc OUT.yuv|-|null`
writes planar YUV and one JSON line per picture.

## Features

| Feature | Default | Effect |
|---|---|---|
| `simd` | on | `std::simd` kernels for motion compensation, deblocking and SAO. Requires a nightly toolchain (`portable_simd`). |

Without `simd` (`--no-default-features`) the crate builds on stable and uses the scalar
code, which produces identical output.

## Supported

| Area | Coverage |
|---|---|
| Profiles | Main, Main 10, Main Still Picture; 4:2:0, 8 and 10 bit |
| Parameter sets | VPS, SPS (VUI, HRD skipped), PPS, scaling lists incl. prediction |
| Slices | Dependent slice segments, tiles, wavefront parallel processing (decoded serially), list modification, entry points |
| Coding | CTB 16–64, AMP, transform skip, transquant bypass, sign data hiding, PCM |
| Prediction | Intra (planar, DC, 33 angular, strong smoothing, constrained intra); merge, AMVP, TMVP; default and explicit weighted prediction |
| Loop filters | Deblocking and SAO with slice/tile boundary control |
| References | Short-term RPS with inter-RPS prediction, long-term references, CRA/BLA/IDR, RASL/RADL handling, missing-reference generation |
| Output | DPB bumping (reorder, latency, buffering limits), `no_output_of_prior_pics`, `pic_output_flag`, conformance-window cropping, VUI SAR and colour description |

## Behaviour and limits

- `Picture` carries cropped `width`/`height`, `bit_depth`, `chroma`, three `Plane`s
  sharing the decoder's frame buffers, `pts`, `poc`, `sar` and `colour`. Each picture
  keeps the timestamp of the access unit that carried it through reordering.
- Errors are typed: `Unsupported(&str)` names the tool, `InvalidData(&str)`,
  `ResourceLimit(&str)`, `MissingParameterSet(&str)`. The decoder does not panic on
  malformed input.
- Refused with `Unsupported`: monochrome, 4:2:2, 4:4:4, separate colour planes, bit depth
  above 10, unequal luma and chroma bit depth, every SPS range-extension tool, PPS
  range-extension tools under RExt profiles, SCC extensions.
- NAL units with `nuh_layer_id > 0` are skipped, so the base layer of a multi-layer
  stream decodes. PPS extension bits are ignored for Main profiles. SEI is ignored
  (decoded picture hashes are not checked).
- RASL pictures after a CRA at stream start or after end of sequence are neither decoded
  nor output, as in FFmpeg.
- Limits: picture size capped at level 6.2 (35,651,584 luma samples, 16,888 per side)
  before allocation; DPB size from `sps_max_dec_pic_buffering` (at most 16).
- A slice error abandons the rest of the picture; what was decoded is still output.
  Behaviour on corrupt streams is not compared with FFmpeg's concealment.

## Conformance

- Specification: Rec. ITU-T H.265 (V11, 01/2026). Tables are transcribed or regenerated
  from the text with the table or equation number in a comment.
- Reference: FFmpeg 9.0.2 raw output on the FATE `hevc-conformance` suite and on
  x265-encoded 8- and 10-bit streams. Every output picture must be bit-exact, with
  identical count and order; out-of-set vectors must be refused. Both the `simd` and
  the scalar build are checked.
- `cargo test -p triergon-codec-hevc` runs unit tests (CABAC, transforms against the
  direct matrix product, scans); `tests/conformance.rs` compares MD5s against
  `tests/ffmpeg_ref_md5.txt` when `.research/m1d4/fate-suite/hevc-conformance/` is
  present (a subset by default, all with `HEVC_CONFORMANCE_FULL=1`) and skips otherwise.
- Comparison scripts: [harness/codecs/hevc](../../harness/codecs/hevc/STATUS.md).
  Project-wide gates: [docs/quality.md](../../docs/quality.md).

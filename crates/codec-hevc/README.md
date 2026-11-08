# triergon-codec-hevc

H.265/HEVC decoder for Annex B byte streams, written from Rec. ITU-T H.265 (V11, 01/2026).
It is a codec-layer crate that depends on [triergon-bitstream](../bitstream/README.md) and [triergon-codec](../codec/README.md)
(it implements `VideoDecoder`). It decodes the Main,
Main 10 and Main Still Picture profiles (4:2:0, 8 and 10 bit) to cropped pictures in
output order, single-threaded. It does not scale, convert colour or conceal errors.

## Usage

```rust
use triergon_codec_hevc::{AccessUnitSplitter, DecodeError, Decoder, Row};

fn decode_stream(stream: &[u8]) -> Result<(), DecodeError> {
    let mut dec = Decoder::new();
    for au in AccessUnitSplitter::new(stream) {
        dec.send(au, None)?;               // one Annex B access unit, opaque Option<i64> pts
        while let Some(pic) = dec.receive() {
            match pic.planes[0].row(0) {   // Row::U8 for 8-bit, Row::U16 for 10-bit
                Row::U8(r) => { /* ... */ }
                Row::U16(r) => { /* ... */ }
            }
        }
    }
    dec.flush();                           // end of stream: output the remaining pictures
    while let Some(pic) = dec.receive() { /* ... */ }
    Ok(())
}
```

`send_nal` takes one NAL unit without start code; `reset` drops the DPB and any partial
picture. `Decoder::with_limits(Limits { live_storage_bytes })` sets the storage budget (it
fails only for a budget below a pool's own charge); `Decoder::new_in` charges a caller's
`Budget` instead. `Plane::raw()` gives zero-copy access to the backing plane and `write_raw`
appends u8 or little-endian u16 samples. `examples/decode_hevc.rs IN.hevc OUT.yuv|-|null`
writes planar YUV and one JSON line per picture.

## Features

| Feature | Default | Effect |
|---|---|---|
| `simd` | off | `std::simd` kernels for motion compensation, deblocking and SAO. Requires a nightly toolchain (`portable_simd`); `cargo xtask check-simd` uses the pinned one. |

Without `simd` the crate builds on stable and uses the scalar code, which produces
identical output.

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
  `ResourceLimit(&str)`, `MissingParameterSet(&str)`, `Budget(StorageError)`. The decoder
  does not panic on malformed input.
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
- Storage: decoded and reference pictures (also while a caller holds a `Picture`), the
  frame pool, collocated motion, per-picture block and CTB maps, parameter sets, tile and
  scan tables, the SAO band buffer and the RBSP of the NAL unit being parsed are charged
  to the budget before they are allocated; a refused charge is `DecodeError::Budget`,
  the picture it was for is not started, and decoding resumes once the storage fits. A frame returns to the pool (one spare per bit depth) when the last picture
  showing it drops; the maps follow the picture size in both directions. The default, 1.25 GiB, holds the largest picture the size cap accepts
  (8192×4352, 107 MB in 10-bit storage) in the 6-picture DPB level 6.2 allows at that
  size (A.4.2), plus the frame pool and two held output pictures: nine frames, 963 MB,
  and about 70 MB of collocated motion, maps and tables, which leaves about 30% for slice
  data. A 2160p Main 10 stream with a full 16-picture DPB needs about 0.5 GB.
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
  `tests/ffmpeg_ref_md5.txt` when `.research/fate-samples/hevc-conformance/` is
  present (a subset by default, all with `HEVC_CONFORMANCE_FULL=1`) and skips otherwise.
- Comparison scripts: [harness/codecs/hevc](../../harness/codecs/hevc/STATUS.md).
  Project-wide gates: [docs/quality.md](../../docs/quality.md).

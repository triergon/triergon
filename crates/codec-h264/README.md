# triergon-codec-h264

H.264/AVC decoder for Annex B byte streams, written from Rec. ITU-T H.264 (08/2024). It
is a codec-layer crate whose only dependency is [triergon-codec](../codec/README.md)
(it implements `VideoDecoder`) and builds on stable Rust. It
decodes 8-bit 4:2:0 (and 4:0:0) Constrained Baseline, Main and High streams to cropped
pictures in output order. It does not scale, convert colour or conceal damaged slices.

## Usage

```rust
use triergon_codec_h264::Decoder;

let mut dec = Decoder::new();
for (au, pts) in access_units {
    dec.send(au, pts)?;                // one Annex B access unit, opaque Option<i64> pts
    while let Some(pic) = dec.receive() {
        // pic.width, pic.height, pic.planes[0].row(y), pic.pts, pic.sar, pic.colour
    }
}
dec.flush();                           // end of stream: output the remaining pictures
while let Some(pic) = dec.receive() { /* ... */ }
```

`send_nals` accepts Annex B data without an implied access-unit boundary; `send_nal`
takes one NAL unit without start code. `reset` drops the DPB and keeps parameter sets.
`examples/decode_h264.rs IN.264 OUT.yuv` writes raw yuv420p and one JSON line per picture.

## Supported

| Area | Coverage |
|---|---|
| Profiles | Constrained Baseline, Main, High; Extended-profile streams that use only these tools |
| Format | 8-bit 4:2:0; 4:0:0 output with chroma planes filled with 128 |
| Structure | Frames, field pictures (PAFF), MBAFF; multiple slices; I, P and B slices |
| Entropy | CAVLC, CABAC |
| Prediction | Intra 4x4/8x8/16x16, constrained intra; spatial and temporal direct; default, explicit and implicit weighted prediction |
| Transform | 4x4 and 8x8, scaling matrices (SPS and PPS, fall-back rules A/B) |
| References | POC types 0/1/2, list modification, sliding window, all MMCO operations, long-term references, frame_num gaps |
| Output | DPB bumping (level limits, `max_dec_frame_buffering`, `max_num_reorder_frames`), cropping, VUI SAR and colour description |

## Behaviour and limits

- `Picture` carries cropped `width`/`height`, three `Plane`s that share the decoder's
  frame buffer (no copy; `row(y)`, `data()`, `raw()`), `pts`, `poc`, `sar`, `colour` and
  `interlaced`. Each picture keeps the timestamp of the access unit that carried it
  through reordering.
- Errors are typed: `Unsupported(&str)` names the feature, `InvalidData(&str)`,
  `ResourceLimit(&str)`. The decoder does not panic on malformed input.
- Refused with `Unsupported`: bit depth above 8, 4:2:2 and 4:4:4, lossless transform
  bypass, flexible macroblock ordering (more than one slice group), SP/SI slices, data
  partitioning.
- Skipped: MVC/SVC slice extensions (NAL 20/21) and prefix/subset SPS (NAL 14/15), so the
  base view decodes. Redundant slices are dropped.
- Limits: at most 1024 macroblocks per side; DPB size from the level or
  `max_dec_frame_buffering`.
- A damaged slice returns `InvalidData`; its macroblocks stay undecoded where FFmpeg
  would conceal them. An unpaired field is output as a frame with the other field's lines
  unset.

## Conformance

- Specification: Rec. ITU-T H.264 (V15, 08/2024). Tables are transcribed from the text
  with the table number in a comment; Table 9-26 prints ctxIdx 641 where 541 is meant.
- Reference: FFmpeg 9.0.2 (`-threads 1 -f framemd5 -pix_fmt yuv420p -fps_mode
  passthrough`) on the FATE `h264-conformance` suite. Every output picture must be
  bit-exact, with identical count and order; out-of-set vectors must be refused.
- `cargo test -p triergon-codec-h264` runs unit tests; `tests/conformance.rs` checks all
  vectors against `tests/ffmpeg_framemd5_digests.txt` when
  `.research/fate-samples/h264-conformance/` is present (`H264_VECTOR` selects one)
  and skips otherwise. `examples/fuzz.rs` mutates the vectors to check for panics.
- Project-wide gates: [docs/quality.md](../../docs/quality.md).

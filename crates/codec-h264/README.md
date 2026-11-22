# triergon-codec-h264

H.264/AVC decoder for Annex B byte streams, written from Rec. ITU-T H.264 (08/2024). It
is a codec-layer crate that depends on [triergon-bitstream](../bitstream/README.md),
[triergon-codec](../codec/README.md) (it implements `VideoDecoder`) and
[triergon-framing](../framing/README.md) (Annex B units and the decoded picture buffer),
and builds on stable Rust. It decodes 8-bit 4:2:0 (and 4:0:0) Constrained Baseline, Main
and High streams to cropped pictures in output order. It does not scale, convert colour
or conceal damaged slices.

## Usage

```rust
use triergon_codec_h264::Decoder;

# let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../playback/tests/fixtures/raw-video/avc-80x48.h264");
let stream = std::fs::read(path)?;     // an Annex B elementary stream
let mut dec = Decoder::new();
let mut pictures = 0;
dec.send_nals(&stream, None)?;         // or send(au, pts) per access unit, opaque Option<i64> pts
while let Some(pic) = dec.receive() {
    // pic.width, pic.height, pic.planes[0].row(y), pic.pts, pic.sar, pic.colour
    pictures += 1;
}
dec.flush();                           // end of stream: output the remaining pictures
while let Some(_pic) = dec.receive() {
    pictures += 1;
}
assert!(pictures > 0);
# Ok::<(), Box<dyn std::error::Error>>(())
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
| Output | The decoded picture buffer of `triergon-framing`'s `picture_output`: removal (C.4.4) and bumping (C.4.5.3) while more frames are stored than the DPB size (level limit or `max_dec_frame_buffering`) or more than `max_num_reorder_frames` wait; cropping, VUI SAR and colour description |

## Behaviour and limits

- `Picture` carries cropped `width`/`height`, three `Plane`s that share the decoder's
  frame buffer (no copy; `row(y)`, `data()`, `raw()`), `bit_depth`, `pts`, `poc`, `sar`,
  `colour` and `interlaced`. Each picture keeps the timestamp of the access unit that
  carried it through reordering.
- Errors are typed: `Unsupported(&str)` names the feature, `InvalidData(&str)`,
  `ResourceLimit(&str)`, `Budget(StorageError)`. The decoder does not panic on
  malformed input.
- Refused with `Unsupported`: bit depth above 8, 4:2:2 and 4:4:4, lossless transform
  bypass, flexible macroblock ordering (more than one slice group), SP/SI slices, data
  partitioning.
- Skipped: MVC/SVC slice extensions (NAL 20/21) and prefix/subset SPS (NAL 14/15), so the
  base view decodes. Redundant slices are dropped.
- Limits: at most 1024 macroblocks per side; DPB size from the level or
  `max_dec_frame_buffering`.
- Output order: after a picture is decoded and marked, an IDR picture or
  `memory_management_control_operation` 5 outputs every prior picture (or discards them
  with `no_output_of_prior_pics_flag`), the picture is stored, and the picture waiting
  with the lowest order count is output while the condition above holds. A first field
  is not output before its second field joins it, or before the next picture shows that
  none comes; pictures with equal order counts leave in decode order. A frame store is
  free once its picture is output and unused for reference; at most three more stores
  than the DPB size are made, and when every one holds a picture, pictures are output
  until one is free (a buffer of reference pictures none of which waits is
  `ResourceLimit`).
- Storage: frame buffers (also while output pictures share them), motion and macroblock
  arrays, the decoded picture buffer, deblocking state, RBSP and slice lists are charged
  to a `Budget` before they are allocated. `Decoder::with_limits(Limits {
  live_storage_bytes })` gives the decoder its own pool (320 MiB by default, which covers
  a level 5.2 DPB plus five more 36864-macroblock frames, about 250 MiB) and
  `Decoder::new_in(&budget)` charges a caller's pool. A refused charge is
  `DecodeError::Budget`; that picture is not decoded and the decoder stays usable.
- A damaged slice returns `InvalidData`; its macroblocks stay undecoded where FFmpeg
  would conceal them. An unpaired field is output as a frame with the other field's lines
  unset.
- Scaling and inverse transforms wrap on overflow, as FFmpeg's `int` arithmetic does; a
  conforming stream never overflows.

## Standards

Rec. ITU-T H.264 (V15, 08/2024). Tables are transcribed from the text with the table
number in a comment; Table 9-26 prints ctxIdx 641 where 541 is meant. Verification:
[harness/codecs](../../harness/codecs/STATUS.md); results in
[docs/status.md](../../docs/status.md).

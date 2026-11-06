# D4 / O61 decoder implementation brief (shared by all codec lanes)

Workspace: the `matroska-d4-decoding` development worktree
(a git worktree; never commit, push, stash, or touch other worktrees, Streem, or the TV).

## What D4 replaces

Streem's desktop simulator pipes compressed access units (AUs) to an external `ffmpeg`
(read-only sources: `native-jelly-streem` personal/main `3d0fca40`
`rust-modules/src/player/sim_{video,audio}.rs`, upstream `3bc30f4e`
`rust-modules/media/src/player/sim_video.rs`):

- video: `ffmpeg -f h264|hevc -i pipe:0 -an -vf scale=1920:1080:force_original_aspect_ratio=decrease,pad=1920:1080:(ow-iw)/2:(oh-ih)/2,format=rgba -fps_mode passthrough -f rawvideo -pix_fmt rgba pipe:1`
  fed Annex-B AUs (parameter sets in-band at keyframes) plus caller PTS.
- audio: `ffmpeg -f ac3|eac3|aac|dts -i pipe:0 -vn -ac 2 -f pulse|alsa` fed self-delimiting
  AC-3 / E-AC-3 / ADTS-AAC / DTS-core frames plus caller PTS.

Each codec lane implements one pure-safe-Rust decoder crate. The session layer (queues,
clock, scaling/RGBA, stereo downmix, s16 conversion) is owned by the coordinator, not you.

## Hard rules

- `#![forbid(unsafe_code)]`; no third-party dependencies (the crate's `[dependencies]` stays
  empty); no FFI, C, assembly, build scripts, or external helper processes in the crate.
- Write an original implementation from the public primary specification. Do **not** read,
  copy or mechanically translate FFmpeg/libav/GPL or other framework decoder source bodies
  (FFmpeg source exists under `.research/` — do not open its `libavcodec` decoder files).
  You may use your own knowledge of the standards, the public spec PDFs, and observed output
  of the FFmpeg binary as a black-box oracle. Tables that the spec defines (Huffman/VLC
  tables, window coefficients, quantizer tables, etc.) must be transcribed from the spec
  (or regenerated from the spec's formula), with a comment naming the spec table.
- Only edit files under your own crate directory (`crates/codec-<name>/`) and your own notes
  file `research/d4/<name>/`. Do not edit the workspace `Cargo.toml`, `Cargo.lock`, other
  crates, `harness/`, or `tools/`. If you truly need a shared change, describe it in your
  final report instead.
- Use your own target dir to avoid lock contention: every cargo call is
  `CARGO_INCREMENTAL=0 CARGO_TARGET_DIR=target/agent-<name> cargo +nightly ... -p triergon-codec-<name>`.
- Mac results are development evidence only. Do not claim qualification, parity or
  performance gates; the coordinator runs native Linux ARM64 gates.
- Large media stays out of git: test vectors live under the gitignored
  `.research/m1d4/fate-suite/<dir>/` (FFmpeg FATE suite, downloaded; rights unreviewed, do not
  copy them into the crate). Tests that need them must skip cleanly (print a note) when the
  file is absent; keep small self-authored or synthetic unit tests inside the crate.

## Numeric contract (frozen before grading: `research/d4/NUMERIC-RULES.json`)

- Audio: exact sample rate, channel count/order/layout, frame/sample counts and output
  start (priming / codec delay) versus FFmpeg 9.0.2's float decoder. Per channel
  RMS(candidate - reference) <= 8.8e-6 and max |diff| <= 6.1035e-5 (full scale 1.0) over
  every sample; no gain fitting, delay search or exclusions.
- Video: bit-exact decoded Y/Cb/Cr at native bit depth for every output picture, exact output
  order/count, cropping, SAR and colour description. No tolerance.

Development oracle on this Mac: `/opt/homebrew/bin/ffmpeg` (9.0.2). Useful commands:

- audio planar float: `ffmpeg -v error -f ac3 -i IN -f f32le -c:a pcm_f32le OUT.f32` gives
  interleaved float in FFmpeg's native channel order; `ffprobe -show_streams` gives layout.
  Use `-af aformat=sample_fmts=flt` only if needed; never let ffmpeg resample or remix
  when building the reference for the decoder itself.
- video: `ffmpeg -v error -f h264 -i IN -f rawvideo -pix_fmt yuv420p OUT.yuv` (use
  `yuv420p10le` for 10-bit), `-fps_mode passthrough`; `framemd5`/`framecrc` muxers give per-frame
  hashes. FATE `tests/ref/fate/*` expectations can be regenerated this way.

## API shape (adapt names, keep semantics)

Audio crate:

```rust
pub struct Decoder { .. }
impl Decoder {
    pub fn new() -> Self;
    /// Decode exactly one self-delimiting access unit (one sync frame, or for ADTS one
    /// ADTS frame). Appends planar f32 output to `out`. Errors are typed, never panics.
    pub fn decode(&mut self, au: &[u8], out: &mut AudioFrame) -> Result<(), DecodeError>;
    /// Drop history (overlap/delay lines) after a seek/discontinuity.
    pub fn reset(&mut self);
}
pub struct AudioFrame { pub sample_rate: u32, pub layout: ChannelLayout, pub samples_per_channel: usize, pub planes: Vec<Vec<f32>> }
```

`ChannelLayout` must state each plane's speaker position (FL, FR, FC, LFE, BL/SL, BR/SR, ...)
so a downmix can be derived without guessing. Order the planes as FFmpeg's native decoder
does for that stream so comparisons are direct; document the mapping.

Video crate:

```rust
pub struct Decoder { .. }
impl Decoder {
    pub fn new() -> Self;
    /// Feed one Annex-B access unit with an opaque caller timestamp (may be None).
    pub fn send(&mut self, au: &[u8], pts: Option<i64>) -> Result<(), DecodeError>;
    /// Pop the next picture in output (display) order, if one is ready (DPB bumping rules).
    pub fn receive(&mut self) -> Option<Picture>;
    /// End of stream: make all remaining pictures available to `receive` (decoded EOF/drain).
    pub fn flush(&mut self);
    /// Seek/discontinuity: drop DPB, parameter sets are kept unless re-sent.
    pub fn reset(&mut self);
}
pub struct Picture { pub width, pub height (cropped), pub bit_depth, pub chroma (420), pub planes: [Plane;3] (u16 samples for >8 bit, u8 otherwise or a generic sample type), pub pts: Option<i64>, pub sar: (u32,u32), pub colour: ColourDescription { primaries, transfer, matrix, full_range } }
```

PTS must follow the picture through reordering (each AU's PTS belongs to the picture it
carries), not a sorted heap. Errors: distinguish unsupported feature (typed, with the
feature named), invalid data, and resource-limit refusals. Bound memory: cap DPB/frame sizes
by level limits and reject absurd dimensions before allocating.

## Performance

The gate is <= 1.25x FFmpeg's runtime for equivalent work on Linux ARM64 (target 0.80),
and lower heap and RSS. Design for that from the start: no per-sample/per-block heap
allocation in steady state, table-driven bit parsing, fast O(N log N) transforms, contiguous
planes, branch-light inner loops written so LLVM auto-vectorizes them. Video crates may use
`#![feature(portable_simd)]` (`std::simd`) for hot kernels (motion compensation, inverse
transforms, deblocking, SAO) with an equivalent scalar path; no `core::arch` pointer
intrinsics.

## Deliverable

1. Working decoder with unit tests and vector tests (skipping cleanly when vectors absent).
2. A small example binary `crates/codec-<name>/examples/decode.rs` that decodes an
   elementary-stream file to raw output (audio: interleaved f32le in plane order; video:
   raw planar YUV, little-endian u16 for >8-bit) so the coordinator can compare it natively.
   For audio, split the stream into AUs with your own sync/frame-length parsing in the example.
3. `research/d4/<name>/STATUS.md`: implemented feature list against the spec, the exact
   selected feature matrix you cover and what you do NOT cover (each named), every vector
   you ran with its result (pass/fail with measured RMS/max or first differing frame),
   development timing vs `ffmpeg` on this Mac (clearly labelled non-evidence), and gaps.
4. Final message: concise summary of the above. Report failures honestly; a passing subset
   is not completion.

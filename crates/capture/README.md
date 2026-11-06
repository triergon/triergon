# triergon-capture

Encodes captured RGBA frames into an MPEG-1 video MPEG-TS stream: RGBA →
YUV 4:2:0 conversion, an MPEG-1 video encoder with I and P pictures, and a
single-program TS muxer. It is a standalone crate with no dependencies,
written from ITU-R BT.601, ISO/IEC 11172-2 (via ITU-T H.262's MPEG-1
syntax) and ITU-T H.222.0. Framebuffer readback, sockets and session rebuild
policy belong to the caller.

## Usage

```rust
use triergon_capture::{Orientation, Session, SessionConfig, ts::MuxConfig};

let b = 2_500_000;
let mut session = Session::open(&SessionConfig {
    width: 960, height: 540,
    bit_rate: b, max_rate: b * 7 / 5, vbv_bits: b / 7, gop: 30,
    mux: MuxConfig::default(),
})?;
let mut out = Vec::new();                       // any `Sink`
let report = session.encode(&rgba, Orientation::BottomUp, &mut out)?;
// `out` holds this frame's complete TS bytes; report.picture.key, .bits
```

Each component is also usable alone: `rgba_to_yuv420` / `convert::Converter`,
`mpeg1::Encoder` with `EncoderConfig`, and `ts::Muxer` with `MuxConfig`.

## Supported

| Stage | Behaviour |
|---|---|
| Conversion | BT.601 matrix, limited range (Y 16–235, Cb/Cr 16–240), alpha ignored. Chroma: mean of each horizontal pixel pair, then centre-sited vertical taps 1/8, 3/8, 3/8, 1/8 over rows 2k−1…2k+2 with edge rows replicated. Top-down or bottom-up input (`Orientation`), read without a separate flip pass |
| Encoder | ISO/IEC 11172-2 I and P pictures, no B. Closed GOP of `gop` pictures with sequence and GOP headers before every I picture. 30 Hz, square pixels. One slice per picture, with per-macroblock quantiser changes. Full- and half-sample forward motion compensation. Default intra matrix, flat non-intra matrix, qscale 2–31. Scene-change I pictures restart the GOP. VBV-constrained rate control |
| Muxer | 188-byte TS. PAT, PMT and a DVB SDT (ETSI EN 300 468 service descriptor). One PES per picture (stream_id 0xE0) with PTS and DTS. PCR = DTS·300 on every PES start, random-access indicator on key pictures. SDT every 500 ms; PAT/PMT every 100 ms and before a key picture that follows a non-key picture. Stuffing in the final packet's adaptation field |
| Session | First output is SDT, PAT, PMT and a sequence-header I picture. PTS = 3000·(k+1), DTS = 3000·k on the 90 kHz clock |

`MuxConfig::default()`: transport stream 1, original network 0xFF01, program
1, PMT PID 0x1000, video PID 0x0100, stream type 0x01, provider "Triergon",
service "Service01".

## Behaviour and limits

- Geometry must be even, non-zero and at most 4095 in each dimension
  (`CaptureError::Geometry`). RGBA input shorter than `width × height × 4` is
  `CaptureError::InputLength`.
- `max_rate` must fit the 18-bit sequence-header field in 400 bit/s units;
  zero rates or VBV size are `CaptureError::BitRate`. `gop` must be at least 1.
- Every `encode` call returns that frame's complete TS bytes immediately; the
  encoder has no reordering delay. Re-encoding the same picture (keepalive)
  still advances PTS, DTS, PCR and continuity counters.
- If the `Sink` refuses or partially writes a frame, the session is poisoned
  and later calls return `CaptureError::Closed`; open a new session.
- Service names must be printable ASCII and fit one SDT packet. PIDs must be
  distinct and outside reserved values; stream type 0x01 or 0x02 only.
- Planes and output buffers are plain `Vec`s owned by the session and reused
  across frames.

### Differences from FFmpeg

| Item | FFmpeg 9.0.2 | This crate | Reason |
|---|---|---|---|
| Latency | frame k's bytes are written at call k+1; the last frame is never written | frame k at call k | lower latency, same timestamps |
| PMT stream type | 0x02 | 0x01 by default, 0x02 selectable | H.222.0 Table 2-34 reserves 0x02 for constrained-parameter 11172-2 video |
| SDT provider | "FFmpeg" | "Triergon" by default, selectable | do not claim another implementation's identity |

## Conformance

The reference is FFmpeg 9.0.2 (`swscale` with `SWS_BILINEAR`, `mpeg1video`
with I/P GOP 30, `mpegts`). The contract and required cases live in
`research/capture/contract.json` and `research/capture/cases.json`; the
harness in `harness/capture/` applies these rules per case:

- **Conversion:** every Y, Cb, Cr sample within ±1 of the reference.
- **Mux:** independent re-demux (FFmpeg and an own TS walker): same PIDs,
  tables (subject to the differences above), PES payload equal to the
  elementary stream, identical per-frame PTS/DTS/PCR, valid continuity and
  table cadence.
- **Encoder:** error-free independent FFmpeg decode of every frame, identical
  picture types, VBV underflow no worse than the reference, and PSNR-Y within
  0.25 dB of the reference at equal or fewer bits. When the reference
  overspends, the encoder is re-run at 1.25–4.5× the requested rate and
  PSNR-Y is interpolated at the reference's bit count.

Unit and integration tests cover the coefficients, PSI bytes and CRCs, PID
and name validation, timing, keepalive, sink failure and bottom-up input.
`src/bin/capture-worker.rs` exposes the same command line as the reference
worker (`convert`, `pipeline`, `recon`, `time`, `memory`). Gates:
[docs/quality.md](../../docs/quality.md).

# triergon-capture

A capture session: RGBA frames in, MPEG-1 video in an MPEG-TS stream out, one call per
frame. It is a composition-layer crate that wires the BT.601 converter of
[triergon-pixel](../pixel/README.md), the encoder of
[triergon-codec-mpeg1](../codec-mpeg1/README.md) and the single-program writer
[`triergon_mpegts::mux`](../mpegts/README.md#writing-ts) together with continuous
timing. Framebuffer readback, sockets and session rebuild policy belong to the
caller.

## Usage

```rust
use triergon_capture::{Session, SessionConfig, pixel::Orientation};

let mut session = Session::open(&SessionConfig::with_rate_rule(320, 180, 500_000))?;
let rgba = vec![0u8; 320 * 180 * 4];           // one frame from the caller
let mut out = Vec::new();                       // any `Sink`
let report = session.encode(&rgba, Orientation::BottomUp, &mut out)?;
// `out` holds this frame's complete TS bytes; report.picture (EncodedPicture), report.ts_bytes
assert!(report.picture.key && out.len() == report.ts_bytes && out.len() % 188 == 0);
# Ok::<(), triergon_capture::CaptureError>(())
```

`SessionConfig` holds `width`, `height`, `bit_rate`, `max_rate`, `vbv_bits`, `gop` and
`mux` (a `mux::MuxConfig`). `SessionConfig::with_rate_rule(width, height, b)` sets
`max_rate = b·7/5`, `vbv_bits = b/7`, `gop = 30` and the default multiplex. The
component crates are re-exported as `pixel`, `mpeg1` and `mux`. `Session` also gives
`geometry()`, `converted()` (the last converted input), `reconstruction()` and
`vbv_underflow_bits()`.

## Supported

| Stage | Behaviour |
|---|---|
| Conversion | `triergon_pixel::Converter`: BT.601, limited range, top-down or bottom-up input |
| Encoding | `triergon_codec_mpeg1::Encoder`: I and P pictures, sequence header before every I picture, 30 Hz |
| Multiplex | `triergon_mpegts::mux::Muxer`: PAT, PMT, DVB SDT, one PES per picture |
| Timing | Picture k has PTS = 3000·(k+1) and DTS = 3000·k on the 90 kHz clock; PCR = DTS·300 |
| Start | The first output is SDT, PAT, PMT and a sequence-header I picture |

## Behaviour and limits

- Every `encode` call hands that frame's complete TS bytes to the `Sink` before it
  returns. Re-encoding the same picture (a keepalive) still advances PTS, DTS, PCR
  and continuity counters.
- Errors are explicit and nothing is retried:

  | `CaptureError` | Cause |
  |---|---|
  | `Pixel` | geometry not even, zero or above 4094; RGBA shorter than `width × height × 4` |
  | `Encoder` | rate or intra period outside the encodable range |
  | `Mux` | a `MuxConfig` field the writer cannot represent (names, PIDs, program number, stream type) |
  | `Sink` | the sink refused or partially wrote the frame (`Sink::send` returned `false`) |
  | `Closed` | an earlier `Sink` failure poisoned the session; open a new one |

- Planes and output buffers are plain `Vec`s owned by the session and reused across
  frames.

### Differences from FFmpeg

| Item | FFmpeg 9.0.2 | This crate | Reason |
|---|---|---|---|
| Latency | frame k's bytes are written at call k+1; the last frame is never written | frame k at call k | lower latency, same timestamps |

Multiplex differences (PMT stream type, SDT provider) are listed under
[Writing TS](../mpegts/README.md#writing-ts).

## Conformance

The reference is FFmpeg 9.0.2 (`swscale` with `SWS_BILINEAR`, `mpeg1video` with an
I/P GOP of 30, `mpegts`) driven by the capture build of `harness/reference.c`. The
contract and required cases live in `harness/capture/contract.json` and
`harness/capture/cases.json`; `harness/capture/` runs both pipelines and applies
the per-stage rules documented in [triergon-pixel](../pixel/README.md#conformance)
and [triergon-codec-mpeg1](../codec-mpeg1/README.md#conformance). For the multiplex,
an independent re-demux (FFmpeg and an own TS walker) must find the same PIDs and
tables (subject to the documented differences), PES payloads equal to the
elementary stream, identical per-frame PTS, DTS and PCR, valid continuity and the
table cadence. The harness measures peak live heap and total resident peak RSS
with the same input and checked output for each worker.

`tests/session.rs` covers the start sequence and timing, keepalive, sink failure,
bottom-up input, geometry and rate extremes, and error mapping.
[capture-worker](../capture-worker/README.md) runs a session from the harness command
line. Gates: [docs/quality.md](../../docs/quality.md).

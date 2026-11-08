# codec-worker

Decoder measurement worker for the harness. It decodes an elementary stream through
the [triergon-session](../session/README.md) decoders, letterboxes video with
[`triergon_pixel::letterbox`](../pixel/README.md) and downmixes audio with
`triergon_session::mix::Downmix`, performing the same operations as the FFmpeg
reference decode worker in `harness/`. It is tooling (`publish = false`), not part of
the library.

## Command line

```text
codec-worker KIND-CODEC-bench  FILE ITERATIONS
codec-worker KIND-CODEC-memory FILE
codec-worker KIND-CODEC-raw    FILE OUT
codec-worker packets FORMAT FILE
```

`KIND-CODEC` is one of `video-h264`, `video-hevc`, `audio-ac3`, `audio-eac3`,
`audio-aac`, `audio-dts`, `audio-flac`, `audio-mp3`, `audio-vorbis`, `audio-opus`. The `simd` feature enables the HEVC decoder's nightly-only
SIMD kernels; output is identical.

Video is split into Annex B access units and audio into frames (AC-3/E-AC-3 by
`access_unit_len`, AAC by the ADTS header, DTS by the core FSIZE) the same way the
reference worker's parser does, so both decode the same units. This is measurement
framing; playback frames access units through `triergon-framing`. Decode errors are
printed to stderr and decoding continues.

## Outputs

The converted output is 1920×1080 RGBA per picture or interleaved stereo s16le per
audio frame. `packets` counts pictures or audio frames.

| Mode | stdout |
|---|---|
| `bench` | `{"schema":1, "iterations", "elapsed_ns", "packets", "bytes", "digest"}`; reading the file is outside the timed interval |
| `memory` | `TRIERGON_BEGIN_V1` and `TRIERGON_END_V1` around one run, then `{"packets", "bytes"}`; the digest goes to stderr |
| `raw` | writes the converted output to `OUT`, then `{"packets", "bytes", "digest"}` |

`digest` is `triergon_harness_protocol::interleaved::InterleavedDigest` over the
exact converted output: a fingerprint of equal output, not a cryptographic hash.

## Reader packets

`packets FORMAT FILE` prints one JSON line per packet a container reader emits, for
`cargo xtask codecs readers`: `ogg` reads through `triergon_ogg::BlockingReader`, `flac`,
`mp3`, `aac`, `ac3` and `eac3` through the `triergon_elementary` reader of that format.
Each line holds the track index, `time_base`, `pts`, `dts`, `duration`, `trim_start` and
`trim_end` as the reader reports them, the payload `size`, the source `position` (Ogg:
the page that completes the packet; elementary streams: the first byte) and `complete`.
Ogg packets also carry `compat`, the reader's FFmpeg-compatible view
(`triergon_ogg::compat::clock`), and the reader's `resync` and `lost` events are lines
of their own. The last line lists the tracks and how reading ended (`eof`, or `error` with the
reader's message).

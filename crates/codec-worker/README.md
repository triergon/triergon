# codec-worker

Decoder measurement workers for the harness (`publish = false`, not part of the library).
They decode an elementary stream through the [triergon-session](../session/README.md)
decoders, letterbox video with [`triergon_pixel::letterbox`](../pixel/README.md) and
downmix audio with `triergon_session::mix::Downmix`, performing the same operations as the
FFmpeg reference decode worker in `harness/`. `codec-worker` serves every operation below;
`codec-aac-worker` serves the AAC decode operations alone (`audio-aac`,
`audio-aac-access-units` and `audio-aac-access-units-compat`).

## Command line

```text
codec-worker KIND-CODEC-bench  FILE ITERATIONS
codec-worker KIND-CODEC-memory FILE
codec-worker KIND-CODEC-raw    FILE OUT
codec-worker KIND-CODEC-fold   FILE
codec-worker packets FORMAT FILE
codec-worker packets mov FILE [TRACK_ID@MICROSECONDS]
codec-worker seek ogg FILE TRACK SECONDS exact|preroll
```

`KIND-CODEC` is one of `video-h264`, `video-hevc`, `audio-ac3`, `audio-eac3`,
`audio-aac`, `audio-aac-access-units`, `audio-aac-access-units-compat`, `audio-dts`,
`audio-flac`, `audio-mp3`, `audio-vorbis`, `audio-opus` or `audio-opus-ambisonic`.
The nightly-only `simd` and `aac-simd` features enable the HEVC and AAC decoders'
SIMD kernels. Decode errors are printed to stderr. Ambisonic decode errors stop
the workload.

The `audio-aac-access-units` inputs, made by `cargo xtask codecs aac-access-units --out
DIR`, start with four little-endian 32-bit words (sample rate, channel count, packet
count, configuration length), then the AudioSpecificConfig, then each packet with a
little-endian 32-bit length and 64 zero padding bytes; at most 64 MiB, and every packet
must give 1024 samples per plane. `audio-aac-access-units` uses the specification channel
mapping, `-compat` the conventional surround mapping.

## Outputs

The output is 1920×1080 RGBA per picture, interleaved stereo s16le per mixed audio frame,
all native PCM planes per configured AAC frame, or interleaved f32le ACN channels for `audio-opus-ambisonic`. The latter accepts 48 kHz first-order Ambisonics with or without non-diegetic stereo and counts decoded nonempty frames. `packets` counts pictures or audio
frames.

| Mode | stdout |
|---|---|
| `bench` | `{"schema":1, "iterations", "elapsed_ns", "packets", "bytes", "digest"}`; reading the file is outside the timed interval |
| `memory` | `TRIERGON_BEGIN_V1` and `TRIERGON_END_V1` around one run, then `{"packets", "bytes"}`; the digest goes to stderr |
| `raw` | writes the converted output to `OUT`, then `{"packets", "bytes", "digest"}` |
| `fold` | `{"packets", "bytes", "digest"}` of one untimed run |

`digest` is `triergon_harness_protocol::interleaved::InterleavedDigest` over the exact
converted output: a fingerprint of equal output, not a cryptographic hash.

## Reader packets

- `packets FORMAT FILE` prints one JSON line per packet a container reader emits (`ogg`
  through `triergon_ogg::BlockingReader`; `flac`, `mp3`, `aac`, `ac3` and `eac3` through
  the `triergon_elementary` reader): track index, `time_base`, `pts`, `dts`, `duration`,
  `trim_start`, `trim_end`, `size`, source `position` and `complete`. Ogg packets also
  carry `compat` (`triergon_ogg::compat::clock`), and `resync` and `lost` events are lines
  of their own. The last line lists the tracks and how reading ended (`eof`, or `error`
  with the reader's message).
- `seek ogg FILE TRACK SECONDS POLICY` seeks track TRACK to SECONDS (`exact`:
  `SeekPolicy::PreviousRandomAccess`; `preroll`: `SeekPolicy::PlaybackPreroll`), prints the
  receipt as a `{"event":"seek",...}` line and then every packet to the end as `packets`
  does.
- `packets mov FILE [TRACK_ID@MICROSECONDS]` reads through `triergon_isobmff::Reader`:
  each line holds the track index and `track_id`, `time_base`, `pts`, `dts`, `duration`,
  `size`, `position`, `sync`, the payload's MD5 as `hash`, `ordinal` and `description`;
  a seek is requested once the movie is open and its receipt is a line of its own.

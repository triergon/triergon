# codec-worker

Decoder measurement workers for the harness. It decodes an elementary stream through
the [triergon-session](../session/README.md) decoders, letterboxes video with
[`triergon_pixel::letterbox`](../pixel/README.md) and downmixes audio with
`triergon_session::mix::Downmix`, performing the same operations as the FFmpeg
reference decode worker in `harness/`. It is tooling (`publish = false`), not part of
the library. `codec-worker` serves every operation below. `codec-aac-worker` serves
the AAC decode operations alone (`audio-aac`, `audio-aac-access-units` and
`audio-aac-access-units-compat`), so that a build of the AAC decoder with other
features or another compiler measures nothing else: the gate build builds it with the
`aac-simd` feature (the AAC decoder's `simd`) by the pinned nightly.

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
`audio-aac`, `audio-aac-access-units`, `audio-aac-access-units-compat`, `audio-dts`, `audio-flac`, `audio-mp3`,
`audio-vorbis` or `audio-opus`. The nightly-only `simd` and `aac-simd` features enable
the HEVC and AAC decoders' SIMD kernels; output is identical.

Video is split into Annex B access units and audio into frames (AC-3/E-AC-3 by
`access_unit_len`, AAC by the ADTS header, DTS by the core FSIZE) the same way the
reference worker's parser does, so both decode the same units. This is measurement
framing; playback frames access units through `triergon-framing`. FLAC, MP3, Vorbis and
Opus files are read sequentially through their container reader and the session's track
decoder, as the reference reads them through its demuxer. Decode errors are
printed to stderr and decoding continues.

For configured AAC, `cargo xtask codecs aac-access-units --out DIR` extracts the
original packets and AudioSpecificConfig with the pinned `ffprobe`, and compares
every sample of the `raw` output (little-endian floats in frame and plane order,
with the ordered speaker IDs) with the AAC accuracy bounds. Each input starts with
four little-endian 32-bit words (sample rate, channel count, packet count,
configuration length), then the configuration, then each packet with a
little-endian 32-bit length and 64 zero padding bytes; it is capped at 64 MiB and
every packet must give 1024 samples per plane. `audio-aac-access-units` uses the
specification channel mapping, `audio-aac-access-units-compat` the conventional
surround mapping. `cargo xtask codecs time` and `memory` measure an operation from
`codec-aac-worker` given as `--candidate-for OPERATION=W`, and admit these
workloads only with the matching `parity.json`.

## Outputs

The output is 1920×1080 RGBA per picture, interleaved stereo s16le per mixed
audio frame, or all native PCM planes per configured AAC frame. `packets` counts
pictures or audio frames.

| Mode | stdout |
|---|---|
| `bench` | `{"schema":1, "iterations", "elapsed_ns", "packets", "bytes", "digest"}`; reading the file is outside the timed interval |
| `memory` | `TRIERGON_BEGIN_V1` and `TRIERGON_END_V1` around one run, then `{"packets", "bytes"}`; the digest goes to stderr |
| `raw` | writes the converted output to `OUT`, then `{"packets", "bytes", "digest"}` |
| `fold` | `{"packets", "bytes", "digest"}` of one untimed run |

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

`seek ogg FILE TRACK SECONDS POLICY` opens an Ogg file the same way, seeks track TRACK
(its index once the reader is open) to SECONDS (a decimal number) with POLICY (`exact`:
`SeekPolicy::PreviousRandomAccess`; `preroll`: `SeekPolicy::PlaybackPreroll`), prints the
receipt as a `{"event":"seek",...}` line (`requested`, `landing` and `discard_until` as
`[numerator, denominator]` seconds, the method and the bytes scanned) and then every
packet to the end of the file as `packets` does, Ogg packets that end a page with their
`granule`. `cargo xtask codecs readers --seeks` compares it with FFmpeg.

`packets mov FILE [TRACK_ID@MICROSECONDS]` reads through the neutral ISO BMFF reader
(`triergon_isobmff::Reader`), for `cargo xtask codecs isobmff-reader`. Each line holds
the track index and `track_id`, `time_base`, `pts`, `dts`, `duration`, the payload
`size`, the sample's source `position`, `sync` (1, 0, or `null` where the source states
nothing), the payload's MD5 as `hash`, the sample `ordinal` and its `description`.
With a seek, `request_seek` asks for that track's time once the movie is open and the
receipt is a line of its own (`{"event":"seek", ...}`). The last line lists the tracks,
with their edit lists and the movie timescale, and how reading ended.

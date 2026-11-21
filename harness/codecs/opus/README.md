# Opus family 2 comparison

`family-2-acn.opus` is an authored 0.1-second Ogg Opus probe, encoded with
FFmpeg 9.0.2 and libopus. It is included as the comparison input; its SHA-256 is
recorded in the [correctness manifest](../correctness-manifest.json). Its OpusHead
has four channels, family 2, four uncoupled streams, mapping `[0, 1, 2, 3]`, 48 kHz input rate and 312 samples of
pre-skip. The channels are first-order SN3D Ambisonics in ACN order.

`family-2-stereo-gain.opus` is reproducibly generated with FFmpeg 9.0.2 and libopus 1.6.1 using
`CARGO_INCREMENTAL=0 cargo xtask codecs opus-control --ffmpeg ffmpeg --out harness/codecs/opus/family-2-stereo-gain.opus`. It encodes six distinct tones and
changes the header to six channels, family 2, mapping `[0, 1, 2, 3, 5, 4]`
and +6 dB output gain (1536 in Q7.8). Four ACN components precede the
non-diegetic left/right pair. The trailing stereo planes carry distinct signals in reversed stream order.
The manifest pins both authored files.

Build `decode_audio` with `CARGO_INCREMENTAL=0 cargo build --release -p
triergon-session --example decode_audio --features codecs`, and place it at
`DIR/audio-decode`. With the pinned FFmpeg decode build:

```sh
CARGO_INCREMENTAL=0 cargo xtask codecs grade --manifest harness/codecs/correctness-manifest.json \
  --lanes opus-ambisonic --root . --bin DIR --ffmpeg FFMPEG --ffprobe FFPROBE \
  --codec-worker CODEC_WORKER --reference REFERENCE_WORKER --out result.jsonl
CARGO_INCREMENTAL=0 cargo xtask codecs summarize result.jsonl summary.json --require-pass
```

Use a fresh output file because `grade` appends. The `codecs-audio` gate checks the
48 kHz layout, six frame lengths, and all 4,800 samples per plane against FFmpeg's
pinned native Opus decoder. The worker comparison also checks the decoded frame
count and interleaved float samples. Both inputs are `audio-opus-ambisonic` timing
and memory workloads. The stereo session refuses Ambisonic speaker downmix.

Both authored comparisons pass on macOS with the pinned decode profile. Native
Linux runtime and memory are pending.

| Input | Planes | Worst RMS error | Worst absolute error |
|---|---:|---:|---:|
| First-order ACN | 4 | 9.83e-9 | 3.73e-8 |
| First-order ACN with stereo and gain | 6 | 1.34e-6 | 3.34e-6 |

The Ogg Opus mappings for duplicated and inactive components are covered by a
decoder test. Pinned local reference probes differ on those mappings, and external
parity for them remains open. See [RFC 7845 §5.1.1](https://www.rfc-editor.org/rfc/rfc7845#section-5.1.1)
and [RFC 8486 §3.1](https://www.rfc-editor.org/rfc/rfc8486#section-3.1).

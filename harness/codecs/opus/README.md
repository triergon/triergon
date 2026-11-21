# Opus family 2 comparison

`family-2-acn.opus` is an authored 0.1-second Ogg Opus probe, encoded with
FFmpeg 9.0.2 and libopus. It is included as the comparison input; its SHA-256 is
recorded in the [correctness manifest](../correctness-manifest.json). Its OpusHead
has four channels, family 2, four uncoupled streams, mapping `[0, 1, 2, 3]`, 48 kHz input rate and 312 samples of
pre-skip. The channels are first-order SN3D Ambisonics in ACN order.

Build `decode_audio` with `CARGO_INCREMENTAL=0 cargo build --release -p
triergon-session --example decode_audio --features codecs`, and place it at
`DIR/audio-decode`. With the pinned FFmpeg decode build:

```sh
CARGO_INCREMENTAL=0 cargo xtask codecs grade --manifest harness/codecs/correctness-manifest.json \
  --lanes opus-ambisonic --root . --bin DIR --ffmpeg FFMPEG --ffprobe FFPROBE \
  --codec-worker CODEC_WORKER --reference REFERENCE_WORKER --out result.jsonl
CARGO_INCREMENTAL=0 cargo xtask codecs summarize result.jsonl summary.json --require-pass
```

Use a fresh output file: `grade` appends. This lane compares native float PCM;
the stereo device path is not meaningful for this layout. It requests float output
from FFmpeg’s native Opus decoder, probes its application-facing layout, and checks rate, channel
count, layout, each frame length, sample count and every interleaved sample using the existing audio
RMS and maximum-error limits. The raw PCM is in sample-major order, with channels
0, 1, 2 and 3 in each sample. A numeric result alone cannot establish that the
reference labels those channels correctly.

On arm64 macOS 15.6.1, with Homebrew FFmpeg 9.0.2 and libopus 1.6.1, the comparison
of decoder revision `3fafb6f3` fails: the candidate reports `ambisonic 1`, whereas
the libopus decoder reports `quad` (FL, FR, BL, BR). Both output 48 kHz, four
channels and 4,800 samples per channel in six frames. FFmpeg reports frame lengths
648, 960, 960, 960, 960 and 312; the candidate reports six frames in total. In the
interleaved channel order above, the RMS errors are 6.601078959155787e-9,
9.121833829344089e-9, 7.097275634072615e-9 and 5.769907361250846e-9; maximum
absolute errors are 3.725290298461914e-8, 3.725290298461914e-8,
3.725290298461914e-8 and 2.9802322387695312e-8. These meet the numeric limits,
but the layout mismatch fails the exact rule. This development comparison is not
native qualification.

On the same host, Homebrew FFmpeg 9.0.2's native Opus decoder reports `ambisonic 1`
and matches the candidate's frame lengths (648, 960, 960, 960, 960, 312). The
comparison passes its exact and numeric checks; worst channel RMS error is
9.82976633815388e-9 and worst absolute error is 3.725290298461914e-8. This is
a diagnostic using the same release, not the prepared pinned build or native
qualification. The family 2 header specifies ACN component semantics; the reported
layout is separately observed from the selected decoder, not inferred from the header.

The `codecs-audio` gate also checks every interleaved sample and the decoded
frame count against the reference worker. The `audio-opus-ambisonic` file
workload uses the same input and output format for timing and memory.

The `codecs-audio` gate runs this comparison with the pinned `decode` profile.
The comparison passes with that profile on macOS.
It matches layout, frame lengths, 48 kHz, four channels and all 4,800
samples per channel. The worst RMS error is 9.82976633815388e-9. The
largest absolute error is 3.725290298461914e-8. Native runtime and memory
measurements remain open.

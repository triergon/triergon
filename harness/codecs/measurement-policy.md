# Decoder timing and memory: equivalent work

Timing (`harness/codecs/time_codecs.py`) and correctness (`harness/codecs/native_correctness.py`)
compare two workers on the same input, on native Linux ARM64, one run at a time. Pairing,
seeds, confidence bounds and gate thresholds are defined in
[docs/quality.md](../../docs/quality.md).

## Workers

- Reference: `harness/reference.c` built with `-DSAFE_MEDIA_DECODE_REFERENCE`, linked
  statically against the pinned FFmpeg 9.0.2 libraries. It runs the same filter chains as the FFmpeg command line:
  video `scale=1920:1080:force_original_aspect_ratio=decrease`, centred `pad`,
  `format=rgba`; audio `aformat` to stereo s16 at the decoder rate. Its output is
  byte-identical to the command line's.
- Candidate: `crates/codec-worker`, with the same
  access-unit split, operations and outputs. It streams access units from the file.

## Equal work

- Both sides are single-threaded: the reference sets decoder `thread_count = 1` and
  filter graph `nb_threads = 1`. The FFmpeg command line would thread decoding and
  scaling across all cores; that is a resource choice, not different work.
- The reference probes stream parameters at load (`avformat_find_stream_info`) for video
  only, as the command line does, so H.264/HEVC reorder delay matches. Audio is not
  probed, so neither side has decoder code resident before the measured window.
- File loading and process start are outside the timed interval on both sides.
- Both workers fold every output byte into the same 8-way word digest
  (`Digest` / `struct fold`). Every pair must produce identical output unit and byte
  counts; digest equality is recorded (it differs where the rules allow a 1 LSB audio
  difference).
- Memory runs report `{packets, bytes}` on stdout and the digest on stderr, so work
  records are identical on both sides. Output exactness is graded only by the
  correctness run under `NUMERIC-RULES.json`.
- Development timings are never measurement evidence.

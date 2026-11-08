# Stereo downmix of integer decoder output

The device path (`numeric-rules.json`, `audio_device_path`) is FFmpeg's default
rematrix to stereo s16. FFmpeg's FLAC decoder outputs s16 for streams of up to 16 bits
per sample and s32 above, so its resampler mixes integer input. These rules were measured
as a black box.

## Method

FFmpeg 9.0.2 (Homebrew, AArch64 macOS) read crafted raw PCM
(`-f s16le|s32le|f32le -ar 48000 -ch_layout L -i in.raw`) and converted it with the
reference worker's chain, `-af aformat=sample_fmts=s16:channel_layouts=stereo` (with
`-loglevel debug` for the resampler's reported internal format and matrix). Each Q15
gain was fitted on a full-scale ramp of one input channel with the others silent: the
fit `(x * C + 2^14) >> 15` is unique. Random frames, full-scale frames and frames at
the extremes then checked how the channels combine. Layouts: mono, 3.0, 2.1, quad,
quad(side), 3.1, 4.0, 4.1, 5.0, 5.0(side), 5.1, 5.1(side), 6.0, 6.0(front),
hexagonal, 6.1, 6.1(back), 6.1(front), 7.0, 7.0(front), 7.1, 7.1(wide),
7.1(wide-side), octagonal, 5.1.2 and thirteen custom masks.

## s16 input

- The resampler reports `s16p` internally. A stereo s16 stream passes through unchanged.
- Gains are the reported matrix rounded to the nearest 1/32768. Each output sample is
  `(sum(x * C) + 2^14) >> 15`, rounded once, and truncated to 16 bits: it wraps and
  does not clip. No mismatches in 3,500 to 36,000 random and extreme frames per layout.
- 5.0 and 5.0(side) gains are 13573, 9598, 9598, which sum to 32769. A full-scale frame
  overshoots and wraps: every channel at 32767 gives -32768, every channel at -32768 gives
  32767. Our clipping made this the 65534 LSB difference.
- 5.1, 5.1(side) and 7.1 (FL FR FC LFE then BL BR, SL SR or both) give the centre the
  row's remainder, so each row sums to 32768: 9597 instead of 9598 for 5.1, 7424
  instead of 7423 for 7.1. This depends on the layout, not the matrix: FL+FR+FC+TC+BL+BR
  and 5.1.2 have the same matrices and use plain rounding. The centre gain caused the
  2 LSB differences.
- Open: with non-default levels (`aresample=center_mix_level=…`, `lfe_mix_level`,
  `surround_mix_level`) those three layouts follow neither rule. Gains move by more
  than one unit from the matrix, the L and R rows differ, and the channels do not
  combine as one Q15 sum. FFmpeg's FLAC decoder exports no downmix levels, so the
  device path for 16-bit input always uses the default levels.

## s32 input

- The resampler reports `fltp` internally. For mono and every multichannel layout tested
  (mono, 3.0, 5.0, 5.1, 6.1, 7.1), the output is identical to `f32` input of
  `x / 2^31` (42,000 frames per layout, 24-bit and full 32-bit values). This is the float
  path the other decoders use.
- Stereo s32 to s16 is `x >> 16` exactly (21,413 samples, including values next to each
  boundary). That equals the float path's `floor(f * 32768)` when `f` is exact (up to 24
  significant bits). Above 24 bits, rounding to `f32` moves about 0.03% of samples by
  1 LSB.

## Result

`codec-worker audio-flac-raw` against the reference worker linked to the same FFmpeg
(interleaved stereo s16, maximum absolute difference per sample):

| Files | Before | After |
|---|---|---|
| s16 3, 4 and 7 channels, quad(side) mask, 8- and 12-bit 7 channels | 1 | 0 |
| s16 5 channels, 5.0(back) mask | 65534 | 0 |
| s16 6 and 8 channels | 2 | 0 |
| s16 mono and stereo | 0 | 0 |
| 24-bit stereo and mono | 0 | 0 |
| 20/24/32-bit 6, 7 and 8 channels, 28-32-bit stereo | 1 (25-54 samples) | unchanged |

The FLAC planes stay `f32`, so 25-to-32-bit stereo keeps its rounding differences. The
`FL+TBC+TBR` mask fixture is refused by FFmpeg's resampler. Linux AArch64 was not
re-measured. The integer path has no float rounding, but that has not been confirmed
on Linux.

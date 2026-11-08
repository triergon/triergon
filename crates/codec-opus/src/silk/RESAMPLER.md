# SILK resampling to 48 kHz

RFC 6716 leaves the SILK resampler to the implementation (section 4.2.9). FFmpeg's Opus
decoder, the reference this crate is compared with, does not use the RFC's resampler; it
resamples the SILK output with libswresample. The behaviour below was established from
FFmpeg 9.0.2's output alone (black box) and is what `resampler.rs` implements.

## The filter

- Measured: unit impulses at 8, 12 and 16 kHz through
  `ffmpeg -af aresample=48000:filter_size=16`.
- Shown: a zero-phase polyphase interpolator with 16 taps per phase,
  `h(t) = sinc(t) * I0(9 sqrt(1 - (t/8)^2)) / I0(9)` for |t| < 8 input samples, taps
  stored as f32 and not renormalised per phase. The recovered taps equal the formula to
  within 4e-17.
- Measured: this decoder's internal-rate output of `silk-lbrr-mono.mka`, resampled with
  `aresample` under candidate options, against FFmpeg's native decode.
- Shown: `filter_size=16` matches to an RMS of 6.5e-11 over the first 13 frames; the default
  `filter_size=32` leaves 4.9e-6, and cutoff, Kaiser beta, phase count, linear
  interpolation and filter type do not do better.

## Start, latency and flush

- Measured: frame sizes from `ffprobe -show_frames` with `-flags2 +skip_manual`, and the
  best alignment of FFmpeg's output against this decoder's.
- Shown: after a start (or a change of internal rate) the resampler is primed with 4, 8
  or 11 zero input samples at 8, 12 or 16 kHz, and it only emits an output sample once the
  input under its whole filter span has arrived. At 8 kHz the first frame therefore comes
  out 24 samples short (FFmpeg emits a 936-sample frame for 960, in `testvector02`,
  `silk-lbrr-mono` and at each switch to narrowband in `testvector12`); at 12 and 16 kHz no
  frame is short.
- The held-back samples are released when SILK stops or changes rate (`testvector12`
  shows 984-sample frames when narrowband switches to a wider band) and at the end of the
  stream.
- Measured: FFmpeg's decode of `silk-lbrr-mono.mka` truncated to 20 packets.
- Shown: the released samples continue the input by its mirror image, last sample first
  (`x[n + k] = x[n - 1 - k]`); this matches to 6e-9, zero padding to 7.6e-5 only.

## Alignment

- Mono SILK output enters the resampler two samples late (RFC 6716 imposes one, like the
  stereo unmixing); stereo output follows the RFC's one-sample delay. Measured as the best
  shift of the mono and stereo segments of `testvector02` (zero with these delays).
- Hybrid frames: FFmpeg adds CELT to exactly this resampled SILK signal. The part of
  FFmpeg's output below 5 kHz minus this decoder's SILK output has a relative energy of
  1e-10 at zero shift in `silk-lbrr`, `testvector05` and `testvector06` (2e-2 one sample
  off).

## Related output conventions

The same black-box comparison showed FFmpeg departing from the RFC's reference decoder in
these places, which this decoder follows:

- The bandwidth expansion of the LPC coefficients rounds its products instead of
  truncating (filters close to instability are sensitive to the last bit; truncation left
  errors of 1e-3 in `testvector04`).
- The range limiting of RFC 6716 section 4.2.7.5.7 is not applied: the Q12 coefficients
  are kept in 16 bits and an out-of-range filter is bandwidth expanded by the stability
  check like any other (applying it left 1e-3 errors in generated wideband streams).
- In a mid-only stereo frame, the delayed side sample carried over from the previous frame
  is zero too.
- A change of internal rate keeps the SILK channel state (only the resampler restarts),
  including the last 16 LPC synthesis outputs whatever the previous order, which a switch
  to wideband continues from (`testvector12` matches to 1e-7 after every switch).

One divergence is not followed: on 40 and 60 ms packets that carry LBRR frames (libopus
`-fec 1`), FFmpeg 9.0.2's output turns into full-scale noise for the following packets,
while libopus decodes them normally; this decoder follows the RFC there (its frame
energies match libopus's, and its internal-rate output matches the RFC reference decoder).

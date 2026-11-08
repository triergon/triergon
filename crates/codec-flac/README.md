# triergon-codec-flac

FLAC decoder written from RFC 9639. It decodes one frame per access unit to planar `f32`
in FFmpeg's channel order (the [triergon-codec](../codec/README.md) `AudioDecoder`
contract), or to the lossless integer samples. It depends on
[triergon-codec](../codec/README.md), [triergon-bitstream](../bitstream/README.md) and
[triergon-framing](../framing/README.md) (STREAMINFO and frame headers).

## Usage

```rust
use triergon_codec_flac::{AudioFrame, DecodeError, Decoder, IntegerFrame, Limits};

fn decode(configuration: &[u8], frames: &[&[u8]]) -> Result<(), DecodeError> {
    // `fLaC` and the metadata blocks: the native stream header, Matroska CodecPrivate.
    let mut decoder = Decoder::from_configuration(configuration, Limits::default())?;
    let mut float = AudioFrame::default();
    let mut integer = IntegerFrame::default();
    for au in frames {
        let info = decoder.decode_float(au, &mut float)?; // or AudioDecoder::decode
        decoder.decode_integer(au, &mut integer)?; // integer.plane(0), ...
        let _ = (info.sample_rate, info.bits_per_sample, info.layout);
    }
    Ok(())
}
```

`examples/decode_flac` decodes the first FLAC track of a file (native FLAC, Ogg,
Matroska), read through the `triergon` facade, to interleaved f32le: the decoder's whole
output, without container trims. `--per-frame` prints each frame's format and
`--reject-crc` rejects frames whose CRC fails.

## Supported

The whole decoding process of RFC 9639: CONSTANT, VERBATIM, FIXED (orders 0 to 4) and LPC
(orders 1 to 32) subframes, wasted bits, Rice and Rice2 partitioned residuals (partition
orders up to 15) with escaped partitions, left/side, side/right and mid/side stereo,
4 to 32 bits per sample (the 33-bit side channel of 32-bit stereo included), 1 to 8
channels, block sizes up to 65535, fixed and variable blocking, and sample rates and
channel counts that change between frames.

## Behaviour and limits

- `decode_float` scales as FFmpeg converts its FLAC decoder output to `f32`: up to 16
  bits per sample the samples go through 16-bit (`(x << (16 - bits)) as i16 / 2^15`),
  above that through 32-bit (`(x << (32 - bits)) as f32 / 2^31`, which rounds 25- to
  32-bit samples to `f32`). `decode_integer` returns the samples themselves.
- LPC prediction accumulates in 32 bits when RFC 9639 section 9.2.5's bound
  (sample bits + coefficient precision + log2 order <= 32) allows it, else in 64 bits;
  fixed predictors always use 32-bit arithmetic, which is exact modulo 2^32.
- Following FFmpeg 9.0.2's decoder: the frame CRC-16 is not checked by default
  (`CrcPolicy::Ignore`; `with_crc_policy(CrcPolicy::Reject)` refuses mismatches with
  `CrcMismatch`); a frame whose sample size differs from STREAMINFO's (or, without one,
  from the first frame's) is refused as `Unsupported`; so are negative LPC shifts. A
  STREAMINFO maximum block size below 16, a block above it, a predictor order above the
  first residual partition, wasted bits not below the sample size and a frame missing
  any byte of its CRC-16 are `Invalid`.
- Layout: FFmpeg's FLAC defaults (mono, stereo, 3.0, quad, 5.0(side), 5.1(side), 6.1,
  7.1; the five- and six-channel surround pair is side left/right where RFC 9639 says
  back/surround). A `WAVEFORMATEXTENSIBLE_CHANNEL_MASK` Vorbis comment in the
  configuration replaces it, as FFmpeg's demuxers do: the first such comment, read like
  C `strtol` with base 0, naming exactly as many of the 18 WAVEFORMATEXTENSIBLE speakers
  as STREAMINFO has channels. A frame with another channel count uses the default
  layout of its count.
- An empty configuration decodes a stream picked up at a frame header (no STREAMINFO):
  each frame then supplies its own sample rate, and its sample size or the first
  frame's.
- A failed frame leaves the output empty and the decoder usable. FLAC frames carry no
  inter-frame state, so `reset` has nothing to drop.
- Storage: the channel buffers (two blocks of `i32`, plus one of `i64` for 32-bit stereo)
  and the output frames are charged to a `Budget` before they grow; a refusal is
  `DecodeError::Budget`. The default `Limits::live_storage_bytes` is 6 MiB: the largest
  frame RFC 9639 allows is 65536 samples of 8 channels, 2 MiB in each output, plus
  1 MiB of channel buffers. No allocation once the largest block has been seen.

## Known gaps

- FFmpeg drops the channel mask of a stream whose channel count changes within the
  frames its demuxer probes (it labels even the first frames with the default layout);
  this decoder keeps the mask for every frame with STREAMINFO's channel count.
- Malformed streams whose decoded samples exceed their sample size are reproduced as
  FFmpeg wraps them in the 16- or 32-bit output, but the choice between 32- and 64-bit
  LPC accumulation is only known to match FFmpeg for valid streams.

## Conformance

- Specification: RFC 9639.
- Vectors (outside git): the IETF CELLAR decoder testbench
  (`subset`, `uncommon`, `faulty`; CC0) and streams `harness/codecs/flac/flac_vectors.py generate` makes
  with FFmpeg's encoder and the reference `flac` encoder over bit depths 4 to 32, 1 to 8
  channels, sample rates 1 Hz to 768 kHz, block sizes 16 to 65535, every compression level,
  LPC type, stereo mode, partition order and prediction order 1 to 32.
- `harness/codecs/flac/flac_vectors.py grade target/release/examples/decode_flac DIR...` compares the
  output with `ffmpeg -f f32le -c:a pcm_f32le` byte for byte, and the per-frame layouts
  and sample counts with `ffprobe -show_frames`; FLAC is graded bit-exact.
- `cargo test -p triergon-codec-flac` runs predictor, layout and hand-built frame tests
  (every subframe type and stereo mode, the refusals, truncation at every byte, every bit
  flipped, a too-small budget) and, when the vectors are present, checks the integer
  output against each stream's STREAMINFO MD5 and the float output against it.

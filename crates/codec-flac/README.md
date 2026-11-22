# triergon-codec-flac

FLAC decoder written from RFC 9639. It decodes one frame per access unit to planar `f32`
in FFmpeg's channel order (the [triergon-codec](../codec/README.md) `AudioDecoder`
contract), or to the lossless integer samples. It depends on
[triergon-codec](../codec/README.md), [triergon-bitstream](../bitstream/README.md) and
[triergon-framing](../framing/README.md) (STREAMINFO and frame headers).

## Usage

```rust
use triergon_codec_flac::{AudioFrame, DecodeError, Decoder};

fn decode(configuration: &[u8], frames: &[&[u8]]) -> Result<(), DecodeError> {
    // `fLaC` and the metadata blocks: the native stream header, Matroska CodecPrivate.
    let mut decoder = Decoder::new(configuration)?;
    let mut float = AudioFrame::default();
    let mut integer = decoder.integer_frame();
    for au in frames {
        let info = decoder.decode(au, &mut float)?; // or AudioDecoder::decode
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

- `decode` scales as FFmpeg converts its FLAC decoder output to `f32`: up to 16
  bits per sample the samples go through 16-bit (`(x << (16 - bits)) as i16 / 2^15`),
  above that through 32-bit (`(x << (32 - bits)) as f32 / 2^31`, which rounds 25- to
  32-bit samples to `f32`). `decode_integer` returns the samples themselves.
- Rice residuals share a cache refill across several samples. The decoder checks
  the frame bit limit after each partition and rejects a partition whose final
  code crosses it.
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
- Storage: the channel buffers, the float output and the integer frames
  (`Decoder::integer_frame`) are charged to the decoder's `Budget` as they grow, and a
  refusal is `DecodeError::Budget`. The default `Limits::live_storage_bytes`, 7 MiB,
  holds the largest frame RFC 9639 allows (65536 samples of 8 channels, 2 MiB) in both
  outputs with the channel buffers of 32-bit stereo and a buffer's copy while it grows;
  `with_limits` and `new_in` set another limit or a shared pool.

## Known gaps

- FFmpeg drops the channel mask of a stream whose channel count changes within the
  frames its demuxer probes (it labels even the first frames with the default layout);
  this decoder keeps the mask for every frame with STREAMINFO's channel count.
- Malformed streams whose decoded samples exceed their sample size are reproduced as
  FFmpeg wraps them in the 16- or 32-bit output, but the choice between 32- and 64-bit
  LPC accumulation is only known to match FFmpeg for valid streams.

## Conformance

The `codecs-files` item grades decoded PCM and stereo device output through the file reader,
including both timed FLAC inputs.

- Specification: RFC 9639.
- The external vectors come from the IETF CELLAR decoder testbench (`subset`,
  `uncommon`, `faulty`, licensed CC0) and from
  [`flac_vectors.py`](../../harness/codecs/flac/flac_vectors.py). Run
  `python3 harness/codecs/flac/flac_vectors.py FFMPEG FLAC .research/flac-generated`
  with the FFmpeg executable from the decode profile and the reference `flac` 1.4.3
  executable to reproduce the pinned inputs. The script generates deterministic WAV
  signals in a temporary directory and encodes them to FLAC across bit depths, channel
  counts, sample rates, block sizes and encoder options. The
  [`codec-vector-inputs` evidence set](../../harness/evidence/sets/codec-vector-inputs.json)
  includes both groups of vectors.
- `cargo test -p triergon-codec-flac` runs predictor, layout and hand-built frame tests
  (every subframe type and stereo mode, the refusals, truncation at every byte, every bit
  flipped, a too-small budget) and, when the vectors are present, checks the integer
  output against each stream's STREAMINFO MD5 and the float output against it.
- `cargo xtask codecs grade` decodes them through the container readers and compares the
  output with the pinned FFmpeg's byte for byte (the `flac` lane, under the lossless
  section of the numeric rules; [harness/codecs/STATUS.md](../../harness/codecs/STATUS.md)).
  The three vectors with sample-rate or channel-count changes compare complete runs of
  decoded frames at each native format and stereo output at each native sample rate.
  `flac_3ch_odd_mask` matches FFmpeg's decoder, but FFmpeg refuses its layout on the
  stereo output. Runtime and memory against the [gates](../../docs/quality.md#gates):
  [status](../../docs/status.md).

The `flac-crc-reject` comparison grades frame counts, block sample counts and
decoded PCM on damaged inputs against FFmpeg with CRC checking enabled.

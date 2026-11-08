# triergon-codec-vorbis

Vorbis I decoder written from the Vorbis I specification (Xiph.Org, 2020-07-04 revision with
the 2015 errata). It decodes one audio packet per access unit to planar `f32` in FFmpeg's
channel order (the [triergon-codec](../codec/README.md) `AudioDecoder` contract). It
depends on [triergon-codec](../codec/README.md) (`AudioFrame`, layouts, the shared
inverse MDCT and window slope, the storage budget),
[triergon-bitstream](../bitstream/README.md) (`LsbReader`, `LsbWindow`) and
[triergon-framing](../framing/README.md) (identification header, Xiph lacing).

## Usage

```rust
use triergon_codec::{AudioDecoder, AudioFrame};
use triergon_codec_vorbis::{DecodeError, Decoder};

fn decode(configuration: &[u8], packets: &[&[u8]]) -> Result<(), DecodeError> {
    // The identification, comment and setup headers, Xiph-laced (Matroska
    // CodecPrivate); `Decoder::from_headers_in` takes the three packets of an Ogg stream.
    let mut decoder = Decoder::new(configuration)?;
    let mut frame = AudioFrame::default();
    for packet in packets {
        if decoder.decode(packet, &mut frame).is_ok() {
            // frame.samples_per_channel samples in frame.planes, at frame.sample_rate.
        }
    }
    Ok(())
}
```

`Decoder::with_limits` sets the decoder's own pool; `Decoder::new_in` and
`Decoder::from_headers_in` charge a caller's `Budget` instead.
`examples/decode_vorbis` decodes the first Vorbis track of a file, read through the
`triergon` facade, to interleaved f32le: the decoder's whole output, without container
trims. In a chained Ogg file each link's headers either continue the decoder (when they
repeat) or start a new one.

## Supported

The whole of Vorbis I: codebooks with lookup types 0, 1 and 2 (ordered, sparse and
single-entry codebooks), floor 0 (LSP) and floor 1, residue types 0, 1 and 2, mappings
with submaps and square-polar coupling, every mode and block size (64 to 8192, with
the hybrid windows between short and long blocks), 1 to 93 channels (the most an output
layout holds) at any sample rate.

## Behaviour and limits

- Each packet yields the samples from the centre of the previous block to the centre
  of its own (`previous / 4 + current / 4`); the first packet after construction or
  `reset` primes the overlap and yields none. Without that priming output, a stream's
  output starts where FFmpeg's does after its decoder skips it. The end trim of a stream
  (its last packet cut to the final granule position) belongs to the container reader.
- Codewords decode by one table lookup on the next 10 bits; longer codewords by a binary
  search over the remaining ones. Over- and underspecified Huffman trees are refused
  except a single-entry codebook of length 1, which reads one bit (errata 20150226),
  and a codebook without used entries, from which nothing decodes.
- The inverse MDCT runs as the shared DCT-IV, its unfolding folded into the windowed
  overlap-add; work past the last spectral line a packet's residues code is skipped.
- Following FFmpeg 9.0.2: reads past the end of a packet return zero bits, so floor and
  residue decode run to their end instead of stopping at the end of the packet; inverse
  coupling treats a zero magnitude as positive (the specification's "greater than zero"
  flips signs when the magnitude is exactly zero, which FFmpeg's encoder produces);
  floor 0 curves are evaluated in single precision, which keeps the output within
  1.2e-5 of FFmpeg's where double precision drifts to 3.4e-5.
- Layout: the Vorbis I channel orders mapped to FFmpeg's native layouts (mono, stereo,
  3.0, quad, 5.0, 5.1, 6.1, 7.1). Beyond eight channels the planes stay in coded order
  under a layout without positions (`ChannelLayout::mask() == 0`), where FFmpeg reports an
  unspecified order.
- Malformed packets return `DecodeFailure` (an `Invalid` error inside) and leave the
  decoder usable; malformed headers are `Invalid`, a version other than 0 `Unsupported`.
- Memory: everything the stream sizes is charged to the decoder's budget before it is
  allocated (`Limits::live_storage_bytes`, 32 MiB by default): the setup header's
  codebook, floor, residue and mapping tables, the per-channel spectrum, overlap and
  floor buffers, the window slopes, the transform tables and the output frame. A refused
  charge is `DecodeError::Budget` and, for the output frame, leaves the decoder as it
  was. The FATE vectors peak at 0.6 MiB (six channels); 93 channels of 8192-sample
  blocks need about 7 MiB of buffers. Nothing is allocated per packet.
- Setup work is bounded by the header's size and the storage charged before it: an
  ordered codebook, which declares up to 2^24 entries in a few bits, charges its
  codeword tables before it assigns a codeword, so an oversized one is refused
  (`Budget`) or rejected as an overspecified tree (`Invalid`) without per-entry work.

## Conformance

- Specification: Vorbis I (Xiph.Org, 2020-07-04 revision with the 2015 errata).
- `cargo xtask codecs grade` runs the `vorbis` lane
  ([harness/codecs/STATUS.md](../../harness/codecs/STATUS.md)), which decodes each file
  through the container readers with its trims applied and compares the result with FFmpeg
  9.0.2's native decoder under the [numeric
  rules](../../harness/codecs/numeric-rules.json).
- On the gate build 21/21 pass: FATE's `vorbis` directory. Runtime and memory against the
  [gates](../../docs/quality.md#gates): [status](../../docs/status.md).

# triergon-codec-mp3

MPEG-1/2 audio Layer I, II and III decoder written from ISO/IEC 11172-3 and
ISO/IEC 13818-3 (low sampling frequencies), with the MPEG-2.5 extension to 8, 11.025 and
12 kHz. It decodes one frame per access unit to planar `f32` (the
[triergon-codec](../codec/README.md) `AudioDecoder` contract) and depends on
[triergon-codec](../codec/README.md), [triergon-bitstream](../bitstream/README.md) and
[triergon-framing](../framing/README.md) (`MpegAudioHeader`).

## Usage

```rust
use triergon_codec::{AudioDecoder, AudioFrame};
use triergon_codec_mp3::{DecodeError, Decoder};

fn decode(frames: &[&[u8]]) -> Result<(), DecodeError> {
    // MPEG audio has no configuration: every frame header describes its frame.
    let mut decoder = Decoder::new();
    let mut frame = AudioFrame::default();
    for au in frames {
        if decoder.decode(au, &mut frame).is_ok() {
            // frame.samples_per_channel samples in frame.planes, at frame.sample_rate.
        }
    }
    Ok(())
}
```

An access unit is one whole frame, header included. `Decoder::new_in` charges a
caller's `Budget`; `frame_header` reads a frame header as the decoder does.
`examples/decode_mp3` decodes the first MPEG audio track of a file (a raw stream or a
container), read through the `triergon` facade, to interleaved f32le: the decoder's whole
output, without container trims. `--reset-at N` resets the decoder before access unit N.

## Supported

- Layer III: MPEG-1, MPEG-2 and MPEG-2.5 side information, the bit reservoir, MPEG-1
  scalefactors with `scfsi` and the low-sampling-frequency scheme, every Huffman table
  with linbits and both count1 tables, requantization with pre-emphasis and subblock
  gain, short and mixed blocks, mid/side and intensity stereo (both schemes), alias
  reduction, the 36- and 12-point IMDCT with the four window shapes (the 36-point one
  through a 9-point complex DFT), overlap and frequency inversion.
- Layer II: the four MPEG-1 allocation tables, chosen by sample rate and bit rate per
  channel, and the table of the low sampling frequencies; scalefactor selection,
  grouped quantizers and the joint stereo bound.
- Layer I, joint stereo included.
- All three layers share one polyphase synthesis filterbank (a 32-point DCT by Lee's
  factorisation, then the 512-tap window). Mono frames give one plane (`FC`); stereo,
  joint stereo and dual channel give two (`FL`, `FR`). A stream may change mode, sample
  rate or layer between frames.

Free-format Layer III frames are accepted by `Decoder::decode_framed` and
`AudioDecoder::decode` when the caller supplies exactly one complete frame,
including its header and optional CRC.
The header does not encode its extent, so the decoder cannot verify the boundary.
Free-format Layer I and II remain unsupported. `decode_frame` requires the header
to specify the frame length.

## Behaviour and limits

- Output is FFmpeg's float decoders' (`mp1float`, `mp2float`, `mp3float`): every
  frame decodes to its full 384, 1152 or 576 samples per channel, with no delay
  removed. Encoder delay and padding (a LAME tag) are a container trim the reader
  applies: FFmpeg skips `encoder_delay + 529` samples at the start and
  `encoder_padding - 529` at the end. A Xing, Info or VBRI frame at the start of a
  file is the reader's to drop, as FFmpeg's demuxer does.
- CRCs are not checked, as FFmpeg does not by default.
- Bit reservoir: a granule whose data starts before the first byte the stream
  supplied (at the start, or after `reset`) decodes as silence; the rest of the frame
  decodes normally. A frame whose side information FFmpeg refuses (`big_values` above
  288, block type 0 with window switching) is `DecodeError::Invalid` but its main
  data still joins the reservoir.
- A frame cut short decodes as if the missing bytes were zero; bytes after a frame are
  ignored, for frames whose header specifies their length. For free-format Layer III,
  the complete side information must be present and main data must fit the fixed
  reservoir's 1441-byte current-frame capacity. The reserved emphasis value is
  accepted, and the forbidden Layer I allocation 15 reads as 16 bits. The mixed-block
  flag of a start or stop block gives
  its two lowest subbands the normal window. In the intensity stereo of the low
  sampling frequencies every position is intensity coded, the largest value of its
  field included.
- The decoder charges its `Budget` (`Limits::live_storage_bytes`, 1 MiB by default)
  for its state, fixed in size and built with the first frame (about 22 KiB: synthesis
  history, overlap, the reservoir and scratch), and for the output frame before it
  grows (at most 1152 samples of two channels, 9 KiB). A refused charge is
  `DecodeError::Budget`, leaves the frame empty, and the decoder stays usable. The
  Huffman lookups and the `|x|^(4/3)` and transform tables (about 62 KiB) are built
  once per process.
- Malformed data never panics: it is `DecodeError::Invalid`, and the next frame decodes
  normally. `reset` drops the reservoir, overlap and synthesis history, and keeps the
  scalefactors a following granule may reuse (`scfsi`), as FFmpeg's flush does.

## Tables

The Huffman code tables (11172-3 Table B.7), scalefactor bands (Table B.8 and 13818-3
Table B.2), pre-emphasis (Table B.6), alias-reduction coefficients (Table B.9),
synthesis window (Table B.3), Layer I/II scalefactors (Table B.1) and allocation tables
(Table B.2 a to d and 13818-3 Table B.1) are written in the source from the standards.
The Huffman tables are checked to be complete prefix codes.

## Verification

`harness/codecs/mp3/make_vectors.sh FFMPEG OUT` generates the test streams (libmp3lame,
mp2 and mp2fixed encodes at MPEG-1, 2 and 2.5 rates, and random Layer I/II frames from
`harness/codecs/mp3/random_frames.py`) with the FFmpeg it is given. Comparison:
[harness/codecs](../../harness/codecs/STATUS.md); results in
[docs/status.md](../../docs/status.md).

For the free-format PCM comparison, put the FATE input `he_free.bit` under
`.research/fate-samples/mp3-conformance/`. The test checks its pin and the
authored mono stream's pin, then compares every sample with mpg123 1.33.7:

```sh
CARGO_INCREMENTAL=0 TRIERGON_MPG123=/path/to/mpg123 \
  cargo test -p triergon-codec-mp3 --test decoder free_format_layer_three_matches_mpg123 -- --ignored --nocapture
```

The comparison reads complete access units through the normal reader and
uses each decoder's PCM before gapless trimming. It reports the worst RMS and
peak sample differences across all frames and channels.

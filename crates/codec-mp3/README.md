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
use triergon_codec_mp3::{DecodeError, Decoder, Limits};

fn decode(frames: &[&[u8]]) -> Result<(), DecodeError> {
    // MPEG audio has no configuration: every frame header describes its frame.
    let mut decoder = Decoder::from_configuration(&[], Limits::default())?;
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
  reduction, the 36- and 12-point IMDCT with the four window shapes, overlap and
  frequency inversion.
- Layer II: the four MPEG-1 allocation tables, chosen by sample rate and bit rate per
  channel, and the table of the low sampling frequencies; scalefactor selection,
  grouped quantizers and the joint stereo bound.
- Layer I, joint stereo included.
- All three layers share one polyphase synthesis filterbank (a 32-point DCT by Lee's
  factorisation, then the 512-tap window). Mono frames give one plane (`FC`); stereo,
  joint stereo and dual channel give two (`FL`, `FR`). A stream may change mode, sample
  rate or layer between frames.

Not supported: free-format frames (bit rate index 0), refused as
`DecodeError::Unsupported`.

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
  ignored. The reserved emphasis value is accepted, and the forbidden Layer I
  allocation 15 reads as 16 bits. The mixed-block flag of a start or stop block gives
  its two lowest subbands the normal window. In the intensity stereo of the low
  sampling frequencies every position is intensity coded, the largest value of its
  field included.
- The output frame is charged to the decoder's `Budget` before it grows
  (`Limits::live_storage_bytes`, 1 MiB by default; the largest frame is 1152 samples of
  two channels, 9 KiB). A refused charge is `DecodeError::Budget` and the decoder stays
  usable. The rest of the state (about 27 KiB: synthesis history, overlap, the
  reservoir and scratch) is fixed in size and allocated with the decoder; the Huffman
  lookups and the `|x|^(4/3)` and transform tables (about 62 KiB) are built once per
  process.
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

`harness/codecs/mp3/make_vectors.sh` generates the test streams under `.research/mp3/` (libmp3lame,
mp2 and mp2fixed encodes at MPEG-1, 2 and 2.5 rates, and random Layer I/II frames from
`harness/codecs/mp3/random_frames.py`) and their FFmpeg reference output with `harness/codecs/mp3/reference.c`.
`harness/codecs/mp3/grade.sh` decodes every vector and the ISO conformance streams and grades them
with `harness/codecs/mp3/compare.py` (per channel RMS at most 8.8e-6, maximum difference at most
6.1035e-5, exact sample counts).

`cargo xtask codecs grade` runs the same streams through the container readers (the
`mp1`, `mp2` and `mp3` lanes, [harness/codecs/STATUS.md](../../harness/codecs/STATUS.md));
Layer I and II compare with FFmpeg's float decoders. On the gate build 62/64 pass
(Layer I 5/5, Layer II 20/20, Layer III 37/39). Open: `he_mode` changes channel mode
mid-stream, which FFmpeg's CLI converts to the first frame's format; `he_free` is free
format, refused by both. Runtime and memory against the [gates](../../docs/quality.md#gates):
[status](../../docs/status.md).

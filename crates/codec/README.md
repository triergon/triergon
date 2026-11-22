# triergon-codec

The decoder vocabulary shared by the codec crates: the `VideoDecoder` and
`AudioDecoder` traits, decoded audio as `AudioFrame` with a `ChannelLayout` of
`Speaker`s, stream downmix metadata, the codec selectors, the Kaiser-Bessel-derived
window the AAC and AC-3 filterbanks share, the inverse MDCT the Vorbis and Opus
decoders share, and integer division by a run-time divisor without a library call on
32-bit ARM. It sits in the codec layer below the nine decoders and depends only on
[triergon-core](../core/README.md), whose storage types it re-exports for the
decoders' budgets, and [triergon-pixel](../pixel/README.md), whose picture views (`PictureView`, `Plane`,
`Colour`) it re-exports. It contains no decoder; sessions and workers drive the
decoders only through these traits.

## Usage

```rust
use triergon_codec::{
    AudioDecoder, AudioFrame, DynAudioDecoder, DynVideoDecoder, Error, Speaker, VideoDecoder,
};

// A decoder of a codec chosen at run time refuses with the workspace `Error`.
fn video(decoder: &mut DynVideoDecoder, au: &[u8], pts: i64) -> Result<(), Error> {
    decoder.send(au, Some(pts))?;              // one Annex B access unit
    while let Some(picture) = decoder.receive() {
        // picture.width, .height, .bit_depth, .planes, .pts, .sar, .colour
    }
    Ok(())
}

// A concrete decoder refuses with its crate's own error.
fn audio<D: AudioDecoder>(decoder: &mut D, au: &[u8]) -> Result<(), D::Error> {
    let mut frame = AudioFrame::default();
    let levels = decoder.decode(au, &mut frame)?.unwrap_or_default(); // DownmixLevels
    if let Some(i) = frame.layout.position(Speaker::LowFrequency) {
        let _lfe = &frame.planes[i][..frame.samples_per_channel];
    }
    Ok(())
}

// `boxed` turns the second into the first.
fn erase(decoder: impl AudioDecoder<Error: Into<Error>> + 'static) -> Box<DynAudioDecoder> {
    decoder.boxed()
}
```

Implementations: `triergon_codec_h264::Decoder` and `triergon_codec_hevc::Decoder`
(`VideoDecoder`); `triergon_codec_aac::Decoder`, `triergon_codec_ac3::Decoder` and
`triergon_codec_dca::Decoder`, and the FLAC, Opus, Vorbis and MPEG audio decoders
(`AudioDecoder`). `VideoCodec` (`H264`, `Hevc`) and `AudioCodec` (`Ac3`, `Eac3`, `Aac`,
`Dts`, `Flac`, `Opus`, `Vorbis`, `MpegAudio`) name a decoder;
[triergon-session](../session/README.md) maps them to boxed decoders.

## Supported

| Item | Meaning |
|---|---|
| `VideoDecoder` | `send(au, pts) -> Result<(), Self::Error>`, `receive() -> Option<PictureView>`, `flush`, `reset`; `Send` |
| `AudioDecoder` | `decode(au, &mut AudioFrame) -> Result<Option<DownmixLevels>, Self::Error>`, `reset`; `delay()` (samples a stream starts with that carry no signal, 0 by default) and `flush(&mut AudioFrame)` (samples held back at the end, none by default); `Send` |
| `Error` (associated) | the decoder's error type: `DecodeError`, or a codec crate's own `DecodeError` with the same classes and more detail; each is `Copy` and converts to `triergon_core::Error` without formatting or allocating |
| `DecodeError` | `Unsupported`, `Invalid`, `Policy` (each with a `&'static str` naming the feature, check or cap) and `Budget` (a refused storage charge): the error of the H.264, DTS, MPEG audio, Opus and Vorbis decoders |
| `boxed::<E>()` | the decoder behind a box with its errors converted to `E` |
| `DynVideoDecoder`, `DynAudioDecoder` | `dyn VideoDecoder<Error = Error>` and `dyn AudioDecoder<Error = Error>`: a decoder of a codec chosen at run time |
| `Error`, `StorageError` | `triergon_core::Error`; the second name is the one decoders use for a refused storage charge |
| `AudioFrame` | `sample_rate`, `layout`, `samples_per_channel`, `planes`: planar `f32`, full scale 1.0; `sample_format` (`SampleFormat::Float`, `S16`, `S32`): the decoder's native format, for mixing as FFmpeg mixes it |
| `ChannelLayout` | speaker of each plane in plane order; `new`, `unspecified(n)` (no positions), `default_for(n)` (FFmpeg's default), `speakers`, `channels`, `position`, `mask`, `name` (FFmpeg's layout name), `EMPTY`, `CAPACITY` |
| `layout::{vorbis_order, flac_order, native_order}` | the Vorbis/Opus and FLAC coded channel orders, and the plane each coded channel goes to |
| `mdct::Imdct` | the Vorbis and Opus inverse MDCT and DCT-IV for any `N` with `N / 4` a product of 2, 3 and 5: an iterative mixed-radix FFT over split real and imaginary planes in `f32`, in a fixed operation order the decoders' bit-exact output depends on; its tables are charged to a budget when it is built |
| `Speaker` | loudspeaker position; `name()` gives FFmpeg's short name, `mask()` its `AV_CH_*` bit |
| `DownmixLevels` | `center`, `surround`, `lfe` gains and `SurroundEncoding` (`None`, `Dolby`, `ProLogicII`) |
| `AudioFrame::try_resize` | sizes the planes, charging the frame's capacity to a decoder's `Reservation` first |
| `kbd_window` | rising half of a Kaiser-Bessel-derived window of any length and alpha |
| `divide`, `divide_u32` | `a / b` rounded towards zero for `b > 0`; on 32-bit ARM, whose baseline has no divide instruction, an exact double division instead of the library routine |
| `Divisor` | quotient and remainder by one divisor fixed at run time, as a multiplication and a shift, exact while dividend times divisor stays below 2^31 |

### Channel order

A `Speaker`'s discriminant is FFmpeg's channel id (`AV_CHAN_*`), which is also the bit
index of its `AV_CH_*` mask:

| Id | Speakers |
|---|---|
| 0–8 | FL, FR, FC, LFE, BL, BR, FLC, FRC, BC |
| 9–17 | SL, SR, TC, TFL, TFC, TFR, TBL, TBC, TBR |
| 31–40 | WL, WR, SDL, SDR, LFE2, TSL, TSR, BFC, BFL, BFR |

Every decoder emits one plane per speaker in ascending id order, FFmpeg's native
order, so plane `i` is channel `i` of the reference output. The one exception is an
AAC program configuration element that FFmpeg cannot map to a known layout: FFmpeg
then keeps the element order of the PCE, and so does the AAC decoder.

## Behaviour and limits

- **Video.** `send` takes one Annex B access unit and a caller timestamp; the
  timestamp is carried through reordering to `PictureView::pts` and is never
  interpreted. `receive` returns pictures in output order; the view borrows the
  decoder until the next call. `flush` makes every remaining picture receivable at
  end of stream; `reset` drops all state except parameter sets after a
  discontinuity. How planes, strides and crop offsets are laid out is described in
  [triergon-pixel](../pixel/README.md).
- **Audio.** `decode` replaces the contents of `out` with the output of one access
  unit; `samples_per_channel == 0` means it produced none. Rate and layout may change
  between frames. `AudioFrame::clear` drops samples and keeps allocations. `reset`
  drops all signal history.
- **Layouts.** `ChannelLayout` stores up to `CAPACITY` (93) speakers inline, so it is
  `Copy` and comparing or caching one never allocates; `new` returns `None` beyond
  that. 93 is the largest AAC PCE (15 front, 15 side and 15 back channel pair
  elements plus 3 LFEs); AC-3 stays at 16 planes and DTS at 8. Equality and hashing
  compare speakers in plane order. `mask()` is 0 when a position repeats, because no
  FFmpeg mask describes such a layout.
- **Downmix metadata.** `AudioDecoder::decode` returns the stream's mix levels when
  it carries them (AC-3 and E-AC-3). `DownmixLevels::default()` is the conventional
  −3 dB centre and surrounds (1/√2) with LFE omitted and no surround encoding, for
  streams that carry none.
- **Errors.** A refused access unit leaves the decoder usable. Each decoder reports
  `DecodeError` through the trait, or, where it has more to say, its crate's own
  `DecodeError`, whose variants of the same classes have the same names and texts
  (AAC, AC-3, FLAC, HEVC). Converted to `triergon_core::Error` it keeps its class and
  detail text: invalid data is `Invalid`, a feature outside the decoder's set
  `Unsupported`, a resource cap of the codec `Policy`, a short access unit `Truncated`,
  and a refused storage charge the budget's own error (`Limit`, `Allocation`,
  `Overflow`). What only the decoder's type can say (byte counts, a feature's number)
  stays with that type.

## Conformance

The [layout comparison](../../harness/features/codec-layouts/contract.json) checks the decoded
sample rate, speaker on each plane, layout mask and name, native and Vorbis permutations, and
published downmix gains. The Ogg inputs include Vorbis and Opus with one through eight
channels.

Speaker ids and names follow FFmpeg's channel layout API (`AV_CHAN_*`, `AV_CH_*`).
The channel order is verified through the codec crates, whose output planes are
compared with FFmpeg channel by channel; unit tests pin layout equality, masks and
capacity. Gates: [docs/quality.md](../../docs/quality.md).

The [inverse transform comparison](../../harness/features/dsp-imdct/contract.json)
checks IMDCT and DCT-IV samples at every supported length through 8192 against
the cosine definitions and the public FFmpeg transform under a numeric rule.
The [window comparison](../../harness/features/dsp-windows/contract.json) checks
KBD and Vorbis windows against their formulae within one binary32 ULP.
The [division comparison](../../harness/features/dsp-divide/contract.json)
checks truncation, quotients and remainders against exact integer arithmetic.

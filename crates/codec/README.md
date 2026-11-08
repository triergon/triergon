# triergon-codec

The decoder vocabulary shared by the codec crates: the `VideoDecoder` and
`AudioDecoder` traits, decoded audio as `AudioFrame` with a `ChannelLayout` of
`Speaker`s, stream downmix metadata, the codec selectors, the Kaiser-Bessel-derived
window the AAC and AC-3 filterbanks share, and the inverse MDCT the Vorbis and Opus
decoders share. It sits in the codec layer below the nine decoders and depends only on
[triergon-core](../core/README.md), whose storage types it re-exports for the
decoders' budgets, and [triergon-pixel](../pixel/README.md), whose picture views (`PictureView`, `Plane`,
`Colour`) it re-exports. It contains no decoder; sessions and workers drive the
decoders only through these traits.

## Usage

```rust
use triergon_codec::{AudioDecoder, AudioFrame, DecodeFailure, Speaker, VideoDecoder};

fn video(decoder: &mut dyn VideoDecoder, au: &[u8], pts: i64) -> Result<(), DecodeFailure> {
    decoder.send(au, Some(pts))?;              // one Annex B access unit
    while let Some(picture) = decoder.receive() {
        // picture.width, .height, .bit_depth, .planes, .pts, .sar, .colour
    }
    Ok(())
}

fn audio(decoder: &mut dyn AudioDecoder, au: &[u8]) -> Result<(), DecodeFailure> {
    let mut frame = AudioFrame::default();
    let levels = decoder.decode(au, &mut frame)?.unwrap_or_default(); // DownmixLevels
    if let Some(i) = frame.layout.position(Speaker::LowFrequency) {
        let _lfe = &frame.planes[i][..frame.samples_per_channel];
    }
    Ok(())
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
| `VideoDecoder` | `send(au, pts)`, `receive() -> Option<PictureView>`, `flush`, `reset`; `Send` |
| `AudioDecoder` | `decode(au, &mut AudioFrame) -> Result<Option<DownmixLevels>, DecodeFailure>`, `reset`; `delay()` (samples a stream starts with that carry no signal, 0 by default) and `flush(&mut AudioFrame)` (samples held back at the end, none by default); `Send` |
| `AudioFrame` | `sample_rate`, `layout`, `samples_per_channel`, `planes`: planar `f32`, full scale 1.0; `sample_format` (`SampleFormat::Float`, `S16`, `S32`): the decoder's native format, for mixing as FFmpeg mixes it |
| `ChannelLayout` | speaker of each plane in plane order; `new`, `unspecified(n)` (no positions), `default_for(n)` (FFmpeg's default), `speakers`, `channels`, `position`, `mask`, `name` (FFmpeg's layout name), `EMPTY`, `CAPACITY` |
| `layout::{vorbis_order, flac_order, native_order}` | the Vorbis/Opus and FLAC coded channel orders, and the plane each coded channel goes to |
| `mdct::Imdct` | inverse MDCT and DCT-IV for any `N` with `N / 4` a product of 2, 3 and 5: an iterative mixed-radix FFT over split real and imaginary planes; `table_bytes` charges a budget first |
| `Speaker` | loudspeaker position; `name()` gives FFmpeg's short name, `mask()` its `AV_CH_*` bit |
| `DownmixLevels` | `center`, `surround`, `lfe` gains and `SurroundEncoding` (`None`, `Dolby`, `ProLogicII`) |
| `DecodeFailure` | the refusing codec error's `Debug` text; `from_error` builds one |
| `AudioFrame::try_resize` | sizes the planes, charging the frame's capacity to a decoder's `Reservation` first |
| `kbd_window` | rising half of a Kaiser-Bessel-derived window of any length and alpha |

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
- **Errors.** A refused access unit is a `DecodeFailure`; the decoder stays usable.
  Typed errors are available from each codec crate's own `decode`/`send`.

## Conformance

Speaker ids and names follow FFmpeg's channel layout API (`AV_CHAN_*`, `AV_CH_*`).
The channel order is verified through the codec crates, whose output planes are
compared with FFmpeg channel by channel; unit tests pin layout equality, masks and
capacity. Gates: [docs/quality.md](../../docs/quality.md).

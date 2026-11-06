# triergon-codec-mpeg1

MPEG-1 video encoder (ISO/IEC 11172-2) producing I and P pictures under a VBV
constraint. It is a codec-layer crate whose only dependency is
[triergon-pixel](../pixel/README.md), which supplies the input `Picture` and its
`Geometry`. Each call encodes one picture and returns its complete bytes, with no
reordering delay. It does not convert RGBA (see triergon-pixel), multiplex (see
[`triergon_mpegts::mux`](../mpegts/README.md#writing-ts)) or pace frames.

## Usage

```rust
use triergon_codec_mpeg1::{Encoder, EncoderConfig};
use triergon_pixel::{Geometry, Picture};

let geometry = Geometry::new(960, 540)?;
let b = 2_500_000;
let mut encoder = Encoder::new(EncoderConfig {
    geometry,
    bit_rate: b,          // average target, bit/s
    max_rate: b * 7 / 5,  // VBV fill rate and sequence-header bit_rate, bit/s
    vbv_bits: b / 7,      // VBV buffer size, bits
    gop: 30,              // intra period, pictures
})?;
let mut es = Vec::new();
let picture = encoder.encode(&input, &mut es)?;  // input: Picture of the same geometry
// es: this picture's bytes; picture.key, .index, .bits, .qscale_mean
let recon = encoder.reconstruction();             // what a decoder reconstructs
```

`Encoder::config` returns the configuration and `vbv_underflow_bits` the total VBV
underflow the rate controller observed.

## Supported

| Area | Behaviour |
|---|---|
| Pictures | I and P; no B or D pictures |
| Sequence header | Before every I picture. Size from the `Geometry`, square pixels (pel_aspect_ratio 1), 30 Hz (frame_rate_code 5), bit_rate = `max_rate` in 400 bit/s units, vbv_buffer_size from `vbv_bits` in 16 kbit units (1–1023), default intra and non-intra matrices, constrained_parameters_flag 0 |
| GOP | GOP header before every I picture; time code from the picture index at 30 Hz. An I picture every `gop` pictures; a P picture whose prediction is worse than intra coding across the picture becomes an I picture and restarts the GOP (scene change). Without B pictures every GOP decodes independently; `closed_gop` is set on the first GOP only, as FFmpeg does |
| Slices | One slice per picture; DC and vector predictors run across row ends, and per-row quantiser changes travel in macroblock_quant fields |
| Motion | Forward half-sample vectors, search window ±64 luma samples, f_code up to 4; skipped macroblocks; a P macroblock is intra coded when that is cheaper |
| Quantiser | quantizer_scale 2–31 |
| Rate control | GOP bit allocation by the MPEG-2 Test Model 5 complexity rule (I/P only); one quantiser per picture from a per-type `bits ≈ α · activity / q` model, rows deviating by a bounded amount; VBV modelled as the variable-rate case of Annex C; vbv_delay 0xFFFF |

## Behaviour and limits

- `Encoder::new` refuses `gop == 0` (`Error::Gop`) and a zero `bit_rate`, `max_rate`
  or `vbv_bits`, or a `max_rate` beyond the 18-bit sequence-header field in
  400 bit/s units (`Error::BitRate`). Geometry limits (even, 2–4094) are enforced by
  `Geometry::new`.
- `encode` refuses a picture whose geometry differs from the configuration
  (`Error::PictureGeometry`) before changing any state. `out` is cleared first.
- The input must have its coded padding filled (`Picture::pad_edges`; the pixel
  `Converter` does this).
- Every call encodes and advances the picture index, also for an unchanged source
  picture (a keepalive). The first I picture of an encoder runs a trial pass to seed
  the intra rate model.
- The picture rate is fixed at 30 Hz. Timestamps belong to the caller or muxer.
- A nonzero `vbv_underflow_bits` means some picture exceeded the modelled buffer
  occupancy.
- The `motion` module is public but hidden from the documentation; it is not a
  supported API.

## Conformance

Written from ISO/IEC 11172-2 (sequence to block layers, Table 2-D.4 picture rates,
Annex C VBV model). The reference is FFmpeg 9.0.2 `mpeg1video` with an I/P GOP of 30,
driven through the capture pipeline ([triergon-capture](../capture/README.md)). Per
case the harness requires:

- an error-free independent FFmpeg decode of every picture and identical picture
  types;
- VBV underflow no worse than the reference;
- PSNR-Y within 0.25 dB of the reference at equal or fewer bits. When the reference
  overspends, the encoder is re-run at 1.25–4.5× the requested rate and PSNR-Y is
  interpolated at the reference's bit count.

Unit tests cover the VLC tables (prefix-free and complete), DC differentials, escape
levels, reconstruction oddification and saturation, half-sample rounding, motion
search and bounds, and the zero-residual shortcut against the complete transform;
`tests/encoder.rs` covers configuration and geometry refusal. Gates:
[docs/quality.md](../../docs/quality.md).

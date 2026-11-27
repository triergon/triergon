# triergon-codec-mpeg1

MPEG-1 video encoder (ISO/IEC 11172-2) producing I and P pictures under a VBV
constraint. It is a codec-layer crate whose dependencies are
[triergon-pixel](../pixel/README.md), which supplies the input `CodedPicture` and its
`MacroblockGeometry`, and [triergon-core](../core/README.md), which supplies the storage
`Budget`. Each call encodes one picture and returns its complete bytes, with no
reordering delay. It does not convert RGBA (see triergon-pixel), multiplex (see
[`triergon_mpegts::mux`](../mpegts/README.md#writing-ts)) or pace frames.

## Usage

```rust
use triergon_codec_mpeg1::{Encoder, EncoderConfig, FrameRate};
use triergon_pixel::{CodedPicture, MacroblockGeometry};

let geometry = MacroblockGeometry::new(960, 540)?;
let b = 2_500_000;
let mut encoder = Encoder::new(EncoderConfig {
    geometry,
    frame_rate: FrameRate::Fps30,
    bit_rate: b,          // average target, bit/s
    max_rate: b * 7 / 5,  // VBV fill rate and sequence-header bit_rate, bit/s
    vbv_bits: b / 7,      // VBV buffer size, bits
    gop: 30,              // intra period, pictures
})?;
let input = CodedPicture::new(geometry);         // filled by triergon_pixel::Converter
let mut es = Vec::new();
let picture = encoder.encode(&input, &mut es)?;
// es: this picture's bytes; picture.key, .index, .bits, .qscale_mean
assert!(picture.key && es.starts_with(&[0, 0, 1, 0xb3])); // sequence header first
let recon = encoder.reconstruction();             // PictureView of what a decoder reconstructs
# Ok::<(), Box<dyn std::error::Error>>(())
```

`Encoder::config` returns the configuration and `vbv_underflow_bits` the total VBV
underflow the rate controller observed. `Encoder::with_limits(config, limits)` gives
the encoder a pool of `limits.live_storage_bytes`, and `Encoder::new_in(config, &budget)`
charges a caller's `Budget`.

## Supported

| Area | Behaviour |
|---|---|
| Pictures | I and P; no B or D pictures |
| Sequence header | Before every I picture. Configured geometry, square pixels (pel_aspect_ratio 1), the configured picture rate, bit_rate = `max_rate` in 400 bit/s units, vbv_buffer_size from `vbv_bits` in 16 kbit units (1–1023), default intra and non-intra matrices, constrained_parameters_flag 0 |
| GOP | GOP header before every I picture; non-drop-frame time code from the picture index and configured rate. An I picture every `gop` pictures; a P picture whose prediction is worse than intra coding across the picture becomes an I picture and restarts the GOP (scene change). Without B pictures every GOP decodes independently; `closed_gop` is set on the first GOP only, as FFmpeg does |
| Slices | One slice per picture; DC and vector predictors run across row ends, and per-row quantiser changes travel in macroblock_quant fields |
| Motion | Forward half-sample vectors, search window ±64 luma samples, f_code up to 4; skipped macroblocks; a P macroblock is intra coded when that is cheaper. A picture whose luma repeats the previous source picture's is predicted at the zero vector without a search |
| Quantiser | quantizer_scale 2–31 |
| Rate control | GOP bit allocation by the MPEG-2 Test Model 5 complexity rule (I/P only); one quantiser per picture from a per-type `bits ≈ α · activity / q` model, rows deviating by a bounded amount; VBV modelled as the variable-rate case of Annex C; vbv_delay 0xFFFF |

## Behaviour and limits

- `Encoder::new` refuses `gop == 0` (`EncodeError::Gop`), a zero `bit_rate` or `max_rate`
  or a `max_rate` beyond the 18-bit sequence-header field in 400 bit/s units
  (`EncodeError::BitRate`), and a `vbv_bits` below one picture at the average rate or
  above the 10-bit vbv_buffer_size field in 16 kbit units (`EncodeError::VbvBuffer`).
  `MacroblockGeometry::new` requires even dimensions from 2 to 4094.
- `encode` refuses a picture whose geometry differs from the configuration
  (`EncodeError::PictureGeometry`) before changing any state. A successful encode replaces the
  contents of `out`.
- Storage sized from the configuration is charged to the encoder's budget before it
  is allocated: the reference, reconstructed and previous source pictures, the
  macroblock and motion arrays, the coarse planes (reserved together on first motion
  search; skipped while the source luma repeats), the bit writer and the caller's `out`
  at the bound of one picture (the VBV buffer plus one full and 16 bytes per further
  macroblock), and the first I picture's trial pass (one row). The default
  `Limits::live_storage_bytes` is 96 MiB; 4094x4094 at the largest signalled VBV buffer
  (1023 × 16 kbit) needs about 74 MiB. A refused charge is `EncodeError::Budget`, from the
  constructor or before coding the picture; `out` then keeps its previous bytes, and
  the same input can be retried at the same picture index once space is released.
- The input must have its coded padding filled (`CodedPicture::pad_edges`; the pixel
  `Converter` does this).
- Every successful call encodes and advances the picture index, also for an unchanged
  source picture (a keepalive). The first I picture runs a trial pass to seed the intra
  rate model.
- The picture rate controls the sequence header, GOP time code and VBV refill.
  Timestamps belong to the caller or muxer.
- A nonzero `vbv_underflow_bits` means some picture exceeded the modelled buffer
  occupancy.

## Conformance

Written from ISO/IEC 11172-2 (sequence to block layers, Table 2-D.4 picture rates,
Annex C VBV model). The reference is FFmpeg 9.0.2 `mpeg1video` with an I/P GOP of 30,
driven through the capture pipeline ([triergon-capture](../capture/README.md)); the
comparison is described in [harness/capture](../../harness/capture/README.md) and its
results in [docs/status.md](../../docs/status.md).

The [encoder budget comparison](../../harness/features/mpeg1-encoder-budget/contract.json)
checks constructor limits, storage release, and byte-identical output after
backpressure for aligned and padded pictures.

# triergon-pixel

Picture types and the fixed pixel conversions shared by the capture, playback and
subtitle sides: borrowed views of decoded pictures, a macroblock-aligned YUV 4:2:0
picture, RGBA → YUV 4:2:0 with ITU-R BT.601 coefficients, a letterbox that converts a
decoded 4:2:0 picture into a 1920×1080 RGBA canvas exactly as FFmpeg's scale/pad
chain does, and the Y'CbCr → RGB of subtitle palette entries. It is the pixel layer below the codec crates and depends only on `std`.
It does no general scaling, colour management or other canvas sizes.

## Usage

RGBA in, coded YUV 4:2:0 out (the encoder input of
[triergon-codec-mpeg1](../codec-mpeg1/README.md)):

```rust
use triergon_pixel::{Converter, Geometry, Orientation, Picture};

let geometry = Geometry::new(960, 540)?;           // even, 2..=4094
let rgba = vec![128u8; geometry.rgba_len()];        // the caller's frame: mid grey
let mut picture = Picture::new(geometry);
let mut converter = Converter::new(geometry);       // reusable scratch
converter.convert(&rgba, Orientation::BottomUp, &mut picture)?;
// picture.y(), cb(), cr(): coded planes, strides geometry.luma_stride() / chroma_stride()
assert!(picture.cb().iter().chain(picture.cr()).all(|&c| c == 128)); // grey stays neutral
# Ok::<(), triergon_pixel::Error>(())
```

`rgba_to_yuv420(&rgba, orientation, &mut picture)` is the same conversion with its
own scratch. `Picture::visible_yuv420()` packs the visible planes (Y, Cb, Cr) for
comparisons.

Decoded picture in, letterboxed RGBA out:

```rust
use triergon_pixel::letterbox::{CANVAS_BYTES, Scaler, fit};
use triergon_pixel::{Colour, PictureView, Plane};

// A 64x36 8-bit 4:2:0 picture, lent the way a decoder lends its output.
let (luma, chroma) = (vec![235u8; 64 * 36], vec![128u8; 32 * 18]);
let view = PictureView {
    width: 64,
    height: 36,
    bit_depth: 8,
    planes: [
        Plane::U8 { data: &luma, stride: 64, offset: 0 },
        Plane::U8 { data: &chroma, stride: 32, offset: 0 },
        Plane::U8 { data: &chroma, stride: 32, offset: 0 },
    ],
    pts: None,
    sar: (1, 1),
    colour: Colour { matrix: 1, full_range: false },
};
let mut scaler = Scaler::default();                 // caches per-geometry filter plans
let mut canvas = vec![0u8; CANVAS_BYTES];
scaler.letterbox(&view, &mut canvas);               // writes the whole canvas
let placed = fit(view.width, view.height);          // Fit { width, height, x, y }
assert_eq!((placed.width, placed.height, placed.x, placed.y), (1920, 1080, 0, 0));
```

## Supported

| Area | Coverage |
|---|---|
| Picture views | `PictureView`: cropped `width`/`height`, `bit_depth`, three `Plane`s (`U8` or `U16` samples with `stride` and `offset`), caller `pts`, `sar`, `Colour` (H.273 `matrix` code and `full_range`) |
| Coded picture | `Picture` at the macroblock-aligned size of a `Geometry`; padding replicates the nearest visible edge sample (`pad_edges`) |
| RGBA → YUV | BT.601 matrix, limited range (Y 16–235, Cb/Cr 16–240), alpha ignored; top-down or bottom-up rows (`Orientation`), read without a separate flip pass; FFmpeg's ARMv7 result, which depends on the width (`block_path`) |
| Letterbox input | 4:2:0, 8-bit and high bit depth (10-bit verified); limited and full range |
| Palette Y'CbCr → RGB | `YcbcrMatrix::BT601` and `BT709`, studio range in, full range out, `ChannelRounding::Nearest` or `HalfDown`; `rgb(y, cr, cb, rounding)` gives `0xRRGGBB` |
| Letterbox matrices | H.273 codes 1 (BT.709), 4 (FCC), 7 (SMPTE 240M) and 9 (BT.2020 NCL) select their coefficients; every other code uses BT.601. The same-size 8-bit path also maps code 10 to BT.2020 |

## Behaviour and limits

**Geometry.** `Geometry::new` accepts non-zero even sizes that fit the 12-bit MPEG-1
size fields (at most 4094) and refuses others with `Error::Geometry`. The coded size
rounds each dimension up to 16; `luma_stride` is `16 × macroblock_width()` and
`chroma_stride` half of it.

**RGBA → YUV.** The conversion is the one FFmpeg 9.0.2's `swscale` (`SWS_BILINEAR`,
RGBA to NV12) gives when built for 32-bit ARMv7, on every target. Its result depends
on the width; `block_path(geometry)` tells which applies:

- Widths of at least 16 that are a multiple of 4 take the block path: luma is
  `(66 R + 129 G + 25 B + 4224) >> 8`; each chroma sample comes from the 2×2 block it
  covers, every channel averaged as `(sum + 2) >> 2`, then
  `Cb = (−19 R − 37 G + 56 B + 16448) >> 7` and `Cr = (56 R − 47 G − 9 B + 16448) >> 7`.
- Every other width takes the filtered path: fixed-point coefficients scaled by 2^16
  carry the 219/255 (luma) and 224/255 (chroma) range factors, each chroma row sums
  to zero so neutral grey maps to exactly 128, and chroma is the mean of each
  horizontal pixel pair, decimated vertically with centre-sited taps 1/8, 3/8, 3/8,
  1/8 over rows 2k−1 … 2k+2 with the edge rows replicated, rounded once. Below a
  width of 16 this is what FFmpeg's ARMv7 build does too, within 1 for pictures of at
  least 8 rows. At widths of 2 modulo 4 from 18 up, FFmpeg's ARMv7 build faults
  instead of converting, and this is FFmpeg's AArch64 result.

FFmpeg built for AArch64 takes the filtered path at every width. Input shorter than
`width × height × 4` is `Error::InputLength`; longer input is ignored past that
length. `convert` replicates the coded padding.

**Plane layout.** Visible sample (x, y) of a `Plane` is
`data[offset + y * stride + x]`. Decoders should hand over the whole decoded plane
with `offset = crop_top * stride + crop_left` (chroma: both halved), so samples left
of the crop window stay reachable. A view whose `data` starts at the visible origin
(`offset < stride`) is valid but treated as uncropped on the left.

**Palette Y'CbCr → RGB.** The coefficients are the matrix's, widened by 255/219
(luma) and 255/224 (chroma) and rounded to 1/1024; each channel sum is rounded once,
to nearest or half a step below, and clamped to 0–255. This is how FFmpeg's DVD, DVB
and PGS subtitle decoders convert palette entries, and how its MP4 demuxer converts
a DVD colour table (half a step below).

**Letterbox.** `Scaler::letterbox` writes the complete canvas: opaque black padding
and the fitted picture. It reproduces FFmpeg's
`scale=1920:1080:force_original_aspect_ratio=decrease,pad=1920:1080:(ow-iw)/2:(oh-ih)/2,format=rgba`
bit for bit:

- `fit` scales the display size to fit, rounded to nearest; the offset is
  `(canvas − fitted) / 2` rounded down. Only `width` and `height` are used; `sar` is
  not applied.
- Same-size 8-bit pictures take a direct converter: luma at 15 fractional bits, each
  chroma product floored to 14 bits, one final rounding, chroma shared by each 2×2
  block.
- Everything else (any resampling, every high-bit-depth picture) takes a separable
  path: Keys bicubic taps (C = 3/5) in FFmpeg's fixed-point domain, a horizontal pass
  to 15-bit intermediates and a vertical pass with 12-bit taps, chroma resampled as
  its own plane, then table-driven conversion. An odd fitted width switches to
  FFmpeg's full-width-chroma output stage, including its wrapping 32-bit sum.
- FFmpeg's decoder applies a left crop only as far as its plane pointers stay
  32-byte aligned; the scaler widens the picture by the part of a left crop FFmpeg
  keeps, read from the plane `offset`.

`letterbox` panics if the canvas is not exactly `CANVAS_BYTES` long. Plane data must
cover the declared size and offsets. Filter plans are cached per width, height and
bit depth.

## Conformance

The conversion follows ITU-R BT.601 and ITU-T H.273 MatrixCoefficients. The
letterbox rules were recovered by black-box observation of FFmpeg 9.0.2 (crafted
impulses, ramps and constant fields through its public filter API).

- `tests/rgba_reference.rs` compares whole canvases against digests of 15
  pseudo-random pictures passed through FFmpeg 9.0.2's chain: 8- and 10-bit, 64×240
  to 3840×2160, limited and full range, seven matrix codes, odd fitted widths and a
  tall 528×4216 picture (`tests/fixtures/rgba-reference-digests.txt`).
- A unit test pins the BT.601 palette conversion, in both roundings, against
  FFmpeg's DVD palettes.
- Unit tests pin the fit rule, taps against reference measurements (including
  near ties), the left-crop alignment rule, filter normalisation, the BT.601
  coefficients against their derivation, primaries and extremes, centre-sited
  chroma, bottom-up input and edge padding.
- The RGBA → YUV conversion is compared with FFmpeg's `swscale` (`SWS_BILINEAR`)
  in the capture harness: every Y, Cb and Cr sample within ±1 of the reference. See
  [triergon-capture](../capture/README.md#conformance). The block path's rule comes
  from black-box probes of the capture reference built for ARMv7 (its `convert`
  mode): every one of the 2^24 colours for luma and for uniform 2×2 blocks, random
  and near-tie blocks for the rounding, and random pictures at widths 16 to 4092,
  heights 2 to 540 and both orientations, all exact. Unit tests pin the path
  choice, observed block-path values and both paths' row siting.
- Open: on the filtered path, FFmpeg gives pictures of 2, 4 and 6 rows other
  vertical weights (2 rows: the lower row alone; 4 rows: each pair's mean; 6 rows:
  3/7, 3/7, 1/7 for the top sample). Both of its builds agree there, and the
  filtered path differs from them.

Gates: [docs/quality.md](../../docs/quality.md).

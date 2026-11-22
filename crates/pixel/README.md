# triergon-pixel

Picture types and the fixed pixel conversions shared by the capture, playback and
subtitle sides: borrowed views of decoded pictures, a macroblock-aligned YUV 4:2:0
picture, RGBA → YUV 4:2:0 with ITU-R BT.601 coefficients, a letterbox that converts a
decoded 4:2:0 picture into a 1920×1080 RGBA canvas exactly as FFmpeg's scale/pad
chain does, and the Y'CbCr → RGB of subtitle palette entries. It is the pixel layer
below the codec crates and depends only on `std`. It does no general scaling, colour
management or other canvas sizes.

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
| RGBA → YUV | BT.601 matrix, limited range (Y 16–235, Cb/Cr 16–240), alpha ignored; top-down or bottom-up rows (`Orientation`), read without a separate flip pass; width-dependent conversion or filtering at every width |
| Letterbox input | 4:2:0, 8-bit and high bit depth (10-bit verified); limited and full range |
| Palette Y'CbCr → RGB | `YcbcrMatrix::BT601` and `BT709`, studio range in, full range out, `ChannelRounding::Nearest` or `HalfDown`; `rgb(y, cr, cb, rounding)` gives `0xRRGGBB` |
| Letterbox matrices | H.273 codes 1 (BT.709), 4 (FCC), 7 (SMPTE 240M) and 9 (BT.2020 NCL) select their coefficients; every other code uses BT.601. The same-size 8-bit path also maps code 10 to BT.2020 |

## Behaviour and limits

**Geometry.** `Geometry::new` accepts non-zero even sizes that fit the 12-bit MPEG-1
size fields (at most 4094) and refuses others with `Error::Geometry`. The coded size
rounds each dimension up to 16; `luma_stride` is `16 × macroblock_width()` and
`chroma_stride` half of it.

**RGBA → YUV.** `Converter::new` follows the width-dependent output of FFmpeg
9.0.2's `swscale` (`SWS_BILINEAR`, RGBA to NV12) on 32-bit ARMv7.

- Widths of at least 16 that are a multiple of 4 take the block path: luma is
  `(66 R + 129 G + 25 B + 4224) >> 8`; each chroma sample comes from the 2×2 block it
  covers, every channel averaged as `(sum + 2) >> 2`, then
  `Cb = (−19 R − 37 G + 56 B + 16448) >> 7` and `Cr = (56 R − 47 G − 9 B + 16448) >> 7`.
- Every other width takes the filtered path. It averages each horizontal pixel pair
  and rounds the chroma terms to six fractional bits. The vertical filter applies
  centre-sited taps 1/8, 3/8, 3/8 and 1/8 over rows 2k−1 … 2k+2 with edge rows
  replicated. It rounds to eight bits after filtering. Below width 16 this follows
  FFmpeg's ARMv7 build; at widths of 2 modulo 4 from 18 up, where that build faults, its
  AArch64 build.

On the filtered path, a 2-row picture takes chroma from the lower row. A 4-row
picture averages each pair of rows. The first chroma row of a 6-row picture
uses weights 1755, 1756 and 585 divided by 4096. The other two rows use the
usual four taps.

`Converter::filtered` uses this filter at every width, as FFmpeg's AArch64 build does.
Input shorter than `width × height × 4` is `Error::InputLength`; longer input is ignored
past that length. `convert` replicates the coded padding.

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

**Letterbox.** `Scaler::letterbox` writes the complete canvas: opaque black padding and
the fitted picture. It reproduces FFmpeg's
`scale=1920:1080:force_original_aspect_ratio=decrease,pad=1920:1080:(ow-iw)/2:(oh-ih)/2,format=rgba`
bit for bit:

- `fit` scales the display size to fit, rounded to nearest; the offset is
  `(canvas − fitted) / 2` rounded down. Only `width` and `height` are used; `sar` is
  not applied.
- Same-size 8-bit pictures take a direct converter; everything else (any resampling,
  every high-bit-depth picture) takes FFmpeg's separable bicubic path in its fixed-point
  domain, including its full-width-chroma output stage for an odd fitted width.
- FFmpeg's decoder applies a left crop only as far as its plane pointers stay
  32-byte aligned; the scaler widens the picture by the part of a left crop FFmpeg
  keeps, read from the plane `offset`.

`letterbox` panics if the canvas is not exactly `CANVAS_BYTES` long. Plane data must
cover the declared size and offsets. Filter plans are cached per width, height and
bit depth.

## Conformance

The conversion follows ITU-R BT.601 and ITU-T H.273 MatrixCoefficients. The letterbox
rules were recovered by black-box observation of FFmpeg 9.0.2 (crafted impulses, ramps
and constant fields through its public filter API);
`tests/fixtures/rgba-reference-digests.txt` holds the digests of 15 pseudo-random
pictures passed through FFmpeg 9.0.2's chain. The RGBA → YUV block path's rule comes from
black-box probes of the capture reference built for ARMv7 (its `convert` mode): every one
of the 2^24 colours for luma and for uniform 2×2 blocks, random and near-tie blocks for
the rounding, and random pictures at widths 16 to 4092, heights 2 to 540 and both
orientations. The filtered widths below 16 come from the [capture reference
probe](../../harness/capture/README.md#short-frames). The capture comparison is described
in [harness/capture](../../harness/capture/README.md); results in
[docs/status.md](../../docs/status.md).

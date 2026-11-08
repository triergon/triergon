# triergon-raster

The outline and bitmap stages of an ASS subtitle renderer, matching libass
0.17.5 bit for bit: glyph outlines in 26.6 fixed point, their 3D transform and
its quantisation, border stroking, the tile rasteriser, `\blur` and `\be`, the
composition of a run's fill, border and shadow bitmaps, and rectangle clipping
into image parts. It depends on [`triergon-core`](../core) for `Budget`,
`WorkBudget` and errors, and takes glyph outlines in the point form of
[`triergon-font`](../font). Fonts, shaping, layout and ASS parsing belong to
other crates.

## Usage

A `Raster` holds the scratch every stage reuses. Each stage takes the
`WorkBudget` of the frame it belongs to, so one budget bounds all the work of a
frame.

```rust
use triergon_core::{Budget, WorkBudget};
use triergon_font::{OutlinePoint, Point, PointKind};
use triergon_raster::{
    BorderRequest, Effects, Filter, FilterFlags, FrameScale, GlyphRef, GlyphRequest, Limits,
    Outline, Placement, Raster, Run, Shape, Vector,
};

let budget = Budget::new(64 << 20)?;
let mut raster = Raster::new(&budget, Limits::default());
let mut work = WorkBudget::new(1 << 28);

// A glyph outline as a font gives it at a size: 26.6, y up, tagged points.
let on = |x, y| OutlinePoint { point: Point::new(x, y), kind: PointKind::On };
let points = [on(0, 0), on(640, 0), on(640, 640), on(0, 640)];
let mut outline = Outline::new(&budget);
outline.push_glyph(&points, &[4])?;
let shape = Shape::from_outlines(outline, Outline::new(&budget));

// One glyph at (100, 50) pixels with a 2 pixel border, rotated by \frz15.
let placement = Placement {
    rotation: [0.0, 0.0, 15.0],
    shear: [0.0, 0.0],
    scale: [1.0, 1.0],
    shift: Vector::new(0, 0),
    ascender: 640,
    descender: 0,
    position: Vector::new(100 * 64, 50 * 64),
    outline_scale: [1.0, 1.0],
    outline_offset: [0.0, 0.0],
};
let frame = FrameScale { blur: [1.0, 1.0], border: [1.0, 1.0], pixel_aspect: 1.0 };
let request = GlyphRequest {
    shape: &shape,
    placement,
    frame,
    border: BorderRequest::Outline { size: [2.0, 2.0] },
    karaoke: false,
};
let mut run = Run::default();
let bitmaps = raster.render(&request, &mut run, &mut work)?;

// The run's fill, border and shadow, blurred with \blur1.
let flags = FilterFlags { border: true, shadow: true, ..FilterFlags::default() };
let effects = Effects { flags, be: 0, blur: 1.0, shadow: [2.0, 2.0] };
let glyph = GlyphRef {
    glyph: bitmaps.glyph.as_ref(),
    border: bitmaps.border_bitmap(),
    position: Vector::new(0, 0),
    border_position: Vector::new(
        bitmaps.border_position.x - bitmaps.position.x,
        bitmaps.border_position.y - bitmaps.position.y,
    ),
};
let composite = raster.compose(&Filter::new(&effects, &frame), &[glyph], &mut work)?;
assert!(composite.fill.is_some() && composite.border.is_some() && composite.shadow.is_some());
# Ok::<(), Box<dyn std::error::Error>>(())
```

A bitmap's `position` is its first pixel relative to the origin it is placed
against; `GlyphBitmaps::position` and `GlyphRef::position` are those origins,
for a glyph in the frame and for a run's glyphs relative to the run.

## What it matches

| Stage | libass | Here |
|---|---|---|
| Outlines | `ass_outline.c`: conversion from font outlines, rectangles, rotation by 90°, affine and perspective transforms, the karaoke leftmost x, control boxes | `Outline`, `Matrix`, `Projection` |
| Borders | `ass_outline_stroke`, and the stroking scale `get_bitmap_glyph` chooses | `Raster::stroke`, `Pen` |
| Rasteriser | `ass_rasterizer.c` with 16×16 tiles (the default build, without large tiles) | `Raster::rasterize`, `Shape` |
| Glyph bitmaps | `calc_transform_matrix`, `quantize_transform`, `restore_transform`, `get_bitmap_glyph` with its border and the opaque box of `\bord` style 3 | `Raster::render`, `GlyphRequest`, `Placement`, `FrameScale`, `Run` |
| Blur | `ass_blur.c` (the cascade Gaussian and its coefficient solver), the `\be` box blur, `ass_synth_blur` | `Raster::blur` |
| Composition | `quantize_blur`, `restore_blur`, `ass_composite_construct`, `ass_fix_outline`, `ass_shift_bitmap`, the additive blend | `Raster::compose`, `Effects`, `Filter`, `Bitmap` |
| Clipping | `render_glyph`: `\clip` and `\iclip` rectangles and the `\kf` split; the multiply blends of vector clips | `Clip`, `ClipFrame`, `Bitmap::multiply`, `Bitmap::multiply_inverse` |

The arithmetic is libass's own: 26.6 integer subdivision with its rounding, the
16-bit wrapping of the tile and blur engines, `lrint` rounding to even, and the
C `FFMAX`/`FFMIN` treatment of NaN. Floating-point expressions keep the C
source's order and grouping; the reference is built without floating-point
contraction (`-ffp-contract=off`), so neither side fuses multiply-adds and the
result does not depend on the target. Two further properties are matched:

- **libass's C engine.** The gated reference runs libass's plain C bitmap
  engine on every host (`ass_bitmap_engine_init(ASS_CPU_FLAG_NONE)`: 16×16
  tiles and rows aligned to 16 bytes), which is what libass runs on 32-bit
  ARMv7, where it has no assembly. Bitmaps keep its stride (the width rounded
  up to 16), every pass works over the width and the row padding stays zero.
  libass's SIMD engines depart from it, for example by blurring `\be` over
  whole rows into the padding, and AVX2 pads rows to 32 bytes; the
  `raster-simd` feature records where, on demand.
- **libm.** Sines, cosines, exponentials and logarithms come from the platform
  library, as libass's do.

## Behaviour and limits

Every input-dependent allocation is charged to the `Budget` a value was created
with: outlines, bitmaps and the rasteriser's and blur's scratch, which a
`Raster` keeps and reuses. Work is charged to the `WorkBudget` the caller passes
to each stage, one per frame: subdivision steps, emitted points, polyline
segments and 16×16 tiles filled, and every 16 pixels of a row a blur stage
writes, charged before the stage allocates or runs. `Limits` bounds sizes: the points of a stroked
outline (`outline_points`) and the bytes of a bitmap (`bitmap_bytes`). Running
out of work or exceeding a limit is `Error::Policy`. The bitmap default is
`MAX_BITMAP_BYTES`, libass's own bound just under 2³¹ bytes, so only the budget
limits what libass draws; a bitmap beyond that bound is not drawn, as in
libass, and a blur whose intermediate planes would pass libass's bound leaves
the bitmap unblurred, as libass does. Outline coordinates stay within
±(2²⁸ − 1); conversions and transforms that leave the range are
`Error::Invalid`, and the glyph pipeline drops such outlines as libass does. No
input panics, including arithmetic overflow in debug builds. Where libass's
result depends on the target for values far out of range (conversions of
doubles beyond the `long` range, shifts by 32 or more), the crate follows
libass on 32-bit ARM with GCC (`armv7-unknown-linux-gnueabihf`) on every
target: conversions saturate, NaN converts to 0 and such shifts give 0, so a
coordinate rounds out of range as libass does there.

Not covered: glyph loading, hinting, synthetic styles and decoration lines
(`triergon-font` and the renderer); drawing parsing and the image list,
including where vector clips overlap and how their images are allocated, which
belong to the renderer. The large-tile build of libass
(`--enable-large-tiles`) is not matched.

## Verification

`cargo test -p triergon-raster` runs unit tests and `tests/robustness.rs`,
which mutates random glyph sources and runs every operation on them. The
features `raster-glyphs`, `raster-outlines`, `raster-bitmaps` and
`raster-sweep` (`harness/features/raster-*`, run by
`cargo xtask raster compare [--all]`) compare
[raster-worker](../raster-worker/README.md) with the pinned libass build on
its C engine exactly: outline points and segments, bitmap positions, sizes and every pixel,
glyph and run positions, and clip parts; see [docs/status.md](../../docs/status.md).

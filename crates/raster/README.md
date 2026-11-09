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
    Border, Effects, FrameScale, GlyphRequest, Limits, Outline, Placement, Raster, Run, Shadow,
    Shape, Vector,
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

// One glyph at (100, 50) pixels, rotated by \frz15.
let placement = Placement {
    rotation: [0.0, 0.0, 15.0],
    shear: [0.0, 0.0],
    scale: [1.0, 1.0],
    shift: Vector::new(0, 0),
    ascender: 640,
    descender: 0,
    position: Vector::new(100 * 64, 50 * 64),
    advance: 640,
    scale_fix: 1.0,
    outline_scale: [1.0, 1.0],
    outline_offset: [0.0, 0.0],
};
let frame = FrameScale { blur: [1.0, 1.0], border: [1.0, 1.0], pixel_aspect: 1.0 };

// Its run has a 2 pixel border, a shadow and \blur1.
let effects = Effects {
    border: Border::Outline { size: [2.0, 2.0], keeps_fill: false },
    shadow: Shadow { offset: [2.0, 2.0], keeps_fill: true },
    blur: 1.0,
    be: 0,
};
let request = GlyphRequest { shape: &shape, placement, frame, effects: &effects, karaoke: false };
let mut run = Run::default();
let glyph = raster.render(&request, &mut run, &mut work)?.ok_or("nothing drawn")?;

// The run's fill, border and shadow.
let composite = raster.compose(&effects, &frame, [&glyph], &mut work)?;
assert_eq!(composite.origin, glyph.origin());
assert!(composite.fill.is_some() && composite.border.is_some() && composite.shadow.is_some());
# Ok::<(), Box<dyn std::error::Error>>(())
```

A bitmap's `position` is its first pixel relative to the origin it is placed
against: a glyph's fill and border each count from their `PlacedBitmap::origin`,
and a run's composite from `Composite::origin`, the least origin of its glyphs.

`Raster::render` draws a glyph in one call. A renderer that keeps the bitmaps it
draws, as libass's caches do, renders through `Raster::render_with` and a
`GlyphSource` of its own, which hands out the fill, the opaque box and the
border it has and draws only the others, held through a handle of its own
(`GlyphSource::Bitmap`; `Raster::render` uses `Arc<Bitmap>`), which the run
and `Raster::compose` hold them by. The steps are public too: `Run::place`
quantises and places the glyph without drawing it, giving the `BitmapTransform`
its fill and border are drawn under; `Raster::stroke_border` strokes the border
and `Raster::draw` draws each, the opaque box from `Shape::unit_box`; and
`Run::add` adds what was drawn to the run. A bitmap is then keyed by its shape
and its transform, a stroked border by its glyph and its `BorderStroke`, and a
composite by `Effects::key` and its glyphs' bitmaps and places, as libass keys
them.

## What it matches

| Stage | libass | Here |
|---|---|---|
| Outlines | `ass_outline.c`: conversion from font outlines, rectangles, rotation by 90°, affine and perspective transforms, the karaoke leftmost x, control boxes | `Outline`, `Matrix`, `Projection` |
| Borders | `ass_outline_stroke`, and the stroking scale `get_bitmap_glyph` chooses | `Raster::stroke`, `Pen` |
| Rasteriser | `ass_rasterizer.c` with 16×16 tiles (the default build, without large tiles) | `Raster::rasterize`, `Shape` |
| Glyph bitmaps | `calc_transform_matrix`, `quantize_transform`, `restore_transform`, `get_bitmap_glyph` with its border and the opaque box of `\bord` style 3 | `Raster::render`, `GlyphRequest`, `Placement`, `FrameScale`, `Run`, `GlyphPlacement`, `GlyphBitmaps`; `Quantized::standalone` and `Raster::draw` for the drawing of a vector clip |
| Blur | `ass_blur.c` (the cascade Gaussian and its coefficient solver), the `\be` box blur, `ass_synth_blur` | `Raster::blur` |
| Composition | the filter flags `render_and_combine_glyphs` derives, `quantize_blur`, `restore_blur`, `ass_composite_construct`, `ass_fix_outline`, `ass_shift_bitmap`, the additive blend | `Raster::compose`, `Effects`, `Border`, `Shadow`, `Composite`, `Bitmap` |
| Clipping | `render_glyph`: `\clip` and `\iclip` rectangles and the `\kf` split; the multiply blends of vector clips | `Clip`, `ClipFrame`, `Bitmap::multiply`, `Bitmap::multiply_inverse` |

`Raster::stroke` writes the outer (+offset) and inner (−offset) outline of a
border in one pass over the source's contours. Each segment is joined to the
offset before it: nearly parallel normals merge, and a corner gets a round cap
of quadratic arcs on one side and a spike to the corner on the side the turn
folds over. A line offsets directly; a spline is fitted by one offset spline
when libass's radial and angular error bounds allow and is halved otherwise.
Where an offset intersects itself, that part is replaced by libass's polyline,
and a contour that never leaves a point is drawn as a circle. The result
matches libass point for point and segment for segment.

The arithmetic is libass's own: 26.6 integer subdivision with its rounding, the
16-bit wrapping of the tile and blur engines, `lrint` rounding to even, and the
C `FFMAX`/`FFMIN` treatment of NaN. Floating-point expressions keep the C
source's order and grouping, and the reference is built without
floating-point contraction (`-ffp-contract=off`), so neither side fuses
multiply-adds: additions, multiplications, divisions and square roots, which
IEEE 754 rounds correctly, give the same result on every target. Two further
properties are matched:

- **libass's C engine.** The gated reference runs libass's plain C bitmap
  engine on every host (`ass_bitmap_engine_init(ASS_CPU_FLAG_NONE)`: 16×16
  tiles and rows aligned to 16 bytes), which is what libass runs on 32-bit
  ARMv7, where it has no assembly. Bitmaps keep its stride (the width rounded
  up to 16), every pass works over the width and the row padding stays zero.
  libass's SIMD engines depart from it, for example by blurring `\be` over
  whole rows into the padding, and AVX2 pads rows to 32 bytes; the
  diagnostic `raster-simd` feature records where.
- **libm.** Five functions come from the platform's libm, as libass's do:
  the sine and cosine of the `\frx`, `\fry` and `\frz` rotations
  (`Placement::rotation`), `log1p` and `expm1`, which quantise the `\blur`
  radius and restore its variance (`Effects::blur`), and `exp` in the
  Gaussian blur's coefficients (`Raster::blur` and `Raster::compose`); `log`
  only takes the constant `log 256`. Neither Apple's libm nor glibc
  guarantees correct rounding for these, so a result that depends on them is
  the one libass computes against the same libm, and platforms whose libms
  differ can differ there. The comparison covers Apple's libm on macOS AArch64
  and glibc on Linux x86_64, AArch64 and 32-bit ARMv7, where every case of
  the C engine matches apart from two sweep cases on ARMv7 whose composite cannot be
  allocated in a 32-bit address space ([status](../../docs/status.md)).

## Behaviour and limits

Every input-dependent allocation is charged to the `Budget` a value was created
with: outlines, bitmaps and the rasteriser's and blur's scratch, which a
`Raster` keeps and reuses. Work is charged to the `WorkBudget` the caller passes
to each stage, one per frame: subdivision steps (each spline part the
stroker offsets and each arc it subdivides), emitted points, polyline
segments and 16×16 tiles filled, and every 16 pixels of a row a blur stage
writes or a run's glyph bitmaps are summed into, charged before the stage
allocates or runs. `Limits` bounds sizes: the points of a stroked
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
Runtime and memory are not measured yet. When they are, the reference is timed
from a build of the same libass without its zeroed locals, whose code layout
alone slows small glyphs by up to 40%, and its timed region holds only the work
the candidate's does, without libass's per-run cache reset; see the libass pin
in [docs/quality.md](../../docs/quality.md#reference-and-corpus).

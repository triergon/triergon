# triergon-raster

The outline and bitmap stages of an ASS subtitle renderer, matching libass
0.17.5 bit for bit. The crate root holds the stages: glyph outlines in 26.6
fixed point and their transforms, the quantisation of a transform into a
bitmap key, border stroking, the tile rasteriser, the Gaussian and box blurs,
and the blends of alpha bitmaps. `compat::libass` holds how libass's renderer
uses them: the 3D transform of a glyph and its quantisation against the
glyph's run, the scale a border is stroked at, the opaque box of `\bord`
style 3, the composition of a run's fill, border and shadow, and rectangle
clipping into image parts. It depends on [`triergon-core`](../core) for
`Budget`, `WorkBudget` and errors, and takes glyph outlines in the point form
of [`triergon-font`](../font). Fonts, shaping, layout and ASS parsing belong
to other crates.

## Usage

A `Raster` holds the scratch every stage reuses. Each stage takes the
`WorkBudget` of the frame it belongs to, so one budget bounds all the work of a
frame.

```rust
use triergon_core::{Budget, WorkBudget};
use triergon_font::{OutlinePoint, Point, PointKind};
use triergon_raster::compat::libass::{
    Border, Composite, Effects, FrameScale, GlyphRequest, Placement, Run, Shadow,
};
use triergon_raster::{Limits, Outline, Raster, Shape, Vector};

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
let glyph = run.render(&mut raster, &request, &mut work)?.ok_or("nothing drawn")?;

// The run's fill, border and shadow.
let composite = Composite::new(&mut raster, &effects, &frame, [&glyph], &mut work)?;
assert_eq!(composite.origin, glyph.origin());
assert!(composite.fill.is_some() && composite.border.is_some() && composite.shadow.is_some());
# Ok::<(), Box<dyn std::error::Error>>(())
```

A bitmap's `position` is its first pixel relative to the origin it is placed
against: a glyph's fill and border each count from their `PlacedBitmap::origin`,
and a run's composite from `Composite::origin`, the least origin of its glyphs.

`Run::render` draws a glyph in one call. A renderer that keeps the bitmaps it
draws, as libass's caches do, renders through `Run::render_with` and a
`GlyphSource` of its own. The source hands out the fill, the opaque box and
the border it has and draws only the others (`Raster::draw` under the
`Quantized` transform it is given, `BorderStroke::stroke` for a border), held
through a handle of its own (`GlyphSource::Bitmap`; `Run::render` uses
`Arc<Bitmap>`), which the run and `Composite::new` hold them by. A bitmap is
keyed by its shape and its `BitmapTransform`, a stroked border by its glyph
and its `BorderStroke`, and a composite by `Effects::key` and its glyphs'
bitmaps and places, as libass keys them.

## What it matches

| Stage | Behaviour | Here |
|---|---|---|
| Outlines | conversion from font outlines, rectangles, rotation by 90°, affine and perspective transforms, control boxes | `Outline`, `Matrix`, `Projection` |
| Transforms | quantisation of a transform into the key a bitmap is drawn under, and the matrix it is drawn with | `BitmapTransform`, `Quantized`, `Raster::draw` |
| Borders | the stroker | `Raster::stroke`, `Pen` |
| Rasteriser | the tile rasteriser with 16×16 tiles (libass's default build, without large tiles) | `Raster::rasterize`, `Shape` |
| Blur | the cascade Gaussian and its coefficient solver, the `\be` box blur | `Raster::blur` |
| Bitmaps | the additive blend, the multiply blends of vector clips, the border's subtraction of the fill, the sub-pixel shift of a shadow | `Bitmap` |
| Glyphs | the transform of `\frx` `\fry` `\frz` `\fax` `\fay`, its quantisation against the run, the stroking scale of a border and the opaque box of `\bord` style 3 | `compat::libass`: `Run`, `GlyphRequest`, `Placement`, `FrameScale`, `GlyphSource`, `BorderStroke` |
| Composition | the effects a run's style and tags give, the quantised `\blur` and shadow offset, the summed, blurred and shifted fill, border and shadow | `compat::libass`: `Composite`, `Effects`, `Border`, `Shadow` |
| Clipping | `\clip` and `\iclip` rectangles and the `\kf` split into image parts | `compat::libass`: `Clip`, `ClipFrame` |

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
16-bit wrapping of the tile and blur engines, rounding half to even, and the C
`a > b ? a : b` treatment of NaN. Floating-point expressions keep libass's
order and grouping, and the reference is built without floating-point
contraction (`-ffp-contract=off`), so neither side fuses multiply-adds:
additions, multiplications, divisions and square roots, which IEEE 754 rounds
correctly, give the same result on every target. Two further properties are
matched:

- **libass's C engine.** The gated reference runs libass's plain C bitmap
  engine on every host (16×16 tiles and rows aligned to 16 bytes), which is
  what libass runs on 32-bit ARMv7, where it has no assembly. Bitmaps keep its
  stride (the width rounded up to 16), every pass works over the width and the
  row padding stays zero. libass's SIMD engines depart from it, for example by
  blurring `\be` over whole rows into the padding, and AVX2 pads rows to 32
  bytes; the diagnostic `raster-simd` feature records where.
- **libm.** Five functions come from the platform's libm, as libass's do:
  the sine and cosine of the `\frx`, `\fry` and `\frz` rotations
  (`Placement::rotation`), `log1p` and `expm1`, which quantise the `\blur`
  radius and restore its variance (`Effects::blur`), and `exp` in the
  Gaussian blur's coefficients (`Raster::blur` and `Composite::new`); `log`
  only takes the constant `log 256`. Neither Apple's libm nor glibc
  guarantees correct rounding for these, so a result that depends on them is
  the one libass computes against the same libm, and platforms whose libms
  differ can differ there. The comparison covers Apple's libm on macOS AArch64
  and glibc on Linux x86_64, AArch64 and 32-bit ARMv7, where every case of
  the C engine matches apart from two sweep cases on ARMv7 whose composite cannot be
  allocated in a 32-bit address space ([status](../../docs/status.md)).

## Behaviour and limits

Every input-dependent allocation is charged to the `Budget` a value was created
with: outlines, bitmaps and the rasteriser's, stroker's and blur's scratch,
which a `Raster` keeps and reuses. Work is charged to the `WorkBudget` the caller passes
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
libass on 32-bit ARM hard-float with GCC on every target: conversions
saturate, NaN converts to 0 and such shifts give 0, so a coordinate rounds
out of range as libass does there.

Not covered: glyph loading, hinting, synthetic styles and decoration lines
(`triergon-font` and the renderer); drawing parsing and the image list,
including where vector clips overlap and how their images are allocated,
which belong to the renderer. The large-tile build of libass
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

## Licence

MIT or Apache-2.0, at your option, with libass 0.17.5's ISC licence: the outline,
stroking, rasterising, blur, composition and clipping stages are translated from
libass. `LICENSE` holds the notice, and
[docs/provenance.md](../../docs/provenance.md#triergon-raster) lists the modules.

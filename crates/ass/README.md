# triergon-ass

ASS and SSA subtitle scripts as libass 0.17.5 reads and renders them: whole
scripts and Matroska packet streams, styles and events, typed override tags,
the state each glyph of an event has at a given time (animation, movement,
fades, clips, karaoke timing, transition effects), the drawing language, and
a `Renderer` that turns the events shown at a time into the positioned alpha
images libass's `ass_render_frame` gives. It depends on
[`triergon-core`](../core) for `Budget`, `Bytes`, `WorkBudget` and errors, on
[`triergon-font`](../font) for faces, [`triergon-shape`](../shape) for bidi and
OpenType shaping and [`triergon-raster`](../raster) for outlines and bitmaps.

## Usage

```rust
use triergon_ass::{Context, Drawing, Options, Placement, Script, Symbol};
use triergon_core::Budget;
use triergon_raster::Outline;

let text = b"[Script Info]\nScriptType: v4.00+\nPlayResX: 640\nPlayResY: 480\n\
    [Events]\nFormat: Layer, Start, End, Style, Text\n\
    Dialogue: 0,0:00:00.00,0:00:02.00,Default,{\\move(0,0,100,0)\\fad(200,0)}Hi{\\p1}m 0 0 l 8 0 8 8";
let budget = Budget::new(16 << 20)?;
let script = Script::parse(&budget, text, &Options::default())?;

// The first event one second in.
let event = &script.events()[0];
let layout = script.info().layout_res().unwrap_or(script.info().play_res());
let mut context = Context::new(&script, event, 1000, layout)?;
let mut outline = Outline::new(&budget); // reused for every drawing
while let Some(glyph) = context.next_glyph()? {
    match glyph.symbol {
        Symbol::Char(_code) => {
            // Shape the code with glyph.state.font(), its size, colours, ...
            assert_eq!(glyph.state.font().family, b"Arial");
        }
        Symbol::Drawing { text, .. } => {
            let drawing = Drawing::parse(text, &mut outline, script.limits())?;
            assert!(drawing.is_some_and(|d| d.outline.points().len() == 3));
        }
    }
}
assert_eq!(context.state().placement, Placement::Positioned(50.0, 0.0));
# Ok::<(), Box<dyn std::error::Error>>(())
```

A Matroska track starts with `Script::new`, then `process_codec_private` and one
`process_chunk` per packet, timed by the container; `flush_events` follows a seek
and `prune_events` drops what has been shown:

```rust
use triergon_ass::{Options, Script};
use triergon_core::Budget;

let budget = Budget::new(16 << 20)?;
let mut track = Script::new(&budget, &Options { extract_fonts: true, ..Options::default() })?;
track.process_codec_private(b"[Script Info]\nScriptType: v4.00+\n[Events]\n\
    Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n")?;
track.process_chunk(b"0,0,Default,,0,0,0,,Hello", 1000, 2000)?;
track.process_chunk(b"0,0,Default,,0,0,0,,the same ReadOrder is dropped", 1000, 2000)?;
assert_eq!(track.events().len(), 1);
track.prune_events(4000);
assert!(track.events().is_empty());
# Ok::<(), Box<dyn std::error::Error>>(())
```

A player renders frames with the fonts it attaches:

```rust,no_run
use triergon_ass::{Change, Fonts, Options, Renderer, Script, Settings, Size};
use triergon_core::{Budget, Bytes};

let budget = Budget::new(256 << 20)?;
let font = Bytes::copy_from_slice(&budget, &std::fs::read("Arimo-Regular.ttf")?)?;
let fonts = Fonts {
    attached: vec![font],
    default_family: Some(b"Arimo".to_vec()),
    default_font: None,
};
let mut settings = Settings::new(Size::new(1920, 1080));
settings.storage = Some(Size::new(1280, 720)); // the video's size
let mut renderer = Renderer::new(&budget, &settings, &fonts)?;
let options = Options { extract_fonts: true, ..Options::default() };
let mut script = Script::parse(&budget, &std::fs::read("episode.ass")?, &options)?;
let frame = renderer.render(&mut script, 61_500)?;
if frame.change == Change::None {
    // Keep showing the previous frame's images.
}
for image in frame.images {
    // Blend `image.colour` (alpha inverted) through each alpha row.
    for (row, alpha) in image.rows().enumerate() {
        let _ = (image.x, image.y + row as i32, alpha);
    }
}
# Ok::<(), Box<dyn std::error::Error>>(())
```

## What it reads

**Lines.** Input ends at its first NUL byte and is split at CR and LF; UTF-8 byte
order marks are skipped at every line start, and leading blanks of a line are
ignored. Section headers match by prefix, ignoring ASCII case; `[V4 Styles]` and
`[V4+ Styles]` also set the dialect. Other keys are case-sensitive. Text is bytes:
nothing is validated as UTF-8. Charset conversion (libass's `iconv` codepage
argument) is the caller's; convert to UTF-8 first.

**`[Script Info]`.** `ScriptType`, `PlayResX/Y`, `LayoutResX/Y`, `Timer`,
`WrapStyle`, `ScaledBorderAndShadow`, `Kerning`, `YCbCr Matrix` and `Language`
(its first two bytes). `Info::play_res` applies libass's defaults (384×288,
1280×1024, else 4:3 from the given side). A whole script with neither a script
type nor a styles section is `Error::Invalid`, as libass returns no track.

**Styles.** `Format:` field names in any order and case (a missing format line
selects the dialect's standard one); missing trailing fields keep zero values.
The last `Default` style becomes the default style after libass's own. Colours
are `&HAABBGGRR` integers, byte-swapped to `Rgba`; v4+ alignment is converted
from numpad numbering, and SSA's `AlphaLevel` and back colour are converted as
libass converts them. Values keep their meaning in their types: `Bold`, `Italic`
and `BorderStyle` keep the integer libass keeps (glyph runs split where it
changes), underline and strike-out are booleans, `Alignment` is its horizontal
and vertical part, and `WrapStyle` and `Justify` are the values libass names.

**Events.** `Dialogue:` lines with the event format (or the dialect's standard
one), up to `Text`, which takes the rest of the line; `Comment:` lines are
dropped. Times are `H:M:S.CC` read as `sscanf` does, with the last field in
centiseconds whatever its length (so `.5` is 50 ms and `.500` is 5 s); a
malformed time is 0. Styles are looked up as libass looks them up. Events of a
whole script are numbered in file order.

**Streams.** `process_chunk` reads `ReadOrder, Layer`, then the event format after
its first three names, and drops a packet whose ReadOrder was seen, as libass
does. `prune_events` frees the identifiers of the events it removes.
`process_styles` reads style lines as `ass_read_styles` does, from memory.

**Embedded fonts.** `[Fonts]` with `fontname:` lines and SSA's uuencode variant,
in a whole script, a codec private or more data. Fonts are kept with
`Options::extract_fonts`.

**Style overrides.** `Options::style_overrides` are `[Style.]Field=Value` items as
`ass_process_force_style` applies them after a whole script or a codec private:
header fields by name, then style fields in the named style (ignoring case) or in
all styles.

**Override tags.** `Tags` lexes a `{...}` block as `ass_parse_tags` does,
including VSFilter's argument rules, and converts numbers with libass's own
decimal conversion. `Tag` covers `\b \i \u \s \bord \xbord \ybord \shad \xshad
\yshad \be \blur \fn \fs \fsp \fscx \fscy \fsc \frx \fry \frz \fr \fax \fay \an \a
\q \pos \move \org \fad \fade \t \k \kf \K \ko \kt \c \1c-\4c \alpha \1a-\4a \clip
\iclip \p \pbo \fe \r`, each with its argument validated as libass validates it;
`\N \n \h \{ \}` and tabs are text escapes.

**Evaluation.** `Context` walks an event's text at a time in milliseconds,
applying blocks and yielding glyphs with a copy of the state, as `parse_events`
does: the first `\pos`/`\move`, `\org`, `\fad`/`\fade` and `\an`/`\a` win; `\t`
animates with `((t - t1) / (t2 - t1))^accel`; colours animate per channel; `\r`
resets to the event's or a named style; drawing mode turns the text up to the
next `{` into one drawing glyph. The `Banner` and `Scroll up/down` effects are
read with VSFilter's delay rescaling. `Syllables` gives libass's karaoke
syllables from laid-out positions.

**Drawings.** `Drawing::parse` reads `m n l b s p c` into a
`triergon_raster::Outline` of lines and cubic splines in 26.6 fixed point (y
down), as `ass_drawing_parse` does. A coordinate beyond ±(2^28 − 1) in 26.6 makes
the drawing invalid (`None`). The `\p` level scales by `drawing_scale`.

## How it renders

`Renderer::render` draws the events shown at a time as libass's
`ass_render_frame` does with its defaults (no hinting, complex shaping, no
selective style overrides), in the frame and storage size and margins of its
`Settings`, and returns a `Frame` of positioned alpha images:

- **Fonts.** The `Fonts` a player gives (attached font files, a default family
  and a default font file) and the script's embedded fonts are matched by family,
  full or PostScript name, weight and slant with `triergon_font::select`, as
  libass matches fonts without a system font provider. Fallback for a character
  no face maps tries the requested family, the default family and the default
  font file, so it depends on what was drawn before, as in libass. A face
  selected for a character its text charmap does not map is tried through each
  of its charmaps in turn, as `ass_font_get_index` does; that retry is
  `Error::Unsupported` for a face with charmaps `triergon-font` does not read,
  as is a lookup through a charmap whose codes libass converts.
- **Shaping.** Scripts per character, runs of one face, script and style,
  FriBidi levels per run (or per paragraph for `\fe-1`), and OpenType shaping of
  each run with libass's font functions and features through `triergon-shape`.
- **Layout.** Outlines at 256 pixels with libass's synthetic bold and oblique and
  its underline and strike-out lines, scaled to the glyph's size; letter spacing;
  wrapping, whitespace trimming and line metrics; karaoke; the visual order of
  each line; alignment and justification; positioning by margins, `\pos`,
  `\move` or a scroll; and the rotation origin. Glyphs of `@` fonts from U+02F1
  on are drawn turned by 90°, as libass draws them.
- **Bitmaps.** Each glyph's fill and border or opaque box through
  `triergon_raster::Raster::render`, each run composited with its blur, `\be` and
  shadow by `Raster::compose`, then each run's shadow, border and fill as images,
  clipped by the clip rectangle (with `\kf`'s split), multiplied by a vector
  clip, and the box of `BorderStyle` 4.
- **The frame.** Events are taken in layer and read order; within a layer,
  events placed by their margins move clear of each other and keep their place
  on later frames (the record lives with the event in the `Script`, and a
  refused frame changes none); fully transparent images are dropped.
- **Change detection.** `Frame::change` is what libass's `ass_detect_change`
  reports: `Change::Content` when the image count differs or an image's size,
  stride, colour or bitmap does, `Change::Positions` when only places do, else
  `Change::None`. A frame of a script without events reports new content.

Settings beyond the frame and storage size (margins, `use_margins`, a pixel
aspect, font scale, line spacing and position) follow libass's formulas.

**Caches.** `Settings::cache` holds the cache limits (`CacheLimits`): by default
libass's (10,000 outlines, 128 MiB of glyph bitmaps, 64 MiB of composites), and
`CacheLimits::new(glyphs, megabytes)` gives what `ass_set_cache_limits` sets.
Outlines, glyph bitmaps and composited runs are kept between frames and cut as
libass cuts its caches (`ass_cache_cut`): least recently used first, never an
entry the previous frame's images still hold, so eviction never changes a change
value. Requested fonts and the faces they gathered are kept up to
`CacheLimits::fonts` font descriptions (10,000 by default); libass keeps them
all, so a caller that wants libass's behaviour sets it to `usize::MAX`. Shaping
plans are kept up to 256. All of it is charged to the renderer's budget.

## Target-dependent results

Floating-point operations follow libass's C source in order, without fused
multiply-add; a compiler that contracts `a * b + c` would round some animated
values differently in the last bit. Where libass's result depends on the target,
the crate chooses as the
[reference-compatible behaviour](../../docs/architecture.md#reference-compatible-behaviour)
convention asks:

- Values libass reads through `strtol` (event times, a packet's ReadOrder,
  `Banner` and `Scroll` parameters, booleans) and the karaoke split's `lrint`
  follow each target's `long` width, through `core::ffi::c_long`.
- The other conversions of a `double` to `int` whose out-of-range result C
  leaves to the target (`\pbo`, an animated `\clip` or `\iclip`, `\fade`'s
  alpha, an effect's delay, a drawing coordinate in 26.6, and the renderer's
  `lrint`s, `lround`s and `(int)` casts) saturate at `int`'s range with NaN as
  0, as libass on 32-bit ARM with GCC converts. An out-of-range drawing
  coordinate therefore discards the drawing. A coordinate landing exactly on
  `INT_MIN`, which libass built with GCC accepts through `abs(INT_MIN)`
  overflow, is rejected.
- Collision handling sorts the placed boxes as libass does on glibc (`qsort`
  with `cmp_rect_y0`, a stable merge sort), including the order its overflowing
  comparison gives for tops 2³¹ or more apart; another C library may order such
  boxes, or equal tops, differently.
- A `BorderStyle` 4 event whose doubled horizontal border overflows `int` loses
  its box, as in libass built with GCC; event box values outside the signed
  32-bit range saturate, as on 32-bit ARM, where libass for AArch64 keeps their
  low 32 bits.

## Behaviour and limits

A `Script` charges its styles, events, strings, format lines, fonts and ReadOrder
bitmap to its `Budget`; an `Outline` charges its points to its own. `Limits`
bounds the work of one call (`work`, default 2²⁸ units: input bytes and lines,
format fields, style lookups, ReadOrder scans, tags, glyphs and drawing points;
a `Context` takes one call's work for its whole walk), the styles held
(`styles`, default 2¹⁶) and the events held (`events`, default 2²²). Exceeding
one is `Error::Policy`; an exhausted budget is `Error::Limit` or
`Error::Backpressure`. Evaluating an event allocates nothing.

A `Renderer` charges its glyph lists, outlines, plans, fonts, bitmaps, images
and scratch to its `Budget`. `Settings::work` bounds the work of a whole frame,
by default `Settings::DEFAULT_WORK` (2²⁸ units), shared by every stage of every
shown event: evaluation, font selection, shaping, outlines, layout, drawing and
collision handling. The charge grows linearly with the events and their text.
Where libass would go on drawing, running out is `Error::Policy`; a caller that
wants libass's unbounded behaviour raises the setting. Shaping and bidi also run
under the limits of their crates. Malformed input is read as libass reads it,
skipping what it skips, and never panics.

## Cargo features

- `work-probe`: a development aid that adds `Renderer::work_spent`, the work the
  last frame spent. Not needed in production.

## Not supported

libass's simple shaping level and hinting modes, its selective style override
(`ass_set_selective_style_override`; with libass's default settings it changes
nothing), track features (`ass_track_set_feature`), `ass_step_sub`,
`ass_configure_prune` (a player calls `prune_events`), system font providers,
reading files and charset conversion.

## Verification

`cargo test -p triergon-ass` runs unit tests, a mutation test and rendering tests
with a font built in memory. The comparison with libass 0.17.5 runs through
[`ass-worker`](../ass-worker/README.md) and the `harness/features/ass-*`
features; [docs/status.md](../../docs/status.md) has the results.

## Licence

MIT or Apache-2.0, at your option, with libass 0.17.5's ISC licence: the script reader,
the tag parser and the renderer's stages are translated from libass. `LICENSE`
holds the notice, and [docs/provenance.md](../../docs/provenance.md#triergon-ass) lists
the modules.

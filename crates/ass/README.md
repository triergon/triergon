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

**`[Script Info]`.** `ScriptType` (`4.00` before the last blank, optionally `+`),
`PlayResX/Y`, `LayoutResX/Y`, `Timer`, `WrapStyle`, `ScaledBorderAndShadow`,
`Kerning`, `YCbCr Matrix` and `Language` (its first two bytes). Integers take
`&H`/`0x` only at the very start and wrap modulo 2^32; booleans are `yes` in any
case or a positive integer. `Info::play_res` applies libass's defaults (384×288,
1280×1024, else 4:3 from the given side). A whole script with neither a script
type nor a styles section is `Invalid`, as libass returns no track. A custom style
or event format line makes `ScaledBorderAndShadow` default to yes, as do FFmpeg's
converted scripts from before it wrote the header.

**Styles.** `Format:` field names in any order and case (a missing format line
selects the dialect's standard one); missing trailing fields keep zero values. A
leading `*` of a name is dropped and an empty name is `Default`; the last
`Default` becomes the default style after libass's own (VSFilter's defaults, kept
as style 0). Colours are `&HAABBGGRR` integers, byte-swapped to `Rgba`. v4+
alignment is converted from numpad numbering; SSA's `AlphaLevel` sets the front
alphas and the back alpha becomes 0x80, and its back colour is also the outline
colour. Scales are divided by 100; scales, spacing, outline and shadow are
clamped at 0; bold and italic become 0 or 1.

Values keep their meaning in their types: `Bold` and `Italic` keep the integer
libass keeps (glyph runs split where it changes, so 1 and 700 differ although both
request weight 700) and give the weight and slant libass requests;
`BorderStyle` keeps its integer for the same reason. Underline and strike-out are
booleans, `Alignment` is its horizontal and vertical part, `WrapStyle` and
`Justify` are the values libass names, and every other value libass treats as one
of them.

**Events.** `Dialogue:` lines with the event format (or the dialect's standard
one), up to `Text`, which takes the rest of the line without trailing blanks and
CRs; a line ending earlier is dropped, as is `Comment:`. `End` and `Duration`,
`Name` and `Actor` are aliases. Times are `H:M:S.CC` read as `sscanf` does, with
the last field in centiseconds whatever its length (so `.5` is 50 ms and `.500` is
5 s); a malformed time is 0. Styles are looked up as libass does: leading `*`
dropped, `Default` in any case, the last exact match, else the default style.
Events of a whole script are numbered in file order.

**Streams.** `process_chunk` reads `ReadOrder, Layer`, then the event format after
its first three names, and drops a packet whose ReadOrder was seen: by libass's
bitmap of identifiers (dropped for any identifier outside 0 to 83886079, after
which held events are scanned instead, which libass's bitmap and scan answer
differently for packets rejected after marking). `prune_events` frees the
identifiers of the events it removes, and scans the events only when one may end
before the deadline. `process_styles` reads style lines as `ass_read_styles`
does, from memory.

**Embedded fonts.** `[Fonts]` with `fontname:` lines and SSA's uuencode variant
(`c - 33` in six bits; a final group of two or three characters, a single one is
malformed), in a whole script, a codec private or more data. Fonts are kept with
`Options::extract_fonts`.

**Style overrides.** `Options::style_overrides` are `[Style.]Field=Value` items as
`ass_process_force_style` applies them after a whole script or a codec private:
the value after the last `=`, the header fields by name, then style fields in the
style named before the last `.` (ignoring case) or in all styles; values are
stored as written, without the style line's conversions.

**Override tags.** `Tags` lexes a `{...}` block as `ass_parse_tags` does, including
VSFilter's argument rules: names match by prefix in libass's order, the rest of a
plain tag's name is an argument after any parenthesised ones, a backslash inside
parentheses makes the rest up to the next `)` one argument, and a missing `)` ends
at the block. Numbers convert with libass's own decimal conversion (at most 18
significant digits, exponents saturating at 511, an `e` consumed without digits)
and `strtoll` saturation clamped to 32 bits, reading from an argument's start.
`Tag` covers `\b \i \u \s \bord \xbord \ybord \shad \xshad \yshad \be \blur \fn
\fs \fsp \fscx \fscy \fsc \frx \fry \frz \fr \fax \fay \an \a \q \pos \move \org
\fad \fade \t \k \kf \K \ko \kt \c \1c-\4c \alpha \1a-\4a \clip \iclip \p \pbo \fe
\r`, each with its argument validated as libass validates it; `\N \n \h \{ \}` and
tabs are text escapes.

**Evaluation.** `Context` walks an event's text at a time in milliseconds,
applying blocks and yielding glyphs with a copy of the state, as `parse_events`
does: the first `\pos`/`\move`, `\org`, `\fad`/`\fade` and `\an`/`\a` win; `\t`
animates with `((t - t1) / (t2 - t1))^accel` and a `\t` inside a `\t` sets the
power of the tags after it; colours animate per channel; `\r` resets to the
event's or a named style; drawing mode turns the text up to the next `{` into one
drawing glyph. The `Banner` and `Scroll up/down` effects are read with VSFilter's
delay rescaling. `Syllables` gives libass's karaoke syllables from laid-out
positions.

**Drawings.** `Drawing::parse` reads `m n l b s p c` into a `triergon_raster::Outline`
of lines and cubic splines in 26.6 fixed point (y down), as `ass_drawing_parse`:
commands before the first valid `m` are ignored, an `n` first rejects the drawing
(empty), incomplete point groups are consumed and dropped, B-splines are converted
with libass's integer arithmetic, and a coordinate beyond ±(2^28 − 1) in 26.6 makes
the drawing invalid (`None`). The `\p` level scales by `drawing_scale`.

## How it renders

`Renderer::render` draws the events shown at a time as libass's
`ass_render_frame` does with its defaults (no hinting, complex shaping, no
selective style overrides), in the frame and storage size and margins of its
`Settings`:

- **Fonts.** The `Fonts` a player gives (attached font files, a default
  family and a default font file) and the script's embedded fonts, registered
  at each frame as libass registers them (a renderer that draws several
  scripts registers each one's as it first draws them, where libass, whose
  tracks share one library, registers every track's as soon as it is read;
  fonts registered once stay selectable in both), are matched by family, full
  or PostScript name, weight and slant with `triergon_font::select`, as libass
  matches fonts without a system font provider. Each requested font keeps the
  faces it gathers: the one selected for it, then, for a character none of
  them maps, one selected for that character from the requested family, the
  default family or the default font file, so fallback depends on what was
  drawn before, as in libass. Each face reads characters through the charmap
  libass selects on opening it; a face selected for a character it does not
  map is tried through each of its charmaps in turn and keeps the one that
  maps it, or the last, for every later lookup and for shaping until another
  such lookup moves it (`ass_font_get_index`). That is how libass draws C1
  controls (U+0080 to U+009F) through a Macintosh Roman charmap. The retry is
  `Error::Unsupported` for a face with charmaps FreeType lists and
  `triergon-font` does not read, as is any lookup through a charmap whose
  codes libass converts (`Font::text_lookup`). The default font file's face gets an id of its
  own, so a font never takes it for a face it already has: libass leaves
  that id uninitialised, and this is its result whenever the id does not
  collide with a registered face's; the reference is built with locals
  zeroed, so it never does.
- **Shaping.** Scripts per character, HarfBuzz's ignorables, runs of one face,
  script and style, FriBidi levels per run (or per paragraph for `\fe-1`),
  and OpenType shaping of each run at the size its glyphs load at with
  libass's font functions and features through `triergon-shape`.
- **Layout.** Outlines at 256 pixels with libass's synthetic bold and oblique
  and its underline and strike-out lines, scaled to the glyph's size (a font
  size of 0 keeps libass's size 0, at which FreeType loads glyphs and their
  advances unscaled, in font units, with no line ascent or descent); the
  italic correction and letter spacing; wrapping (greedy, then rebalanced
  unless `\q1`), whitespace trimming and line metrics; karaoke; the visual
  order of each line; alignment and justification; the baseline shear; the
  position by margins, `\pos`, `\move` or a scroll; and the rotation
  origin.
- **Bitmaps.** Each glyph's fill and border or opaque box through
  `triergon_raster::Raster::render`, quantised against its run, and each run
  composited with its blur, `\be` and shadow by `Raster::compose`; then each
  run's shadow, border and fill as images, clipped by the clip rectangle (with `\kf`'s
  split), multiplied by a vector clip, and the box of `BorderStyle` 4. A
  vector clip whose drawing has no contour empties every image of a regular
  clip and leaves those of an inverse clip, as libass's empty mask does; one
  whose drawing libass cannot place (its centre 125,000 frame pixels or more
  from the origin) is not applied, as in libass.
- **The frame.** Events are taken in layer and read order, and in the
  script's order where both are equal (libass's `qsort` leaves that order
  unspecified; a whole script's events differ in ReadOrder, a stream's need
  not); within a layer, events placed by their margins move clear of each
  other and keep their place on later frames (the record lives with the event
  in the `Script`, and a refused frame changes none); fully transparent
  images are dropped.
- **Change detection.** `Frame::change` is what libass's `ass_detect_change`
  reports: `Change::Content` when the image count differs or an image's size,
  stride, colour or bitmap does, `Change::Positions` when only places do,
  else `Change::None`. Bitmaps are told apart as libass tells them apart, by
  the cached composite they show: an image shows the same bitmap as one of
  the previous frame when it is the same part and window of the same
  composite. A run finds the composite of an equal key (its effects key, and
  each glyph's fill and border bitmap keys and origins in the run), and a
  glyph the bitmap of an equal key (its outline and quantised transform), so
  glyphs that draw nothing but an opaque box find the same composite
  whatever their outlines. Whatever the previous frame shows survives the
  caches' cut, so eviction changes no change value; pixels a vector clip or
  a `BorderStyle` 4 box makes are always new.
  A frame of a script without events reports new content and leaves the
  previous frame in place for the next comparison, as libass does.

Settings beyond the frame and storage size (margins, `use_margins`, a pixel
aspect, font scale, line spacing and position) follow libass's formulas but
are not compared. The glyphs of `@` fonts from U+02F1 on are drawn as libass
draws them, turned by 90° about their scaled vertical advance and the
typographic descender and advancing by their linear vertical advance, and are
shaped with the default-load vertical advance of every glyph libass's glyph
metrics cache keeps rotated: a glyph's first lookup through a character from
U+02F1 decides, for the renderer's life and for each requested font (family,
weight, slant), face and size, as libass keys that cache. Not rendered: libass's simple shaping
level, its hinting modes, selective style overrides and track features.

**Caches.** `Settings::cache` holds the cache limits (`CacheLimits`): its
default is libass's (10,000 outlines, 128 MiB of glyph bitmaps, 64 MiB of
composites), and `CacheLimits::new(glyphs, megabytes)` gives what
`ass_set_cache_limits` sets. Outlines (glyphs, drawings, stroked
borders and the unit square of opaque boxes), glyph bitmaps (with a
bitmap that draws nothing) and composited runs are kept between frames in one kind of cache, bounded by
the outline count and the two byte limits. When a frame
starts the composites, then the bitmaps, then the outlines are cut as libass
cuts its caches (`ass_cache_cut`): least recently used first, until what is
kept fits, never one the previous frame's images or another entry's key
hold, which leaves the order and goes the moment nothing holds it; a run
holds its glyphs' bitmaps while it is drawn, and a vector clip its bitmap. A cache is
also cut the same way when an insertion takes it past its limit and when
the renderer lets go of a run or an event, so it holds at most its limit
beyond what is in use; libass keeps a frame's entries until the next frame,
and nothing it would show differs. Sizes are counted as libass counts them:
an outline as 1; a bitmap's pixels, libass's records in the target's words
and the points and segments of its outline; a composite's pixels and
records. A composite is keyed by its glyphs' bitmap keys where libass holds
the bitmaps, which finds the same composites and lets the bitmaps go.
Requested fonts and the faces
they gathered are kept up to `CacheLimits::fonts` font descriptions (10,000
by default), cut at each frame the same way, never one the previous frame
asked for; libass keeps them all, so a font asked for again after it left
starts over from its first face. A caller that wants libass's behaviour sets
`CacheLimits::fonts` to `usize::MAX`. Shaping plans are kept up to 256. All of it
is charged to the renderer's budget, family names and drawing texts by
length, and looked up by the names and texts the script holds, without
copying them. Every bitmap or composite lookup charges `Settings::work` a
unit per bitmap and per glyph of a composite key; only a miss charges the
drawing besides.

## Arithmetic

Floating-point operations follow libass's C source in order, without fused
multiply-add, as the libass reference is built. A compiler that contracts
`a * b + c` into one fused operation (Clang and GCC may, on targets that have it)
rounds some animated values differently in the last bit.

Where libass's result depends on the target, the crate chooses deliberately, as
the [reference-compatible behaviour](../../docs/architecture.md#reference-compatible-behaviour)
convention asks:

- `long`-width behaviour follows each target's libass. libass reads event times
  (`sscanf`'s `%d`), a Matroska packet's ReadOrder and the parameters of a
  `Banner` or `Scroll` effect (`atoi`) through `strtol`, `parse_bool` calls
  `strtol`, and the karaoke split rounds with `lrint`. A value beyond `int`
  saturates at `long`'s range and is then narrowed to `int`, so it differs
  between 64-bit and 32-bit `long`; the crate converts these, and only these,
  through `core::ffi::c_long` in one module.
- Every conversion of a `double` to `int` whose result C leaves to the target
  out of range follows libass on 32-bit ARM with GCC: it saturates at `int`'s
  range, and NaN is 0. Those are the casts libass makes for `\pbo` in a
  drawing glyph, an animated `\clip` or `\iclip`, `\fade`'s alpha and the
  delay of a `Banner` or `Scroll` effect (undefined out of range), and a
  drawing coordinate in 26.6: `double_to_d6`'s `lrint`, whose result is
  unspecified beyond `long`'s range, narrowed from `long` to `int` (rounded as
  `triergon_raster::pixels_26_6` rounds). So an out-of-range drawing
  coordinate lies beyond the outline bound and discards the drawing, and a NaN
  one (such as `0e400`) is 0. libass's own result is the compiler's and the
  target's.
- The one exception: libass built with GCC accepts a coordinate landing exactly
  on `INT_MIN` through `abs(INT_MIN)` overflow, and the crate rejects it, so
  the raster stages never see that coordinate.

Those cases are compared in `ass-parse-host`, pinned per target. The exception's
two cases are open there on the GCC targets, as are, on
`aarch64-unknown-linux-gnu`, the two drawings whose coordinates libass converts
with a 64-bit `long` there.

The renderer follows the same rule where libass's renderer converts: its
`lrint`s (`triergon_core::round_ties_even_i32`: glyph boxes, advances and
metrics), its `lround`s (the layout resolution, `\kf`'s split column, a
`BorderStyle` 4 shadow, clip and scroll rectangles), its `(int)` casts (the
collision box, line borders, the baseline shear, the content area) and
`double_to_d6` (`triergon_raster::pixels_26_6`) all saturate at `int`'s range
with NaN as 0, also where libass's `long` is 64 bits wide. Collision handling
sorts the boxes placed so far as libass does on glibc: `qsort` with
`cmp_rect_y0`, which subtracts their tops in `int` and so overflows, undefined
in C and wrapping as GCC builds it, for tops 2³¹ or more apart, run as glibc's
stable merge sort. While the tops lie closer that comparison is a total
preorder and the crate orders them by value, keeping equal tops in the order
they were placed in. Farther apart it is not transitive, and the order
depends on which pairs the sort compares: the crate sorts with a top-down
merge sort that sorts the first half, of the length halved and rounded down,
and then the rest, and merges them stably. `collision-far-margins.ass` of
`ass-render` places boxes that far apart, each within `int`'s range, and the
crate places its events there as libass does on both targets; rounding the
half up instead places them differently. Another C library's `qsort` may
order such boxes, or equal tops, differently. The far event's position there
leaves `int`'s range in 26.6 fixed point: libass for ARMv7 and the crate draw
nothing of it, and libass for AArch64 draws it inside the frame, so the case
clips it away. No other fixture of `ass-render` converts out of range.

libass's renderer also overflows `int`, undefined in C, where it doubles an
event's horizontal border for the width of its box. Built with GCC it wraps,
and the renderer doubles it in wrapping `int` arithmetic too, so a
`BorderStyle` 4 event whose horizontal border exceeds 2³⁰ frame pixels loses
its box as it does in libass. The case is `ass-render-host`'s, pinned per
target: libass built with GCC for `aarch64-unknown-linux-gnu` converts the
event box's top, height, left and width from `double` to `int` together, with
vector instructions that keep the low 32 bits of each where 32-bit ARM
saturates, so there it also loses the box of a vertical border beyond 2³⁰
frame pixels, and a huge border moves colliding events elsewhere. The renderer
follows 32-bit ARM.

## Behaviour and limits

A `Script` charges its styles, events, strings (one allocation per style and per
event), format lines, fonts and ReadOrder bitmap to its `Budget`; an `Outline`
charges its points to its own. `Limits` bounds the work of one call (input bytes
and lines, format fields, style lookups, ReadOrder scans, tags, glyphs and
drawing points; a `Context` takes one call's work for its whole walk), the styles
and the events held; exceeding one is `Error::Policy`, an exhausted budget
`Error::Limit` or `Error::Backpressure`. A `\t` inside a `\t` continues in place,
so tag evaluation recurses at most one level. `Syllables` is linear in the glyphs
it is given. Evaluating an event allocates nothing. A `Renderer` charges its
glyph lists, outlines, plans, fonts, bitmaps, images and scratch to its
`Budget`. `Settings::work` bounds the work of a whole frame, by default
`Settings::DEFAULT_WORK` (2²⁸ units), and every stage of every shown event
shares that one budget: evaluation (a pass over the effect and the text),
font selection (each registered face matched, each gathered face tried),
shaping (each run's characters and the few characters of context it reads),
outlines (each one built and its points), layout (a pass per stage over the
glyphs, and one for every rebalancing pass), drawing (glyph bitmaps,
composites and vector clips, charged as `triergon-raster` charges them) and
collision handling (each event against those placed before it in its
layer, and the merge sort of the placed boxes after each one joins them). The charge grows linearly with the events and their text. Where
libass would go on drawing, running out is `Error::Policy`; a caller that
wants libass's unbounded behaviour raises the setting. With the `work-probe`
feature, a development aid, `Renderer::work_spent` gives what the last frame
spent. Shaping and bidi also
run under the limits of their crates.
Malformed input is read as libass reads it, skipping what it skips, and never
panics.

## Verification

- `cargo test -p triergon-ass`: unit tests per module, a deterministic mutation
  test (`tests/robustness.rs`) over authored scripts, including work, event and
  budget limits, and `tests/render.rs`, which renders with a font built in
  memory: image order, collisions across frames, clips and karaoke splits, an
  exhausted budget and 300 mutated scripts.
- `cargo xtask ass compare` runs [ass-worker](../ass-worker/README.md) and the
  libass reference worker (`harness/reference_ass.h`) on every authored fixture in
  `harness/fixtures/ass` and requires identical work: `ass-parse-scripts` (whole
  scripts), `ass-parse-chunks` (Matroska packet streams) and `ass-parse-events`
  (every event walked at the fixture's times), and `ass-parse-host` the
  operations whose libass result differs by target, each against its target's
  pin. `cargo xtask fuzz --contract
  harness/features/<feature>/contract.json --reference` mutates the fixtures and
  compares both workers on every mutation.
- `ass-render` renders the 36 scripts and 3 packet streams of
  `harness/fixtures/ass-render` as a player configures libass (attached fonts,
  a default family and font file, no system font provider, no hinting) at
  1920×1080 over 1280×720 storage and at 1280×720 over anamorphic 720×576
  storage, and compares every image's kind, position, size, colour and alpha
  bytes in order, and every frame's change value, with libass's
  (`harness/reference_ass_render.h`, with libass's cache limits set to 1000
  glyphs and 24 MiB, which `cache-eviction.ass` overflows): all 78 pass. The
  box behind a `BorderStyle` 4 event, whose type libass leaves uninitialised,
  is told by its own buffer, also after a collision shift has cut rows off its
  top, and compared as a kind of its own.
- `ass-render-host` renders, as `ass-render` does, the fixtures whose libass
  result differs by target (`border-x-overflow.ass`, and `collision-far-tops.ass`,
  colliding events whose boxes end up 2³¹ or more rows apart), each pinned for
  `aarch64-unknown-linux-gnu` and `thumbv7neon-unknown-linux-gnueabihf` from that
  target's reference. All four cases pass on ARMv7; on AArch64 all four are
  open as a deliberate divergence.
- Runtime and memory of both features are taken against `libass-player-native`
  (`harness/reference_ass_player.h`: `libass.a` linked into the worker, none of
  its sources compiled in) by `cargo xtask report`, 30 timing and 30 memory
  pairs per case, and graded by `cargo xtask grade`. The reference replaces
  libass's bitmap engine with its plain C one, and libass is built with `-O2
  -ffp-contract=off -ftrivial-auto-var-init=zero`; a player's libass with its
  NEON code on AArch64 is faster than this reference. The timed spans favour
  the renderer slightly: in 12 cases libass opens the default font file inside
  its span, which the renderer reads before its own, and libass's side copies
  the script inside its span.
- Measured at `5b3e03f0` on a c6g.2xlarge, before the later default glyph load
  with CFF hinting and the harness's memory and worker change, which are not
  timed on AArch64. At that revision every passing case was within the
  runtime ceiling of [docs/quality.md](../../docs/quality.md) and lower than
  libass in peak heap and total RSS. On Linux AArch64 the `ass-render` ratios
  had a median of 0.813 and a highest upper bound of 1.066
  (`cache-keys-ass-720p`), with peak heap ≤0.841 and total RSS ≤0.943. On
  ARMv7, over all 80 cases, the highest upper bound was 0.943, peak heap
  ≤0.928 and total RSS ≤0.961, the renderer's virtual peak over libass's
  resident peak. Measured again at `22ed8232` for
  `thumbv7neon-unknown-linux-gnueabihf` under the kernel RSS bound, over all
  82 cases on ARMv7 the highest upper bound is 0.780, peak heap ≤0.928 and
  total RSS ≤0.911.
  [docs/status.md](../../docs/status.md) names the evidence.
- `cargo xtask ass equivalence --base REV` renders the `ass-render`
  fixtures at their frames and every 40 ms, mutations of them with varied
  settings and style overrides, and scripts generated from a seed, with the
  renderer of REV and of the working tree (`examples/equivalence.rs`, built
  with the `work-probe` feature), and requires the same change, images,
  error and work for every frame (`--work any` leaves the work out, for a
  change such as a cache that does less of it, and counts apart a case whose
  frames part where one side runs out of work; `--cache` gives every case
  other cache limits); it prints the peak budget of each workload for
  both.

The comparison establishes agreement with libass on that corpus, not conformance
with any specification of the format.

## Not here

libass's simple shaping level and hinting modes,
its selective style override (`ass_set_selective_style_override`; with libass's
default settings it changes nothing), track features (`ass_track_set_feature`),
`ass_step_sub`, `ass_configure_prune` (a player calls
`prune_events`), reading files and charset conversion.

# triergon-ass

ASS and SSA subtitle scripts as libass 0.17.5 reads them: whole scripts and
Matroska packet streams, styles and events, typed override tags, the state each
glyph of an event has at a given time (animation, movement, fades, clips,
karaoke timing, transition effects) and the drawing language. It depends on
[`triergon-core`](../core) for `Budget`, `Bytes`, `WorkBudget` and errors, and on
[`triergon-raster`](../raster) for the `Outline` a drawing is read into, the one
libass rasterises.

This crate gives each glyph the state libass would give it, in script units.
Layout, font selection, shaping, line wrapping, collisions, rendering and the
image list are not here.

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

## Behaviour and limits

A `Script` charges its styles, events, strings (one allocation per style and per
event), format lines, fonts and ReadOrder bitmap to its `Budget`; an `Outline`
charges its points to its own. `Limits` bounds the work of one call (input bytes
and lines, format fields, style lookups, ReadOrder scans, tags, glyphs and
drawing points; a `Context` takes one call's work for its whole walk), the styles
and the events held; exceeding one is `Error::Policy`, an exhausted budget
`Error::Limit` or `Error::Backpressure`. A `\t` inside a `\t` continues in place,
so tag evaluation recurses at most one level. `Syllables` is linear in the glyphs
it is given. Evaluating an event allocates nothing. Malformed input is read as
libass reads it, skipping what it skips, and never panics.

## Verification

- `cargo test -p triergon-ass`: unit tests per module and a deterministic mutation
  test (`tests/robustness.rs`) over authored scripts, including work, event and
  budget limits.
- `cargo xtask ass compare` runs [ass-worker](../ass-worker/README.md) and the
  libass reference worker (`harness/reference_ass.h`) on every authored fixture in
  `harness/fixtures/ass` and requires identical work: `ass-parse-scripts` (whole
  scripts), `ass-parse-chunks` (Matroska packet streams) and `ass-parse-events`
  (every event walked at the fixture's times), and `ass-parse-host` the
  operations whose libass result differs by target, each against its target's
  pin. `cargo xtask fuzz --contract
  harness/features/<feature>/contract.json --reference` mutates the fixtures and
  compares both workers on every mutation.

The comparison establishes agreement with libass on that corpus, not conformance
with any specification of the format.

## Not here

Rendering and everything it needs: layout, line wrapping, collisions, shaping,
font selection and fallback, bidi, text glyph outlines, borders, blur, clips as
bitmaps, compositing and the image list. Also not implemented: libass's selective
style override (`ass_set_selective_style_override`; with libass's default settings
it changes nothing), track features (`ass_track_set_feature`), `ass_step_sub`,
`ass_configure_prune` (a renderer calls `prune_events`), reading files and
charset conversion.

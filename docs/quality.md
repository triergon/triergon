# Quality: reference, gates and harness

Every module is measured against a pinned reference implementation doing the same
work: FFmpeg for containers, framing, subtitle decoding, codecs and capture, FreeType
for font tables and outlines, HarfBuzz and FriBidi for shaping and bidi, and libass
for rasterising and subtitle rendering. The gates and the
measurement rules below are the same for every reference. A module is complete only
when it passes all three gates on every case in its corpus.

## Gates

| Gate | Requirement |
|---|---|
| Parity | Every declared feature matches the reference on every required case. Missing inputs, crashes, unsupported declared cases and unexplained differences are failures, never omitted rows. |
| Runtime | `T_triergon / T_reference` at most **1.00** per case, using the upper bound of a paired 95% confidence interval ([early stop](#early-stop)). The optimisation target is **0.80**. Both values are defined in [`harness/policy.json`](../harness/policy.json) and nowhere else; `cargo xtask check` keeps this line and every status row in step with it. |
| Memory | Peak live heap and total resident peak RSS both strictly lower than the reference, each at the upper bound of a paired 95% interval ([early stop](#early-stop)). |

The harness reads these limits from `harness/policy.json`. Each feature's contract and
corpus manifest live in `harness/features/<feature>/`.
RSS growth from the start of the operation is reported separately when every pair
has positive growth bounds.

The work being compared must be identical: the same selection, the same packets, the
same output. A gate is never passed by narrowing the work, dropping slow cases or
weakening a comparison. A failed result stays recorded against the exact source and
binaries that produced it.

The reference is the compatibility reference, not the specification. Disagreements are
investigated against the standard. A reference bug or a deliberately stricter
resource limit becomes an explicit, reviewed rule for that case.

## Reference and corpus

- **Pinned downloads.** Reference sources and corpus files are fixed by their SHA-256.
  Each may list mirrors, tried in order, and any bytes that don't match the pin are
  refused. With `TRIERGON_DOWNLOAD_CACHE` set, verified downloads are also kept in that
  directory under their hash and reused before any network access. CI keeps this
  cache between runs.
- **Reference builds.** Each prepare command (`prepare-ffmpeg`, `prepare-mpg123`, `fonts prepare`,
  `shape prepare`, `raster prepare`, `ass prepare`) keys its build by the SHA-256 of
  what the build depends on besides its own directory: the xtask code that defines it
  (with its pinned source hashes, configure flags and link steps) and the worker link
  policy's, its directory (the configure prefix), the keys of the builds it links, the
  C and C++ compilers and their target, the installed package list (FFmpeg's default
  configuration detects optional system libraries) and the compiler and `pkg-config`
  environment variables. The key is recorded in `reference-key.json` beside
  `build.json`. A prepare whose directory holds its key does nothing; one whose key is
  packed in `.research/reference-cache/NAME/KEY.tar.zst` unpacks the pack, which
  restores the directory as the packed build left it, so `build-workers` links the same
  libraries and records the same identity; only a miss builds. `cargo xtask references
  keys` lists each build's key and whether it is current, cached or missing, `pack`
  packs current builds, and with `TRIERGON_EVIDENCE_ARCHIVE` set `fetch` and `upload`
  move packs to and from `qualification/reference-builds/NAME/KEY.tar.zst`. A pack is
  not evidence: it expires 90 days after its upload, and the next prepare that misses
  it builds and uploads it again.
- **FFmpeg pins.** `cargo xtask prepare-ffmpeg --profile P` builds FFmpeg from a pinned,
  hash-checked source with recorded configure flags, compiler and binary hashes, and
  `cargo xtask build-workers` builds the reference worker of every prepared profile
  and records the baseline version from the prepared installation. Version
  discovery for an unpacked archive stops at the reference source boundary.
  Containers are measured against git revision `d32b387f` on the 9.0 branch: `matroska`
  for the container harness, `formats` for MOV, TS and the HLS segment reference,
  `video` (the Matroska build plus the H.264 and HEVC decoders that stream discovery
  needs) for the playback startup and seek reference, and `subtitles` (the PGS, DVB,
  DVD, closed-caption and teletext decoders, the latter through a hash-checked
  libzvbi 0.2.46, with the video decoders caption extraction needs) for the subtitle
  decode comparison. A decoder that refuses to open with the stream's
  configuration and options is a result both subtitle workers fold alike, as no
  subtitles and the one word 2^64 − 1, and trace as a `"refused":"open"` line in
  place of the header. The `subtitles` worker is linked with `--wrap` for the C
  library calls whose result can differ between its targets for the same
  arguments: `strtol`, `strtoul` and `sscanf`'s integer conversions, which read
  through `long`, and the rounding functions with results C leaves unspecified
  out of range. Its `sub-fold` flags a run that reached such a difference
  (`harness/reference_subtitles_host.h`), and a flagged case fails any feature
  not pinned per target. Because the wrappers run inside the operation, that
  build is never timed or measured: the subtitle features name the same build
  without them, `ffmpeg-subtitles-plain-native`, as their
  `measurement_reference`. Decoders and
  capture are measured against the FFmpeg 9.0.2 release tarball: `decode` (the default
  configuration) for the codec comparison, whose `ffmpeg` and `ffprobe` also decode
  capture output independently, and `capture` (gating) and `capture-small` for the
  capture comparison. The two capture profiles link FFmpeg's component libraries
  into the worker so the allocation observer can audit the code doing the work.
- **DTS transport reference.** The `transport-dts` profile enables the MPEG-TS
  demuxer and DTS parser for the [DVB DTS comparison](../harness/features/mpegts-dts-core/contract.json).
  The legacy packet operation uses the same transport fold as the formats reference.
  The [grouped DTS-HD comparison](../harness/features/mpegts-dts-hd-grouped/contract.json)
  uses the demuxer without parsing, then explicitly feeds each aligned finite PES
  to the public DTS parser and drains it. Its source-clock fold includes original
  PES headers, physical and elementary ranges, one-use PTS/DTS and the exact
  reference-clock duration. A small independent fixed-prefix reader supplies the
  source duration, distinct from the parser's asset-derived duration. The
  [multiple-extension comparison](../harness/features/mpegts-dts-hd-extensions/contract.json)
  uses complete demuxer packets, because the explicit parser separates extension
  substreams. These operations keep the legacy DTS-HD comparison unchanged.
- **Container scope.** A candidate worker carries only the runner families its
  reference build can demux. `media-worker` reads Matroska for the `matroska` build's
  container harness; `playback-matroska-worker` runs Matroska startup and seek (`feed-`)
  and Matroska audio clocks (`mkv-audio-`) against `video`, whose only demuxer is
  Matroska, and still links the playback engine's MP4 and TS backends, which `Engine`
  holds beside Matroska; and
  `playback-worker` carries every playback runner family for MP4 forward and range, TS
  programs, raw Annex B and HLS segments against `formats` and its 12 demuxers (`ass`,
  `h264`, `hevc`, `matroska`, `mov`, `mpegps`, `mpegts`, `mpegtsraw`, `srt`, `sup`,
  `vobsub`, `webvtt`). Both playback binaries are built from the same runners
  (`crates/playback-worker`); a contract's `candidate` names its binary.
- **FreeType pin.** `cargo xtask fonts prepare` builds the FreeType 2.15.0 release
  tarball (hash-checked, tree verified before and after the build) as a static
  library without optional dependencies and records the configure flags, library
  and compiler in `.research/freetype/build.json`; `cargo xtask build-workers` then
  links the reference worker (`harness/reference_font.h`). `cargo xtask fonts fetch`
  fetches the font corpus listed with sources, licences and hashes in
  `harness/fonts/corpus.json` and writes its authored fonts with the font crate's
  `authored-fonts` example. Its files are the cases of four features run like
  every other feature: `font-outlines` (face data and unscaled outlines),
  `font-hinting-none` (libass's default configuration: unhinted outlines and
  default-load advances at 256 pixels) and `font-hinting-default` (every glyph's
  default-load metrics and hinted outline there), which `cargo xtask check`
  requires, and `font-hinting-modes` (the nine other heights libass requests when
  hinting is on), run on demand and not gated yet. `cargo xtask fonts compare`
  reports the required three, with `--all` all four. Behaviour of TrueType
  programs that no specification settles is established by probes:
  `harness/fonts/probes/*.probe` describe small faces and their programs,
  `cargo xtask fonts probe` assembles them and loads them through both font
  workers, and each `.dump` beside a probe records the reference's default load
  on 32-bit ARMv7 (`--record`).
- **HarfBuzz and FriBidi pin.** `cargo xtask shape prepare` builds the FriBidi
  1.0.17 and HarfBuzz 14.6.0 release tarballs (hash-checked) as static libraries:
  FriBidi with its own configure, HarfBuzz from its single-file amalgamation
  without optional integrations, both `-O2 -ffp-contract=off`, recorded in
  `.research/harfbuzz/build.json`; `cargo xtask build-workers` then links the
  reference worker (`harness/reference_shape.h`) with them and the pinned
  FreeType. Its features shape the font corpus with authored texts and seeded
  random strings as libass does, through FreeType-backed font functions at 256
  pixels (`shape-libass`) and through HarfBuzz's own at units per em
  (`shape-text`), resolve bidi paragraphs and lines (`shape-bidi`), run libass's
  simple shaping level (`shape-simple`), read every code point's properties
  (`shape-unicode`) and every glyph's extents of the corpus's CFF, CFF2 and
  collection faces at units per em (`shape-extents`), all required by `cargo
  xtask check`; `shape-text-all`, every corpus face with every script at units
  per em, runs on demand. Acceptance of the shaper rests on `shape-libass`,
  `shape-bidi`, `shape-simple` and `shape-unicode`. `cargo xtask shape compare`
  reports the required six, with `--all` all seven.
- **libass pin.** `cargo xtask raster prepare` builds the libass 0.17.5 release
  tarball (hash-checked, tree verified after the build) as a static library
  without a system font provider (`--disable-fontconfig --disable-coretext
  --disable-directwrite --disable-libunibreak
  --disable-require-system-font-provider`), `-O2 -ffp-contract=off
  -ftrivial-auto-var-init=zero`, against
  the FreeType, FriBidi and HarfBuzz builds above, so each library has one
  build, and records it in `.research/libass/build.json`. `cargo xtask
  build-workers` then links the reference worker (`harness/reference_raster.h`):
  it includes `ass_render.c` from the pinned tree to reach libass's internal
  stages and is compiled with the flags libass compiled itself with
  (`libass-internal.pc`). It replaces the engine `ass_renderer_init` picks for
  the host with libass's plain C bitmap engine
  (`ass_bitmap_engine_init(ASS_CPU_FLAG_NONE)`: C functions, 16×16 tiles and
  16-byte rows), recorded in `build.json` and checked at startup, on every
  host: it is what libass runs on 32-bit ARMv7, where it has no assembly, and
  it does not depend on the host. Uninitialised automatic variables are zeroed
  for the same reason: the pinned source reads one (the face `uid` in
  `add_face`), and zero is the value both deployment targets' results agree
  with. The flag also covers the reference worker's own code
  (`harness/reference.c` and `harness/reference_raster.h`), which takes the
  same flags; there a read of an uninitialised variable fails the build
  (`-Wall -Werror -Werror=uninitialized`). libass's included sources are
  compiled as system headers, without warnings. Runtime is not measured yet;
  two rules hold when it is. Timing uses an unflagged build: the same source,
  pins and configure flags without `-ftrivial-auto-var-init`, both recorded in
  `build.json`, while parity keeps the flagged one. The flag changes the
  reference's instruction counts by under 0.5% on the required workloads, but
  its code layout makes small-glyph workloads 8% to 40% slower on Linux
  AArch64 and x86_64. And the reference's timed region holds only the work
  the candidate's does: libass's per-run cache reset (`ass_cache_empty`),
  most of a small-glyph operation, leaves the clock, or both sides do the
  equivalent reset inside it, whichever matches how a caller uses the
  library; caches that would make repeats unfairly cheap are disabled for
  timing, not reset inside the clock. The change that first times the
  rasterising or rendering features makes `cargo xtask check-design` or the
  timing code refuse a lane whose reference build is flagged. Its features run libass's outline, stroker,
  rasteriser, blur, composition and clipping stages on glyphs of the font
  corpus loaded as libass loads them with hinting off (FreeType in the
  reference, `triergon-font` in the candidate; `raster-glyphs`) and on seeded
  random outlines (`raster-outlines`) and bitmaps (`raster-bitmaps`), all
  required by `cargo xtask check`; `raster-sweep`, every corpus file at three
  heights with larger random sets, runs on demand. `raster-simd` runs the
  required cases against the host's own SIMD engine (NEON, AVX2, SSSE3 or
  SSE2) with their pins, the C engine's folds, and records where that engine
  departs.
  `cargo xtask raster compare` reports the required three, with `--all` also
  the sweep, with `--diagnostic` `raster-simd`, and with both all five.
- **libass parsing.** The ASS parsing lane uses the same libass build: `cargo xtask
  ass prepare` is `cargo xtask raster prepare`, and `cargo xtask build-workers`
  also links the ASS parsing reference worker (`harness/reference_ass.h`), which
  includes `ass_render.c` from the pinned tree for the render context, and
  `ass_drawing.c` and a copy of `ass_parse.c` that `cargo xtask ass prepare`
  and `build-workers` write, to see the conversions they make, and restates,
  citing them, the two static functions it needs: the text loop of
  `parse_events` and `ass_read_styles` from memory. Its features read the authored
  fixtures in `harness/fixtures/ass` with libass's parse-level functions: whole
  scripts with `ass_read_memory` (`ass-parse-scripts`), Matroska packet streams
  with `ass_process_codec_private`, `ass_process_chunk` and its ReadOrder
  deduplication, `ass_flush_events` and `ass_prune_events` (`ass-parse-chunks`),
  with style overrides and font extraction where a fixture asks, and every event
  walked at the fixture's times through `ass_parse_tags`, `ass_get_next_char`,
  `ass_drawing_parse` and `ass_process_karaoke_effects` (`ass-parse-events`). All
  three are required by `cargo xtask check`. The reference flags a case whose
  result C leaves to the target, by an explicit check where libass makes the
  step, on every host: a conversion from `double` to `int` out of range or for
  NaN (a drawing coordinate at `double_to_d6`, `lrint` narrowed from `long`;
  the casts for `\pbo` in a drawing glyph, an animated `\clip` or `\iclip`,
  `\fade`'s alpha, a `Banner` or `Scroll` delay; the copy of `ass_parse.c`
  wraps each of its conversions, and the edit fails if the source does not
  match), or a drawing coordinate landing
  on `INT_MIN`, whose `abs` its bound check takes. A flagged case fails those
  three features. It is a fixture of its own in `ass-parse-host`, with the
  reads of values that go through C `long` beyond `int` (an event time, a
  ReadOrder); that feature is pinned per target. `cargo xtask ass compare`
  reports all four.
- **libass rendering.** The same reference worker renders frames with
  `ass_render_frame` (`harness/reference_ass_render.h`) with libass's plain C
  bitmap engine on every host, as the rasterising reference sets it, as a
  player embeds libass: the fonts a fixture attaches given to `ass_add_font`,
  font extraction on, `ass_set_fonts` with the fixture's default font file
  and family and no system font provider, no hinting, libass's cache limits
  at 1000 glyphs and 24 MiB, and a frame larger than the storage size.
  `ass-render` renders every script and Matroska packet stream of
  `harness/fixtures/ass-render` at 1920×1080 over 1280×720 storage
  and at 1280×720 over anamorphic 720×576 storage, at the times the fixture
  names, and compares every frame's change value (`ass_detect_change`) and
  every image's type, position, size, colour and alpha bytes in list order
  with `triergon_ass::Renderer`'s; the box behind a `BorderStyle` 4 event,
  whose type libass leaves uninitialised, is told by its own buffer and
  compared as a type of its own. It is required by
  `cargo xtask check` and reported by `cargo xtask ass compare`. The fixtures
  whose libass result differs by target, because libass's renderer overflows
  `int` or converts out of its range on the way, are `ass-render-host`'s
  instead, pinned per target like `ass-parse-host`; the renderer follows
  libass built with GCC for 32-bit ARM hard-float.
  The parity reference is instrumented: it compiles libass's rendering,
  drawing and parsing sources into itself with checks of its own, so it is
  never timed or measured. Both rendering features name a
  `measurement_reference`, `libass-player-native`
  (`harness/reference_ass_player.h`), which renders and folds the same frames
  with `libass.a` linked as a player links it, none of its sources compiled in;
  `report`, `grade` and the case evidence refuse runtime or memory against an
  instrumented reference, and `cargo xtask check-design` requires a
  `measurement_reference` to be an uninstrumented build of the same pinned
  libraries. The timed span is reading the script or running the stream and
  rendering its frames, from the configured renderer to the last frame on both
  sides: libass's renderer setters each empty its caches (`ass_reconfigure`),
  and the teardown frees them, so neither is timed. The memory window holds the
  whole operation. These cases are timed by the same paired runner as the
  codecs and capture: one lock, the same pair loop, contamination watch,
  retries and validator, and the same minimum of pairs.
- **Generated manifests and pins.** The font, shaping, rasterising and ASS
  features commit no case rows. `cargo xtask fonts manifest`, `cargo
  xtask shape manifest`, `cargo xtask raster manifest` and `cargo xtask ass
  manifest` generate them from `harness/fonts/corpus.json`, the shaping texts,
  seeded random cases, the ASS fixtures and each feature's settings
  (`xtask/src/{fonts,shape,raster,ass}/manifest.rs`, registered in
  `xtask/src/lanes.rs`), and take each case's expected work from
  `harness/features/<feature>/pins.json`. Pins are the reference's folds:
  re-record them with `cargo xtask fonts manifest --record`, `cargo xtask shape
  manifest --record`, `cargo xtask raster manifest --record` or `cargo xtask ass
  manifest --record` after `cargo xtask build-workers`, and never edit them by
  hand. `cargo xtask check-design` fails when the generated case ids and the
  pins differ or a pin file is not in the form the recorder writes. Each
  feature's contract says when it runs in `gate`: `"required"` (by `cargo xtask
  check` and `COMMAND compare`), `"on-demand"` (also by `COMMAND compare --all`)
  or `"diagnostic"` (by `COMMAND compare --diagnostic`); `cargo xtask
  check-design` fails when it is missing or anything else.
  A contract may set `timeout_seconds`, how long one worker run of its cases may
  take: a whole number of seconds from 1 to 3600, 30 when absent. `report` (its
  parity, timing and memory runs), `external-vector`, `trace-identities` and `fuzz
  --contract` (unless `--timeout-seconds` is given) honour it, and `cargo xtask
  check-design` fails on any other value. `raster-sweep` sets 120 for its heaviest
  cases on 32-bit ARMv7. `parity_timeout_seconds`, within the same bounds, limits the
  worker runs of `report`'s parity comparison alone: a parity run is not timed and
  both folds must still be equal, so a reference slow on one target can have more time
  there while timing and memory keep `timeout_seconds`. The font, shaping and ASS
  contracts set 120: on 32-bit ARMv7 FreeType folds the NotoSansCJK collection's
  outlines at 16384 in up to 29.78 s, and 120 is four times that, rounded up.
  `required_targets` names the targets every case must pass on. Every feature the
  gate runs names both Linux AArch64 and 32-bit ARMv7. A report on one of them
  can accept that target's slice; accepting the whole module gate also needs the
  other target's evidence, which one run does not hold.
  A contract with `"pinned_per_target": true` pins each case per target triple:
  `pins.json` maps each case to a fold for each of the contract's
  `required_targets`, except the targets `unpinned_targets` lists for a case
  with a reason. A report compares a case with its pin for the host's target and
  runs and reports, without failing, a case that has none there. `COMMAND
  manifest --record` records the host target's pins, and on a target whose
  workers `build-workers` cannot record, `--record --target TRIPLE
  --reference-worker PATH` records that target's pins with the given reference.
  check-design fails when a case lacks a target's pin or has one it should not,
  when a case is the same on every target (it belongs in a feature pinned for
  every target), and when a feature not pinned per target has a pin the
  reference flags.
  A feature with a written manifest pins a case per target in its
  `expected_by_target`. The five bitmap, caption and teletext subtitle features
  pin each case once, from the reference on AArch64, and require 32-bit ARMv7
  too, so every ARMv7 run compares that target's reference with those pins: a
  case whose reference result differs between the targets fails there, flagged
  or not, until a feature pinned per target holds it. `subtitle-dvd-host` holds
  the DVD cases the subtitles reference flags. The decoder follows FFmpeg built
  with GCC for 32-bit ARM hard-float, and such a case is open on
  AArch64.
  For text, host text, PGS capsules, DVB and teletext, `cargo xtask
  subtitle-fixtures verify FEATURE REFERENCE` runs the independent FFmpeg worker
  over the authored inputs and checks the complete manifest byte for byte.
  Use `record` to write the manifest once every fold succeeds. For
  `subtitle-text-host`, both commands take `--target TRIPLE` and use that
  target's reference. Recording preserves the other targets' folds.
  The [subtitle worker guide](../crates/subtitle-worker/README.md#recording-and-verifying-subtitle-fixtures)
  lists the workers and input roots.
  Pins are recorded with the feature's `worker_operation`, the operation
  `report`, `grade` and `fuzz` run. A diagnostic feature that compares another
  reference with these pins names the features in `cases_from` (`raster-simd`
  takes the required rasterising features'): its manifest is their rows and
  pins, in order, and it commits no pin file; check-design fails if such a
  feature is not diagnostic or has one.
  Every harness command reads such a manifest at `harness/features/<feature>/corpus.json`;
  its identity is the SHA-256 of the generated JSON, the bytes the generator
  writes, so it is deterministic.
- **Open cases.** A contract may list `open_cases`: case ids that fail today, each
  tied to one of its `pending_module_requirements`. `cargo xtask report` still runs
  them and prints them as open failures with their count, and passes only when every
  other case passes and every listed case still fails; a listed case that starts
  to match must leave the list. `cargo xtask grade` applies the same rule: it grades
  a listed case's parity and skips its timing and memory, refuses a listed case that
  passes or is not in the corpus, and still needs measurements for any other failing
  case. A module stays unaccepted while any case of one of its gating features is
  listed. Features that a module's documentation names as additional coverage, such
  as `shape-text` and `shape-text-all` for the shaper, keep their open cases listed
  without blocking it.
- **Bounds.** `cargo xtask grade` fails on a case whose runtime bound is above the
  ceiling, or whose heap or total RSS bound is not below 1, or whose measurement of
  either is not valid, and prints one line per failing bound and case, as the codec
  bounds do. A contract may list `open_bounds`: each entry a `bound` (`runtime`,
  `heap` or `total_rss`), the `cases` it fails for today, its measured `cause` and
  optionally the gate `targets` it is open on. Such a bound fails as `open as listed`
  and does not fail the step; the case's other bounds are graded as any case's, and a
  listed bound that holds is printed for removal from the list. `cargo xtask check`
  validates every contract's `open_bounds` against its corpus. Suites that have no
  grade apply the same verdict to their own collections: the progressive TS suite's
  timing and memory collections, and `capture qualify --stage memory` for each case
  not open on the target; the ISO BMFF suites and the codec workload lists already
  fail on a failing bound.
- **Input rights.** A case is measured only when its `rights` are
  `authored-synthetic`, `reviewed-licence` with a `licence` under which a
  reviewed input manifest (`harness/fonts/corpus.json`) records the file's hash, or
  `fetched-not-redistributed` for an upstream regression sample, or a local remux
  of one, whose hash `harness/fate/samples.json` records: fetched into the ignored
  corpus cache, compared locally and never redistributed, its redistribution
  rights unreviewed. Any other case fails as `rights-unreviewed`.
- **Authored corpus.** Small canonical streams generated from synthetic signals and
  authored structures, so inputs stay identical across encoder versions
  (`cargo xtask fixtures`, `harness/features/<feature>/{contract,corpus}.json`).
- **FATE.** Upstream regression identities are indexed as data, without running
  Make (`cargo xtask fate-inventory`, `harness/fate/index.json`), and mapped case by
  case. A FATE name is not a test until its input, operation and expected result are
  mapped and run. Samples are fetched into an ignored cache with
  recorded hashes; nothing is redistributed without reviewed rights.
- **Required vectors.** Larger external vectors are fetched and hash-verified by
  `cargo xtask fetch-vectors` and kept outside version control.

## Correctness by module family

| Family | Compared |
|---|---|
| Time | `cargo xtask core-time` compares exact rational arithmetic, rounding, overflow, signed extremes and wide arithmetic against independent Python integer and Fraction results in debug and release builds. Workspace tests also cover 32-bit narrowing. |
| Demux | Track identity and configuration; payload hashes in per-track decode order; signed PTS, DTS, duration and trim; random-access facts; side data, metadata, attachments and chapters |
| Seek | Landing point, preceding random-access point, preroll and the packets that follow; fresh, repeated, backward and after-EOF seeks; missing indexes; changing input windows; cancellation |
| Packet framing | Equivalent access units after documented framing normalisation; configuration and payload preserved |
| Subtitles | Exact cue timing, text and flags; normalised bitmap rectangles, palettes and alpha |
| Decoders | Exact output where the standard guarantees it; otherwise predeclared per-format numeric tolerances |
| Encoders | Standards compliance, independent decode, quality at equal bitrate, throughput and memory |

## Harness

The harness runs the same operation through a Triergon worker and a reference worker and
compares canonical output. Workers speak one line protocol with `trace`, `bench` and
`memory` modes, framed by begin/end markers and a named digest.

```sh
cargo xtask prepare-check   # build the FFmpeg reference, fixtures and workers
cargo xtask check           # format, test, clippy, rustdoc, release build, worker tests, design checks
cargo xtask report --fate-cache .research/fate-samples
                            # correctness report against the reference
cargo xtask report --benchmark --memory --fate-cache .research/fate-samples --require-acceptance
                            # correctness, runtime and memory with acceptance
```

Prepare the formats reference with `cargo xtask prepare-ffmpeg --profile formats`
and rebuild workers before running the container and packet-framing commands.
`cargo xtask isobmff-bench` compares all 1,020 open and index outcomes, including
48 refusals. `--benchmark --memory` measures the 972 output labels, with 956
physical operations and 16 explicit aliases. `--subset52` selects the separate
52-operation measurement set while retaining the full correctness comparison, and
`--part I/N` every Nth physical operation from the Ith with their aliases, so that N
parts measure the set once between them.
`cargo xtask isobmff-seek` compares all 476 seek and range-plan outcomes, including
4 refusals, and `--benchmark --memory` measures the other 472.
`cargo xtask framing --benchmark --memory` measures all 135 packet-framing scripts.
Their contracts and input manifests are in `harness/features/isobmff-open-index/`,
`harness/features/isobmff-seek/` and `harness/features/packet-framing/`.

`report --memory-pairs N` selects a separate memory sample count, with
`--pairs` applying to both measurements by default.

`cargo xtask progressive` requires three comparable forward-only AVC/AAC TS188
packet folds and all five functional inputs before measurement. All ten literal
worker outcomes are asserted, including positive normative Rust output and the
recorded reference refusal on the legal duplicate control. `--benchmark --memory`
measures only the three original comparable cases under the [runtime and memory
gates](#gates). Its contract, separate functional-control manifest and
hash-pinned inputs live in `harness/features/progressive-ts188/`.

`cargo xtask report` runs the correctness comparison of one feature contract
(`--contract`, `--manifest`, `--corpus`; a manifest may record its corpus `directory`)
and grades it. The Matroska Cues comparison uses the same six probe times for
every input, both on a fresh reader and after EOF. The compatibility seek must
match the full reference trace. The playback seek must keep every packet of
that reference trace in order. Additional audio or subtitle preroll must occur
before the seek target and before that track's first reference seek packet in
the sequential trace. Its packet fields must match the sequential reference in
source order, with initial trim cleared. An untimestamped continuation must
include the preceding timestamped packet for that track from the sequential
trace in the added preroll.
A contract may name its own candidate worker and reference, as
`matroska-startup-seek` and `matroska-audio-clocks` do with `playback-matroska-worker`
and the `video` reference and `hls-segments` with `playback-worker` and the `formats` reference, the six bitmap, caption and teletext `subtitle-*` features with
`subtitle-worker` and the `subtitles` reference, `subtitle-text` with `subtitle-worker`
and the `formats` reference, the three font features with `font-worker` and FreeType, the six
shaping features with `shape-worker` and HarfBuzz and FriBidi, the five rasterising
features with `raster-worker` and libass, and the four ASS parsing features,
`ass-render` and `ass-render-host` with `ass-worker` and libass.
Such a worker takes one input per operation through the contract's
`worker_operation` prefix (`PREFIXfold`, `PREFIXbench`, `PREFIXmemory`). `--benchmark` adds paired
runtime and `--memory` the heap and RSS measurement under the native observers
(`harness/allocations*.c`), including the Linux link-closure and active-window guard
certificates. The observers count `malloc`, `calloc`, `realloc`, `free`,
`posix_memalign`, `aligned_alloc`, `strdup` and `strndup`. On Linux a worker is
measured only if every allocation entry point it and its libraries import is one of
these; a guarded one; an operator `new` or `delete` form, when one library of the
closure, not the observer, defines every form and the probe ran this one at the same
version; or a weak import that nothing in the closure defines, which assumes no library
is loaded with `dlopen` before the operation. The probe links libstdc++ to run the
twenty forms, so every Linux certificate's sealed closure includes libstdc++, libm and
libgcc_s, and building it needs `-lstdc++`. Inside the operation, the Linux observer
admits read-only file mappings, private or shared, and their exact `munmap`. Their
resident pages count toward total RSS, the kernel bound adds each one's whole length
(`VmPeak` already includes it), and the receipt reports them as `file_mappings` and
`file_mapped_bytes`. Any other mapping or unmapping voids the measurement, on either
side. The observer does not guard `mprotect`: a private file mapping made writable with it
holds memory the heap figure misses, though total RSS still counts it. Receipts of this observer (`linux-glibc-v4`) list eight hooks in `symbol_order`,
so a worker's `strdup` and `strndup` move from the `malloc` counters to their own; v2
and v3 receipts list six and still validate when re-graded. The report records the build identity, source provenance digest and input
hashes it was measured with; when a result is cited, its source revision and raw-sample
SHA-256 go in the [measured revisions](status.md#measured-revisions) table. `cargo xtask compare` compares a single input, `cargo xtask external-vector`
qualifies the mandatory external vector and `cargo xtask fate-run` runs one supported
upstream FATE case. `cargo xtask build-workers` builds the workers and records the build
identity every collection is checked against. `cargo xtask trace-identities` records the
digest of every candidate trace, so a change can show its traces are byte-identical
(`--compare` an earlier record): the Matroska cases for `media-worker`, every case of
each feature that runs the `--candidate` worker through a prefix otherwise.
Pass `--corpus FEATURE=DIR` for each prefix feature whose files are outside its
recorded directory or whose manifest omits the directory. Relative directories are
resolved from the repository root. Prefix comparisons verify the reference worker's
build hash and the manifest hashes of the primary input, ordered entities and
companion files before running a case. DVD palette paths resolve inside the
selected corpus and keep their manifest spelling in recorded traces.
`cargo xtask worker-tests` runs the integration tests
of the built workers: work digests, catalogue, colour, content encoding, seek, timing
and inflate behaviour against the reference workers and the oracles. The independent
oracles stay in Python (`harness/*_oracle.py`, run through `python3 -m harness.oracles`,
or the Python `TRIERGON_PYTHON` names, as for the capture input generator),
as do the numpy table-recovery chains under `harness/codecs/` and the capture input
generator `harness/capture/generate_inputs.py`, whose numpy PCG64 and float draws the
cited capture inputs depend on. `cargo xtask codecs` grades, times, measures the memory
of and records the identities of the codec crates against the pinned FFmpeg decoders,
and compares the packets of the audio container readers, and the Ogg reader's
packets after seeks, with its `ffprobe` (`harness/codecs/STATUS.md`; the gate runs the
Ogg lanes); `cargo xtask capture` does the same for the capture
pipeline ([harness/capture](../harness/capture/README.md)).
`cargo xtask codecs aac-access-units` prepares the [configured AAC
workloads](../harness/features/aac-access-units/contract.json) from the pinned
QuickTime and authored audio-only packets. It compares the ordered speakers and
every decoded PCM plane. The authored clips cover every supported fixed layout
and indexed rate, and program configurations from mono to eight channels.
`cargo xtask codecs aac-sources --out DIR` records the source
clips from independently varying channel tones using the pinned encoder. The
committed sources fix the encoded input bytes across architectures.
The paired codec timing and memory commands require passing parity bound to
the workload manifest, both workers, the pinned probe and the numeric rules.
The measured work includes decoder construction, decoding every
packet, folding every PCM plane, flushing and teardown. File loading and
container parsing take place before that work.
`cargo xtask fuzz` runs a deterministic, replayable byte-mutation campaign against
debug, release and reference workers from a pinned manifest, with a timeout per
input (`--timeout-seconds`, 3 by default, or the contract's `timeout_seconds`). With `--contract` it builds that manifest from a prefix feature's cases: the
release candidate and its dev build, plus the reference with `--reference`; listed
open cases are expected to be rejected by both builds, and the campaign keeps the
manifest as `manifest.json` for replay. `cargo xtask codecs fuzz --lane L` builds it
from a codec lane's FATE vectors instead: release and `checked` builds of `codec-worker`
(release optimisation with overflow checks and debug assertions) decode each mutation
whole. `cargo xtask check` runs one round on smoke cases of each required font,
shaping, rasterising and ASS feature.

On native AArch64 or 32-bit ARMv7 Linux, `cargo xtask memory-admission CANDIDATE REFERENCE`
checks the worker libraries using the probe and observer in `target/harness`.
It prints the calibration certificate and exits nonzero if coverage cannot
be established. Use built workers you trust.

`cargo xtask build-workers --only framing-worker` rebuilds the primary `media-worker`
and only the named additional Rust worker. `--only media-worker` builds just the
primary Rust worker. Reference workers and memory observers are still built as usual.
The new identity contains only rebuilt candidates; unselected binaries left on disk
are not current. Without `--only`, all Rust workers are built. The gate-build guard
and the comparison-only meaning of `--comparison-build` apply with either selection.

### Separate timing and memory collections

`cargo xtask grade TIMING MEMORY --contract F --manifest F --output DIR` joins
complete standard-harness reports and recomputes runtime and memory bounds from
their raw paired samples before applying the feature gates. Parity and FATE checks
use the stored outcomes; grading does not rerun workers or oracles. Both collections
must contain every case in the registered corpus, with identical parity, inputs,
contract, workers, source graphs, compiler settings and instrumentation. Each report
records the CPUs it ran on, which may differ: timing runs alone on one CPU, memory on
several. The memory build identity may add a kernel RSS proof pin; every other
identity field must match. Both input reports and their raw samples are hash-linked
from the graded report.

`cargo xtask report` writes each case's raw measurements once and records their
SHA-256 in `partial.json`. After validating the complete collection it publishes
the compact `collection.json`, then streams the self-contained `report.json`.
Pass `collection.json` and keep its `measurements/` directory alongside it to grade
large collections with one case's samples in memory at a time. Use a new `--output`
directory for each invocation of `report`, `progressive` or `grade` so earlier
measurements, traces and grades remain available.

For ISO BMFF, collect `isobmff-bench --benchmark` and `isobmff-bench --memory`
in separate output directories. Pass their `collection.json` files to `grade`
with the registered open and index contract and corpus. Keep the `case-NNNN/`
directories with both collections so the grader can verify the measurements.
On AArch64, the sharded gate grades each ISO BMFF timing and memory pair on its
measurement host with `grade --isobmff-part I/N`. The grader checks the full parity
corpus and the exact partition of measurements, including matching aliases. The
gate summary requires a passing grade for each planned part.

Single external-vector reports can be graded as a pair too. Supply each required
external grade with `--additional-report PATH` when grading its corpus.
`--require-slice-acceptance` requires the feature gates; `--require-acceptance` also
requires the recorded FATE coverage and complete module requirements.

Timing requires the RSS kernel module to be unloaded throughout warm-up and every
pair. Standard memory bounds need no kernel proof. For kernel-backed memory,
follow the [kernel proof workflow](../harness/rss-bound/README.md) after timing and
keep the module loaded through memory collection and grading. The grader validates
the recorded module-absence observations for timing and rechecks the live memory
proof. Rebuilding workers to record the proof must reproduce the same worker and
instrumentation hashes.

## Measuring performance

Qualification enforces the iteration count declared by the contract. `--iterations`
may override that count for diagnostic comparisons.

- Gate results come from the gate build: workers compiled by the toolchain in
  `rust-toolchain.toml` with no extra compiler configuration, and nothing else counts.
  One exception is pinned too: the AAC decode path. `codec-aac-worker`, which serves
  the AAC decode operations alone, and the AAC decode example that `codecs-audio`
  grades are built by the dated nightly xtask pins (`NIGHTLY`, the one `cargo xtask
  check-simd` uses) with the AAC decoder's `simd` feature. The AAC workloads of
  `workloads.json` and the AAC lane's output are measured from that worker, through
  `--candidate-for audio-aac=` and `--codec-worker-for audio-aac=`, and so is AAC in MOV,
  whose contract names it as the candidate. On the 32-bit ARM
  target LLVM emits no NEON float code from scalar loops, and explicit `std::simd`
  vectors are nightly-only; the decoder's SBR QMF banks need them there. Every other
  worker stays on the stable pin, `codec-worker` included: the video, AC-3, E-AC-3, DTS
  and file-format workloads, the conformance vectors and the readers: the
  nightly compiler builds the video decoders slower on ARMv7
  ([status](status.md#32-bit-armv7)).
  One revision built by a newer compiler or with `-C target-cpu` differs from it by up
  to 0.04 on a worst bound, more than a case's margin to the ceiling, so results from
  different builds are not comparable. `cargo xtask build-workers` refuses a `rustc`
  that differs from the pin, however it was found, and `RUSTFLAGS`,
  `CARGO_ENCODED_RUSTFLAGS`, `RUSTC`, `RUSTC_WRAPPER`, target or profile overrides and
  Cargo configuration with the same effect. The grader refuses a report that records
  any of these, or doesn't say. Each worker's record in `identity.json` names its
  toolchain and that compiler's `rustc -vV`, and a worker built by another toolchain
  than the gate build gives it is refused the same way. `--comparison-build` builds
  workers for paired comparisons and quick checks only. Those name their build and
  produce no gate result. The opt-in HEVC `simd` lane uses the same nightly and is not
  a gate build.
- Workers and references follow one link policy, `static-toolchain-runtime-trained-order-v1`:
  on GNU/Linux each side links its toolchain's runtime support libraries from their static
  archives, libgcc's unwinder (`libgcc_eh`) for the Rust workers and libgcc, libstdc++
  and libatomic for the C and C++ references. The C library, libc with libm, and
  third-party libraries, such as zlib, stay shared on both sides. A C program already
  links libgcc statically by default; the Rust standard library instead loads `libgcc_s`
  and runs its initialisation in every program, one that never unwinds included, and
  HarfBuzz and libass load `libstdc++`. Mapped shared, each costs its process resident
  pages its own code does not use: on ARMv7, 18 for `libgcc_s` in the framing worker and
  276 for `libstdc++` in the libass player reference. No worker or reference maps
  libatomic today: the FFmpeg references name it, and the linker leaves it out. glibc's
  static libm needs the static libc, so libm stays with libc. Elsewhere both sides link
  as their toolchains do.
- Every Rust worker enters through C's `main`, as the C references do, instead of
  through the Rust standard library's start-up, so a measurement compares the library's
  work with the reference's and not the Rust runtime's start-up. On glibc that start-up
  finds the main thread's stack guard by reading `/proc/self/maps` through
  `pthread_getattr_np` and `sscanf`, which makes libc's stdio and scanf code resident,
  about 40 pages of the ARMv7 framing worker that the reference never holds. Libraries
  forbid unsafe code; worker packages deny it, and each worker binary's C entry point,
  `#[allow(unsafe_code)] #[unsafe(no_mangle)] extern "C" fn main`, is its one allowed
  exception. The entry point calls
  [`triergon_worker_entry::enter`](../crates/worker-entry), which is safe: it runs the
  worker's `fn() -> ExitCode`, flushes standard output and standard error, and returns
  the exit status. `cargo xtask check` admits that item once per worker binary and no
  other unsafe syntax, and keeps the worker packages' lints equal to the workspace's
  apart from `unsafe_code`. A worker gives up the standard start-up's stack-overflow
  message (an overflow ends with `SIGSEGV`), its `SIGPIPE` handling (a write to a closed
  pipe ends the worker with `SIGPIPE`, as it ends the reference), its reopening of
  closed standard streams and its implicit flush of standard output, which `enter`
  replaces; a panic still ends with status 101, and `std::env::args` is unchanged.
- On GNU/Linux the policy also changes how a Rust worker with a trained order for the
  target links: the pinned toolchain's own `lld` links it, in place of the toolchain's
  default linker, with the functions its trained order lists first, in that order, and
  every other function after them in module order. A worker without one, and every
  reference, links as above. The code one operation runs is a fraction of a worker's,
  and the kernel faults code in 64 KiB windows, so how much of a worker becomes
  resident depends on how its functions lie among those windows. A result of an
  ordered worker therefore reflects the linker, the module order and the trained order
  together. Which workers take an order on which target is what
  `harness/link-order/training.json` gives; [status](status.md) has their results.
  A smaller worker does not take the order's place. Linked by the default linker, the
  functions the training corpus runs lie in every 64 KiB window of each ordered worker's
  code; packed, they fill 6 of the media worker's 11 on ARMv7. Built for size (`opt-level = "s"` or
  `"z"`), the ordered workers' images are 12 to 23% smaller, yet linked without the
  order they keep most of the resident memory the order saves: per worker and target,
  the median training operation recovers 21 to 64% of it, and some recover none
  ([status](status.md#32-bit-armv7)).
  - The trained order of a worker for a target, `harness/link-order/TARGET/WORKER.txt`,
    lists by demangled name the functions a training corpus runs in the release
    worker, in the order the operations first run them, and names in its header the
    SHA-256 of the operations and training files it was recorded from. `cargo xtask
    link-order train` records it on a prepared gate host of the target after
    `build-workers`: it runs each training operation under QEMU's user-mode emulator
    (`qemu-arm -cpu cortex-a15` for ARMv7 in the armhf user space, `qemu-aarch64 -cpu
    neoverse-n1`; package `qemu-user`) with `-d page,in_asm`, which logs every block of
    code when it first runs, and maps those addresses to the worker's functions. The
    workers carry no instrumentation. A worker takes a trained order for a target when
    the training manifest has operations for it that run on that target.
  - The training corpus is its own. `harness/link-order/training.json` lists its files
    by SHA-256 and the operations, each a worker's command line with `@NAME` for a
    training file and optionally the targets it runs on; the files are kept as an
    evidence set and unpacked at `.research/link-order-training`. `cargo xtask
    link-order corpus OUT` makes them with the `ffmpeg` and `ffprobe` on `PATH`, from
    FFmpeg's own video and audio generators encoded with libx264, libx265 and FFmpeg's
    AAC, AC-3, E-AC-3 and Opus encoders, and makes the playback capsules and
    packet-framing scripts from those files; another FFmpeg build may encode
    differently, and the pins then refuse its output. `cargo xtask link-order separate`,
    which `train` runs first, fails unless every training file has its pin and none has
    the SHA-256 of a file any gate item reads on either target or of a committed file:
    the corpus is apart from the gate by exact bytes. `cargo xtask link-order check`
    compares the pins with every gate input known without a prepared host.
  - The operations run in the manifest's order, so a worker's first operations decide
    which of its code lies together. The playback worker's start with the two
    progressive TS188 operations, which run on Linux AArch64 only: the code the
    progressive reader runs for every packet, PES and access unit then lies in one
    stretch rather than among the code the other playback operations ran first, and
    a Graviton2 core's 64 KiB instruction cache misses it less often
    ([status](status.md#measured-revisions)). ARMv7 has no progressive operation, so
    this does not change its order.
  - `cargo xtask link-order BINARY [OUT] --trained FILE` writes a binary's symbol
    ordering file: the functions the trained order lists, in its order, then every
    other function in module order, sorted by the path of the item it belongs to (a
    function by its own path, a method by its type's, a closure by its function's,
    drop glue by the type it drops, generic arguments left out), then by name. A name
    the binary does not hold is skipped.
  - `build-workers` fails when the committed trained orders disagree with the training
    manifest: an order of a worker or target the manifest does not train, a missing
    one, or one recorded from other operations or files. It links each ordered worker
    once, writes its ordering file to `target/harness/link-order/WORKER-HASH.txt`, HASH
    the first 16 hex digits of its SHA-256 so that cargo links again whenever the order
    changes, links it again with that file, and fails when the worker holds fewer than
    95% of its trained order's names or the second link's ordering file differs.
    `identity.json` records for each worker the SHA-256 of its trained order, how many
    names it lists and how many the worker holds, and the SHA-256 of the ordering file,
    or null for a worker linked without one. `cargo xtask check` runs `cargo xtask
    link-order check`, which checks the committed orders against the manifest in the
    same way and, on GNU/Linux, that the worker built for the checking host links in
    each of its orders and holds 95% of the names of its order for that host's target.
    Of an order for another target it holds fewer, as some functions exist on one
    architecture only (64 of the 76 of the AArch64 framing worker's order on ARMv7), so
    that share is printed and not checked; the gate build checks it on each target. The linker arguments, after the policy's
    own, are

    ```text
    -C link-arg=-B$(rustc --print sysroot)/lib/rustlib/HOST/bin/gcc-ld
    -C link-arg=-fuse-ld=lld
    -C link-arg=-Wl,--symbol-ordering-file=ORDER
    -C link-arg=-Wl,--no-warn-symbol-ordering
    ```

    where `HOST` is the host triple of `rustc -vV` and the first link leaves out the
    last two; lld's warnings about ordered names it does not find are off because a
    local name the binary holds more than once is listed once, and the held count
    above takes their place. `build-workers` records the policy in `identity.json`, and
    `report` and `grade` refuse a build that records another policy or none, or whose
    workers' records disagree with the training manifest and the committed trained
    orders of the checking host's target.
- A gate run builds with `cargo xtask build-workers --all-references`, which fails when
  any profile reference's FFmpeg, FreeType, HarfBuzz or libass profile is not prepared.
  Without the flag, `build-workers` builds the references whose profiles are prepared
  and prints each one it skipped; `comparison_references` in `identity.json` lists
  exactly those it built.
- Gate results assume the workspace release profile, `lto = "fat"` and
  `codegen-units = 1`, and a program that depends on these crates matches them only
  with the same two settings in its own release profile and, on GNU/Linux, linked in
  the playback worker's trained order as the workers are (the
  [README](../README.md#getting-started) gives the commands). Cargo's default profile has
  neither. On ARMv7, building the playback worker without fat LTO adds 12.6% user-mode
  instructions on MP4 forward and 25.0% on TS program, and TS program takes 1.228 times
  as long; 16 codegen units add up to another 2.5%. `panic = "abort"` saves
  a further 2 to 3% and is safe: no library code catches or relies on unwinding. A
  program that wraps calls into these crates in `catch_unwind` loses that protection
  under abort.
- `report`, `external-vector`, `codecs time` and `capture qualify` time through one
  paired runner. It holds the host-wide benchmark lock, runs and checks one warm-up of
  each worker, observes every pair from the snapshot that closed the one before, proves
  the RSS kernel module absent at each stage, and retries a contaminated session up to
  twice on the next seeds, keeping the rejected attempts. Each session writes one timing
  record, and one validator grades it: the pairs, the worker and input hashes, a clean
  CPU record around every pair, the same work in both warm-ups and every pair, and a
  ratio interval its samples reproduce. Work a contract names must match between the
  workers. A worker's own output, such as an encoder's stream or a decoder digest that
  may differ, need only repeat across that worker's runs.
- A sample is one worker run of the declared iterations, timed in-process from the first
  iteration to the last; the process warm-up comes before the pairs. A capture sample is
  one run of `--iterations` complete sessions, each opened afresh, and the run fails if
  any session's stream differs. Work counts are totals over the iterations.
- `report`, `external-vector`, `codecs time` and `capture qualify` record the CPUs the
  run may use and those online. On Linux a timing session refuses to start unless the
  process may use exactly one CPU (`taskset -c CPU`), or when it cannot read which CPUs
  it may use, so every Linux bound is measured pinned and alone on its CPU.
- Time optimised builds doing equivalent work. For demuxing, the timed operation is
  open, probe, the required reads and packet production for the same selection, with
  process startup and serialisation outside the timing.
- Use 30 or more randomised A/B pairs per case after warm-up, keep every raw sample,
  and report p50, p95 and the confidence bound per case. An aggregate never hides a
  failing case. A gate session may end earlier, by the rule [below](#early-stop).
- Measure on an isolated machine. Never time while compiling, fuzzing or running
  another benchmark; the runner watches the whole run and rejects it if a tracked
  process uses CPU or steal time grows.
- Allocation tracking and timing are separate runs, because instrumentation changes
  timing.
- To compare two revisions, build both on the same machine and run `cargo xtask report
  --benchmark --baseline-worker W` from the changed tree, with `W` the same worker from
  the base tree. Correctness is still compared with the reference; the pairs and ratios
  are against `W`. Such a report is never an acceptance run.
- Prepare both workers with the same sequential write and paths of equal length.
  Linker output and a sequentially written copy can occupy the page cache differently.
  Timing uses freshly written copies. Linux memory collection does the same for workers
  selected by the report's build registration, one pair of copies per
  [memory slot](#memory-in-parallel). It keeps both original paths for explicitly
  supplied workers and when the loader resolves libraries relative to an executable.
  Keep the measured executables with the raw memory samples. Grading Linux memory
  results rehashes the worker paths in the observer certificate, including any copies
  in the build tree. Before every memory run on Linux, both workers are written back and
  their cached pages dropped (`sync FILE`, then `dd iflag=nocache`, which asks for
  `POSIX_FADV_DONTNEED`; no root needed), so each run of either worker maps its
  executable from the same, empty, page-cache state. Without it, a freshly written copy
  can stay cached in larger blocks, and the kernel then makes more of the executable
  resident on some runs than on others: 320 kB more of a subtitle worker's own mapping,
  and so of the resident peak, on a c8g.xlarge. A failed eviction fails the run. Memory
  results collected before this eviction may include that effect, on either side.
- Before a timing pass, `cargo xtask count CANDIDATE REFERENCE ARGUMENT...` runs both
  workers on one workload under Linux `perf stat`, five runs each, and prints the least
  user-mode instructions, cycles and branch misses. It fails when instructions rise more
  than 0.25%. A count takes seconds, so a code change that adds work shows here first.
  Passing it says nothing about speed: it cannot see load latency, cache misses or where
  functions sit, and it grades neither cycles nor mispredictions, so a change can pass
  and still run slower. The timing pass decides. On a stock Linux host, run
  `sudo sysctl kernel.perf_event_paranoid=1` first, or `perf` reports no supported events.
- A change not meant to affect speed gets a quick check: `cargo xtask same-code`
  on both workers, then one round of 30 pairs against a main build for every contract
  the change reaches, graded by `cargo xtask screen`. It flags a case above 1.5% or a
  group median above 0.75%, so a slowdown of 1% on one case passes. Where functions
  sit moves a case by about 1% either way, so a flagged case with unchanged functions
  and instruction counts is placement. A screen never replaces a recorded result.
- Qualification runs natively: on AArch64 Linux, and on 32-bit ARMv7 Linux in an armhf
  user space (below). Results on one platform do not establish another.
- The gates qualify on one host class for both targets: the AWS c6g.4xlarge
  (Graviton2, 16 vCPUs, 32 GiB), which runs 32-bit user spaces and has the memory
  kernel-bound memory needs. Runtime from another class is not comparable, because
  another core moves the candidate/reference ratio of the same instructions: on 13
  Linux AArch64 rows measured first on c8g (Graviton4) and again on a c6g.4xlarge at
  `79e0ae13`, each candidate's user-mode instruction count stayed within -0.4% to +0.3%
  of the earlier build's (one row's code got 9% cheaper) while the runtime upper bounds
  moved by -0.31 to +0.18 (`cargo xtask count`, below). Every row of
  [status](status.md) names the class it was measured on.

### Running the gates

`cargo xtask gate` holds the full gate as one plan. Its items are the work for one
measured thing: a feature's timing, memory and grade, a part of the ISO BMFF suite's
measurements, the ISO BMFF seek, packet-framing and progressive TS suites, the video
codecs' parity (`codecs`), the audio codecs' correctness lanes (`codecs-audio`: AAC,
AC-3, E-AC-3 and DTS, failing on any vector that fails and is not a listed open case),
a codec workload list's timing, a part of its memory (`codecs memory --part
I/N`, every Nth workload), a capture case (`capture qualify --stage timing`, then
`--stage memory` from its records under the kernel bound), the checked or the release
tests, and the font, shaping, rasterising and ASS comparisons. A contract with
`"runner": "cargo xtask report"` adds its feature to the plan. Its optional
`corpus_directory` supplies the gate's `--corpus` argument. Specialized commands and
on-demand or diagnostic exclusions are listed in `xtask/src/gate.rs`. The planner
rejects a feature with no runner or exclusion. Both
targets run every item. `cargo xtask gate plan --shards N`
splits the items into N shards of about equal planning weight, each for its own host;
the weights are the minutes each item took on a c6g.4xlarge, the more of the two
targets. `--items NAME,...`, given to every gate command of a run, narrows the plan to
those items, for a run of part of the gate; the shards record it, and `gate summary`
checks them against the same narrowed plan, prints that the run is partial and writes
`"partial": true` with the selection at the top of its result. A result cited as the
full gate is a summary whose `partial` is false; a partial one is cited only for the
items it ran.

A change that reaches a few items gates them alone, one host per target, from a
checkout at the change's head:

```sh
tools/gate-aws.sh run aarch64 1 HEAD .research/gate-aarch64 --items matroska-startup-seek,matroska-audio-clocks
tools/gate-aws.sh run armv7 1 HEAD .research/gate-armv7 --items matroska-startup-seek,matroska-audio-clocks
```

`cargo xtask gate plan --target TARGET --items NAME,...` prints what the selection runs.
A change reaches the items whose contract, corpus or inputs it changes, and those whose
workers link a crate it changes (`cargo tree -i CRATE` lists the workers; a contract's
`candidate` names its feature's worker). To let a later run take such a run's results,
add each `OUT/shard-I/entry.json` to the index with its reason and promote it in the
pull request that cites the run. A result is [reused](#reuse) only while its key
holds: a change to `xtask/src`, to a worker the item runs or to any other input of the
key measures it again.

A measurement that exits 0 with a case it did not measure fails: `gate run` fails a
timing step whose report holds a case not `measured`, or a memory step one not
`qualified`, naming the cases, and `codecs time` and `codecs memory` fail on a workload
they did not measure, after writing every record. `gate run` writes its verdict into the
step's `gate-result.json` (its `status`, with `"cases_checked": true`), so a step that
[reuses](#reuse) the result takes the verdict; a reused result that no gate run checked
fails the step, which `--full` measures again. Where the codec gates apply (native
Linux with glibc on either gate target, the gate build) they also fail on a workload
whose runtime or memory bound does not hold, and print a grade's verdict lines, so
`gate summary` tells failures that are only a list's `open_workloads` on that target
from others. Each open workload names its measured `cause`, and one with `targets` is
open on those targets alone; elsewhere the codec bounds are diagnostics, which they
print as such and fail nothing. So no feature measures nothing in silence. A workload list's `memory_timeout_seconds` gives `codecs memory` its observer
run limit per target triple (30 s when absent), and every memory observation records how
long its run took (`process_elapsed_ns`). Each list sets four times its slowest observed
run on each target, rounded up, from a run at `8154812e`: `workloads.json` 221 s on Linux
AArch64 and 208 s on 32-bit ARMv7 (`hevc-main10-2160p`, 55.16 s and 51.97 s), and
`file-workloads.json` 170 s and 134 s (`flac-24-96k-5.1`, 42.44 s and 33.48 s).

`cargo xtask gate check --shards N` runs on any machine in a second, before a host
exists: it resolves the preflight of every shard as the shard resolves it, and fails,
naming the item and the path, on any input a prepared host would not have. A host has
the committed tree; the fixtures `fixtures` and `framing fixtures` generate from it,
which the check generates under `target/gate-check`; the manifests the `fonts`,
`shape`, `raster` and `ass` generators write; the vectors `fetch-vectors` fetches, by
their pins; and the files of the kept evidence sets unpacked at the repository root
(`"unpacked_at": "repository"` in the [index](../harness/evidence/sets/)), by
the set's listing, without fetching any. Where preflight pins an input's hash, its
source's must match. `tools/gate-aws.sh run` runs it before it launches anything.

On one host, a shard's run is:

1. `tools/gate-host.sh ready I/N`: the prepare commands, the workers with
   `cargo xtask build-workers --all-references`, so that every profile reference is
   built, and `cargo xtask gate preflight`, which runs `cargo xtask preflight` with
   everything the shard measures (`--feature ID`, with `--corpus ID=DIR` and
   `--manifest ID=FILE` as `report` takes them, `--isobmff`, `--isobmff-seek`, `--codecs` for each
   workload list, `--correctness` for the codec correctness lanes, `--external-vector`, `--capture` for the capture cases), with the
   [committed RSS module review](../harness/rss-bound/README.md#proof) of the tree's
   module source for the running kernel (`cargo xtask preflight` also takes
   `--reference NAME` for any other reference a run names by path, `--review FILE`
   for another review, and `--standard-memory` for a run without the kernel bound).
   Preflight checks that the gate build's workers and references match the build
   identity, that every case names a reference the identity holds and has each input it
   reads with its pinned hash, that the references ISO BMFF, the codecs and capture
   measure against were built, that the capture input generator's Python has numpy,
   that the review covers this module source and kernel, and that the module is
   unloaded. A feature without cases or a workload list without workloads
   fails, and every failure is printed at once. Then `prove.sh` runs once into a
   scratch directory and the module is unloaded again, so a module that will not
   build, load or prove itself stops the run here.
2. `tools/gate-host.sh run I/N OUT` times the shard alone on one CPU with the RSS
   module unloaded (`taskset -c 1`, `cargo xtask gate run --stage timing`), after its
   parity steps.
3. It [proves](../harness/rss-bound/README.md) the kernel bound, rebuilds the workers
   with the proof, checks them with `cargo xtask gate preflight --kernel`, and collects
   memory on the remaining CPUs (`taskset -c 1-15` on a 16-CPU host, `cargo xtask gate
   run --stage memory`). The shard's capture memory steps run as one `capture qualify`,
   their runs joined by `--and`, so every case is one job of a single collection, side
   by side as in a run of all cases (see [Memory in parallel](#memory-in-parallel)).
   Each run still reads its own timing records, writes its own output and gate result
   and is reused on its own; each step's record takes the status of its gate result and
   names the batch (`memory-capture`) and its time (`batch_seconds`); the batch's log
   is `logs/memory-capture.log`, and a step's outcome lines are those naming its case.
4. It grades with the module loaded. Grades measure nothing, so several run at once,
   from a release build of xtask.

Never time while memory, a build or anything else runs on the host. Each step's log,
and a record of its command, status and time, go under OUT. `cargo xtask gate summary
--output FILE DIR...` joins the shards' outputs into one result; it fails when a
planned step did not run exactly once, a step failed, or the shards measured different
source revisions or timing builds, and names the failed grades whose only failures are
the contract's listed open cases.

`cargo xtask build-workers --all-references` builds a worker for every reference profile
and exits 1, writing no build identity, when a profile is not prepared, so that no gate
run can measure without one of its references. Prepare them first, in this order (libass
links the FreeType, FriBidi and HarfBuzz builds):

```sh
for profile in matroska formats video subtitles decode capture capture-small; do
    cargo xtask prepare-ffmpeg --jobs 8 --profile "$profile"
done
cargo xtask fonts prepare --jobs 8
cargo xtask shape prepare --jobs 8
cargo xtask ass prepare --jobs 8      # the libass build, as raster prepare makes it
cargo xtask build-workers --all-references
```

`cargo xtask preflight`, with or without `--kernel`, takes the committed review of the
tree's module source for the running kernel unless `--review FILE` names another. `cargo
xtask grade` joins one feature's timing and memory collections: it takes the paths of
their `report.json` files and a new output directory, as a shard's grade step does:

```sh
cargo xtask grade OUT/benchmark/mp4-forward/report.json OUT/memory/mp4-forward/report.json \
    --contract harness/features/mp4-forward/contract.json \
    --manifest harness/features/mp4-forward/corpus.json \
    --output OUT/graded/mp4-forward
```

#### Early stop

Packet framing collects every timing and memory pair.

A gate timing session (`report`, `isobmff-bench`, `isobmff-seek`, `external-vector`,
`codecs time`, `capture qualify`) may end after 20 of its 30 pairs, taken as two blocks
of ten: when
every ratio `T_triergon / T_reference` of the second block and at least eight of the
first lie strictly below the ceiling, and the lag-1 autocorrelation of the 20 ratios is
at most 0.42. A gate memory collection stops only when its peak heap ratios and its
total resident ratios each meet the same three conditions under 1. Otherwise either
runs all 30 pairs and is graded by the paired bootstrap as before. A session that stops
records the rule (`early_stop`) and, as its interval, the median of its 20 ratios
bounded by their 3rd and 18th smallest. The validator and the memory evaluator replay
the rule on the recorded pairs, so a session that stopped where the rule does not, or
ran on where it stops, does not count; the record must name exactly the ceiling of
[`harness/policy.json`](../harness/policy.json), and grading holds its planned pairs to
the contract's minimum. A stopped session prints where it stopped, and its record
labels its interval `order statistics of the look`. Comparisons with another build
(`--baseline-worker`, quick checks) always run every pair, and `--no-early-stop` makes
any gate command do the same.

The bound is a sign test. When every ratio of the second block lies below the ceiling,
so does the 18th smallest of all 20. For independent pairs whose true median ratio is
at the ceiling, each ratio lies below it with probability at most ½, so the second block
qualifies with probability at most 2⁻¹⁰, and with eight of the first block, at most
2⁻¹⁰ · 56/2¹⁰ = 0.005%, whatever their distribution. Because the second block alone
bounds it by 2⁻¹⁰ = 0.098%, a warm-up or drift confined to the first pairs cannot stop a
case the second block would not. The autocorrelation limit, the 99th percentile of 20
independent ratios, refuses sessions whose pairs are visibly not independent. This adds
to the unchanged error of the 30-pair bound and keeps the one-sided 5% false-pass
guarantee of a 95% bound.

The guarantee assumes independent pairs. In the archived 30-pair sessions
(`armv7-thumb2-3df7ae6d-linux-armv7` on ARMv7; `runners-gate-build-linux-arm64` and
`mpegts-pes-gate-build-linux-arm64` on AArch64) the paired ratios behave as independent
draws: against each session's own shuffled ratios, the share of sessions whose lag-1
autocorrelation exceeds the shuffled 95th percentile is 4.5% to 5.3% of memory
collections and 3.0% to 6.0% of timing sessions (5% for independent pairs), and the mean
of pairs 1 to 10 against pairs 11 to 30 differs by −0.04 to +0.04 standard deviations on
average, with 1.5% to 6.8% of sessions beyond either 5% tail of the shuffled shifts.
Simulated with the true median exactly at the ceiling, the rule stops a session early
with probability 0.007% for independent pairs, 0.025% at a lag-1 autocorrelation of 0.2,
0.27% at 0.5, and 0.02% to 0.06% with the first ten pairs shifted favourably by 0.5 to 3
standard deviations, where looks after 10 and 20 pairs would stop 2.7% to 99%.

Replayed on those sessions and on `isobmff-armv7-runtime-regate-linux-arm64`, the rule
stops 1,597 of 1,663 AArch64 and 2,980 of 3,037 ARMv7 timing sessions and 1,337 of 1,663
AArch64 and 2,956 of 3,335 ARMv7 memory collections, and no session it stops failed at
30 pairs (a collection refused as an invalid observation is refused either way).
Drawing pairs from the ARMv7 timing sessions' own ratios rescaled to a median exactly at
the ceiling, 0.006% of 128,000 simulated sessions stop early, and the share that passes,
3.22%, is the 30-pair bound's; from the ARMv7 memory collections' total resident ratios
at a median of 1, 0.003% of 60,000 stop early and 2.54% pass, as with 30 pairs. Against
fresh 30-pair runs of `matroska-demux` and ISO BMFF on the same worker bytes, every
session that stopped kept its verdict on both targets. Five ARMv7 `matroska-demux`
memory cases, whose total resident bounds lie between 0.978 and 1.011, ran every pair on
both sides and changed verdict between the two runs, as two 30-pair bounds that close to
the ceiling can. ISO BMFF timing took 12.0 instead of 15.8 minutes on AArch64 and 14.8
instead of 19.2 on ARMv7, and its memory 13.5 instead of 19.2 and 10.7 instead of 15.0.
The replays, simulations and runs are evidence sets `gate-early-stop-two-blocks`,
`gate-early-stop-runs-aarch64` and `gate-early-stop-runs-armv7`.

#### Memory in parallel

Memory collections run side by side, one slot per CPU the process may use (at most
32, the sessions the kernel RSS bound admits), each slot's thread pinned to its CPU so
the workers it starts run there alone. Each slot measures one case at a time from its
own copies of the worker executables, and only those copies are evicted before every
run, so one slot's eviction never touches an executable another slot runs. Inputs are
not evicted: they stay in the page cache as in a serial run, and cases that read a
common file (the input, an ordered entity or a companion) run in the same slot one
after another. Workers measured from their own paths, as a comparison with another
build is, are such a file, so their cases run one at a time; `codecs memory` and
`capture qualify` measure copies whenever both workers are the gate build, as the other
commands do unless they compare with another build. `capture qualify --stage timing`
runs correctness and timing alone, on one CPU, and `--stage memory --timing DIR` then
collects every case's memory in one collection from the records the timing run wrote,
refusing them if the sources, workers, cases or host differ; without `--stage` one run
does both, its memory after all its timing. Collecting side by side, it gives each
case its own copy of its generated input, checked against the hash its timing recorded
and removed when the run ends, so cases of one input no longer share a file; on one CPU
they read the input itself. Pinned to one CPU, collection runs exactly as before. The
reference's memory is measured afresh in every pair, not reused from earlier runs: on a
second host of the same class it gives the same heaps and resident peaks within their
spread (evidence set `gate-reference-memory-cross-host`), but caching it would save a
few minutes and give up the interleaving of both workers within each pair.
Collected serially and in parallel under the kernel bound with 30 pairs at `a92f2d0a`
(evidence set `gate-parallel-memory-matroska`), `matroska-demux` gives every case the
same peak heap sample for sample on Linux AArch64 and on the ARMv7 gate target, moves
no case's median total RSS by more than one page on AArch64 or ten pages on ARMv7,
both inside the serial collection's own spread, and grades every case the same. With
each slot pinned to its CPU, at `8607afdd` on AArch64 and `247e8560` on ARMv7 (evidence
set `gate-parallel-memory-matroska-head`), the same comparison gives the same result:
heaps identical, medians within one page on AArch64 and twelve on ARMv7, verdicts
unchanged.
ISO BMFF, collected serially and in parallel at `a92f2d0a` (evidence sets
`gate-parallel-memory-isobmff-aarch64` and `gate-parallel-memory-isobmff-armv7`), gives
all 956 cases the same peak heap sample for sample, moves no case's median resident peak
by more than two pages on AArch64 or nineteen on ARMv7, inside the serial spread, and
passes every case either way; the collection took 82 instead of 252 minutes on AArch64
and 96 instead of 296 on ARMv7.
Capture, collected at `4b43bf74` from one timing run's records serially on five cases and
in parallel on all fifteen (evidence sets `gate-parallel-memory-capture-codecs-aarch64`
and `gate-parallel-memory-capture-codecs-armv7`), gives those five cases the same peak
heap sample for sample on both targets, keeps every resident median inside the serial
spread, and grades them the same; the fifteen took 10 minutes on AArch64 and 11 on ARMv7,
where the five took 31 serially. The gate collects a shard's capture memory steps the
same way: the 15 capture items at `c82b4452`, each target on one host (evidence sets
`gate-run-aarch64-c82b4452-20261118-143205-shard-1` and
`gate-run-armv7-c82b4452-20261118-143302-shard-1`), against their serial steps at
`f7b1d8eb` on the same worker bytes (`gate-run-aarch64-f7b1d8eb-20261118-104434-shard-1`
to `-5` and `gate-run-armv7-f7b1d8eb-20261118-104533-shard-1` to `-5`), give every case
the same 20 pairs, both workers' peak heap identical sample for sample, each total
resident median inside the serial collection's range, moved by at most one page on
AArch64 and 13.5 on ARMv7, and the same heap and total RSS verdicts; the memory pass took
10.1 minutes on AArch64 and 11.6 on ARMv7, where the serial steps took 88.6 and 83.2. The codec workloads, collected in parallel at the same
revision, grade every workload as the serial collection of the same worker bytes at
`8154812e` does, with the reference's heap identical sample for sample. The candidate's
heap is identical in 21 of 30 workloads on AArch64 and 25 of 30 on ARMv7, and 8 or 16
bytes apart in the others, in every sample: the codec worker keeps its own path, which a
slot's copy lengthens. Collected serially, those workloads give the serial collection's
heaps when measured from the workers' own paths and the parallel collection's when
measured from a slot's copies.

Each collection records the number of slots (`memory_slots`) and what the host's memory
did over it (`host_memory`): `MemAvailable` at its start and end and the change of the
reclaim and compaction counters of `/proc/vmstat`, which count the whole host. Run
kernel-bound memory gates on a host of the class they are qualified on, a c6g.4xlarge
with 32 GB, where no archived collection serial or parallel has had a pair voided by
another task's update of a worker's resident counters; `preflight --kernel` refuses a
host with less than 30 GiB of `MemTotal`. On smaller hosts such pairs occur, and a
collection measures them again within the limit the [kernel
bound](../harness/rss-bound/README.md#what-it-measures) sets; `host_memory` shows
whether reclaim ran while they were measured.

#### Reuse

Every gate command (`report --benchmark`, `report --memory`, `grade`, `isobmff-bench`,
`isobmff-seek`, `framing`, `progressive`, `external-vector`, `codecs time`, `codecs memory`, `capture
qualify`) derives
a key before it measures and writes it, with its exit status, to `gate-result.json` in
its output directory. The key covers the command and its settings (pairs, seed,
iterations, acceptance requirement, the kernel bound), the contract, manifest and other
documents that define its cases, the bytes of every worker it runs and of every shared
library their loader resolves, the allocation observer and probe, every shared input
of the build identity (oracles, specification sources, reference headers, generator
inputs), every file of `xtask/src` and `harness/*.py`, the reviewed input lists, the
observers and the RSS module with `Cargo.lock`, `rust-toolchain.toml` and
`harness/policy.json`, the Rust, C and Python versions with numpy's, the environment
variables that change a build (`RUSTFLAGS`, `RUSTC_WRAPPER`, `CARGO_PROFILE_*`,
`CARGO_BUILD_*`, `CARGO_TARGET_*`, `CC`, `CFLAGS` and the like), the gate target, and
the host class (architecture and personality, kernel release, CPU model and
features, machine type, page size, the CPUs online and, for a timed step, those the
process may use). A
probe that fails is an error. A `grade` key also binds the two collections it joins:
each one's key and the SHA-256 of its report, so grading reports measured again with
`--full` is never answered with an earlier grade. When an [evidence](#evidence) set
that the index keeps lists a
result with the same key, the command takes that result instead of measuring: it
prints the set, writes a `gate-result.json` that names it as `reused` with the set's
source revisions, and exits with the recorded status. `--full` measures (or, for
`grade`, grades) anyway. A `capture qualify --stage timing` it reuses also restores the
records of that step from the kept set, because its memory stage reads them and measures
each case at the input this run generates, with the hash its timing recorded. The memory
stage then requires of a reused timing run only what its key binds: the same workers,
observer and cases and the same host class, so that run's host name, kernel build and
the source files the key does not bind (a README, say) may differ; a timing run of the
same gate run must match them all. A gate host downloads the set through an address
`tools/gate-aws.sh` gives it (`.research/evidence-addresses`, from `cargo xtask evidence
restorable`), and the restore is written to disk before the next step starts; a step
whose set cannot be fetched within the transfer limits is measured. Grading two reused collections takes the cited grade; a
reused collection beside a measured one cannot be graded, so measure both with
`--full`.

`cargo xtask gate test --profile checked|release` runs the workspace tests for the gate
target and is reused the same way. Its key holds the profile and adds every tracked
file as the working tree holds it and the untracked files git does not ignore, but the
evidence index (which a promotion changes) and `docs/`, and the test inputs the crate
tests read when present (`.research/fate-samples`, `.research/flac-generated`,
`.research/flac-test-files`). The few tests that read the evidence index skip the reused
run and run on every `gate test`, so a failing index is never reused as a pass. The gate
runs each profile once. The release tests build in `target/gate-tests`, so they never
rebuild the gate build's workers that later steps on the same host time. The `checked` profile
is the release optimisation with every overflow check and every `debug_assert!` and
`#[cfg(debug_assertions)]` item of a debug build, so the same 4,429 tests run; it loses
only the unoptimised build's larger stack frames, which bring stack depth problems out
sooner, and on 32-bit ARMv7 it takes about 6.5 minutes against 29.7 for the debug
profile. The release tests run the optimisation the workers are measured with.

### 32-bit ARMv7

The harness runs natively in a 32-bit ARM Linux user space. The gate target there is
`thumbv7neon-unknown-linux-gnueabihf`: Thumb-2, as the armhf GCC that builds the FFmpeg
and libass references compiles by default, and assuming NEON, whose code FFmpeg selects
at run time on the same cores. Its workers therefore do not run on an ARMv7 core
without NEON. rustup's host toolchain there, `armv7-unknown-linux-gnueabihf` (A32
without NEON), builds xtask only: `cargo xtask build-workers`, `cargo xtask check` and
the comparison commands compile the workers and every test, xtask's own included, with
`--target thumbv7neon-unknown-linux-gnueabihf`, so the workers land in
`target/thumbv7neon-unknown-linux-gnueabihf/release`, and reports name that target.
Add it once with `rustup target add thumbv7neon-unknown-linux-gnueabihf`.
An AWS Graviton2 host (Neoverse N1) runs a 32-bit user space
on its AArch64 kernel; later Graviton generations cannot.

The results in [status](status.md#32-bit-armv7) come from the Ubuntu 24.04 armhf user
space archived as evidence set `armv7-userland` (211 packages; the image holds no home
directory, toolchain or build output). Unpack it on the host and enter it:

```sh
sha=f93de19128a5bc101c125a4a1f7c61f42062675186cfc86f627d220d1d079533
aws s3 cp "$TRIERGON_EVIDENCE_ARCHIVE/evidence/armv7-userland/$sha.tar.gz" armv7-userland.tar.gz
echo "$sha  armv7-userland.tar.gz" | sha256sum --check
sudo mkdir -p /srv/armhf
sudo tar -xpzf armv7-userland.tar.gz --numeric-owner -C /srv/armhf
for d in proc sys dev dev/pts; do sudo mount --bind /$d /srv/armhf/$d; done
sudo cp /etc/resolv.conf /srv/armhf/etc/resolv.conf
sudo install -d -o 1000 -g 1000 /srv/armhf/home/ubuntu
sudo chroot /srv/armhf setarch linux32 sudo -u ubuntu -H bash -l
```

The image was made with `debootstrap --arch=armhf --variant=buildd
--components=main,universe noble` from `http://ports.ubuntu.com/ubuntu-ports`, with
`noble-updates` and `noble-security` enabled, then `apt-get install build-essential
pkg-config zlib1g-dev python3 python3-numpy make git zstd curl ca-certificates sudo jq
time`. It has a user `ubuntu` (uid 1000, passwordless `sudo`); its home directory is not
part of the image.
A user space built the same way later can pick up newer packages; compare its
`dpkg-query -W` list before treating its results as comparable. Inside, install rustup
as on any host, add the gate target, and run every command under `setarch linux32`. Record the CPU, kernel
and user-space package list with the results. The `subtitles` reference builds libzvbi
from its source archive, which needs `apt-get install autoconf automake libtool autopoint
gettext` in the user space first.

`tools/gate-host.sh setup armv7` does all of this on a gate host, and mounts the user
space at every boot.

The kernel RSS bound works for a 32-bit process through the module's 32-bit ioctl
entry point. The module belongs to the host's kernel, so build and load it on the host,
which needs the kernel's headers, and bind debugfs into the user space before entering
it. Run `prove.sh` on the host from the repository inside the user space, naming the
user space's root; it builds and runs the check, and records the architecture and page
size, inside it:

```sh
sudo mount --bind /sys/kernel/debug /srv/armhf/sys/kernel/debug
cd /srv/armhf/home/ubuntu/triergon
harness/rss-bound/prove.sh .research/rss-bound /srv/armhf
```

Then build the workers with `TRIERGON_RSS_KERNEL_PROOF` and collect memory with
`TRIERGON_RSS_KERNEL_BOUND=1` inside the user space, as the
[kernel proof workflow](../harness/rss-bound/README.md) describes.

### Gate hosts

`tools/gate-aws.sh` runs gate hosts on AWS from a workstation with the `aws` CLI; the
hosts hold no AWS credentials. `tools/gate-aws.sh image aarch64|armv7 REVISION
INPUT...` builds a target's gate image on a Graviton2 c6g.4xlarge: Ubuntu 24.04 with
the build packages, the kernel headers and Rust; the kernel the committed RSS module
review of REVISION's module source covers, as GRUB's default, with every kernel package
held and unattended and periodic apt runs masked and off, so the image never boots
another (the image step launches a host from the new image and removes the image
unless that host runs the reviewed kernel with the holds in place); for ARMv7 the
armhf user space with its own Rust and the gate target; the repository at REVISION
with every reference build (restored from the archive's packs where it holds their
keys, the packs of the others uploaded), the generated fixtures, vectors, font corpus
and manifests, and the workers; and each INPUT, an archive of media inputs the
repository does not generate, with repository-relative paths: evidence set
`gate-media-inputs` holds the FATE samples, the codec workload inputs and the
subtitle corpus the gate reads, set `codec-file-workload-inputs` the inputs of
[file-workloads.json](../harness/codecs/file-workloads.json), and set
`ogg-reader-inputs` the inputs of the Ogg reader comparison, set
`subtitle-cc-inputs` the closed caption streams `gate-media-inputs` lacks, and set
`codec-derived-streams` the 71 elementary streams of the audio correctness lanes that
`cargo xtask codecs derive` makes from FATE samples
(`s3:evidence/SET/SHA.tar.gz`, their keys in the
[index](../harness/evidence/sets/)); a gate image is built with all five. The image
is tagged `Name=triergon-gate-TARGET`, and the earlier images of the target and
their snapshots are removed. Run it again when the reference builds' keys or the media inputs change,
or to take a new base image.

`tools/gate-aws.sh` needs bash 4 or later and refuses to start under an older one,
such as macOS's `/bin/bash` 3.2, where a shard's exit handler never runs and its
outputs would be lost; on macOS run it with Homebrew's bash first on `PATH`, or as
`/opt/homebrew/bin/bash tools/gate-aws.sh`.

`tools/gate-aws.sh run TARGET N REVISION OUT`, in a checkout at REVISION without
changes to tracked files, runs `cargo xtask gate check --shards N` and refuses to
start when it fails. Then it launches N hosts from the image,
checks out REVISION on each, restores reference builds whose keys changed from the
archive's packs (building and uploading the others), unpacks each `--input s3:KEY` (a
media inputs archive in the evidence archive, for a kept input set the image lacks) at
the repository root, gets each host ready and runs shard I of N on host I. `gate check`
counts the files of a kept set unpacked at the repository root as present whether or
not the image holds them. When a shard ends, finished or not, its host's outputs are
archived under `qualification/gate-runs/RUN/shard-I/`, where they expire after 90 days
unless promoted, and checked by downloading them back on the host;
`OUT/shard-I/entry.json` holds the archive's index entry, which lists the [gate
results](#reuse) it holds. Its logs and step records are copied to OUT, and the host is
terminated. The last shard to end keeps its host
for the summary when every shard finished, and a host whose outputs could not be
archived stays until the command ends. OUT/hosts.txt records when each host was
launched, ready, done and terminated. The command exits non-zero when a shard did not
finish or the summary fails. Every host is a c6g.4xlarge, the class kernel-bound memory is qualified
on, SSH is open to one address (`TRIERGON_SSH_CIDR`, a /32), and a timer on the host's
disk powers it off, and so terminates it, 72 hours after it boots. Both
commands refuse to launch anything unless `TRIERGON_SSH_CIDR` is the workstation's
current public address as a /32, which they read from `https://checkip.amazonaws.com`
and print beside it; they never widen it. When a poll fails because the workstation's
public address is no longer the one the session's security group admits, the run says so
and waits, polling on, instead of giving up on hosts that are still working: open the
group to the new address (one /32) within 30 minutes and it carries on; after that the
shard fails and the session's teardown removes its host. It never changes the group
itself. A group it cannot read is reported as a credential failure, and counts as a
failed poll.

## Robustness

Malformed input must never panic, hang or escape its budget. Tests and fuzzing cover
truncation at every structural boundary, corrupted lengths, counts and timestamps,
deep nesting, decompression bombs, duplicate identifiers and conflicting indexes.
A transport simulator covers short reads, delayed ranges, changing length, exact end of
file, reconnect failure and cancellation. Results are reproducible from a saved seed
and edit recipe.

## Evidence

A result names its exact source revision, worker binaries, reference build, corpus and
input hashes, platform and all per-case outcomes. Summaries and identities are kept
in the pull request that changes the result and in [status](status.md), whose
[measured revisions](status.md#measured-revisions) table names the revision and
raw-sample SHA-256 behind each cited result. Raw samples are archived by content hash,
outside the repository. The evidence index lists every archived set, one file per set,
[harness/evidence/sets/SET.json](../harness/evidence/sets/):

- its key and SHA-256;
- whether it is kept (cited results) or expires after 90 days;
- each file it holds, by path and SHA-256.

Where a set lives decides how long it is kept: sets under `qualification/` expire 90
days after upload, and sets under `evidence/` are kept. With `TRIERGON_EVIDENCE_ARCHIVE`
set to the archive location:

1. `cargo xtask evidence pack DIR OUT` writes the deterministic archive of a run. An
   `OUT` ending in `.tar.gz` stores the tar uncompressed in gzip blocks; one ending in
   `.tar.zst` compresses it with the system `zstd`, which suits large sets of reports.
2. `cargo xtask evidence upload SET OUT` uploads it under `qualification/SET/` and
   prints its index entry, which lists the fresh [gate results](#reuse) the set holds;
   with its reason filled in, it is the set's file `harness/evidence/sets/SET.json`.
   Every new set starts there. `cargo xtask evidence entry SET OUT` prints the same
   entry and uploads nothing, for a host without archive credentials whose upload
   streams through another machine to the entry's key.
3. `cargo xtask evidence promote SET` moves an indexed set under `evidence/` and
   records it as kept. Promote a set in the pull request that first cites it; an
   uncited upload simply expires.
4. `cargo xtask evidence fetch SET` downloads a set, verifies it against the index and
   unpacks it into `.research/evidence/SET/`; a `.tar.zst` set is decompressed with the
   system `zstd`. Packing and fetching stream, hashing and writing each file as it
   passes, so a set larger than memory unpacks on a 32-bit host.

[harness/evidence/index.json](../harness/evidence/index.json) records where keys lie
and how identifiers were renamed. A result key is listed by one kept set at most, so
reuse finds one result for it.

Every revision status cites stays reachable from `main`: `cargo xtask check-revisions`,
part of `check`, fails when `HEAD` does not reach a cited commit. A pull request that
cites its own commits merges with a merge commit. A cited commit that is off `main`
joins it through a merge that keeps `main`'s tree (`git merge -s ours`).
An abbreviated revision must identify one Git object, and that object must be a
commit in the checkout history. Keep a bundle of the measured source with the
archived result so its exact commit can be recovered.

# Status

Everything is experimental. No module has yet passed every gate on its complete scope
(see [quality](quality.md)). Runtime is the worst per-case upper bound of
`T_triergon / T_reference`, graded against the [ceiling](quality.md#gates). Memory is peak
heap and total resident peak RSS relative to the reference. Unless noted, measurements are native
AArch64 Linux. Linux memory is measured under the `linux-glibc-v4` observer, which counts
`strdup` and `strndup`, admits libstdc++'s operator `new` and `delete`, and counts read-only
file mappings made during the operation toward total RSS ([harness](quality.md#harness)).
Containers, framing, subtitles and playback are measured against FFmpeg git revision
`d32b387f` on the 9.0 branch; codecs, decode sessions, capture and the playback MP4
terminal-media matrix against the FFmpeg 9.0.2 release; fonts against FreeType 2.15.0;
shaping against HarfBuzz 14.6.0 and FriBidi 1.0.17;
rasterising, ASS parsing and ASS rendering against libass 0.17.5
([reference pins](quality.md#reference-and-corpus)).

| Area | Implemented | Verified against the reference | Runtime | Memory | Open |
|---|---|---|---|---|---|
| Core | Charged storage, exact time, input windows, work budgets, neutral container model with one codec identity | 51,235 exact-arithmetic cases against an independent oracle | via dependents | via dependents | fuzzing |
| Matroska / WebM | Incremental reader: lacing, header removal, bounded zlib, Cues and preroll, colour and static HDR, chapters, tags, attachments, side data | 116 workloads in six feature groups, including an external HDR sample, and four host FATE demux comparisons | 1.1423 (116/116 pass) | heap ≤0.6263, total RSS ≤0.9831 (116/116 lower) | full EBML/Matroska scope |
| Matroska startup and seek | Startup and seek with lazy scan replay | 44 cases, 1,344 packets | 1.1488 (44/44 pass) | heap ≤0.4038, total RSS ≤0.9202 (44/44 lower) | FATE mapping |
| Matroska audio clocks | AAC, AC-3, E-AC-3 startup and forward clocks | 10/10 packet folds (`matroska-audio-clocks`) | 1.1408 (10/10 pass) | heap ≤0.8103, total RSS ≤0.9875 (10/10 lower) | audio seek and FATE coverage |
| ISO BMFF / MP4 | Regular and fragmented files, edit lists, seek and range plans, neutral `ContainerReader` over seekable sources, QuickTime compatibility, bounded AAC configuration and regular table recovery, timing, sync and terminal media prefixes; AVC, HEVC, AAC, AC-3, E-AC-3, DTS core, AV1, VP9 and Dolby Vision sample entry identification | 255 inputs, 1,020 open and index comparisons (972 matching outputs and 48 matching refusal outcomes with compatibility policies) | 0.8854 (972/972 pass) | heap ≤0.4132, total RSS ≤0.7260 (972/972) | seek and range-plan timing; neutral reader not yet compared with FFmpeg |
| MP4 range playback | Regular AVC/AAC and AVC/Opus startup and backward seek, fragmented AVC/AAC with open-ended edits, dedicated HEVC and AV1 sample entries, source-declared Opus seek preroll, QuickTime AAC with version-2 sound descriptions including 7.1 AAC and fragmented version-1 sound descriptions, and media playback in the presence of RTP hint tracks | 39 complete open and backward-seek comparisons (`mp4-range`), 5,169 packets on AArch64 Linux | 1.1232 (39/39 pass) | heap ≤0.7121, total RSS ≤0.9841 (39/39 lower) | other range playback profiles |
| AAC in MOV | Raw AAC-LC access units with indexed rates, 1024 samples and fixed configurations 1–7, 11, 12 and 14 | Three authored 5.1 and 7.1 QuickTime clips at 44.1 and 48 kHz match rate and sample count, and every PCM sample passes the AAC accuracy bounds. The container comparison records the distinct labels for the single surround pair in 5.1. The [configured decoder workloads](../harness/codecs/aac/access-unit-workloads.json) also compare all native PCM planes over the same 537 original packets. | not measured | not measured | further raw AAC profiles, runtime and memory |
| Ogg | Pages with CRC and resync, multiplexed and chained streams, Vorbis, Opus and FLAC mappings with canonical configurations, granule timing, pre-skip and end trims, bisection seeking | Packets against the pinned `ffprobe` (`cargo xtask codecs readers`) on 21 FATE Vorbis files and 13 generated Opus, Vorbis and FLAC files (multiplexed, chained, damaged, truncated), in two views: the FFmpeg-compatible `compat::clock` 31/34 pass, 3 differ only by reviewed FFmpeg behaviours; the reader's own timing 7/34 pass, 25 differ only where `compat::clock` reproduces FFmpeg, 2 by reviewed behaviours ([reader rules](../harness/codecs/reader-rules.json), [frozen differences](../harness/codecs/reader-differences.json)) | within the codec workloads | within the codec workloads | `b_vorbis.ogg`, `chain_vorbis.ogg`: a negative last duration FFmpeg prints modulo 2^32; `crc_bad.opus`: the first packet after a page that fails its CRC, which FFmpeg times from its own page and `compat::clock` from the stream start; Theora and Speex untimed; chained seeks not compared with FFmpeg |
| Audio elementary streams | Native FLAC (with or without `fLaC`), MPEG audio with ID3v2/ID3v1/APE tags, Xing/Info/VBRI and LAME gapless trims, ADTS, AC-3/E-AC-3; one shared frame-stream engine | Packets as the readers emit them against the pinned `ffprobe` on 271 FLAC, MPEG audio, ADTS, AC-3 and E-AC-3 files (the IETF FLAC testbench, MP3 conformance, generated and crafted damaged streams): 253/271 pass, 16 differ only by reviewed FFmpeg behaviours, 2 refused by both (testbench faulty `06 - missing streaminfo metadata block`, `07 - other metadata blocks preceding streaminfo metadata block`) | within the codec workloads | within the codec workloads | FLAC `flac_s20_rate768k`, `06 - samplerate 768kHz` (no durations at 768 kHz), `01 - changing samplerate`, `27 - old format variable blocksize file created with Flake 0.11`, `11 - file starting with unparsable data`, `11 - incorrect metadata block length`, `08 - blocksize 65536`, `09 - blocksize 1`; crafted `ac3_tags`, `adts_mid_junk`, `flac_corrupt`, `flac_id3`, `flac_mid_junk`, `mp3_foreign_frame`, `mp3_mid_junk_falsesync`; MP3 `he_free` (free format, refused by FFmpeg); E-AC-3 dependent substreams on real files; ADTS and AC-3 seek cost |
| MPEG-TS | PSI, continuity, PES assembly and ordered TS188/M2TS192 entities with control-only and bounded fault completion, with format-owned track classification | 97/97 PES compatibility-view comparisons across all 272 requests | 0.9413 (97/97 pass) | heap ≤0.065330, total RSS ≤0.891584 (97/97 lower) | strict source clocks, full FATE mapping |
| MPEG-TS access units | `ContainerReader` and `AnyReader` for TS188 and M2TS192, with AVC, HEVC, ADTS AAC-LC, LATM in LOAS, signalled AC-3 and E-AC-3, DVB subtitle segments and ordered entities | 169/169 transport compatibility folds, including LATM PES and opaque observations, with all 272 original request associations | 1.1209 (169/169 pass) | heap ≤0.088210, total RSS ≤0.909287 (169/169 lower) | additional carriages and seeking |
| MPEG-TS program playback | Selected TS188 program and M2TS192 PID with packet payloads and clock projection | 35/35 packet folds | 1.0267 (35/35 pass) | heap ≤0.087743, total RSS ≤0.867077 (35/35 lower) | additional program and carriage profiles |
| MPEG-TS progressive playback | TS188 with AVC and AAC-LC, bounded sequential input and access units before EOF | 3 packet folds and all 5 functional inputs with 10 literal outcomes | 1.1185 (3/3 pass) | heap ≤0.015079, total RSS ≤0.723530 (3/3 lower) | broader transport and codec profiles |
| HLS segments | Independent TS188 segments with contiguous AVC or HEVC video and AAC, AC-3 or E-AC-3 audio | 71 packet comparisons, 71 measured workloads | 1.0336 (71/71 pass) | heap ≤0.020733, total RSS ≤0.808122 (71/71 lower) | other profiles |
| Packet framing | AVC/HEVC configuration and NAL framing, AV1, VP9, AC-3, E-AC-3, DTS core | 125 AVC/HEVC reference comparisons; DTS core byte-exact on 37,125 projections and 1,172 FATE packets, recorded without a timing runner | 1.3846 (45/125 pass for AVC/HEVC) | heap ≤0.985 (125/125 lower), total RSS ≤1.048 (0/125 lower) | runtime and RSS optimization; full AVC/HEVC/ADTS grammar |
| Subtitles, text | SRT, WebVTT, ASS passthrough, mov_text and tx3g to ASS, charset handling | `subtitle-text`: 275 cases (82 captured mov_text cases, the samples of the 79 tx3g tests, 74 charset cases, 8 ASS packets, 32 SubRip and WebVTT documents) folded by `subtitle-capsule-worker` against the reference's recorded folds, 24 of them with differences the corpus names; not gated | not measured | not measured | timing and memory; gating `subtitle-text` |
| Subtitles, PGS | Presentation, palette and object segments to subtitle rectangles, with FFmpeg's recovery from damaged and incomplete segments | 317/317 cases (`subtitle-pgs`: TS, SUP and Matroska) | 0.8564 (317/317 pass) | heap ≤0.966 (317/317 lower), total RSS ≤0.959 (317/317 lower) | FATE coverage |
| Subtitles, DVB | Display definition, page, region, CLUT and object segments to bitmap rectangles, including `compute_clut` | 229/229 cases (`subtitle-dvb`: TS and Matroska) | 0.8568 (229/229 pass) | heap ≤0.991, total RSS ≤0.813 (229/229 lower) | FATE coverage |
| Subtitles, DVD | SPU assembly and control sequences, run-length raster, palettes from VobSub `.idx`, Matroska and MP4 private data, DVD program streams, HD SPU | 223/223 cases (`subtitle-dvd`) | 0.8592 (223/223 pass) | heap ≤0.959, total RSS ≤0.853 (223/223 lower) | FATE coverage |
| Closed captions | CEA-608 captions from A/53 and SCTE 20 user data in MPEG-2 video, A/53 SEI in H.264 and HEVC, and SCC files, to ASS | 197/197 cases (`subtitle-cc`) | 0.9751 (197/197 pass) | heap ≤0.123, total RSS ≤0.956 (197/197 lower) | FATE coverage; HEVC picture output at a new sequence (open case `hevc-eos-ts`: no DPB output model, so `no_output_of_prior_pics` after an end of sequence is not applied) |
| Teletext | DVB teletext pages to text and ASS, as FFmpeg's libzvbi 0.2.46 decoder emits them | 70/70 cases (`subtitle-teletext`) | 0.4257 (70/70 pass) | heap ≤0.622, total RSS ≤0.676 (70/70 lower) | FATE coverage |
| Playback | Matroska, MP4, TS and raw AVC/HEVC feeds; seeking; end-of-file continuation | MP4 terminal-media matrix against recorded FFprobe 9.0.2 output (evidence set `playback-terminal-matrix-ffprobe`): 39 inputs in 13 source geometries (12 nonseekable for every input, 1 seekable for the 31 original inputs), 499 rows, each run in debug and in release. `Mp4Open::new_recovery` 499/499 pass: 497 identical packet lists, and 2 seekable reads of a zero-size fragment sample that the opener refuses in the header, where FFprobe also refuses. `Mp4Open::new` 446/499 pass. Packet folds: MP4 forward 39/39 (`mp4-forward`), 566 packets, TS program 35/35 (`ts-program`), raw Annex B 6/6 (`raw-annex-b`) | MP4 forward 0.9911 (39/39 pass), TS program 1.1621 (35/35 pass), raw Annex B 1.0810 (6/6 pass) | MP4 forward heap ≤0.5590, total RSS ≤0.9975 (39/39 lower); TS program heap ≤0.1008, total RSS ≤0.9090 (35/35 lower); raw Annex B heap ≤0.127, total RSS ≤0.901 (6/6 lower) | other profiles; legacy Matroska `A_AAC/MPEG4/*` codec IDs are not read as AAC |
| Codecs | H.264 8-bit 4:2:0; HEVC Main and Main10; AAC LC, HE, HEv2; AC-3 and E-AC-3; DTS core with XCh and X96; storage charged to a budget | 417/417 comparable conformance and FATE vectors | 1.0551 (17/17 pass) | heap 0.92, RSS 0.82 | error concealment, unverified tool paths; AAC charges in-band reconfiguration when it appears, and a refused access unit leaves the decoder unchanged unless an earlier raw data block or Main-prediction element of it was decoded |
| Audio codecs in files | FLAC (RFC 9639, 4 to 32 bits, 1 to 8 channels); Opus (RFC 6716 and 8251: SILK, CELT, hybrid, families 0, 1 and 255); Vorbis I; MPEG-1, 2 and 2.5 audio Layer I, II and III | 315/321 pass through the container readers (FATE Vorbis, Opus and MP3 conformance, the IETF FLAC testbench, generated streams): FLAC bit-exact, the others within the [numeric rules](../harness/codecs/numeric-rules.json) | 1.0834 (10/10 pass) | heap ≤0.904, total RSS ≤0.966 (10/10) | FLAC `01 - changing samplerate`, `02 - increasing number of channels`, `03 - decreasing number of channels` and MP3 `he_mode` change format mid-stream, which the CLI converts; FLAC `flac_3ch_odd_mask`, whose layout FFmpeg refuses on its stereo output; MP3 `he_free` (free format: FFmpeg finds no two consecutive frames, and the decoder refuses free-format frames), refused by both; runtime toward the target for Opus music (1.0301) and 5.1 (1.0834), Vorbis 5.1 (0.9356) and stereo (0.8423) and Opus speech (0.8719); Opus families 2 and 3 |
| Decode sessions | Bounded queues, clock-gated output, flush and seek, RGBA letterbox, stereo mix; container track decoding with trims and start delays | with the codecs | 1.0551 (17/17 pass) | with the codecs | — |
| Fonts | sfnt and collections; `head`, `hhea`, `maxp`, `hmtx`, `vhea`, `vmtx`, `OS/2`, `name`; `cmap` formats 0, 2, 4, 6, 8, 10, 12, 13, and the glyph-name and encoding charmaps FreeType adds to CFF faces, in its charmap order; `glyf`/`loca`; CFF and CFF2 at the default instance; `kern` format 0; loading at a size as libass does with hinting off (unhinted outlines, synthetic bold and oblique, size metrics, kerning, decoration bands, default-load advances where hinting cannot move them, and the vertical advances libass draws and shapes the glyphs of `@` fonts with); FreeType's default load of CFF, CFF2 and CID-keyed glyphs, hinted by Adobe's hinter as FreeType's CFF driver hints them, and of TrueType glyphs without bytecode, through FreeType's auto-hinter with its Latin and dummy writing systems, with the grid-fitted metrics of each; face matching with libass's rules (compared through the renderer's `ass-render` feature) | The gate covers libass's default configuration (hinting off). All 70 faces of 53 corpus files (TrueType including Arabic, Hebrew, Indic and Thai fonts, name-keyed and CID-keyed CFF, CFF2, a 10-face collection, six authored fonts, among them a face for each way FreeType reads vertical metrics and a CFF face with each charmap FreeType adds; 836,345 glyphs) identical to FreeType: names, every charmap FreeType lists with its mappings, libass's text charmap, the kerning of every listed pair, advances and unscaled outlines (`font-outlines` 53/53); at 256 pixels, size metrics, kerning, every glyph's unhinted outline with its synthetic styles, default-load advances, and its linear and scaled vertical advances unhinted and of the default load (`font-hinting-none` 159/159); at that size, every glyph's default-load metrics and hinted outline, which libass's shaper reads for advances, extents and contour points (`font-hinting-default` 9/53: every CFF, CFF2 and CID-keyed face and the auto-hinted authored-cmap-formats; 44 TrueType files open, DejaVuMathTeXGyre under the auto-hinter's CJK writing system and 43 under the TrueType interpreter); Linux AArch64 and [32-bit ARMv7](#32-bit-armv7), each with its own FreeType build, and macOS AArch64 before the vertical cases | not measured | not measured | libass's hinting-enabled modes (NATIVE, LIGHT, NORMAL): their nine other sizes are measured, not gated (`font-hinting-modes` 900/918 before the fifth and sixth authored fonts: unhinted outlines 459/459, default-load advances 441/459), and the hinted outlines those modes draw are not compared; the auto-hinter's CJK and Indic writing systems, for the hinted outlines of the glyphs they style, which leave DejaVuMathTeXGyre open in `font-hinting-default` (its advances match); the TrueType interpreter, so the default-load extents and contour points of instructed TrueType faces, which HarfBuzz reads to position marks in fonts without GPOS mark positioning, are open (`font-hinting-default`); tricky fonts and instruction-control programs; `cmap` format 14; runtime and memory |
| Shaping | OpenType layout (GDEF, every GSUB and GPOS lookup type, feature variations at the default instance, `kern` format 0 fallback, fallback mark positioning) with HarfBuzz's default, Arabic, Hebrew, Thai/Lao, Hangul, Indic, Khmer, Myanmar and Universal shapers for horizontal runs; every BCP 47 language mapped to OpenType language systems as HarfBuzz maps it; libass's FreeType-backed font functions over a sized face, with the rotated advances of `@` fonts, the empty extents of glyphs without points, and the default-load extents and contour points of CFF glyphs; bidi as FriBidi resolves and reorders it for libass; libass's simple shaping level; Unicode properties generated from pinned UCD files; planning, shaping and bidi bounded by work limits | Identical to HarfBuzz and FriBidi on Linux AArch64 and [32-bit ARMv7](#32-bit-armv7), and on macOS AArch64 before the charmap face joined the shaping corpus. Gating: `shape-libass` 195/201 (6 open: NotoEmoji's fallback mark positioning reads the extents of glyphs the auto-hinter gives a CJK style), 3,105 runs over 26 corpus files and 22 scripts shaped as libass does at 256 pixels with hinting off; `shape-bidi` 24/24; `shape-simple` 6/6; `shape-unicode` 1/1 (every code point). Coverage at units per em: `shape-text` 200/201 and, on demand, `shape-text-all` 1,745/1,746 before the charmap face joined the shaping corpus (101,691 runs over every corpus face), one open case each (CFF extents as HarfBuzz's own interpreter reads them) | not measured | not measured | hinted glyph extents and contour points of TrueType glyphs and auto-hinted advances, which libass reads from FreeType: `shape-libass`'s six open cases (the auto-hinter's CJK extents) block qualifying `triergon-ass` as well; vertical directions (libass shapes `@` fonts horizontally); AAT tables and Apple `kern`, and OpenType `kern` formats 1, 2 and 3 and cross-stream kerning, which HarfBuzz applies (faces that need them are refused); `stch` stretching; variable instances other than the default; runtime and memory |
| Rasterising | libass's outline and bitmap stages: conversion from font outlines, rotation by 90°, affine and perspective transforms, border stroking, the 16×16 tile rasteriser, the glyph transform and its quantisation against a run with borders and opaque boxes, the karaoke leftmost x, `\blur` and `\be`, composition of fill, border and shadow, the additive and multiplying blends, shadow shifts and rectangle clipping with the karaoke split; storage charged to a budget, work to a `WorkBudget`, bitmaps bounded by libass's own size limit | Identical to libass's plain C bitmap engine (what libass runs on 32-bit ARMv7, pinned on every host), with glyphs loaded by `triergon-font` as libass loads them with hinting off. Gating: `raster-glyphs` 12/12 pass (576 operations over 8 glyphs of a file of each outline format and an authored font at 13.33 and 256 pixels) and `raster-outlines` 2/2 pass (1,019 operations on 200 random outlines), both with transforms of moderate reach, and `raster-bitmaps` 2/2 pass (560 operations on 80 random bitmap sets, a tenth of their blurs and `\be` passes beyond libass's clamps, among them radii it still blurs); on demand, `raster-sweep` 303/303 pass (72,445 operations: every corpus file at three heights with 30 glyphs each, 4,500 random outlines, 3,000 bitmap sets, with transforms as far as libass draws them). Platforms: macOS AArch64, Linux x86_64 and Linux AArch64 all four pass (12/12, 2/2, 2/2, 303/303); [32-bit ARMv7](#32-bit-armv7) passes the three gating features (12/12, 2/2, 2/2) and `raster-sweep` 301/303: on `outlines-71` and `outlines-83` libass silently draws nothing when its composite allocation fails; the crate returns `Allocation` (open cases on ARMv7, compared and passing elsewhere). `raster-simd`, the required cases against the host's SIMD engine, records where it departs: NEON on macOS and Linux AArch64 14/16 pass (4 of the 80 blur cases depart, all with `\be` passes); AVX2 on Linux x86_64 0/16 (AVX2: every bitmap differs in row padding; visible-pixel comparison needs its own fold) | not measured | not measured | underline and strike-out placement (needs the source outline's orientation as FreeType computes it); the overlap and allocation logic of vector clips at image-list level, which belongs to the renderer; libass's large-tile build and its SIMD engines (NEON blurs `\be` over whole 16-byte rows into the padding; AVX2 pads rows to 32 bytes); runtime and memory; `outlines-71` and `outlines-83` on ARMv7, open there: libass silently draws nothing when its composite allocation fails, the crate returns `Allocation`; a visible-pixel fold for `raster-simd` |
| ASS parsing | Whole ASS and SSA scripts and Matroska packet streams as libass reads them: headers, styles, events, style overrides, embedded fonts; codec private, packets with ReadOrder deduplication, flushing and pruning; typed override tags with libass's argument rules; every glyph's state at a time (animation, movement, fades, clips, karaoke tags, transition effects); karaoke syllables from laid-out positions; the drawing language into `triergon-raster` outlines; storage charged to a budget, work to a `WorkBudget` | Identical to libass on 54 authored fixtures, on macOS AArch64 and, with each target's own libass build, on aarch64-unknown-linux-gnu and armv7-unknown-linux-gnueabihf: `ass-parse-scripts` 48/48 pass (every track, style and event field, 87 styles, 222 events, embedded fonts, one script without a track), `ass-parse-chunks` 4/4 pass (10 track states, ReadOrder deduplication by bitmap and by scan, flushes, prunes, fonts in a codec private and in more data), `ass-parse-events` 21/21 pass (695 event walks, 6,976 glyph states and karaoke results, 48 drawing and clip outlines, 3 of them rejected as libass rejects them). `ass-parse-host` holds the 15 operations whose libass result differs by target, each pinned per target: on armv7-unknown-linux-gnueabihf 13/15 pass with 2 open, on aarch64-unknown-linux-gnu 11/15 pass with 4 open; elsewhere all 15 are reported. The reference flags every conversion from `double` to `int` out of range or for NaN (drawing coordinates, `\pbo`, an animated `\clip` or `\iclip`, `\fade`'s alpha, a `Banner` or `Scroll` delay) and a drawing coordinate on `INT_MIN`, and a flagged case fails any other feature. 2,728 mutated fixtures (`fuzz --contract`, 30 rounds of each feature, with the reference for all but `ass-parse-host`) gave identical work in both workers | not measured | not measured | `long`-width behaviour follows each target's libass, and out-of-range drawing coordinates are rejected as libass does on 32-bit ARM with GCC; every other out-of-range conversion from `double` to `int` saturates as there. Open as deliberate divergences: a coordinate landing exactly on `INT_MIN`, which libass built with GCC accepts and the crate rejects (`drawings-int-min-x` and `drawings-int-min-y`, on both targets), and `drawings-infinite` and `drawings-int-min` on aarch64-unknown-linux-gnu, whose coordinates libass converts with a 64-bit `long` there; the font description a glyph asks for (`GlyphState::font`) and karaoke run starts from style runs, shaping and wrapping are compared through the renderer (`ass-render`), not here; selective style overrides, track features, `ass_step_sub`; runtime and memory |
| ASS rendering | `Renderer`: the events shown at a time to positioned alpha images as libass's `ass_render_frame` draws them with hinting off and complex shaping: font selection and fallback over attached, embedded and default fonts, shaping and bidi, wrapping, alignment, positioning, scrolling, collisions kept across frames, change detection, glyph and border bitmaps, blur and shadows, clips, karaoke and the image list; storage charged to a budget, and all the work of a frame (evaluation, font selection, shaping, outlines, layout, drawing and collisions) to one per-frame bound, `Settings::work` | Identical to libass on Linux AArch64 and [32-bit ARMv7](#32-bit-armv7), each with its own libass build, and on macOS AArch64 before the cases of `@` fonts, unregistered languages, glyphs without points, the charmap retry, two `@` font styles and vector clips without a contour or beyond libass's reach (macOS was not run with them), configured as a player embeds it (attached fonts, a default family and font file, no system font provider, no hinting, storage smaller than the frame): `ass-render` 76/76 pass over 35 authored scripts and 3 Matroska packet streams (ReadOrder deduplication, flushes, prunes, an embedded font) at 1920×1080 over 1280×720 storage and 1280×720 over anamorphic 720×576 storage, at each fixture's frame times; every image's kind, position, size, colour and alpha bytes, and every frame's change value as `ass_detect_change` reports it (732 frames: 356 unchanged, 12 moved, 364 new content, including frames after a flush and after pruning every event, opaque boxes around different glyphs that draw nothing, vector clips whose drawing has no contour, which empty their event's images, and one whose drawing libass cannot place, which it does not apply, frames whose caches overflow and are cut, C1 controls drawn through a fallback face's Macintosh charmap that the face keeps for later lookups, a style font size of 0, whose glyphs FreeType loads unscaled, `@` fonts drawn rotated and shaped with their vertical advances, a `Language` header HarfBuzz does not register, fallback mark positioning that measures a glyph without points, and a fallback face whose characters libass finds by trying its format 2 and 8 subtables, the Unicode charmap FreeType builds from its glyph names and its CFF encoding in FreeType's order, and two styles of one `@` font that select the same face, whose rotation decisions libass keeps apart); the box behind a `BorderStyle` 4 event, whose type libass leaves uninitialised, is told by its own buffer, also after a collision shift cuts rows off its top, and compared as a type of its own; with libass's cache limits at 1000 glyphs and 24 MiB; `ass-render-host` pins per target the rendering whose libass result C leaves to the target (`border-x-overflow.ass`, a box width that doubles a huge border in wrapping `int`, and `collision-far-tops.ass`, colliding events whose boxes end up 2^31 or more rows apart, sorted as glibc's merge sort sorts them with libass's overflowing comparison): 4/4 pass on ARMv7 | 1.0658 at `5b3e03f0` (76/76 pass; worst `cache-keys-ass-720p`, median of the cases 0.813) | 76/76 lower than libass at `5b3e03f0`, worst peak heap 0.841 and worst total RSS 0.943, both `animation-ass-1080p` | on AArch64, `ass-render-host`'s 4 cases, a deliberate divergence: libass built with GCC for AArch64 converts an event box's top, height, left and width to `int` with vector instructions that keep the low 32 bits where 32-bit ARM saturates, and the renderer follows 32-bit ARM, so they are not timed or measured there; settings other than frame and storage size are unverified; libass's simple shaping and hinting modes, selective overrides and track features; qualifying also waits on the shaper's six open `shape-libass` cases (the auto-hinter's CJK extents); of 5,182 generated scripts and 540 fixture mutations rendered frame by frame against libass for ARMv7, 50 scripts and no mutation differ, each for a stated cause: the renderer stops at the default `Settings::work` bound (17), which on ARMv7 is about 9 s of rendering for one frame (a median 33 ns per unit); with the bound lifted, 34 of those frames take 7 to 21 s each there and 110 run out of the 32-bit address space, so the bound refuses only frames no player could show in time; it runs out of the 32-bit address space (19 allocations refused, 8 at the worker's budget) where libass for ARMv7 peaks at 2.6 to 4.1 GB; it refuses glyph extents of FreeType's hinted load (2); and in 4, and in earlier frames of 2 of the above, libass for ARMv7 alone departs from libass for AArch64 at 3.7 to 4.0 GB of its 4 GB address space, and the renderer matches libass for AArch64 (`random-2258` from frame 46). Against libass for AArch64, 115 scripts and 1 mutation differ: 79, the mutation among them, that are identical on ARMv7 and differ there through libass's 64-bit `long` narrowing and its vectorised conversion of event boxes, both deliberate divergences; 34 at the work bound, where libass peaks at 3.5 to 23 GB; the same 2 refusals of hinted extents; and `random-1428`, where libass is killed for memory and the renderer refuses at its budget |
| Capture | RGBA to YUV 4:2:0, MPEG-1 video with budgeted storage, MPEG-TS mux | 15/15 quality cases against FFmpeg 9.0.2 | 1.1210 (15/15 pass) | heap ≤0.548, total RSS ≤0.978 (15/15) | — |

## Measured revisions

Each row names the source revision a result above was measured on and, where the raw
samples are archived, their SHA-256. Update the row in the same change as the result.

The Matroska rows use the [gate build](quality.md#measuring-performance)
at `513149a970caad5b30c61becba1bc2bda1c5e193` on a c8g.large pinned to CPU 1,
with 30 timing pairs of 50 operations and 30 separate memory pairs per case.
Codec and decode-session runtime at
`add6b27` is one unpinned gate-build pass of 30 pairs on a c8g.2xlarge. Capture at
`ceb5076` reports the higher of two unpinned gate-build passes on the same instance type.
The audio codecs in files, Ogg and audio elementary-stream rows use the gate build at
`ccff5b5734c113c1d88160b84825157508c701c0` on a c8g.2xlarge, unpinned, against the `decode` profile:
30 timing pairs per workload of `harness/codecs/file-workloads.json`, each demuxing,
decoding and mixing to stereo s16 on both sides (`cargo xtask codecs time`), 30 heap and
30 RSS pairs under the native observer (`cargo xtask codecs memory`), and the reader lane
against the profile's `ffprobe`. The Ogg and audio elementary-stream rows rerun the reader
lane with both Ogg views at `ccd3090572433e79d8ab65a377fcd9896d2839d9` on the same instance type.
The codec, decode-session, file-audio and capture timing passes at `add6b27`, `ccff5b57`
and `ceb5076`, and the ARMv7 codec timing and capture passes at `78a873dd`, predate the
shared paired runner and keep their recorded results; the shared validator has not
re-graded them, as their records hold no warm-ups or module observations. Their codec
pairs were checked pair by pair rather than watched over the whole run, and each capture
sample was the median of three in-process sessions after a discarded one. A diagnostic
comparison on a c6g.2xlarge, with the codec workload timed twice by each runner:

| Workload | Earlier runner, `642316cf`: median (upper 95%) | Shared runner, `c21176e0`: median (upper 95%) |
|---|---|---|
| `aac-lc-stereo` | 0.7350 (0.7357), 0.7339 (0.7361) | 0.7344 (0.7363), 0.7342 (0.7351) |
| `c07-ui-static-480-bottomup` | 1.3026 (1.3047) | 1.2978 (1.3001) |

The remaining rows retain their earlier measurements.
The packet-framing row uses the gate build at
`04512a07fbb7efdf0d6d7173db08ec1267f2fae3` on a c8g.large pinned to CPU 1,
with 30 timing pairs of 50 complete operations and 30 memory pairs for all 125 cases.
All memory observations are valid; lower total RSS remains unachieved on every case.

The MPEG-TS and HLS rows use the gate build at
`21c9eef2f08a11bb7bd1c457997095326f2bb42d` on a c8g.large pinned to CPU 1.
All 375 cases pass parity, runtime, peak heap and total RSS with 30 timing
pairs of 50 operations and 30 memory pairs under the kernel RSS proof.
All five progressive functional inputs pass their ten outcomes. Each HLS
video unit passes through `contiguous_feed` inside the measured operation.

The raw Annex B results and the earlier Matroska audio clock comparison use the gate
build at `b3579f729494a37a4dbebe03c475f7b38484046e` on a c8g.2xlarge pinned to CPU 1,
with 30 timing pairs and 30 memory pairs under the kernel RSS proof.

In the earlier audio clock comparison, `laced-fractional-cadence` reached 1.4510,
`laced-unknown-duration` 1.4089, `two-laced-before-first-known-pts` 1.3968 and
`damaged-lace-before-valid-anchor` 1.3878. Peak heap reached 1.822 on
`no-absolute-anchor` and 1.321 on `dolby-bad-first`. Total RSS ranged from 1.038 to
1.043 across all ten cases.

The subtitle rows use gate builds on a c8g.xlarge pinned to CPU 1, with 30 timing pairs
of 50 operations and 30 memory pairs under the kernel RSS proof; every row is a graded
report. DVB is from `0d70bebfc0799f65df14f170ed97e68c5a35daa7`, DVD from
`531e2d04ba7386513ff7afe7b44f53b2a93ca9dc`, PGS, closed captions and teletext from
`956c42961f4a27d88f96d5800d59ff1eeb088018`. Between these builds, the subtitle worker's
functions differ only in the DVB and DVD receivers, apart from constant addresses. PGS memory is
from `03f3efd3`, which keeps planes resident by writing the last display's buffer again
(30 memory pairs under the kernel RSS proof); its runtime is from `956c4296`, and against main
the instruction counts of `03f3efd3` are a median +0.20% per case, −1.51% in total, over the
subtitle-pgs cases and +0.26%, +0.15% in total, over the capsule cases. `long-movie-mkv` total RSS
went from 1.010 to 0.958: the receiver had mapped and unmapped its first planes before glibc's
dynamic mmap threshold adapted, and the kernel bound counts pages faulted in again.

Codec and decode-session memory await gate-build measurement with repository runners.

ISO BMFF open and index measurements use `cargo xtask isobmff-bench`, with
`--subset52` selecting the 52-operation set. Packet framing uses `cargo xtask framing`.

The ISO BMFF row uses the gate build at `f99d893196908d562c897bccb20c6a972cd25d11`
on a c8g.large pinned to CPU 1, 30 pairs. Runtime comes from the timing collection and
memory from a separate collection under the kernel RSS bound; both are in the raw
samples. `cargo xtask grade` cannot join them, because FFmpeg's log lines carry heap
addresses that differ between runs.

The MP4 forward and range results use the gate build at
`a41648dd4de2933d526415cce86a9ebbd1d4444b` on a c8g.large pinned to CPU 1, with
30 timing pairs of 50 complete operations and 30 memory pairs. All 39 forward
and 27 range cases match and pass runtime, peak heap and total RSS. Forward
playback covers encoded edit projection with composition offsets, fragmented
input and unknown movie duration. Regular Opus cases cover edit-list trim,
variable packet duration and secondary audio alignment on a backward video seek.
The five source-roll cases compare the video landing, both audio ordinals and
decoder preroll. Broader profile and FATE coverage remain pending.

The rasterising rows on Linux compare the gate build at
`a7b6635e20cc2dc9d4592a874ba0eecab24f2870` with the libass reference: the four
features of `cargo xtask raster compare --all` and the SIMD diagnostic, with the font
and shaping comparisons and the crate's tests, on a c7i.2xlarge (x86_64) and a
c7g.2xlarge (AArch64), and the four features and the tests in the ARMv7 user space on
a c6g.2xlarge. The 64-bit raw samples also hold the instruction counts and paired
timing of the reference built with and without zeroed automatic variables.

The ASS rendering runtime and memory use the gate build at
`5b3e03f0e9410853152043d033b4f4d757e98a40` on a c6g.2xlarge, natively on AArch64 unpinned and
in the ARMv7 user space pinned to CPU 1, with 30 timing pairs and 30 memory pairs per case,
graded together by `cargo xtask grade`, against `libass-player-native`: libass linked as a
player links it, none of its sources compiled into the worker, with
`ass_set_cache_limits(renderer, 1000, 24)`. The reference replaces libass's bitmap engine
with its plain C one, and libass is built with `-O2 -ffp-contract=off
-ftrivial-auto-var-init=zero`, so a player's libass with its NEON code on AArch64 is faster
than this reference. The timed spans favour the renderer slightly: in 12 cases libass opens
the default font file inside its span, which the renderer reads before its own, and
libass's side copies the script inside its span. Both features are timed by the shared paired
runner that times the codecs and capture. Memory is measured under the `linux-glibc-v4`
observer; font files libass maps during an operation count toward its total RSS. Before
timing, `cargo xtask ass compare --all` passed at the same commit on both targets. Over the
76 `ass-render` cases the median of the per-case medians is 0.813 on AArch64 (5 cases above
1.0, the highest upper bound 1.0658) and 0.790 on ARMv7 (the highest upper bound 0.9427).
`ass-render-host` is timed and measured on ARMv7 only (worst upper bound 0.8688, peak heap
0.614, total RSS 0.866); on AArch64 its 4 cases are a deliberate divergence.
Two changes merged after that commit are not measured here: the glyph loading (FreeType's
default load with CFF hinting, CFF DICT operands read once, and the `@` font names grown by
`BudgetVec`), which is in the renderer's font and shaping path, and the harness's memory and
worker change, which prepares both workers for memory collection and keeps measured worker
identities verifiable. The current code has not been timed.

The font, shaping and ASS rendering comparisons on Linux compare `cc09ffa8f270f0bbbe2a5de0db4dbf3ffb45be03`
with FreeType 2.15.0, HarfBuzz 14.6.0 with FriBidi 1.0.17 and libass 0.17.5 on a c6g.2xlarge:
`cargo xtask fonts compare`, `shape compare` and `ass compare` natively on AArch64 and in the ARMv7
user space, each target with its own reference builds, with the rasterising features and the crates'
tests on AArch64. The commits after it change only this file, the evidence index, and, merged
from main, the check of the revisions this file cites. The same
comparisons at `a1aa6f0e1981d07ce9d27efcdc5b6815839f5481`, before the FontMatrix was applied in
32 bits and main's ASS renderer timing was merged, at
`ec9a4f1627b2734efbd4134ceb86c3a71740815d`, before the linear advances and the FontMatrix's
rounding and check followed 32-bit FreeType, and at `cf5c7504c077ff47ab5da521261e5c7a04046f16`, before
the auto-hinter followed 32-bit FreeType's arithmetic and the CFF DICT readers its operand rules,
gave the same results, with the xtask
tests, clippy and rustfmt at `5061518644d081ede35891bfbfc21d5b6256e6c7`. The comparisons before the auto-hinter, at
`b531944603a96d8fad9a1bb50e99cd538528280e`, `e551b607466657b4d604d8c107cad8735dd09e75` and, before the ASS renderer's latest changes,
`43c12afaf83f69076d430d830c7f247e51881c3e`, have `font-hinting-none` 155/159,
`font-hinting-default` 8/53 and `shape-libass` 193/201 with the other results the same; at the
last, the archived first ARMv7 run has the FreeType reference timing out on one NotoSansCJK
case of `font-hinting-none`, which its second run passes. The earlier comparisons at
`8058a77c444958ba82dcf4b5783720fc9aafe66f`, `8786192bac7b0abea286ee999c8e03073a6ac796`,
`944fd7cfc7712f505bd66e046afaa77f7eee6aac`, `a6e984526d20029e75b48650e9cc317f192da776`,
`354f7d735ac22df3b63e7849eb5869488c087bd9`, `b5bc0d90f1e2427849e43b4c00d2f0ef15ce2506` and
`8e137d8d5c4e6dbe76c5289a769242cea9beda2d` are kept for the steps between.

The ASS rendering comparison on Linux runs `cargo xtask ass compare --all` and the crate's tests
at `01ccb49792cf1d4d05b74addf35dc616763b88e0` with each target's libass build on an r6g.2xlarge,
natively on AArch64 and in the ARMv7 user space; the pins recorded on each target are identical.
The frame-by-frame run of 5,722 generated scripts and fixture mutations, which the remaining
counts and causes come from, renders them at `3469d84e360ac2217d603effa4d1a9fd70a87ea1` on both
targets; `01ccb497` changes only a fixture, its pins and a test. Every case finished within libass's
time limit. The work-bound scripts' frames were timed on ARMv7 on one CPU of the
idle host. Before these changes 1,497 generated scripts and 67 mutations differed against libass
for AArch64 (`fedfb790bfb52131389cb7c5134bab7ab813b55f`), and 1,069 scripts and 38 mutations
against libass for ARMv7 at `28e5ccf0ecb5419f7105d52c91ebe8d6c09f053c`, before the font work
merged at `642316cf260e427f8d2044adc2253e427dd13d2a`; that earlier set also holds the replay of
libass's collision sort, the conversions GCC compiles for each target, and libass for ARMv7
against libass for AArch64 where it runs out of memory.

| Area | Revision | Raw samples SHA-256 |
|---|---|---|
| Matroska / WebM | `513149a970caad5b30c61becba1bc2bda1c5e193` | `883ce5128410aefb3ec9affe3579a798ee8d1c5d082c1d129a462d67728e7af9` |
| Matroska startup and seek | `513149a970caad5b30c61becba1bc2bda1c5e193` | `883ce5128410aefb3ec9affe3579a798ee8d1c5d082c1d129a462d67728e7af9` |
| Matroska startup and seek, preceding comparison | `0e28298d038bea072629b3bad6389dcc806b4861` | `a20dcbbe804bfd21ee631cc42f61466d2400d5115abb8ea97dd243ef733ba74f` |
| ISO BMFF / MP4 | `f958c0b9624a8b7c7bd91b7d2a0c30299ae1dbed` | `fd718807e0ec1e6287bcb690226bf7056b79985796e74eba54f43544a034bc2c` |
| ISO BMFF comparison | `0a033ec3406aebd09d191298a546659e02c6b76c` | `f56d3fa78b8cc70222f58c7cfa9a17331b0924a9070c7ff6ecc6dc1f213561bc` |
| ISO BMFF open and index runtime and memory | `f99d893196908d562c897bccb20c6a972cd25d11` | `806f2f996b19370bb23350ae4a166f3b7688b3e0e499352d77c3b2610c90aa7b` |
| MPEG-TS PES | `21c9eef2f08a11bb7bd1c457997095326f2bb42d` | `8ad96ce3a9ca2b5e4b23dfcc53659fb4cc99ae0638d502e114188b3acbdad2f5` |
| MPEG-TS access units | `21c9eef2f08a11bb7bd1c457997095326f2bb42d` | `8ad96ce3a9ca2b5e4b23dfcc53659fb4cc99ae0638d502e114188b3acbdad2f5` |
| MPEG-TS program playback | `21c9eef2f08a11bb7bd1c457997095326f2bb42d` | `8ad96ce3a9ca2b5e4b23dfcc53659fb4cc99ae0638d502e114188b3acbdad2f5` |
| MPEG-TS progressive playback | `21c9eef2f08a11bb7bd1c457997095326f2bb42d` | `8ad96ce3a9ca2b5e4b23dfcc53659fb4cc99ae0638d502e114188b3acbdad2f5` |
| HLS segments | `21c9eef2f08a11bb7bd1c457997095326f2bb42d` | `8ad96ce3a9ca2b5e4b23dfcc53659fb4cc99ae0638d502e114188b3acbdad2f5` |
| MPEG-TS progressive playback, preceding comparison | `23f68766543bcd57a96e88bfe5350095236ff88c` | `a372562b91bb14dce3bbd7117f71deb098f6dba49492c5f4396a70336012d6fc` |
| MPEG-TS program playback, preceding comparison | `23f68766543bcd57a96e88bfe5350095236ff88c` | `a372562b91bb14dce3bbd7117f71deb098f6dba49492c5f4396a70336012d6fc` |
| MPEG-TS progressive playback, earlier comparison | `5ec7c1f8a7816b1427631f22a8ca77ba74abeeee` | `fb8a3a3b1f7f3d687254a3f4c1849dede7a78bc3c62dbc7905b09a45f675709d` |
| MPEG-TS program playback, tight carrier comparison | `eb3279f36c19c97dd9bdd68b1e66f371f28e38c1` | `7e18edf0a340b6ff4bb296b52deffc720f706c464340c7c73392127f9c87248c` |
| MPEG-TS program playback, earlier comparison | `3125c1e61d73a575419f7b653a1dae44a3269dae` | `beb5fa935b0109d0e33dbc3f57c22481c875a4c2b065820dda247535dc431f3a` |
| HLS segments, earlier comparison | `5ec7c1f8a7816b1427631f22a8ca77ba74abeeee` | `fb8a3a3b1f7f3d687254a3f4c1849dede7a78bc3c62dbc7905b09a45f675709d` |
| Packet framing | `04512a07fbb7efdf0d6d7173db08ec1267f2fae3` | `677ce4b5aef58efd8523868cd2318d9cfde07868d2b7451d0017f013dfb3130b` |
| MP4 range playback, earlier crop comparison | `63f6fe787b4f2976c339ee0548f032d21781dfb6` | `cfb8b1edc7592533b43cf1327be9cfd98a83598c90f3390cabd4770f147e1223` |
| MP4 range playback, preceding gate build | `f61d41fedac63b7e2bae08586de4e2d5bb8e9ad0` | `b3b814daff8a50d01c2ed89ae6219b1ba8cd2fbe094d7c3d61fb805cc76105e6` |
| MP4 range playback, 18-case comparison | `77155a8e6f6d4ce3b4823bcaf6824713446feab3` | `48c4f1e51785ae87e5bf52ec0bda92255149aa08d178ee50b597ba21134d8c13` |
| MP4 range playback, source-roll gate | `d688d4a01f48e5cb9353b9aa16770806f1f5dff3` | `0b08e42f9e332814e7b4848fad4a773c5d9c1d9a8684ca97593ae5f514655fe1` |
| MP4 range playback, 22-case comparison | `35912fe96ebc0c484a8a4b930260d54d54fcf1d0` | `30ed94ea480d780bd4101c88e4a0786bfbe1302a9ca924e42cc28c4f5a8565ac` |
| MP4 range playback, preceding source-roll comparison | `fa02eb61592bc85ad2749a87f3a925385a5e31f2` | `405b5bec9c2c76a59b0343d4276f684b7b61a0d4847ce06e31d0d74831446e25` |
| MP4 range playback, 15-case comparison | `24f2299a8d12e120eabc7fc4a32e7eb8febc953e` | `7cdbd50c398a89b4ec25f7d5f4184e0a46948c60cb6a22829a7ba1ac4a2a00e6` |
| MP4 range playback, dedicated sample entries | `17cad5c5bad576fe7102f53f78f3e150abbe24c8` | `632c275355a60d74974d1850f261a52ad4635414bddf7c2e0396ac914f59c97e` |
| MP4 range playback, before track classification | `b35259fcac541aebaa14058f9ae4682f89a61fdb` | `dbdbdacc0d86df6eefacb0403c0bcc0b7f5ff1c480415112e4eca791724a55ba` |
| MP4 range playback, preceding gate build | `668ecf58571fd19eb7e9aba4020606b409229625` | `46789d11f9768aef71eead36ce8f2acc05c820abc95e19b4a3ffb36a09e9e71f` |
| MP4 range playback, source Opus roll | `a1bb29d99ef0573e40357e32d65f3dcec83249f7` | `fa73233358526ff6a4aad24e20467b1c1bcaaef772d86339b4df29ac5c300589` |
| MP4 range playback, Opus preroll implementation | `7213bca65559deb8d87860afd9f74c8193a837d5` | `ba09a896d810c6fbca384f2b47ae1b7b5981c9fbd36b9c4262caa36c3567d1e9` |
| MP4 range playback, regular Opus | `af1b9b933480dbe44856bc54afcda636128b0702` | `1d64f438deca9655092d6b01d80730db4c34d05343c7152791bcef29296e4254` |
| MP4 range playback, AVC/AAC | `f58e444b72f94bb3962990dd515f28be1aaaa574` | `bb59fb61a667c1010a0a8e46604ca9b10d8cab6a3f99e05c2b68170c18d82755` |
| MP4 range playback, fragments | `a2f009026bc53be3757a9064247fb4e3ace80e0b` | `d95f142f73c0790d211f30d7de613f734f68409eb3015c7d48905c3c5e542105` |
| MP4 range playback, fragments | `348e0b0cb2e79c5e51a2769e2d9f319c2af00983` | `25546feeeb50edf887dd0656502e5cca96b3d61f90431ebe7035e41e1f25dca3` |
| MP4 range playback, fragments | `a690f0f7fe7db02b64c2506e57e7a1526b3f092a` | `48a28a728709ff0354a588a73e511ae6db3e0e3b01b362a352e407fd31da38aa` |
| MP4 range playback, fragments | `777279863f4eb9909ae51a4c3f88fad344e01061` | `824850eeab7e00345aff8892ef0d92633e52f1487ddbef83550404946fe936da` |
| MP4 range playback, earlier comparison | `90ecdb3f85f92402df9d7ecbec17f023535a5bf5` | `78e5d64743203173b7dfd8beed2dc4ab750d137e6fe3b00df321536b15057f05` |
| Codecs and decode sessions, runtime | `add6b272cb622bae0d921dc3fbbf0812958c2023` | `ab2a174847322658159e93b2cd9bb994cdd9f4f9438926191518897aff28d929` |
| Audio codecs in files | `ccff5b5734c113c1d88160b84825157508c701c0` | `0ddcc517728bb39e2d74cd2e3bf4c00071a6f3815ad3409d5215d216af9a0a53` |
| Ogg, audio elementary streams | `ccd3090572433e79d8ab65a377fcd9896d2839d9` | `8123e0f020a9c4ccedd83aea3e665d4486d55e26612e8bc29626e9a3026e2cd1` |
| Capture, timing and memory | `ceb5076659e00ca9a2d3f3cb55ee4513d6d0710f` | `b75c8d6e6d64d294a385ec25b176e4ffb8f1a0d3c11b3ca956b3d352ac44c3b4` |
| Codec and capture timing, earlier and shared runner | `c21176e03328f26e0f49d6870dad0a55dd7c5e76` | `9a2cb6808d49462ff33e0666a44773692c44e85414f5aa9b2a21ea27a11a6bcf` |
| MP4 forward and range playback | `a41648dd4de2933d526415cce86a9ebbd1d4444b` | `32aa6f481afc5f6074dd14a683a110bc044b781b9c40b72c84876af9b812f977` |
| MP4 range playback with data tracks | `90944404d8236a6da47d414f67a0652f2fc232c2` | `b368990b90a85a7286f28951ab2316264d98d3820342de51c8ec1b5ad998bdd2` |
| MP4 range playback with QuickTime AAC | `5fd3f0244849f11685fdf42a75c4c71847001bf9` | `018969e71e508093b8b1200f0df59505ffd7420d8a707250f7a7a088a752bdb1` |
| MP4 range playback with fragmented QuickTime AAC, preceding seek path | `b96eb9ec65abbe4548b4bc00a74e8504a6d0001d` | `2b2030d312f1af3a9716b2178afc5c04b7176822329f03240e4f41fca2f18478` |
| MP4 range playback with fragmented QuickTime AAC | `d2218b5026a5cca87516360cf5a724cc57d15b1a` | `cea6d87e41ab618d2db679b659856276788a37a6f962eb27e5e71a2b8825c48b` |
| MP4 range playback with projected tracks, preceding fragment seek path | `5ab026d75783575aaf2ebf0120b0338dac85369a` | `2b2030d312f1af3a9716b2178afc5c04b7176822329f03240e4f41fca2f18478` |
| MP4 range playback with projected media tracks | `dea0855e1bd45ea6df68e24ccfd83a47e7b46234` | `cea6d87e41ab618d2db679b659856276788a37a6f962eb27e5e71a2b8825c48b` |
| MP4 range playback with QuickTime AAC, earlier preload | `4c59b8121079719a3c988835c1cef6e65eae1719` | `b368990b90a85a7286f28951ab2316264d98d3820342de51c8ec1b5ad998bdd2` |
| AAC in MOV PCM | `63f5e8e5626ed5dac0bbeb9c0e742e0eec6146e6` | `100613a296e2f18f9631c60992057cb0debc108f1926c41a33b8c3f6f06f4556` |
| AAC in MOV and fragmented QuickTime host comparisons | `9559dae0b89dfd2df2b08b9a97267bc75711be65` | `0bc8fb8726cff0d2ba18d7c138ba83375baf13265bb148086522047bf816aea2` |
| MP4 range playback with configured 7.1 AAC | `09c154a3d4c8e38c34f59815ad7219df1e86ee26` | `cea6d87e41ab618d2db679b659856276788a37a6f962eb27e5e71a2b8825c48b` |
| MP4 forward and range playback, earlier RSS comparison | `733ee1582e4fce4af1d89c91171e29e41a16d294` | `23cf7cdb3de800c1ca0d03dfe937ce6fda6cc768bef5933370223a88cda78c87` |
| MP4 forward and range playback, earlier comparison | `8bbf6eab798f3d58f62dce14915fb30acd14488e` | `a3e74e11216d7769b7b167ac57cb17c043e6d92b4feb39a8422879c27493eebe` |
| MP4 forward and range, encoded edits | `2a1b26347fee56f5506db63efbc8e3e3c66ad388` | `fe95646449aa52a4a121aeecb3346308b270923aee9b0751b2125501abf36004` |
| Matroska audio clocks | `513149a970caad5b30c61becba1bc2bda1c5e193` | `883ce5128410aefb3ec9affe3579a798ee8d1c5d082c1d129a462d67728e7af9` |
| Earlier Matroska audio clocks and playback profiles | `b3579f729494a37a4dbebe03c475f7b38484046e` | `907bb7a35d70d09673ccbf0904a8b1ab85890a632454b8adf291d73a50672136` |
| Subtitles: PGS, closed captions, teletext | `956c42961f4a27d88f96d5800d59ff1eeb088018` | `61b52a272e82eeee913c9c4b4d6331caea3283a99ebad4a897aba65dab3c891d` |
| Subtitles: PGS memory | `03f3efd3a0fde0d558c68f5408c521d632802fb7` | `6c0cc3a669152b3eeba9a1189939c53b1b68e2f5cb9ab6cb81f903ec4358634d` |
| Subtitles: DVD | `531e2d04ba7386513ff7afe7b44f53b2a93ca9dc` | `01ef0a02aad45d7b754c0e409e12949437e8f7f0270e252e82a3f7ebff0e6391` |
| Subtitles: DVB | `0d70bebfc0799f65df14f170ed97e68c5a35daa7` | `06d1a46a967e052ce365795827803ddbc5ae8ff0e2452ae83428f53744949077` |
| MPEG-TS | `5634f47d33ca2a9ba671c82e805398ce4eafd83e` | `1076155fd7e9fa939a19889a55a29005463f1f3772e85e4e25b37a31a35891f2` |
| MPEG-TS finite fault recovery, host | `f3f439b803e079208c9ab422270a77feb6281207` | `69642daca51d9609dcd1cb5de3ca2c5828150f221b9937e37a7bef3bc8f99b9f` |
| MPEG-TS source boundaries, macOS AArch64 | `f939fb92a6e1309fdafcd84b0bc037130ed35ee3` | `8d5bca73013e5ea711ba82e928f7b6bd770a9008234c3654ee1ca9813abee656` |
| MPEG-TS ordered PES, macOS AArch64 | `576cf0a0da0d0d10232331626828610b12103a17` | `99ef872cb64a76ddb09036f432f3ccabc7b4ce02e6a2162020fd1303f3b58b33` |
| MPEG-TS ordered PES, AArch64 Linux | `576cf0a0da0d0d10232331626828610b12103a17` | `0abecbcda1308ab81d78f669a68bce42ab329f98b2cfda9558d1a7868912dfb1` |
| MPEG-TS ordered PES, carrier reuse | `02dd8abe7e92026061d848b2d6b93fcbb286532c` | `c3ccc85fe2cd3a44c2cc46489e2330d9a1fcc427491b0eddad4bcd878a6accdf` |
| MPEG-TS PES gate | `0f42a16e8e3c8f1375890efee80abe711a05c77b` | `e09a5509dcf30826a5aad4101ccdde0fe676545616a279f11ed23cb4240adb4c` |
| MPEG-TS PES gate, earlier comparison | `1b2ad742e6b17762c831ef37a934167f58332b54` | `e8358290aa65f631134af4a70bc7d90ad3858b5cc7118f4c4ed43cfd1e8c8b8a` |
| MPEG-TS ordered PES, plain carriers | `2e28dbc71d323643c4127609821fb61c4a73d7d2` | `b9cfbecdde7ae92f6af342db61f254934b9e1733ad23591bfa5467de2f1424b8` |
| MPEG-TS, earlier measurement | `090c856df65f9247d24ea9f1c927e35cf5d528e8` | `6f5c1b65540e9bbe2dc79a1f8c1dc41e684aba468cd8fdd082bf603d3bfb6ca1` |
| MPEG-TS control-only completion (macOS) | `65eeada0888bcd112d4cbd90e68795fea93fd864` | `d31f3bd8c76faf5d1b0a6e3ebe3756b8f71f3fb452fbc1d1ec38bfdb63048e1b` |
| ARMv7: Matroska / WebM, startup and seek, HLS, codec timing, capture | `78a873dd983c878c861a7b5549639a58d5a599c1` | `773f9f077ec4e556f8692132c35d73b8256f24c9c26a5e407aa11df32820a116` |
| ARMv7: ISO BMFF / MP4 | `f99d893196908d562c897bccb20c6a972cd25d11` | `bb7a7df1df72022fd2cdb617e6f0876327405c2833a0e056d319ff97d5fcbfe1` |
| ARMv7: packet framing, codec conformance, tests | `9621f6063a2e517fecc1c28757e6bed50b5228a8` | `9eaa9347010b3c4aaf5c35bda715ee4ed1b2c7a14d3329b21976a2414455439e` |
| ARMv7: MP4 forward, TS program | `4c823f7f74419cbfb9c8220a72b4684931c621f9` | `fe4098d3c3812710cc17b7b78155f2ee87eba366ce3bbe3d9de9569631bd7106` |
| ARMv7: Matroska audio clocks, MPEG-TS, raw Annex B | `7be886648e47f711b0f258412df82d214158a709` | `b1df5b6f79fa2644106b8e87c61799b5c6fa9075c33096017bf4c50d89ef95a0` |
| Rasterising, Linux x86_64 | `a7b6635e20cc2dc9d4592a874ba0eecab24f2870` | `1edfe455ee5a8ba42c9197bd9e8e97f42e4325eeb3f8abd6d13f0b479345ee3c` |
| Rasterising, Linux AArch64 | `a7b6635e20cc2dc9d4592a874ba0eecab24f2870` | `0ba86c152cef51adc4b57d37f7d9eeeab29a2137425aec95c9b7198b0245a44f` |
| ARMv7: rasterising | `a7b6635e20cc2dc9d4592a874ba0eecab24f2870` | `d328061b906a28f523eddd7aeaa778b62d13b7c7750c68347659a457497465d7` |
| ASS parsing, Linux AArch64 | `21642833a1c1f4d8e587a5c5bc903910f215428e` | `c039ca3e2cb63255140973528ddd22cc7e96b32e5254d22ea4f954011822a35d` |
| ARMv7: ASS parsing | `21642833a1c1f4d8e587a5c5bc903910f215428e` | `c039ca3e2cb63255140973528ddd22cc7e96b32e5254d22ea4f954011822a35d` |
| ASS rendering runtime and memory, Linux AArch64 and ARMv7 | `5b3e03f0e9410853152043d033b4f4d757e98a40` | `632964773b8ca4cbd5b2b3c09a2b6ddeafdaeeaff56588ea345bae8550012bb7` |
| Earlier: ASS rendering memory against the instrumented libass build, Linux AArch64 | `e6de7e70827bb930a8ef9e5cb4d30b1e4e7d0727` | `799386520d27ae7be5eab6c827fcbe0608950ddb4c32c93be7ea3cf660d4b0e3` |
| ASS rendering comparison and generated scripts, Linux AArch64 and ARMv7 | `01ccb49792cf1d4d05b74addf35dc616763b88e0` | `c510f31add480041fa7197ddbca26fe143926cf84106890b061b3b99897749be` |
| Earlier: ASS rendering parity, Linux AArch64 and ARMv7 | `28e5ccf0ecb5419f7105d52c91ebe8d6c09f053c` | `bc26dfc21b141cc79b5bb4f8cb3d636dcfaa3f64fa24eec2d200409eff0c32f0` |
| Earlier: ASS rendering comparison and generated scripts, Linux AArch64 and ARMv7 | `fedfb790bfb52131389cb7c5134bab7ab813b55f` | `f10a94cf995ed49ac8e9befda3441efcf3c900a984105b1385c3d34444418d1f` |
| Fonts, shaping and ASS rendering comparison, Linux AArch64 and ARMv7 | `cc09ffa8f270f0bbbe2a5de0db4dbf3ffb45be03` | `18028d8fa7452fd3d43d0d7d254560578d7f7a38d7a553d9fb42b7246a9df927` |
| Earlier: the 32-bit linear advances, before the FontMatrix's transform, Linux AArch64 and ARMv7 | `a1aa6f0e1981d07ce9d27efcdc5b6815839f5481` | `a22a7f4312953485804e6d3bfd654261d959eca9903ea2d92822157b525d6af3` |
| Earlier: the auto-hinter's 32-bit arithmetic before the linear advances', Linux AArch64 and ARMv7 | `ec9a4f1627b2734efbd4134ceb86c3a71740815d` | `cae121896f313cec3378f289f19b28fe1ddb73b8fcb5d0079f71247babb36010` |
| Earlier: the auto-hinter before its 32-bit arithmetic, Linux AArch64 and ARMv7 | `cf5c7504c077ff47ab5da521261e5c7a04046f16` | `64b022b36608f08b8609d128e0deaba3e7b37fe86c8ca94c312e41e411e7df79` |
| Earlier: the default load with CFF DICTs read once, Linux AArch64 and ARMv7 | `b531944603a96d8fad9a1bb50e99cd538528280e` | `a6c0e18c8ad7a3dfc38c8d4144c55f364b32b8c4aa4bf63f37b71f0316fd24a8` |
| Earlier: the default load at the merge with the ASS renderer, Linux AArch64 and ARMv7 | `e551b607466657b4d604d8c107cad8735dd09e75` | `8b20bf3d1a9c1e8da9c4d6f497f93d2a5636d8d6a80e531c49f623c98ce42feb` |
| Earlier: the default load before the ASS renderer's latest changes, Linux AArch64 and ARMv7 | `43c12afaf83f69076d430d830c7f247e51881c3e` | `d3780ba08af7d67ed07ad587056870132b988e6737104ff8343d7cea2496d600` |
| Earlier: HarfBuzz's cmap sanitizing and remapping, Linux AArch64 and ARMv7 | `8058a77c444958ba82dcf4b5783720fc9aafe66f` | `95b5c4d659408f264cbe8bfc24f20ca43a303863e23bbc107d3192956f5c6f7b` |
| Earlier: FreeType's format 4 and HarfBuzz's cmap selection, Linux AArch64 and ARMv7 | `8e137d8d5c4e6dbe76c5289a769242cea9beda2d` | `fe2bee783e52adedca9e0c427a16b3fa30afa70471aaa507f38be9e177dd2b8a` |
| Earlier: rotations by font, Linux AArch64 and ARMv7 | `b5bc0d90f1e2427849e43b4c00d2f0ef15ce2506` | `8a5799509af3fa883fa3686ea181d7b630dee90db6e71e60cb55023c65e8cd37` |
| Earlier: fonts, shaping and ASS rendering, Linux AArch64 and ARMv7 | `8786192bac7b0abea286ee999c8e03073a6ac796` | `996b7f0a128c48391f232106246ef9b2cf7f4dd27ce6ec1eb709a68d8d86f008` |
| Earlier: vertical metrics, Linux AArch64 and ARMv7 | `354f7d735ac22df3b63e7849eb5869488c087bd9` | `f4e2ec2449012a0efc1ee4ec6c75cfff8122528e3c629fbbe68c1380d9eff7f5` |
| Earlier: charmaps, Linux AArch64 and ARMv7 | `944fd7cfc7712f505bd66e046afaa77f7eee6aac` | `c2ffb0cd91e0e5e313e08d4df02ff034978b054fffb88a27451a337e7e770d0e` |
| Earlier: `@` fonts, Linux AArch64 and ARMv7 | `a6e984526d20029e75b48650e9cc317f192da776` | `d5b68414a5a9db73c98b0cab8f49919980da2d7754e46fcb84cf7cc6f8039f0a` |

## Platforms

Qualification runs on native AArch64 Linux and, where the next section says so, on
32-bit ARMv7 Linux. x86_64 has no performance runs yet.

### 32-bit ARMv7

The gate build for `armv7-unknown-linux-gnueabihf` (Rust 1.99.1, default flags, so no
NEON), with every FFmpeg reference built from the same pins in the same user space
(FFmpeg selects NEON at run time). The Ubuntu 24.04 armhf user space archived as
`armv7-userland` ([how to repeat it](quality.md#32-bit-armv7)) on an AWS c6g.4xlarge
(Graviton2, Neoverse N1 r3p1), kernel `7.0.0-1014-aws`, pinned to CPU 1, 30 pairs.
Memory uses the standard bounds, because the kernel RSS proof has no 32-bit entry
point: total RSS is the conservative ceiling (the candidate's declared resident ceiling
over the reference's observed peak), not a proven result, and the observed resident
peaks are given beside it.

| Area | Verified against the reference | Runtime | Memory |
|---|---|---|---|
| Core | 51,235 exact-arithmetic cases; the workspace tests (3,646 in debug, 3,645 in release) and the worker tests pass | via dependents | via dependents |
| Matroska / WebM | 116/116 (six feature groups and the external HDR vector) | 1.0940 (116/116 pass) | heap ≤0.775; total RSS ceiling ≤1.728, observed peaks ≤1.136 |
| Matroska startup and seek | 44/44 | 0.9267 (44/44 pass) | heap ≤0.396; total RSS ceiling ≤1.448, observed peaks ≤1.016 |
| HLS segments | 71/71 | 0.7366 (71/71 pass) | heap ≤0.020; total RSS ceiling ≤1.353, observed peaks ≤0.843 |
| ISO BMFF / MP4 | 1,020/1,020 open and index comparisons (972 matching outputs, 48 matching refusals) | 1.0863 (972/972 pass) | heap ≤0.364 (972/972 lower); total RSS ceiling ≤1.410, observed peaks ≤0.893 |
| Packet framing | 125/125 AVC/HEVC reference comparisons | 0.9195 (125/125 pass) | heap ≤0.765 (125/125 lower); total RSS ceiling ≤1.962, observed peaks ≤1.112 |
| Codecs | 347/347 comparable H.264 and HEVC conformance vectors (of 383 run, 31 are outside the supported profiles and 5 are refused by both); the 71 derived audio streams were not run, because nothing in the repository generates them; the six video timing workloads decode to FFmpeg's digests | 1.7785 (8/17 pass) | no repository runner |
| Capture | 0/15: mux and encoder pass on all 15, conversion fails on all 15 (below) | 3.7355 (2/15 pass) | heap ≤0.564, total RSS ≤0.981 (14/15; `c05-noise-960` has no valid observation) |
| Matroska audio clocks | 10/10 packet folds (`matroska-audio-clocks`) | 0.9337 (10/10 pass) | heap ≤1.681 (8/10 lower); total RSS ceiling ≤2.007, observed peaks ≤1.317 |
| MPEG-TS | 66/97 compat-view operations (`mpegts-pes`); 31 open cases in that measured revision | 0.9298 (66/66 pass) | heap ≤0.101 (66/66 lower); total RSS ceiling ≤1.885, observed peaks ≤0.986 |
| Playback: MP4 forward | 37/38 packet folds (`mp4-forward`) at `4c823f7f` | 2.4583 (27/37 pass) | heap ≤2.949 (36/37 lower); total RSS ceiling ≤1.986, observed peaks ≤1.243 |
| Playback: TS program | 35/35 packet folds (`ts-program`) | 1.2811 (33/35 pass) | heap ≤0.141 (35/35 lower); total RSS ceiling ≤1.858, observed peaks ≤1.088 |
| Playback: raw Annex B | 6/6 packet folds (`raw-annex-b`) | 0.6434 (6/6 pass) | heap ≤0.126 (6/6 lower); total RSS ceiling ≤1.833, observed peaks ≤0.984 |
| Rasterising | `raster-glyphs` 12/12, `raster-outlines` 2/2 and `raster-bitmaps` 2/2 pass; `raster-sweep` 301/303 (below); the crate's tests pass | not measured | not measured |
| ASS parsing | `ass-parse-scripts` 48/48, `ass-parse-chunks` 4/4 and `ass-parse-events` 21/21 pass; `ass-parse-host` 13/15 (2 open) | not measured | not measured |
| Fonts | `font-outlines` 53/53, `font-hinting-none` 159/159 and `font-hinting-default` 9/53 (the same open cases as on AArch64) | not measured | not measured |
| Shaping | `shape-libass` 195/201 and `shape-text` 200/201 (the same open cases as on AArch64), `shape-bidi` 24/24, `shape-simple` 6/6, `shape-unicode` 1/1 | not measured | not measured |
| ASS rendering | `ass-render` 76/76 and `ass-render-host` 4/4 pass; the crate's tests pass | 0.9427 (80/80 pass) | heap ≤0.928, total RSS ≤0.961 (80/80 lower) |

The subtitle, closed-caption and teletext features have not been run on ARMv7: their parity,
runtime and memory there are not verified; nor are the runtime and memory of fonts and shaping.

Above the ceiling on ARMv7: codecs `h264-high-1080p` 1.5371, `h264-main-1920x800`
1.3155, `dts-core-es-6.1` 1.7268, `dts-core-7.1-master` 1.7785, `ac3-5.1-448` 1.3602,
`ac3-2.0-192` 1.5987, `eac3-7.1` 1.2606, `eac3-5.1-spx` 1.2126, `eac3-stereo-spx`
1.4841; capture `c01-ui-static-960` 3.4321, `c02-ui-scroll-960` 2.3130,
`c03-ui-transition-960` 3.0576, `c04-motion-960` 2.8436, `c06-ui-scroll-960-bottomup`
2.2526, `c07-ui-static-480-bottomup` 3.7355, `c08-ui-scroll-480-bottomup` 2.4663,
`c09-motion-480-bottomup` 2.5628, `c10-ui-transition-480-topdown` 3.3268,
`c11-motion-960-rate1` 1.6835, `c12-motion-960-1m` 2.7349, `c13-motion-960-6m` 2.7882,
`c14-motion-960-rate255` 2.6747; MP4 forward `progressive-ten-sequential-batches` 2.4583,
`progressive-pending-reverse-ranges` 1.7070, `progressive-pending-two-mdats` 1.6593,
`progressive-pending-terminal` 1.6022, `progressive-unknown-clock-terminal` 1.6012,
`progressive-pending-pair` 1.6010, `progressive-pending-unknown-clock` 1.5930,
`progressive-first-source-clock-unknown` 1.5892, `progressive-pending-description` 1.5783
and `progressive-f-explicit` 1.2004; TS program `entry-hevc-aac-44100-00` 1.2811 and
`entry-hevc-aac-44100-01` 1.2430. In the progressive MP4 forward cases most of the time
is the C library's `memcpy` and the kernel, copying input into the playback engine. Every ISO BMFF output is within it; the closest are
the two-track 64-bit clock outputs indexed in `tiny` windows (`order27-`), worst
`order27-u64-last-scale1000-cat21-native-index-tiny` 1.0863. The raw samples'
`report.json` lists each bound.

Capture conversion must stay within ±1 of FFmpeg's on every sample. On
`c01-ui-static-960`, Triergon's conversion output is byte-identical on ARMv7 and
AArch64, while FFmpeg's ARMv7 conversion differs from its AArch64 output. Across the 15
cases FFmpeg's ARMv7 conversion is up to 36 from Triergon's (on the noise cases), so all
15 fail the rule on this target; the encoder and mux pass.

The rasterising row compares libass's C engine, the one libass runs on ARMv7, at
`a7b6635e` on a c6g.2xlarge in the same user space; it measures no runtime or memory.
`raster-sweep` fails `outlines-71` and `outlines-83`: libass silently draws nothing
when its composite allocation fails; the crate returns `Allocation`. Both cases draw
glyph and border bitmaps of 1 to 2 GB each, so their composite does not fit a 32-bit
address space; both sides record the same 265 and 267 packets
and differ only in that record.
Both are listed as open cases on `armv7-unknown-linux-gnueabihf` only, so they are
compared there and must pass on every other target.
Its heaviest case, `bitmaps-24`, takes about 50 s on each side, within
`raster-sweep`'s 120 s `timeout_seconds`.

Every Matroska, startup and seek and HLS case has a total RSS ceiling above the
reference. The observed resident peaks are above it in every Matroska feature-group
case except `colour-many-tracks`, `attachments-large` and `attachments-large-late` (112
of 115, up to 1.136), and in 3 of the 44 startup and seek cases:
`seek-hevc-seek-no-default-0-eof` 1.0007, `seek-hevc-seek-no-default-2700-fresh` 1.0095
and `seek-hevc-seek-no-default-2700-eof` 1.0163. Every packet-framing case has its total
RSS ceiling and its observed resident peak above the reference (observed up to 1.112);
every ISO BMFF output has its total RSS ceiling above the reference, and none its observed
peak. The ISO BMFF ceiling is the comparison worker's virtual peak, 3,194,880 bytes in
every pair, three pages below the worker measured on `main`; the ratio is higher than on `main`
(1.387) because the reference's observed peaks were lower in this run.

Every measured Matroska audio clocks, MPEG-TS, MP4 forward, TS program and raw Annex B
case has a total RSS ceiling above the reference. Peak heap is above it in Matroska audio
clocks `dolby-bad-first` 1.2145 and `no-absolute-anchor` 1.6808, and in MP4 forward
`regular-encoded-avc-aac-native-seek` 2.9492. The observed resident peaks are above it in
all 10 Matroska audio clocks cases (up to 1.317), all 37 measured MP4 forward cases (up to
1.243) and 12 of the 35 TS program cases (`ts188-basic`, `program-eof-avc-aac`,
`program-pmt-retype`, `program-private-avc-dvb`, `carriage-aac-adts-direct`,
`carriage-hevc-direct`, `pts-negative-against-dts`, `transport-partial-final-packet`,
`pmt-opaque-language-descriptors`, `entry-hevc-aac-44100-00`, `entry-hevc-aac-44100-01`,
`m2ts-entry-hevc-aac-44100-01-video`, up to 1.088), and in no MPEG-TS or raw Annex B case.
`cargo xtask grade` graded the Matroska audio clocks and raw Annex B reports. The MPEG-TS,
MP4 forward and TS program numbers come from their complete timing and memory reports: the
MPEG-TS and MP4 forward runs predate grading of open cases, and the later MP4 forward and
TS program runs used `cargo xtask report`, which records no collection to grade.

The [measured revisions](#measured-revisions) table names the revision and raw samples
behind each ARMv7 row.

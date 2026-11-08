# Status

Everything is experimental. No module has yet passed every gate on its complete scope
(see [quality](quality.md)). Runtime is the worst per-case upper bound of
`T_triergon / T_reference`, graded against the [ceiling](quality.md#gates). Memory is peak
heap and total resident peak RSS relative to the reference. Unless noted, measurements are native
AArch64 Linux.
Containers, framing, subtitles and playback are measured against FFmpeg git revision
`d32b387f` on the 9.0 branch; codecs, decode sessions, capture and the playback MP4
terminal-media matrix against the FFmpeg 9.0.2 release; fonts against FreeType 2.15.0;
shaping against HarfBuzz 14.6.0 and FriBidi 1.0.17;
rasterising, ASS parsing and ASS rendering against libass 0.17.5
([reference pins](quality.md#reference-and-corpus)).

| Area | Implemented | Verified against the reference | Runtime | Memory | Open |
|---|---|---|---|---|---|
| Core | Charged storage, exact time, input windows, work budgets, neutral container model with one codec identity | 51,235 exact-arithmetic cases against an independent oracle | via dependents | via dependents | fuzzing |
| Matroska / WebM | Incremental reader: lacing, header removal, bounded zlib, Cues and preroll, colour and static HDR, chapters, tags, attachments, side data | 116 workloads in six feature groups, including an external HDR sample, and four host FATE demux comparisons | 1.1590 (116/116 pass) | heap ≤0.627, RSS ≤0.965 | full EBML/Matroska scope |
| Matroska startup and seek | Startup and seek with lazy scan replay | 44 cases, 1,344 packets | 1.1606 (44/44 pass) | heap ≤0.404, total RSS ≤0.849 | — |
| Matroska audio clocks | AAC, AC-3, E-AC-3 startup and forward clocks | 10/10 packet folds (`matroska-audio-clocks`) | 1.4510 (6/10 pass) | heap ≤1.822 (8/10 lower), total RSS ≤1.043 (0/10 lower) | audio seek |
| ISO BMFF / MP4 | Regular and fragmented files, edit lists, seek and range plans, neutral `ContainerReader` over seekable sources, QuickTime compatibility, bounded AAC configuration and regular table recovery, timing, sync and terminal media prefixes; AVC, HEVC, AAC, AC-3, E-AC-3, DTS core, AV1, VP9 | 255 inputs, 1,020 open and index comparisons (972 matching outputs and 48 matching refusal outcomes with compatibility policies) | 0.8854 (972/972 pass) | heap ≤0.4132, total RSS ≤0.7260 (972/972) | seek and range-plan timing; neutral reader not yet compared with FFmpeg |
| MP4 range playback | Regular AVC/AAC startup and backward seek with sample table durations and coded-frame trim | 4 complete open and backward-seek comparisons, 845 packets; AAC duration and trim regressions | 1.0466 (4/4 pass) | heap ≤0.556, total RSS ≤0.945 (4/4 lower) | other range playback profiles |
| Ogg | Pages with CRC and resync, multiplexed and chained streams, Vorbis, Opus and FLAC mappings with canonical configurations, granule timing, pre-skip and end trims, bisection seeking | Packets against the pinned `ffprobe` (`cargo xtask codecs readers`) on 21 FATE Vorbis files and 13 generated Opus, Vorbis and FLAC files (multiplexed, chained, damaged, truncated), in two views: the FFmpeg-compatible `compat::clock` 31/34 pass, 3 differ only by reviewed FFmpeg behaviours; the reader's own timing 7/34 pass, 25 differ only where `compat::clock` reproduces FFmpeg, 2 by reviewed behaviours ([reader rules](../harness/codecs/reader-rules.json), [frozen differences](../harness/codecs/reader-differences.json)) | within the codec workloads | within the codec workloads | `b_vorbis.ogg`, `chain_vorbis.ogg`: a negative last duration FFmpeg prints modulo 2^32; `crc_bad.opus`: the first packet after a page that fails its CRC, which FFmpeg times from its own page and `compat::clock` from the stream start; Theora and Speex untimed; chained seeks not compared with FFmpeg |
| Audio elementary streams | Native FLAC (with or without `fLaC`), MPEG audio with ID3v2/ID3v1/APE tags, Xing/Info/VBRI and LAME gapless trims, ADTS, AC-3/E-AC-3; one shared frame-stream engine | Packets as the readers emit them against the pinned `ffprobe` on 271 FLAC, MPEG audio, ADTS, AC-3 and E-AC-3 files (the IETF FLAC testbench, MP3 conformance, generated and crafted damaged streams): 253/271 pass, 16 differ only by reviewed FFmpeg behaviours, 2 refused by both (testbench faulty `06 - missing streaminfo metadata block`, `07 - other metadata blocks preceding streaminfo metadata block`) | within the codec workloads | within the codec workloads | FLAC `flac_s20_rate768k`, `06 - samplerate 768kHz` (no durations at 768 kHz), `01 - changing samplerate`, `27 - old format variable blocksize file created with Flake 0.11`, `11 - file starting with unparsable data`, `11 - incorrect metadata block length`, `08 - blocksize 65536`, `09 - blocksize 1`; crafted `ac3_tags`, `adts_mid_junk`, `flac_corrupt`, `flac_id3`, `flac_mid_junk`, `mp3_foreign_frame`, `mp3_mid_junk_falsesync`; MP3 `he_free` (free format, refused by FFmpeg); E-AC-3 dependent substreams on real files; ADTS and AC-3 seek cost |
| MPEG-TS | PSI, continuity, PES assembly; TS188 and M2TS192 | 66/97 compat-view operations (`mpegts-pes`: the 272 recorded requests through the public `compat::pes` API); 31 open cases need the MPEG-TS `ContainerReader`: `pcr-timeline-discontinuity`, `pcr-extension-invalid`, `transport-tei`, `transport-scrambled`, `transport-afc-reserved`, `transport-adaptation-overrun`, `transport-partial-final-packet`, `transport-sync-loss`, `psi-crc-invalid`, `psi-pointer-overrun`, `psi-length-over-policy`, `psi-pending-not-current`, `pat-atomic-two-sections`, `pat-incomplete-version`, `pat-network-pid-not-track`, `psi-pointer-finishes-old-starts-new`, `psi-two-sections-one-payload`, `pmt-version-wrap-retypes-pid`, `pmt-opaque-language-descriptors`, `pmt-descriptor-overrun`, `pmt-multi-section-forbidden`, `psi-stuffing-tail-invalid`, `psi-truncated-eof`, `pes-timestamp-marker`, `pes-forbidden-time-flags`, `pes-optional-header-overrun`, `pes-start-prefix-invalid`, `hls-contiguous-pes-carry`, `hls-external-map`, `hls-explicit-discontinuity`, `opcr-without-pcr-invalid` | 1.0029 (66/66 pass) | heap ≤0.073, total RSS ≤0.818 (66/66 lower) | the 31 open cases; the strict source view and its native clock are not compared; access-unit demux, broader profiles |
| MPEG-TS progressive playback | TS188 with AVC and AAC-LC, bounded sequential input and access units before EOF | 3 packet folds and all 5 functional inputs with 10 literal outcomes | 1.1951 (3/3 pass) | heap ≤0.016, total RSS ≤0.648 (3/3 lower) | broader transport and codec profiles |
| HLS segments | Independent TS188 segments with contiguous AVC or HEVC video and AAC, AC-3 or E-AC-3 audio | 71 packet comparisons, 71 measured workloads | 1.1520 (71/71 pass) | heap ≤0.022, total RSS ≤0.727 (71/71 lower) | other profiles |
| Packet framing | AVC/HEVC configuration and NAL framing (up to four AVC descriptors inline), AV1, VP9, AC-3, E-AC-3, DTS core | 125 AVC/HEVC reference comparisons; DTS core byte-exact on 37,125 projections and 1,172 FATE packets, recorded without a timing runner | 1.3846 (45/125 pass for AVC/HEVC) | heap ≤0.985 (125/125 lower), total RSS ≤1.048 (0/125 lower) | runtime and RSS optimization; full AVC/HEVC/ADTS grammar |
| Subtitles, text | SRT, WebVTT, ASS passthrough, mov_text and tx3g to ASS, charset handling | 82 mov_text cases, 79 tx3g regressions | not measured | not measured | timing and memory |
| Subtitles, bitmap | PGS, DVB display state and colour lookup, DVD units | 229 PGS cases, replayed by tests from recorded output; 5,300 DVB display sets, recorded; no runner in the repository | PGS 1.11 | PGS heap 0.62, RSS 0.90 | full PGS recovery, DVB and DVD decoding |
| Playback | Matroska, MP4, TS and raw AVC/HEVC feeds; seeking; end-of-file continuation | MP4 terminal-media matrix against recorded FFprobe 9.0.2 output (evidence set `playback-terminal-matrix-ffprobe`): 39 inputs in 13 source geometries (12 nonseekable for every input, 1 seekable for the 31 original inputs), 499 rows, each run in debug and in release. `Mp4Open::new_recovery` 499/499 pass: 497 identical packet lists, and 2 seekable reads of a zero-size fragment sample that the opener refuses in the header, where FFprobe also refuses. `Mp4Open::new` 446/499 pass. Packet folds: MP4 forward 37/38 (`mp4-forward`), TS program 35/35 (`ts-program`), raw Annex B 6/6 (`raw-annex-b`) | MP4 forward 1.6427 (35/37 pass), TS program 1.4695 (33/35 pass), raw Annex B 1.0810 (6/6 pass) | MP4 forward heap ≤3.382 (36/37 lower), total RSS ≤1.025 (18/37 lower); TS program heap ≤0.102, total RSS ≤0.910 (35/35 lower); raw Annex B heap ≤0.127, total RSS ≤0.901 (6/6 lower) | other profiles; legacy Matroska `A_AAC/MPEG4/*` codec IDs are not read as AAC; `mp4-forward` open case `regular-encoded-avc-aac-indexed-seek`: the native MOV view refuses an edit list over reordered samples |
| Codecs | H.264 8-bit 4:2:0; HEVC Main and Main10; AAC LC, HE, HEv2; AC-3 and E-AC-3; DTS core with XCh and X96; storage charged to a budget | 417/417 comparable conformance and FATE vectors | 1.0551 (17/17 pass) | heap 0.92, RSS 0.82 | error concealment, unverified tool paths; AAC charges in-band reconfiguration when it appears, and a refused access unit leaves the decoder unchanged unless an earlier raw data block or Main-prediction element of it was decoded |
| Audio codecs in files | FLAC (RFC 9639, 4 to 32 bits, 1 to 8 channels); Opus (RFC 6716 and 8251: SILK, CELT, hybrid, families 0, 1 and 255); Vorbis I; MPEG-1, 2 and 2.5 audio Layer I, II and III | 315/321 pass through the container readers (FATE Vorbis, Opus and MP3 conformance, the IETF FLAC testbench, generated streams): FLAC bit-exact, the others within the [numeric rules](../harness/codecs/numeric-rules.json) | 1.0834 (10/10 pass) | heap ≤0.904, total RSS ≤0.966 (10/10) | FLAC `01 - changing samplerate`, `02 - increasing number of channels`, `03 - decreasing number of channels` and MP3 `he_mode` change format mid-stream, which the CLI converts; FLAC `flac_3ch_odd_mask`, whose layout FFmpeg refuses on its stereo output; MP3 `he_free` (free format: FFmpeg finds no two consecutive frames, and the decoder refuses free-format frames), refused by both; runtime toward the target for Opus music (1.0301) and 5.1 (1.0834), Vorbis 5.1 (0.9356) and stereo (0.8423) and Opus speech (0.8719); Opus families 2 and 3 |
| Decode sessions | Bounded queues, clock-gated output, flush and seek, RGBA letterbox, stereo mix; container track decoding with trims and start delays | with the codecs | 1.0551 (17/17 pass) | with the codecs | — |
| Fonts | sfnt and collections; `head`, `hhea`, `maxp`, `hmtx`, `OS/2`, `name`; `cmap` formats 0, 4, 6, 10, 12, 13; `glyf`/`loca`; CFF and CFF2 at the default instance; `kern` format 0; loading at a size as libass does with hinting off (unhinted outlines, synthetic bold and oblique, size metrics, kerning, decoration bands, default-load advances where hinting cannot move them); face matching with libass's rules (compared through the renderer's `ass-render` feature) | The gate covers libass's default configuration (hinting off). All 65 faces of 51 corpus files (TrueType including Arabic, Hebrew, Indic and Thai fonts, name-keyed and CID-keyed CFF, CFF2, a 10-face collection, four authored fonts; 836,315 glyphs) identical to FreeType: names, cmap, libass's text charmap, the kerning of every listed pair, advances and unscaled outlines (`font-outlines` 51/51); at 256 pixels, size metrics, kerning, every glyph's unhinted outline with its synthetic styles, and default-load advances (`font-hinting-none` 100/102, 2 open: auto-hinter); macOS AArch64 | not measured | not measured | libass's hinting-enabled modes (NATIVE, LIGHT, NORMAL): their nine other sizes are measured, not gated (`font-hinting-modes` 900/918: unhinted outlines 459/459, default-load advances 441/459), and hinted outlines are not compared; the FreeType auto-hinter, which leaves DejaVuMathTeXGyre and authored-cmap-formats open at every size; tricky fonts and instruction-control programs; vertical metrics (`vhea`/`vmtx`, linear vertical advance) for `@` fonts, which leaves `ass-render`'s two `cjk-ass-*` cases open; `cmap` format 14; runtime and memory; Linux runs |
| Shaping | OpenType layout (GDEF, every GSUB and GPOS lookup type, feature variations at the default instance, `kern` format 0 fallback, fallback mark positioning) with HarfBuzz's default, Arabic, Hebrew, Thai/Lao, Hangul, Indic, Khmer, Myanmar and Universal shapers for horizontal runs; libass's FreeType-backed font functions over a sized face; bidi as FriBidi resolves and reorders it for libass; libass's simple shaping level; Unicode properties generated from pinned UCD files; planning, shaping and bidi bounded by work limits | Identical to HarfBuzz and FriBidi on macOS AArch64. Gating: `shape-libass` 191/200 (9 open: FreeType's auto-hinter sets eight fonts' default-load advances, and one CFF font without GPOS needs hinted extents), 3,029 runs over 25 corpus files and 22 scripts shaped as libass does at 256 pixels with hinting off; `shape-bidi` 24/24; `shape-simple` 6/6; `shape-unicode` 1/1 (every code point). Coverage at units per em: `shape-text` 199/200 and, on demand, `shape-text-all` 1,745/1,746 (101,691 runs over every corpus face), one open case each (CFF extents as HarfBuzz's own interpreter reads them) | not measured | not measured | hinted glyph extents and contour points and auto-hinted advances, which libass reads from FreeType: `shape-libass`'s nine open cases (eight auto-hinted advances, and the CFF font without GPOS whose fallback mark positioning reads hinted extents) block qualifying `triergon-ass` as well; vertical text and `@` fonts; AAT tables and Apple `kern`, and OpenType `kern` formats 1, 2 and 3 and cross-stream kerning, which HarfBuzz applies (faces that need them are refused); `stch` stretching; variable instances other than the default; runtime and memory; Linux runs |
| Rasterising | libass's outline and bitmap stages: conversion from font outlines, rotation by 90°, affine and perspective transforms, border stroking, the 16×16 tile rasteriser, the glyph transform and its quantisation against a run with borders and opaque boxes, the karaoke leftmost x, `\blur` and `\be`, composition of fill, border and shadow, the additive and multiplying blends, shadow shifts and rectangle clipping with the karaoke split; storage charged to a budget, work to a `WorkBudget`, bitmaps bounded by libass's own size limit | Identical to libass's plain C bitmap engine (what libass runs on 32-bit ARMv7, pinned on every host), with glyphs loaded by `triergon-font` as libass loads them with hinting off. Gating: `raster-glyphs` 12/12 pass (576 operations over 8 glyphs of a file of each outline format and an authored font at 13.33 and 256 pixels) and `raster-outlines` 2/2 pass (1,019 operations on 200 random outlines), both with transforms of moderate reach, and `raster-bitmaps` 2/2 pass (560 operations on 80 random bitmap sets, a tenth of their blurs and `\be` passes beyond libass's clamps, among them radii it still blurs); on demand, `raster-sweep` 303/303 pass (72,445 operations: every corpus file at three heights with 30 glyphs each, 4,500 random outlines, 3,000 bitmap sets, with transforms as far as libass draws them). Platforms: macOS AArch64, Linux x86_64 and Linux AArch64 all four pass (12/12, 2/2, 2/2, 303/303); [32-bit ARMv7](#32-bit-armv7) passes the three gating features (12/12, 2/2, 2/2) and `raster-sweep` 301/303: on `outlines-71` and `outlines-83` libass silently draws nothing when its composite allocation fails; the crate returns `Allocation` (open cases on ARMv7, compared and passing elsewhere). `raster-simd`, the required cases against the host's SIMD engine, records where it departs: NEON on macOS and Linux AArch64 14/16 pass (4 of the 80 blur cases depart, all with `\be` passes); AVX2 on Linux x86_64 0/16 (AVX2: every bitmap differs in row padding; visible-pixel comparison needs its own fold) | not measured | not measured | underline and strike-out placement (needs the source outline's orientation as FreeType computes it); the overlap and allocation logic of vector clips at image-list level, which belongs to the renderer; libass's large-tile build and its SIMD engines (NEON blurs `\be` over whole 16-byte rows into the padding; AVX2 pads rows to 32 bytes); runtime and memory; `outlines-71` and `outlines-83` on ARMv7, open there: libass silently draws nothing when its composite allocation fails, the crate returns `Allocation`; a visible-pixel fold for `raster-simd` |
| ASS parsing | Whole ASS and SSA scripts and Matroska packet streams as libass reads them: headers, styles, events, style overrides, embedded fonts; codec private, packets with ReadOrder deduplication, flushing and pruning; typed override tags with libass's argument rules; every glyph's state at a time (animation, movement, fades, clips, karaoke tags, transition effects); karaoke syllables from laid-out positions; the drawing language into `triergon-raster` outlines; storage charged to a budget, work to a `WorkBudget` | Identical to libass on 54 authored fixtures, on macOS AArch64 and, with each target's own libass build, on aarch64-unknown-linux-gnu and armv7-unknown-linux-gnueabihf: `ass-parse-scripts` 48/48 pass (every track, style and event field, 87 styles, 222 events, embedded fonts, one script without a track), `ass-parse-chunks` 4/4 pass (10 track states, ReadOrder deduplication by bitmap and by scan, flushes, prunes, fonts in a codec private and in more data), `ass-parse-events` 21/21 pass (695 event walks, 6,976 glyph states and karaoke results, 48 drawing and clip outlines, 3 of them rejected as libass rejects them). `ass-parse-host` holds the 15 operations whose libass result differs by target, each pinned per target: on armv7-unknown-linux-gnueabihf 13/15 pass with 2 open, on aarch64-unknown-linux-gnu 11/15 pass with 4 open; elsewhere all 15 are reported. The reference flags every conversion from `double` to `int` out of range or for NaN (drawing coordinates, `\pbo`, an animated `\clip` or `\iclip`, `\fade`'s alpha, a `Banner` or `Scroll` delay) and a drawing coordinate on `INT_MIN`, and a flagged case fails any other feature. 2,728 mutated fixtures (`fuzz --contract`, 30 rounds of each feature, with the reference for all but `ass-parse-host`) gave identical work in both workers | not measured | not measured | `long`-width behaviour follows each target's libass, and out-of-range drawing coordinates are rejected as libass does on 32-bit ARM with GCC; every other out-of-range conversion from `double` to `int` saturates as there. Open as deliberate divergences: a coordinate landing exactly on `INT_MIN`, which libass built with GCC accepts and the crate rejects (`drawings-int-min-x` and `drawings-int-min-y`, on both targets), and `drawings-infinite` and `drawings-int-min` on aarch64-unknown-linux-gnu, whose coordinates libass converts with a 64-bit `long` there; the font description a glyph asks for (`GlyphState::font`) and karaoke run starts from style runs, shaping and wrapping are compared through the renderer (`ass-render`), not here; selective style overrides, track features, `ass_step_sub`; runtime and memory |
| ASS rendering | `Renderer`: the events shown at a time to positioned alpha images as libass's `ass_render_frame` draws them with hinting off and complex shaping: font selection and fallback over attached, embedded and default fonts, shaping and bidi, wrapping, alignment, positioning, scrolling, collisions kept across frames, change detection, glyph and border bitmaps, blur and shadows, clips, karaoke and the image list; storage charged to a budget, and all the work of a frame (evaluation, font selection, shaping, outlines, layout, drawing and collisions) to one per-frame bound, `Settings::work` | Identical to libass on macOS AArch64, configured as a player embeds it (attached fonts, a default family and font file, no system font provider, no hinting, storage smaller than the frame): `ass-render` 54/56 pass over 25 authored scripts and 3 Matroska packet streams (ReadOrder deduplication, flushes, prunes, an embedded font) at 1920×1080 over 1280×720 storage and 1280×720 over anamorphic 720×576 storage, every 250 ms; every image's kind, position, size, colour and alpha bytes, and every frame's change value as `ass_detect_change` reports it (606 frames, 588 of them in passing cases: 292 unchanged, 6 moved, 290 new content, including frames after a flush and after pruning every event, and opaque boxes around different glyphs that draw nothing); the box behind a `BorderStyle` 4 event is compared without its type, which libass leaves uninitialised, with libass's cache limits at 1000 glyphs and 24 MiB | not measured | not measured | vertical metrics (`vhea`/`vmtx`, linear vertical advance) for `@` fonts (2 open cases); settings other than frame and storage size are unverified; libass's simple shaping and hinting modes, selective overrides and track features; qualifying also waits on the shaper's nine open `shape-libass` cases (auto-hinted default-load advances, hinted extents for fallback mark positioning); no bitmap/composite cache; runtime not measured |
| Capture | RGBA to YUV 4:2:0, MPEG-1 video with budgeted storage, MPEG-TS mux | 15/15 quality cases against FFmpeg 9.0.2 | 1.1210 (15/15 pass) | heap ≤0.548, total RSS ≤0.978 (15/15) | — |

## Measured revisions

Each row names the source revision a result above was measured on and, where the raw
samples are archived, their SHA-256. Update the row in the same change as the result.

The Matroska and startup and seek rows use the [gate build](quality.md#measuring-performance)
at `0e28298d038bea072629b3bad6389dcc806b4861` on a c8g.large pinned to CPU 1.
The HLS row uses the gate build at `5ec7c1f8a7816b1427631f22a8ca77ba74abeeee`
on a c8g.large pinned to CPU 1. It covers all 71 cases in `corpus.json`, with
100 timing pairs of 50 operations and 30 memory pairs. Every video unit passes through
`contiguous_feed` inside the measured operation. Codec and decode-session runtime at
`add6b27` is one unpinned gate-build pass of 30 pairs on a c8g.2xlarge. Capture at
`ceb5076` reports the higher of two unpinned gate-build passes on the same instance type.
The audio codecs in files, Ogg and audio elementary-stream rows use the gate build at
`ccff5b5734c113c1d88160b84825157508c701c0` on a c8g.2xlarge, unpinned, against the `decode` profile:
30 timing pairs per workload of `harness/codecs/file-workloads.json`, each demuxing,
decoding and mixing to stereo s16 on both sides (`cargo xtask codecs time`), 30 heap and
30 RSS pairs under the native observer (`cargo xtask codecs memory`), and the reader lane
against the profile's `ffprobe`. The Ogg and audio elementary-stream rows rerun the reader
lane with both Ogg views at `ccd3090572433e79d8ab65a377fcd9896d2839d9` on the same instance type.
The remaining rows retain their earlier measurements.
The packet-framing row uses the gate build at
`04512a07fbb7efdf0d6d7173db08ec1267f2fae3` on a c8g.large pinned to CPU 1,
with 30 timing pairs of 50 complete operations and 30 memory pairs for all 125 cases.
All memory observations are valid; lower total RSS remains unachieved on every case.

The progressive TS188 row uses the gate build at
`5ec7c1f8a7816b1427631f22a8ca77ba74abeeee`, with 30 timing pairs of 50 complete
sessions on a c8g.large pinned to CPU 1 and 30 memory pairs. All three cases match
the reference and pass runtime, peak heap and total RSS. All five functional inputs
and ten outcomes pass. The same source passes every selected gate on all 74
progressive and HLS cases; full module and FATE coverage remain pending.

The Matroska audio clock and playback profile rows use the gate build at
`b3579f729494a37a4dbebe03c475f7b38484046e` on a c8g.2xlarge pinned to CPU 1, with 30 timing pairs and
30 memory pairs under the kernel RSS proof. Cases whose parity fails are not timed or
measured. The Matroska audio clock, TS program and raw Annex B rows are graded reports; the
MP4 forward row comes from its complete timing and memory reports.

Above the ceiling: Matroska audio clocks `laced-fractional-cadence` 1.4510,
`laced-unknown-duration` 1.4089, `two-laced-before-first-known-pts` 1.3968,
`damaged-lace-before-valid-anchor` 1.3878; MP4 forward `regular-encoded-avc-aac-native-seek`
1.6427, `progressive-ten-sequential-batches` 1.5011; TS program
`m2ts-entry-hevc-aac-44100-01-video` 1.4695, `entry-hevc-aac-44100-00` 1.3236.

Memory at or above the reference: Matroska audio clocks heap on `no-absolute-anchor` 1.822
and `dolby-bad-first` 1.321, total RSS on all 10 (1.038 to 1.043); MP4 forward heap on
`regular-encoded-avc-aac-native-seek` 3.382, total RSS on that case (1.025),
`regular-pending-terminal`, `regular-unknown-clock-terminal`, the four `regular-f-*` cases and
the twelve `progressive-pending-*`, `progressive-f-*`, `progressive-unknown-clock-terminal`
and `progressive-first-source-clock-unknown` cases (1.000 to 1.004).

The MPEG-TS row uses the gate build at `090c856df65f9247d24ea9f1c927e35cf5d528e8` on a
c8g.2xlarge pinned to CPU 1, with 30 timing pairs of 50 operations and 30 memory pairs
under the kernel RSS proof. The 31 open cases are not timed or measured, and
`cargo xtask grade` needs a timing for every case, so the row comes from the complete
timing and memory reports. Only `pes-header-every-byte` is above the runtime target
(1.0029).

ISO BMFF and PGS, plus codec and decode-session memory, await gate-build measurement with
repository runners.

ISO BMFF open and index measurements use `cargo xtask isobmff-bench`, with
`--subset52` selecting the 52-operation set. Packet framing uses `cargo xtask framing`.

The ISO BMFF row uses the gate build at `f99d893196908d562c897bccb20c6a972cd25d11`
on a c8g.large pinned to CPU 1, 30 pairs. Runtime comes from the timing collection and
memory from a separate collection under the kernel RSS bound; both are in the raw
samples. `cargo xtask grade` cannot join them, because FFmpeg's log lines carry heap
addresses that differ between runs.

The MP4 range playback row uses the gate build at
`f58e444b72f94bb3962990dd515f28be1aaaa574` on a c8g.large pinned to CPU 1.
All four original cases and 845 packets match, with 30 timing pairs of 50 complete
operations and 30 memory pairs. Every case passes runtime, peak heap and total RSS;
full module and FATE coverage remain pending. The earlier compiler-override
comparison remains archived separately.

The rasterising rows on Linux compare the gate build at
`a7b6635e20cc2dc9d4592a874ba0eecab24f2870` with the libass reference: the four
features of `cargo xtask raster compare --all` and the SIMD diagnostic, with the font
and shaping comparisons and the crate's tests, on a c7i.2xlarge (x86_64) and a
c7g.2xlarge (AArch64), and the four features and the tests in the ARMv7 user space on
a c6g.2xlarge. The 64-bit raw samples also hold the instruction counts and paired
timing of the reference built with and without zeroed automatic variables.

| Area | Revision | Raw samples SHA-256 |
|---|---|---|
| Matroska / WebM | `0e28298d038bea072629b3bad6389dcc806b4861` | `a20dcbbe804bfd21ee631cc42f61466d2400d5115abb8ea97dd243ef733ba74f` |
| Matroska startup and seek | `0e28298d038bea072629b3bad6389dcc806b4861` | `a20dcbbe804bfd21ee631cc42f61466d2400d5115abb8ea97dd243ef733ba74f` |
| ISO BMFF / MP4 | `f958c0b9624a8b7c7bd91b7d2a0c30299ae1dbed` | `fd718807e0ec1e6287bcb690226bf7056b79985796e74eba54f43544a034bc2c` |
| ISO BMFF comparison | `0a033ec3406aebd09d191298a546659e02c6b76c` | `f56d3fa78b8cc70222f58c7cfa9a17331b0924a9070c7ff6ecc6dc1f213561bc` |
| ISO BMFF open and index runtime and memory | `f99d893196908d562c897bccb20c6a972cd25d11` | `806f2f996b19370bb23350ae4a166f3b7688b3e0e499352d77c3b2610c90aa7b` |
| MPEG-TS progressive playback | `5ec7c1f8a7816b1427631f22a8ca77ba74abeeee` | `fb8a3a3b1f7f3d687254a3f4c1849dede7a78bc3c62dbc7905b09a45f675709d` |
| HLS segments | `5ec7c1f8a7816b1427631f22a8ca77ba74abeeee` | `fb8a3a3b1f7f3d687254a3f4c1849dede7a78bc3c62dbc7905b09a45f675709d` |
| Packet framing | `04512a07fbb7efdf0d6d7173db08ec1267f2fae3` | `677ce4b5aef58efd8523868cd2318d9cfde07868d2b7451d0017f013dfb3130b` |
| MP4 range playback | `f58e444b72f94bb3962990dd515f28be1aaaa574` | `bb59fb61a667c1010a0a8e46604ca9b10d8cab6a3f99e05c2b68170c18d82755` |
| MP4 range playback, earlier comparison | `90ecdb3f85f92402df9d7ecbec17f023535a5bf5` | `78e5d64743203173b7dfd8beed2dc4ab750d137e6fe3b00df321536b15057f05` |
| Codecs and decode sessions, runtime | `add6b272cb622bae0d921dc3fbbf0812958c2023` | `ab2a174847322658159e93b2cd9bb994cdd9f4f9438926191518897aff28d929` |
| Audio codecs in files | `ccff5b5734c113c1d88160b84825157508c701c0` | `0ddcc517728bb39e2d74cd2e3bf4c00071a6f3815ad3409d5215d216af9a0a53` |
| Ogg, audio elementary streams | `ccd3090572433e79d8ab65a377fcd9896d2839d9` | `8123e0f020a9c4ccedd83aea3e665d4486d55e26612e8bc29626e9a3026e2cd1` |
| Capture, timing and memory | `ceb5076659e00ca9a2d3f3cb55ee4513d6d0710f` | `b75c8d6e6d64d294a385ec25b176e4ffb8f1a0d3c11b3ca956b3d352ac44c3b4` |
| Matroska audio clocks and playback profiles | `b3579f729494a37a4dbebe03c475f7b38484046e` | `907bb7a35d70d09673ccbf0904a8b1ab85890a632454b8adf291d73a50672136` |
| MPEG-TS | `090c856df65f9247d24ea9f1c927e35cf5d528e8` | `6f5c1b65540e9bbe2dc79a1f8c1dc41e684aba468cd8fdd082bf603d3bfb6ca1` |
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
| MPEG-TS | 66/97 compat-view operations (`mpegts-pes`); the same 31 open cases as on AArch64 | 0.9298 (66/66 pass) | heap ≤0.101 (66/66 lower); total RSS ceiling ≤1.885, observed peaks ≤0.986 |
| Playback: MP4 forward | 37/38 packet folds (`mp4-forward`); the same open case as on AArch64 | 2.4583 (27/37 pass) | heap ≤2.949 (36/37 lower); total RSS ceiling ≤1.986, observed peaks ≤1.243 |
| Playback: TS program | 35/35 packet folds (`ts-program`) | 1.2811 (33/35 pass) | heap ≤0.141 (35/35 lower); total RSS ceiling ≤1.858, observed peaks ≤1.088 |
| Playback: raw Annex B | 6/6 packet folds (`raw-annex-b`) | 0.6434 (6/6 pass) | heap ≤0.126 (6/6 lower); total RSS ceiling ≤1.833, observed peaks ≤0.984 |
| Rasterising | `raster-glyphs` 12/12, `raster-outlines` 2/2 and `raster-bitmaps` 2/2 pass; `raster-sweep` 301/303 (below); the crate's tests pass | not measured | not measured |

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

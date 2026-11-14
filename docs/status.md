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
Every runtime and memory figure below was measured with workers and references linked as
their toolchains link them, before the [link policy](quality.md#measuring-performance)
that links the runtime support libraries of both sides from their static archives. The policy
changes every worker and reference binary, so each row cites its earlier measurement
until it is measured again under the policy.

| Area | Implemented | Verified against the reference | Runtime | Memory | Open |
|---|---|---|---|---|---|
| Core | Charged storage, exact time, input windows, work budgets, neutral container model with one codec identity | 51,235 exact-arithmetic cases against an independent oracle | via dependents | via dependents | fuzzing |
| Matroska / WebM | Incremental reader: lacing, header removal, bounded zlib, Cues and preroll, colour and static HDR, chapters, tags, attachments, side data | 116 workloads in six feature groups, including an external HDR sample, and four host FATE demux comparisons | 1.1423 (116/116 pass) | heap ≤0.6263, total RSS ≤0.9831 (116/116 lower); on [32-bit ARMv7](#32-bit-armv7) the total RSS gate fails, above the reference in 100 of 116 cases | full EBML/Matroska scope |
| Matroska startup and seek | Startup and seek with lazy scan replay | 44 cases, 1,344 packets | 1.1488 (44/44 pass) | heap ≤0.4038, total RSS ≤0.9202 (44/44 lower); on [32-bit ARMv7](#32-bit-armv7) the total RSS gate fails, above the reference in 27 of 44 cases | FATE mapping |
| Matroska audio clocks | AAC, AC-3, E-AC-3 startup and forward clocks | 10/10 packet folds (`matroska-audio-clocks`) | 1.1408 (10/10 pass) | heap ≤0.8103, total RSS ≤0.9875 (10/10 lower); on [32-bit ARMv7](#32-bit-armv7) the total RSS gate fails, above the reference in 10 of 10 cases | audio seek and FATE coverage |
| ISO BMFF / MP4 | Regular and fragmented files, edit lists, seek and range plans, neutral `ContainerReader` over seekable sources, QuickTime compatibility, bounded AAC configuration and regular table recovery, timing, sync and terminal media prefixes; AVC, HEVC, AAC, AC-3, E-AC-3, DTS core, AV1, VP9 and Dolby Vision sample entry identification | 255 inputs, 1,020 open and index comparisons (972 matching outputs and 48 matching refusal outcomes with compatibility policies) | 0.8854 (972/972 pass) | heap ≤0.4132, total RSS ≤0.7260 (972/972) | seek and range-plan timing; neutral reader not yet compared with FFmpeg |
| MP4 range playback | Regular AVC/AAC and AVC/Opus startup and backward seek, including regular AVC with an initial non-sync prefix, fragmented AVC/AAC with open-ended edits, timed text and DVD subpictures including a leading empty subpicture edit, dedicated HEVC and AV1 sample entries, source-declared Opus seek preroll, QuickTime AAC with fixed and program configurations, including regular version-2 descriptions and fragmented version-1 descriptions, and media playback in the presence of RTP hint tracks | 51 matching open and backward-seek comparisons (`mp4-range`), 7,190 packets on AArch64 Linux | 1.1523 (51/51 pass) | heap ≤0.9956, total RSS ≤0.9682 (51/51 lower) | Additional range playback profiles |
| AAC in MOV | Raw AAC-LC access units with 1024 samples per plane and all thirteen indexed rates. Supports fixed configurations 1–7, 11, 12 and 14 and out-of-band programs assigning up to eight distinct speaker positions. | The [configured decoder comparison](../harness/features/aac-access-units/contract.json) covers 34 clips, 3,544 access units and 15,575,040 PCM samples. Two fixed-layout QuickTime clips compare the surround pair in the same plane positions with side or back labels. Every other clip compares exact speaker order. | 0.8262 (34/34 pass) | heap ≤0.8066, total RSS ≤0.4586 (34/34 lower) | 960-sample frames and SBR in raw access units |
| Ogg | Pages with CRC and resync, multiplexed and chained streams, Vorbis, Opus and FLAC mappings with canonical configurations, granule timing, pre-skip and end trims, bisection seeking | Packets against the pinned `ffprobe` (`cargo xtask codecs readers`) on 21 FATE Vorbis files and 13 generated Opus, Vorbis and FLAC files (multiplexed, chained, damaged, truncated), in two views: the FFmpeg-compatible `compat::clock` 31/34 pass, 3 differ only by reviewed FFmpeg behaviours; the reader's own timing 7/34 pass, 25 differ only where `compat::clock` reproduces FFmpeg, 2 by reviewed behaviours ([reader rules](../harness/codecs/reader-rules.json), [frozen differences](../harness/codecs/reader-differences.json)) | within the codec workloads | within the codec workloads | `b_vorbis.ogg`, `chain_vorbis.ogg`: a negative last duration FFmpeg prints modulo 2^32; `crc_bad.opus`: the first packet after a page that fails its CRC, which FFmpeg times from its own page and `compat::clock` from the stream start; Theora and Speex untimed; chained seeks not compared with FFmpeg |
| Audio elementary streams | Native FLAC (with or without `fLaC`), MPEG audio with ID3v2/ID3v1/APE tags, Xing/Info/VBRI and LAME gapless trims, ADTS, AC-3/E-AC-3; one shared frame-stream engine | Packets as the readers emit them against the pinned `ffprobe` on 271 FLAC, MPEG audio, ADTS, AC-3 and E-AC-3 files (the IETF FLAC testbench, MP3 conformance, generated and crafted damaged streams): 253/271 pass, 16 differ only by reviewed FFmpeg behaviours, 2 refused by both (testbench faulty `06 - missing streaminfo metadata block`, `07 - other metadata blocks preceding streaminfo metadata block`) | within the codec workloads | within the codec workloads | FLAC `flac_s20_rate768k`, `06 - samplerate 768kHz` (no durations at 768 kHz), `01 - changing samplerate`, `27 - old format variable blocksize file created with Flake 0.11`, `11 - file starting with unparsable data`, `11 - incorrect metadata block length`, `08 - blocksize 65536`, `09 - blocksize 1`; crafted `ac3_tags`, `adts_mid_junk`, `flac_corrupt`, `flac_id3`, `flac_mid_junk`, `mp3_foreign_frame`, `mp3_mid_junk_falsesync`; MP3 `he_free` (free format, refused by FFmpeg); E-AC-3 dependent substreams on real files; ADTS and AC-3 seek cost |
| MPEG-TS | PSI, continuity, PES assembly and ordered TS188/M2TS192 entities with control-only and bounded fault completion, with format-owned track classification | 97/97 PES compatibility-view comparisons across all 272 requests | 0.9413 (97/97 pass) | heap ≤0.065330, total RSS ≤0.891584 (97/97 lower) | strict source clocks, full FATE mapping |
| MPEG-TS access units | `ContainerReader` and `AnyReader` for TS188 and M2TS192, with AVC, HEVC, ADTS AAC-LC, LATM in LOAS, signalled AC-3 and E-AC-3, DVB subtitle segments and ordered entities; `Reader` can frame selected PIDs while retaining all source events | 169/169 transport compatibility folds for all tracks in the chosen program, including LATM PES and opaque observations, with all 272 original request associations; explicit PID selection has separate reader tests | 1.1861 (169/169 pass) | heap ≤0.088210, total RSS ≤0.911429 (169/169 lower) | additional carriages, seeking and full FATE mapping |
| MPEG-TS program playback | Selected TS188 program and M2TS192 PID with packet payloads and clock projection | 35/35 packet folds | 1.0267 (35/35 pass) | heap ≤0.087743, total RSS ≤0.867077 (35/35 lower) | additional program and carriage profiles |
| MPEG-TS progressive playback | TS188 with AVC and AAC-LC, bounded sequential input and access units before EOF | 3 packet folds and all 5 functional inputs with 10 literal outcomes | 1.1256 (3/3 pass) | heap ≤0.015079, total RSS ≤0.724510 (3/3 lower) | broader transport and codec profiles |
| HLS segments | Independent TS188 segments with contiguous AVC or HEVC video and AAC, AC-3 or E-AC-3 audio | 71 packet comparisons, 71 measured workloads | 1.0336 (71/71 pass) | heap ≤0.020733, total RSS ≤0.808122 (71/71 lower) | other profiles |
| Packet framing | AVC/HEVC configuration and NAL framing, AV1, VP9, AC-3, E-AC-3, DTS core | 125 AVC/HEVC reference comparisons; DTS core byte-exact on 37,125 projections and 1,172 FATE packets, recorded without a timing runner | 1.3846 (45/125 pass for AVC/HEVC) | heap ≤0.985 (125/125 lower), total RSS ≤1.048 (0/125 lower); on [32-bit ARMv7](#32-bit-armv7) the total RSS gate fails, above the reference in 125 of 125 cases | runtime and RSS optimization; full AVC/HEVC/ADTS grammar |
| Subtitles, text | SubRip and WebVTT documents as FFmpeg's `srt` and `webvtt` demuxers read them, ASS packets as its `ass` decoder decodes them, with `sub_charenc` charsets, and mov_text and tx3g to ASS | 367/367 cases (`subtitle-text`: 82 captured mov_text cases, 13 authored mov_text sessions for the override tags written where a style record or the highlight starts or ends, the samples of the 79 tx3g tests, 80 charset cases, 17 ASS packets, 96 SubRip and WebVTT documents) on Linux AArch64 and [32-bit ARMv7](#32-bit-armv7); `subtitle-text-host` pins per target the 23 SubRip and WebVTT documents whose time fields leave the 32-bit range: 23/23 pass on ARMv7, 0/23 on AArch64 (open) | 0.8573 (354/354 pass; worst `tx3g-known_box_trailing_bytes_and_short_bodies_are_errors`, median of the cases 0.383); the 13 `mov-text-tags` cases at `96f660ba`: 0.9860 (13/13 pass; worst `mov-text-tags-ranges`, median of the cases 0.797); instructions at `96f660ba` against main at `339ec261`: 0.916 of main's in sum, `subtitle-text-host` 1.000, no case above main beyond the run spread but `mov-text-tags-nul-transitions` (1.031), whose output is longer | heap ≤0.336, total RSS ≤0.911 (354/354 lower); the 13 `mov-text-tags` cases at `96f660ba`: heap ≤0.336, total RSS ≤0.860 (13/13 lower, kernel-bound); on [32-bit ARMv7](#32-bit-armv7) the total RSS gate fails, above the reference in 1 of 377 cases | on AArch64, `subtitle-text-host`'s 23 cases, a deliberate divergence: FFmpeg for AArch64 reads a SubRip time field beyond the 32-bit range as its value, held to the 64-bit signed range, modulo 2^32, and a WebVTT field whose digits exceed 2^32 − 1 as those digits, held to 2^64 − 1, modulo 2^32, where on 32-bit ARM both saturate; the reader follows 32-bit ARM, so these cases are not timed or measured on AArch64; FATE coverage |
| Subtitles, PGS | Presentation, palette and object segments to subtitle rectangles, with FFmpeg's recovery from damaged and incomplete segments | 319/319 cases (`subtitle-pgs`: TS, SUP and Matroska), on Linux AArch64 and [32-bit ARMv7](#32-bit-armv7), the 2 cases added at `c79ae2da` on A32 there | 0.8780 (317/317 pass); the 2 cases `rx-canvas-bounds-ts` and `rx-ods-first-fragment-ts` at `c79ae2da`: 0.4199 (2/2 pass; worst `rx-canvas-bounds-ts`); instructions at `c79ae2da` against main at `339ec261`: 0.99983 of main's in sum, `subtitle-pgs-capsules` 0.99987, no case above main beyond the run spread | heap ≤0.966 (317/317 lower), total RSS ≤0.984 (317/317 lower); the 2 cases at `c79ae2da`: heap ≤0.108, total RSS ≤0.672 (2/2 lower, kernel-bound); on [32-bit ARMv7](#32-bit-armv7) the total RSS gate fails, above the reference in 1 of 317 cases | FATE coverage |
| Subtitles, DVB | Display definition, page, region, CLUT and object segments to bitmap rectangles, including `compute_clut` (−2 keeps a region's first automatic palette until a new epoch) | 241/241 cases (`subtitle-dvb`: TS and Matroska), on Linux AArch64 and [32-bit ARMv7](#32-bit-armv7) | 0.8593 (229/229 pass); the 10 cases `automatic-*` and `clut-conversion-grid-ts`, `clut-flags-ranges-ts` at `aebbe582`: 0.7077 (10/10 pass; worst `clut-conversion-grid-ts`); the 2 `automatic-once` cases added at `86e5bbe8` are not timed; instructions at `74155089` against main at `2de40661`: 0.7523 of main's in sum, median of the cases 0.9990, no case above main beyond the run spread | heap ≤0.991, total RSS ≤0.811 (229/229 lower); the 10 cases at `aebbe582`: heap ≤0.459, total RSS ≤0.661 (10/10 lower, kernel-bound) | FATE coverage |
| Subtitles, DVD | SPU assembly and control sequences, run-length raster, palettes from VobSub `.idx`, Matroska and MP4 private data, DVD program streams, HD SPU | 233 cases: `subtitle-dvd` 228/228 on Linux AArch64 and [32-bit ARMv7](#32-bit-armv7), the 4 cases added at `c79ae2da` on A32 there, and `subtitle-dvd-host`'s 5, pinned per target, 5/5 on ARMv7 and 1/5 on AArch64 | 0.8595 (225/225 pass); the 4 cases `vobsub-idx-size-largest`, `-past-largest`, `-widest` and `-past-widest` at `c79ae2da`: 0.1713 (4/4 pass; worst `vobsub-idx-size-widest`); instructions at `c79ae2da` against main at `339ec261`: 0.99995 of main's in sum, `subtitle-dvd-host` 0.99991, no case above main beyond the run spread | heap ≤0.959, total RSS ≤0.869 (225/225 lower); the 4 cases at `c79ae2da`: heap ≤0.0029, total RSS ≤0.751 (4/4 lower, kernel-bound) | FATE coverage; on AArch64, `subtitle-dvd-host`'s 4 open cases, target-dependent by design: FFmpeg there reads numbers of a VobSub configuration and of the `palette` option beyond 32 bits with 64, and the decoder follows FFmpeg on 32-bit ARM |
| Closed captions | CEA-608 captions from A/53 and SCTE 20 user data in MPEG-2 video, A/53 SEI in H.264 and HEVC, and SCC files, to ASS | 201/205 cases (`subtitle-cc`), on Linux AArch64 and [32-bit ARMv7](#32-bit-armv7); the other 4 are the open `hevc-eos-ts` cases | 0.5724 (201/201 pass); instructions at `4d81cde0` against main at `339ec261`: 0.9996 of main's in sum, median of the cases 0.9997, no case above main beyond the run spread (the four `h264-dtvcc-mkv` cases +0.014% to +0.016%) | heap ≤0.123, total RSS ≤0.967 (201/201 lower), and on [32-bit ARMv7](#32-bit-armv7) ≤0.798 (201/201 lower) | FATE coverage; HEVC picture output at a new sequence (open case `hevc-eos-ts`: no DPB output model, so `no_output_of_prior_pics` after an end of sequence is not applied) |
| Teletext | DVB teletext pages to text and ASS, as FFmpeg's libzvbi 0.2.46 decoder emits them | 72/72 cases (`subtitle-teletext`), on Linux AArch64 and [32-bit ARMv7](#32-bit-armv7), the 2 cases added at `c79ae2da` on A32 there | 0.4388 (70/70 pass); the 2 cases `teletext-chop-text` and `teletext-chop-ass` at `c79ae2da`: 0.4192 (2/2 pass; worst `teletext-chop-ass`); instructions at `c79ae2da` against main at `339ec261`: 0.99268 of main's in sum, no case above main beyond the run spread | heap ≤0.622, total RSS ≤0.662 (70/70 lower); the 2 cases at `c79ae2da`: heap ≤0.129, total RSS ≤0.654 (2/2 lower, kernel-bound) | FATE coverage |
| Playback | Matroska, MP4, TS and raw AVC/HEVC feeds; seeking; end-of-file continuation | MP4 terminal-media matrix against recorded FFprobe 9.0.2 output (evidence set `playback-terminal-matrix-ffprobe`): 39 inputs in 13 source geometries (12 nonseekable for every input, 1 seekable for the 31 original inputs), 499 rows, each run in debug and in release. `Mp4Open::new_recovery` 499/499 pass: 497 identical packet lists, and 2 seekable reads of a zero-size fragment sample that the opener refuses in the header, where FFprobe also refuses. `Mp4Open::new` 446/499 pass. Packet folds: MP4 forward 46/46 (`mp4-forward`), 673 packets, TS program 35/35 (`ts-program`), raw Annex B 6/6 (`raw-annex-b`) | MP4 forward 0.9921 (46/46 pass), TS program 1.1621 (35/35 pass), raw Annex B 1.0810 (6/6 pass) | MP4 forward heap ≤0.6710, total RSS ≤0.9570 (46/46 lower); TS program heap ≤0.1008, total RSS ≤0.9090 (35/35 lower); raw Annex B heap ≤0.127, total RSS ≤0.901 (6/6 lower); on [32-bit ARMv7](#32-bit-armv7) the total RSS gate fails for MP4 forward, above the reference in 39 of 39 cases | other profiles; legacy Matroska `A_AAC/MPEG4/*` codec IDs are not read as AAC |
| Codecs | H.264 8-bit 4:2:0; HEVC Main and Main10; AAC LC, HE, HEv2 (Parametric Stereo from open publications and probes); AC-3 and E-AC-3; DTS core with XCh and X96; storage charged to a budget | 417/417 comparable conformance and FATE vectors | 1.0551 (17/17 pass); the three HE-AAC workloads at `700c2134`, on a c6g.4xlarge: 1.0300 (3/3 pass; worst `aac-he-stereo`) | heap 0.92, RSS 0.82; at `700c2134`, the HE-AAC workloads heap ≤0.941, total RSS ≤0.899, and AAC-LC heap ≤0.605, total RSS ≤0.887 (5/5 lower) | error concealment, unverified tool paths; three open AAC Parametric Stereo cases (`harness/codecs/aac/ps-open-manifest.json`); AAC charges in-band reconfiguration when it appears, and a refused access unit leaves the decoder unchanged unless an earlier raw data block or Main-prediction element of it was decoded |
| Audio codecs in files | FLAC (RFC 9639, 4 to 32 bits, 1 to 8 channels); Opus (RFC 6716 and 8251: SILK, CELT, hybrid, families 0, 1 and 255); Vorbis I; MPEG-1, 2 and 2.5 audio Layer I, II and III | 315/321 pass through the container readers (FATE Vorbis, Opus and MP3 conformance, the IETF FLAC testbench, generated streams): FLAC bit-exact, the others within the [numeric rules](../harness/codecs/numeric-rules.json) | 1.0834 (10/10 pass) | heap ≤0.904, total RSS ≤0.966 (10/10) | FLAC `01 - changing samplerate`, `02 - increasing number of channels`, `03 - decreasing number of channels` and MP3 `he_mode` change format mid-stream, which the CLI converts; FLAC `flac_3ch_odd_mask`, whose layout FFmpeg refuses on its stereo output; MP3 `he_free` (free format: FFmpeg finds no two consecutive frames, and the decoder refuses free-format frames), refused by both; runtime toward the target for Opus music (1.0301) and 5.1 (1.0834), Vorbis 5.1 (0.9356) and stereo (0.8423) and Opus speech (0.8719); Opus families 2 and 3 |
| Decode sessions | Bounded queues, clock-gated output, flush and seek, RGBA letterbox, stereo mix; container track decoding with trims and start delays | with the codecs | 1.0551 (17/17 pass) | with the codecs | — |
| Fonts | sfnt and collections; `head`, `hhea`, `maxp`, `hmtx`, `vhea`, `vmtx`, `OS/2`, `name`; `cmap` formats 0, 2, 4, 6, 8, 10, 12, 13, and the glyph-name and encoding charmaps FreeType adds to CFF faces, in its charmap order; `glyf`/`loca`; CFF and CFF2 at the default instance; `kern` format 0; loading at a size as libass does with hinting off (unhinted outlines, synthetic bold and oblique, size metrics, kerning, decoration bands, default-load advances where hinting cannot move them, and the vertical advances libass draws and shapes the glyphs of `@` fonts with); FreeType's default load of CFF, CFF2 and CID-keyed glyphs, hinted by Adobe's hinter as FreeType's CFF driver hints them, and of TrueType glyphs without bytecode, through FreeType's auto-hinter with its Latin and dummy writing systems, with the grid-fitted metrics of each; face matching with libass's rules (compared through the renderer's `ass-render` feature) | The gate covers libass's default configuration (hinting off). All 70 faces of 53 corpus files (TrueType including Arabic, Hebrew, Indic and Thai fonts, name-keyed and CID-keyed CFF, CFF2, a 10-face collection, six authored fonts, among them a face for each way FreeType reads vertical metrics and a CFF face with each charmap FreeType adds; 836,345 glyphs) identical to FreeType: names, every charmap FreeType lists with its mappings, libass's text charmap, the kerning of every listed pair, advances and unscaled outlines (`font-outlines` 53/53); at 256 pixels, size metrics, kerning, every glyph's unhinted outline with its synthetic styles, default-load advances, and its linear and scaled vertical advances unhinted and of the default load (`font-hinting-none` 159/159); at that size, every glyph's default-load metrics and hinted outline, which libass's shaper reads for advances, extents and contour points (`font-hinting-default` 10/53 at `204d212285ace2400a36774544188b687b06eff6`: every CFF, CFF2 and CID-keyed face and the auto-hinted authored-cmap-formats and DejaVuMathTeXGyre; 43 instructed TrueType files open under the TrueType interpreter); Linux AArch64 and [32-bit ARMv7](#32-bit-armv7), each with its own FreeType build, and macOS AArch64 before the vertical cases | not measured | not measured | libass's hinting-enabled modes (NATIVE, LIGHT, NORMAL): their nine other sizes are measured, not gated (`font-hinting-modes` 900/918 before the fifth and sixth authored fonts: unhinted outlines 459/459, default-load advances 441/459), and the hinted outlines those modes draw are not compared; the auto-hinter's Indic writing system, for the hinted outlines of the glyphs it styles; the TrueType interpreter, so the default-load extents and contour points of instructed TrueType faces, which HarfBuzz reads to position marks in fonts without GPOS mark positioning, are open (`font-hinting-default`); tricky fonts and instruction-control programs; `cmap` format 14; runtime and memory |
| Shaping | OpenType layout (GDEF, every GSUB and GPOS lookup type, feature variations at the default instance, `kern` format 0 fallback, fallback mark positioning) with HarfBuzz's default, Arabic, Hebrew, Thai/Lao, Hangul, Indic, Khmer, Myanmar and Universal shapers for horizontal runs; every BCP 47 language mapped to OpenType language systems as HarfBuzz maps it; libass's FreeType-backed font functions over a sized face, with the rotated advances of `@` fonts, the empty extents of glyphs without points, and the default-load extents and contour points of CFF glyphs; bidi from UAX #9 with FriBidi's results, resolved and reordered as libass does; libass's simple shaping level; Unicode properties generated from pinned UCD files; planning, shaping and bidi bounded by work limits | Identical to HarfBuzz and FriBidi on Linux AArch64 and [32-bit ARMv7](#32-bit-armv7), and on macOS AArch64 before the charmap face joined the shaping corpus. Gating: `shape-libass` 201/201 at `204d212285ace2400a36774544188b687b06eff6`, 3,105 runs over 26 corpus files and 22 scripts shaped as libass does at 256 pixels with hinting off; `shape-bidi` 24/24; `shape-simple` 6/6; `shape-unicode` 1/1 (every code point); Unicode 14.0.0's `BidiTest.txt` (770,241 cases) and `BidiCharacterTest.txt` (91,707) all pass. Coverage at units per em: `shape-text` 200/201 and, on demand, `shape-text-all` 1,745/1,746 before the charmap face joined the shaping corpus (101,691 runs over every corpus face), one open case each (CFF extents as HarfBuzz's own interpreter reads them) | not measured | not measured | the hinted glyph extents and contour points of instructed TrueType glyphs, which libass reads from FreeType to place marks; vertical directions (libass shapes `@` fonts horizontally); AAT tables and Apple `kern`, and OpenType `kern` formats 1, 2 and 3 and cross-stream kerning, which HarfBuzz applies (faces that need them are refused); `stch` stretching; variable instances other than the default; runtime and memory |
| Rasterising | libass's outline and bitmap stages: conversion from font outlines, rotation by 90°, affine and perspective transforms, border stroking, the 16×16 tile rasteriser, the glyph transform and its quantisation against a run with borders and opaque boxes, the karaoke leftmost x, `\blur` and `\be`, composition of fill, border and shadow, the additive and multiplying blends, shadow shifts and rectangle clipping with the karaoke split; storage charged to a budget, work to a `WorkBudget`, bitmaps bounded by libass's own size limit | Identical to libass's plain C bitmap engine (what libass runs on 32-bit ARMv7, pinned on every host), with glyphs loaded by `triergon-font` as libass loads them with hinting off. Gating: `raster-glyphs` 12/12 pass (576 operations over 8 glyphs of a file of each outline format and an authored font at 13.33 and 256 pixels) and `raster-outlines` 2/2 pass (1,019 operations on 200 random outlines), both with transforms of moderate reach, and `raster-bitmaps` 2/2 pass (560 operations on 80 random bitmap sets, a tenth of their blurs and `\be` passes beyond libass's clamps, among them radii it still blurs); on demand, `raster-sweep` 303/303 pass (72,445 operations: every corpus file at three heights with 30 glyphs each, 4,500 random outlines, 3,000 bitmap sets, with transforms as far as libass draws them). Platforms: macOS AArch64, Linux x86_64 and Linux AArch64 all four pass (12/12, 2/2, 2/2, 303/303); [32-bit ARMv7](#32-bit-armv7) passes the three gating features (12/12, 2/2, 2/2) and `raster-sweep` 301/303: on `outlines-71` and `outlines-83` libass silently draws nothing when its composite allocation fails; the crate returns `Allocation` (open cases on ARMv7, compared and passing elsewhere). `raster-simd`, the required cases against the host's SIMD engine, records where it departs: NEON on macOS and Linux AArch64 14/16 pass (4 of the 80 blur cases depart, all with `\be` passes); AVX2 on Linux x86_64 0/16 (AVX2: every bitmap differs in row padding; visible-pixel comparison needs its own fold) | not measured | not measured | underline and strike-out placement (needs the source outline's orientation as FreeType computes it); the overlap and allocation logic of vector clips at image-list level, which belongs to the renderer; libass's large-tile build and its SIMD engines (NEON blurs `\be` over whole 16-byte rows into the padding; AVX2 pads rows to 32 bytes); runtime and memory; `outlines-71` and `outlines-83` on ARMv7, open there: libass silently draws nothing when its composite allocation fails, the crate returns `Allocation`; a visible-pixel fold for `raster-simd` |
| ASS parsing | Whole ASS and SSA scripts and Matroska packet streams as libass reads them: headers, styles, events, style overrides, embedded fonts; codec private, packets with ReadOrder deduplication, flushing and pruning; typed override tags with libass's argument rules; every glyph's state at a time (animation, movement, fades, clips, karaoke tags, transition effects); karaoke syllables from laid-out positions; the drawing language into `triergon-raster` outlines; storage charged to a budget, work to a `WorkBudget` | Identical to libass on 54 authored fixtures, on macOS AArch64 and, with each target's own libass build, on aarch64-unknown-linux-gnu and thumbv7neon-unknown-linux-gnueabihf: `ass-parse-scripts` 48/48 pass (every track, style and event field, 87 styles, 222 events, embedded fonts, one script without a track), `ass-parse-chunks` 4/4 pass (10 track states, ReadOrder deduplication by bitmap and by scan, flushes, prunes, fonts in a codec private and in more data), `ass-parse-events` 21/21 pass (695 event walks, 6,976 glyph states and karaoke results, 48 drawing and clip outlines, 3 of them rejected as libass rejects them). `ass-parse-host` holds the 15 operations whose libass result differs by target, each pinned per target: on thumbv7neon-unknown-linux-gnueabihf 13/15 pass with 2 open, on aarch64-unknown-linux-gnu 11/15 pass with 4 open; elsewhere all 15 are reported. The reference flags every conversion from `double` to `int` out of range or for NaN (drawing coordinates, `\pbo`, an animated `\clip` or `\iclip`, `\fade`'s alpha, a `Banner` or `Scroll` delay) and a drawing coordinate on `INT_MIN`, and a flagged case fails any other feature. 2,728 mutated fixtures (`fuzz --contract`, 30 rounds of each feature, with the reference for all but `ass-parse-host`) gave identical work in both workers | not measured | not measured | `long`-width behaviour follows each target's libass, and out-of-range drawing coordinates are rejected as libass does on 32-bit ARM with GCC; every other out-of-range conversion from `double` to `int` saturates as there. Open as deliberate divergences: a coordinate landing exactly on `INT_MIN`, which libass built with GCC accepts and the crate rejects (`drawings-int-min-x` and `drawings-int-min-y`, on both targets), and `drawings-infinite` and `drawings-int-min` on aarch64-unknown-linux-gnu, whose coordinates libass converts with a 64-bit `long` there; the font description a glyph asks for (`GlyphState::font`) and karaoke run starts from style runs, shaping and wrapping are compared through the renderer (`ass-render`), not here; selective style overrides, track features, `ass_step_sub`; runtime and memory |
| ASS rendering | `Renderer`: the events shown at a time to positioned alpha images as libass's `ass_render_frame` draws them with hinting off and complex shaping: font selection and fallback over attached, embedded and default fonts, shaping and bidi, wrapping, alignment, positioning, scrolling, collisions kept across frames, change detection, glyph and border bitmaps, blur and shadows, clips, karaoke and the image list; storage charged to a budget, and all the work of a frame (evaluation, font selection, shaping, outlines, layout, drawing and collisions) to one per-frame bound, `Settings::work` | Identical to libass on Linux AArch64 and [32-bit ARMv7](#32-bit-armv7), each with its own libass build, and on macOS AArch64 before the cases of `@` fonts, unregistered languages, glyphs without points, the charmap retry, two `@` font styles and vector clips without a contour or beyond libass's reach (macOS was not run with them), configured as a player embeds it (attached fonts, a default family and font file, no system font provider, no hinting, storage smaller than the frame): `ass-render` 78/78 pass over 36 authored scripts and 3 Matroska packet streams (ReadOrder deduplication, flushes, prunes, an embedded font) at 1920×1080 over 1280×720 storage and 1280×720 over anamorphic 720×576 storage, at each fixture's frame times; every image's kind, position, size, colour and alpha bytes, and every frame's change value as `ass_detect_change` reports it (736 frames: 358 unchanged, 12 moved, 366 new content, including frames after a flush and after pruning every event, opaque boxes around different glyphs that draw nothing, vector clips whose drawing has no contour, which empty their event's images, and one whose drawing libass cannot place, which it does not apply, frames whose caches overflow and are cut, C1 controls drawn through a fallback face's Macintosh charmap that the face keeps for later lookups, a style font size of 0, whose glyphs FreeType loads unscaled, `@` fonts drawn rotated and shaped with their vertical advances, a `Language` header HarfBuzz does not register, fallback mark positioning that measures a glyph without points, and a fallback face whose characters libass finds by trying its format 2 and 8 subtables, the Unicode charmap FreeType builds from its glyph names and its CFF encoding in FreeType's order, and two styles of one `@` font that select the same face, whose rotation decisions libass keeps apart); the box behind a `BorderStyle` 4 event, whose type libass leaves uninitialised, is told by its own buffer, also after a collision shift cuts rows off its top, and compared as a type of its own; colliding events whose boxes lie 2^31 or more rows apart, each within `int`'s range (`collision-far-margins.ass`), placed as glibc's merge sort orders them under libass's overflowing comparison, with the far event clipped away, whose 26.6 position leaves `int`'s range and which libass for AArch64 alone draws inside the frame; with libass's cache limits at 1000 glyphs and 24 MiB; `ass-render-host` pins per target the rendering whose libass result C leaves to the target (`border-x-overflow.ass`, a box width that doubles a huge border in wrapping `int`, and `collision-far-tops.ass`, colliding events whose boxes end up 2^31 or more rows apart, sorted as glibc's merge sort sorts them with libass's overflowing comparison): 4/4 pass on ARMv7 | 1.0658 at `5b3e03f0` (76/76 pass; worst `cache-keys-ass-720p`, median of the cases 0.813) | 76/76 lower than libass at `5b3e03f0`, worst peak heap 0.841 and worst total RSS 0.943, both `animation-ass-1080p` | on AArch64, `ass-render-host`'s 4 cases, a deliberate divergence: libass built with GCC for AArch64 converts an event box's top, height, left and width to `int` with vector instructions that keep the low 32 bits where 32-bit ARM saturates, and the renderer follows 32-bit ARM, so they are not timed or measured there; settings other than frame and storage size are unverified; libass's simple shaping and hinting modes, selective overrides and track features; qualifying also waits on the shaper's six open `shape-libass` cases (the auto-hinter's CJK extents); of 5,182 generated scripts and 540 fixture mutations rendered frame by frame against libass for ARMv7, 50 scripts and no mutation differ, each for a stated cause: the renderer stops at the default `Settings::work` bound (17), which on ARMv7 is about 9 s of rendering for one frame (a median 33 ns per unit); with the bound lifted, 34 of those frames take 7 to 21 s each there and 110 run out of the 32-bit address space, so the bound refuses only frames no player could show in time; it runs out of the 32-bit address space (19 allocations refused, 8 at the worker's budget) where libass for ARMv7 peaks at 2.6 to 4.1 GB; it refuses glyph extents of FreeType's hinted load (2); and in 4, and in earlier frames of 2 of the above, libass for ARMv7 alone departs from libass for AArch64 at 3.7 to 4.0 GB of its 4 GB address space, and the renderer matches libass for AArch64 (`random-2258` from frame 46). Against libass for AArch64, 115 scripts and 1 mutation differ: 79, the mutation among them, that are identical on ARMv7 and differ there through libass's 64-bit `long` narrowing and its vectorised conversion of event boxes, both deliberate divergences; 34 at the work bound, where libass peaks at 3.5 to 23 GB; the same 2 refusals of hinted extents; and `random-1428`, where libass is killed for memory and the renderer refuses at its budget |
| Capture | RGBA to YUV 4:2:0, MPEG-1 video with budgeted storage, MPEG-TS mux | 15/15 quality cases against FFmpeg 9.0.2 | 1.1210 (15/15 pass) | heap ≤0.548, total RSS ≤0.978 (15/15) | — |

## Measured revisions

Each row names the source revision a result above was measured on and, where the raw
samples are archived, their SHA-256. Update the row in the same change as the result.

The DVB rows' 10 cases added at `aebbe582` were measured on a c8g.xlarge pinned to CPU 1, with 30 timing pairs and 30 memory pairs under the kernel RSS proof, run as a slice of `subtitle-dvb`. `cargo xtask grade` does not join a slice with its feature's recorded runs, because the slice's contract differs from the registered feature's, so their per-case results come from each run's report. The instruction counts run both workers from paths of equal length.

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

The MPEG-TS PES, program playback, progressive playback and HLS rows use the
375-case gate build at `21c9eef2f08a11bb7bd1c457997095326f2bb42d` on a c8g.large pinned to CPU 1.
All 375 cases pass parity, runtime, peak heap and total RSS with 30 timing
pairs of 50 operations and 30 memory pairs under the kernel RSS proof.
All five progressive functional inputs pass their ten outcomes. Each HLS
video unit passes through `contiguous_feed` inside the measured operation.

The progressive report comparison uses the gate build at
`ec6afe8635509d7e9007fd3d3c20ce2bf1c25835` on a c8g.large pinned to CPU 1.
All three cases and ten functional outcomes pass with 30 timing pairs of 50
operations and 30 memory pairs. Grading compact and self-contained reports
gives the same results.

The MPEG-TS access-unit comparison uses the gate build at
`230d1314464d34a04f5f2614298e5b7009292f0c` on a c8g.xlarge pinned to CPU 1.
All 169 cases frame all tracks in the chosen program and pass parity, runtime,
peak heap and total RSS with 30 timing pairs of 50 operations and 30 memory
pairs under the kernel RSS proof.

The raw Annex B results and the earlier Matroska audio clock comparison use the gate
build at `b3579f729494a37a4dbebe03c475f7b38484046e` on a c8g.2xlarge pinned to CPU 1,
with 30 timing pairs and 30 memory pairs under the kernel RSS proof.

In the earlier audio clock comparison, `laced-fractional-cadence` reached 1.4510,
`laced-unknown-duration` 1.4089, `two-laced-before-first-known-pts` 1.3968 and
`damaged-lace-before-valid-anchor` 1.3878. Peak heap reached 1.822 on
`no-absolute-anchor` and 1.321 on `dolby-bad-first`. Total RSS ranged from 1.038 to
1.043 across all ten cases.

The subtitle rows on Linux AArch64 use the gate build at
`f263491102d307997d40d40826a6193d8468e87e` on a c8g.xlarge pinned to CPU 1, with 30 timing
pairs of 50 operations and 30 memory pairs under the kernel RSS proof, graded by `cargo xtask
grade` against `ffmpeg-subtitles-plain-native`. Its workers are byte-identical to those of
`8513398b85d27aa093b8212c202b25c32da859bf`: the commit between
changes only the memory runner, which since drops both workers' cached pages before every
run. The closed caption row uses the gate build at `9d048c58fb30c64777f7230eb67c7037a1e1bcf1`
on a c8g.xlarge in the same way, where the caption receiver reads `cc_data` as three-byte
constructs and resolves the field once per packet; on all 205 cases its workers' traces and
folds are byte-identical to those of `d37da04f`. On the eight `mpeg2-a53-limit` cases, which
carry large `cc_data`, it executes 0.66 to 0.68 times the reference's instructions on
AArch64, where `d37da04f` executed 1.58 to 1.64 times, and times at 0.5525 to 0.5724 on the
c8g.xlarge. On a c6g.4xlarge (Graviton2), natively on AArch64, the same eight cases time at
0.6272 to 0.6485 in 30 pairs, where `8513398b` gave them 1.3887 to 1.4481. The earlier rows were
measured at `956c42961f4a27d88f96d5800d59ff1eeb088018` (PGS, closed captions and teletext),
`03f3efd3a0fde0d558c68f5408c521d632802fb7` (PGS memory),
`531e2d04ba7386513ff7afe7b44f53b2a93ca9dc` (DVD) and
`0d70bebfc0799f65df14f170ed97e68c5a35daa7` (DVB). The text row uses the gate build at
`6126b759b9b983afe488c3c362c6e37c2d1425a2` on a c8g.xlarge, whose timing pairs are of 2,000
operations. The 8 cases added to `subtitle-dvd`, `subtitle-pgs` and `subtitle-teletext` use the
gate build at `c79ae2da8b8a0d004d37936100eedd7b46e0a589` on a c8g.xlarge pinned to CPU 1, with
30 timing pairs and 30 memory pairs under the kernel RSS proof, run as a slice of each
feature. `cargo xtask grade` does not join a slice with its feature's recorded runs, because
the slice's contract differs from the registered feature's, so their per-case results come
from each run's report. Their instruction counts run both workers from paths of equal length.
These measurements, and the first instruction counts of the four features, are at `c79ae2da`.
The review commit `821900952ba46678d07300ea7e4bb04ec517f324` was re-checked against main:
the subtitle gates and worker traces on Linux AArch64 (every existing call identical), parity
on ARMv7 (A32), and the instruction sums, 0.99996 (`subtitle-dvd`), 0.99991 (`subtitle-dvd-host`),
0.99981 (`subtitle-pgs`), 0.99983 (`subtitle-pgs-capsules`) and 0.99266 (`subtitle-teletext`)
of main's on Linux AArch64 and 0.99999, 0.99998, 0.99991 and 0.99753 on ARMv7 (A32), with no case
above main beyond the run spread. The 13 `mov-text-tags` cases use the gate build at
`96f660ba99fa33fbc1a2019cda9e73bec6de287d` on a c8g.xlarge pinned to CPU 1, with 30 timing
pairs and 30 memory pairs under the kernel RSS proof, run as a slice of the feature.
`cargo xtask grade` does not join the two runs, because the slice's contract differs from
the registered feature's, so their per-case results come from each run's report.

The ADTS AAC decoder can leave priming to a caller with an explicit container
edit. The default still applies the encoder delay signature. The stereo and
5.1 AAC-LC workloads use the gate build at `96232af7d9431c2285c6b6d248aa38a133b85fe3`
on a c8g.large pinned to CPU 1, with 30 timing pairs and 30 memory pairs
per case. Both match the reference PCM within the decoder's numeric rules.
The upper runtime bound is at most 0.7587, peak heap at most 0.7034, and
total RSS at most 0.4219 of the reference. Other codec and decode-session
memory comparisons await gate-build measurement with repository runners.

ISO BMFF open and index measurements use `cargo xtask isobmff-bench`, with
`--subset52` selecting the 52-operation set. Packet framing uses `cargo xtask framing`.

The ISO BMFF row uses the gate build at `f99d893196908d562c897bccb20c6a972cd25d11`
on a c8g.large pinned to CPU 1, 30 pairs. Runtime comes from the timing collection and
memory from a separate collection under the kernel RSS bound; both are in the raw
samples. `cargo xtask grade` cannot join them, because FFmpeg's log lines carry heap
addresses that differ between runs.

The MP4 forward and range results use the gate build at
`f5453ef9d88712777aa5b6383250875456e9f334` on a c8g.large pinned to CPU 1, with
30 timing pairs of 50 complete operations and 30 memory pairs per case.
All 46 forward and 51 range cases match and pass runtime, peak heap and
total RSS. Forward playback covers encoded edit projection with composition
offsets, fragmented input and unknown movie duration. Three regular AAC cases
cover priming, trailing trim and a fractional edit endpoint. The fragmented
cases cover a priming edit with audio in the first or a later fragment, a
recovered short first AAC packet and an edited MP3 track.

Regular Opus cases cover edit-list trim, variable packet duration and secondary
audio alignment on a backward video seek.
The five source-roll cases compare the video landing, both audio ordinals and
decoder preroll. Broader profile and FATE coverage remain pending.

The earlier QuickTime AAC comparison uses the gate build at
`907ac3dce61c98bd05f52bfa3ff92497efdd141d` on a c8g.large pinned to CPU 1.
All three clips pass the accuracy, runtime, peak heap and total RSS gates,
with 30 timing pairs of four complete decode sessions and 30 separate memory pairs.
The reference and candidate both fold the complete planar float output in
64-bit little-endian words.

The expanded AAC comparison uses the gate build at
`3d422610316d79665c80b48ab8a27a8a57880efe` on the same instance type and CPU,
with 30 timing pairs of four complete decode sessions and 30 separate memory pairs
per clip. All 25 clips pass speaker, PCM, runtime, peak heap and total RSS checks.
The 64, 88.2 and 96 kHz stereo clips exceed the optimization target.

The AAC program comparison uses the gate build at
`fde36dd91d85ca700538421062ceacd45075c5b8` on the same instance type and CPU
with 30 timing and 30 memory pairs per clip. The nine added program layouts
match every PCM plane and ordered speaker. The decoder rejects overlapping
speaker positions. All 34 clips pass delivery and 31 reach the runtime
optimization target.

The AAC Parametric Stereo decoder follows Purnhagen (DAFx 2004), Breebaart et al.
(EURASIP JASP 2005) and 3GPP TS 26.405, with every table and rule they do not print
recovered from FFmpeg 9.0.2's output by scripted probes
([harness/codecs/aac/ps](../harness/codecs/aac/ps/README.md)). On AArch64 and on ARMv7
the aac lane keeps its results (35/36, `aac-sce-in-stereo` as before) and the PS vectors
stay within the numeric rules at a channel RMS of at most 2.2e-8 (`al_sbr_ps_04_new` from
1.1e-6 to 1.7e-8). Three crafted streams of unrecovered paths are open cases
([aac/ps-open-manifest.json](../harness/codecs/aac/ps-open-manifest.json)). Instruction
counts against the previous decoder (perf stat totals): the PS vectors take 7 to 18 % fewer
on AArch64 and 0.6 to 3.3 % fewer on ARMv7 (`thumbv7neon-unknown-linux-gnueabihf`); HE-AAC
without PS and the frozen `aac-lc-5.1` and `aac-lc-stereo` workloads take at most as many
as before on both, 0.014 % fewer at most, so the AAC timing rows stand.

The downmix's conversion to s16 (`mix::to_s16`) also runs fewer instructions on AArch64,
the codec gate target. There the floor was one `frintm`, but the loop clamped in `f32`,
converted and clamped again, two samples at a time; the integer form folds the scale
into a fixed-point `fcvtzs`, saturates with `sqsub` and narrows with `sqxtn`, four
samples at a time. The gate build at `f0744418` against `b89e5844` on a c6g.4xlarge
(`cargo xtask count`, median of three rounds) runs fewer instructions on every audio
workload, while `b89e5844` against itself moves by at most 0.01%. The 577 decoder
outputs are byte-identical and the conversion gives the floor's result for every
`f32`, so the AArch64 codec timing rows stand:

| Workload | Instructions | Cycles | Instructions, main against main |
|---|---:|---:|---:|
| `aac-lc-stereo` | −0.61% | −1.62% | 0.00% |
| `aac-lc-5.1` | −0.22% | −0.50% | 0.00% |
| `aac-he-stereo` | −0.18% | −0.57% | 0.00% |
| `aac-he-5.1` | −0.05% | −0.09% | 0.00% |
| `aac-hev2-stereo` | −0.19% | −0.56% | 0.00% |
| `ac3-2.0-192` | −0.47% | −1.04% | 0.00% |
| `ac3-4.0` | −0.19% | −0.44% | −0.01% |
| `ac3-5.1-448` | −0.17% | −0.39% | −0.01% |
| `eac3-stereo-spx` | −0.45% | −1.03% | 0.00% |
| `eac3-5.1-spx` | −0.15% | −0.30% | +0.01% |
| `eac3-7.1` | −0.09% | −0.22% | 0.00% |
| `eac3-5.1-1536` | −0.11% | +0.10% | 0.00% |
| `dts-core-es-6.1` | −0.09% | −0.29% | +0.01% |
| `dts-core-7.1-master` | −0.11% | −0.29% | 0.00% |

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

The auto-hinter's CJK writing system was compared at `204d212285ace2400a36774544188b687b06eff6`
against main at `b78dd86f2eebb7417c758b0a5757500039d718bf` on a c6g.4xlarge, natively on AArch64
and on the ARMv7 Thumb-2 target, with `fonts compare --all`, `shape compare --all` and
`ass compare --all`: no case fails that passes on main and every passing case folds as on
main; `font-hinting-default` gains DejaVuMathTeXGyre and `shape-libass` NotoEmoji's six cases.

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

The collision boxes' top-down merge sort was measured at
`1d2ae4a0287972ac3402bd2e717e5b7fdaa086ff` on both targets:

- Comparison: `cargo xtask ass compare --all` passes every gate, with `ass-render` 76/76, and the
  crate's tests pass.
- Generated scripts: against libass for AArch64 the same 115 scripts and 1 mutation differ.
  Against libass for ARMv7 49 scripts and no mutation differ. The one fewer is `random-9675`,
  one of the 4 where libass for ARMv7 departs near the end of its address space; in this run
  libass renders it as the renderer does.
- Equivalence with its parent (`cargo xtask ass equivalence --random 5182`): every frame alike
  on AArch64. On ARMv7, 3 cases differ with each side on one thread in its own process, and 124
  on 4 threads. Each is a frame that one side or both refuse for storage at the end of the
  32-bit address space. The same build differs from itself in 128 and 131 such cases between
  4 threads and one.
- Instruction counts: every `ass-render` and `ass-render-host` case is within 0.02% of its
  parent's.

`collision-far-margins.ass` was added at `02e3a75a86bd5c22900d972c02055e23a37103ab`. There
`cargo xtask ass compare --all` and the crate's tests give the results above on both targets,
and libass for AArch64 and for ARMv7 record the same pins for every `ass-render` case. The
merge sort with its first run rounded up instead renders that case's 1080p frames otherwise.

The bidi resolver written from UAX #9 was measured at
`81a17b4edf5e24611eab70ac1ddf9b3cf4c37603` on AArch64, against main at
`8a3f711f8d014411ea64d5b8967367ac3aedbac1`:

- Comparison: `cargo xtask shape compare` and `cargo xtask ass compare --all` give the
  shaping and ASS results above, and the crates' tests, `cargo xtask shape ucd --check` and
  rustdoc with `-D warnings` pass.
- Conformance (`cargo xtask shape conformance`): Unicode 14.0.0's `BidiTest.txt` (770,241
  cases) and `BidiCharacterTest.txt` (91,707) pass.
- Generated texts against FriBidi, through both shape workers' dumps, 20 seeds of 50,000
  lines: 3,000,000 simple shaping lines of Arabic letters of every joining type, marks,
  joiners and formatting characters, 6,000,000 resolutions of deep-nesting lines of every
  class, with bracket pairs, line starts and each base direction, and 1,500,000 such lines
  through simple shaping, where a line may hold a paragraph separator, are all identical, as
  they are through main's shape worker.
- Equivalence with main (`cargo xtask ass equivalence --random 5182`): every frame alike
  (5,674 cases, 229,509 frames). The peak storage is lower in 4,622 cases and higher in 981,
  by at most 24,808 bytes (0.08%).
- Instruction counts: no `shape-bidi`, `shape-simple` or `ass-render` case runs more than
  main's; the sums are 10.1% lower for `shape-bidi`, 24.6% for `shape-simple` and 0.14% for
  `ass-render`.

On ARMv7, in the armhf user space, at the same commit: `cargo xtask shape compare` and
`cargo xtask ass compare --all` give the shaping and ASS results above, both conformance files
pass, and one seed of the generated texts (50,000 lines through each set and base direction) is
identical to FriBidi. No `shape-bidi`, `shape-simple` or `ass-render` case runs more instructions
than main's; the sums are 5.3% lower for `shape-bidi`, 18.3% for `shape-simple` and 0.12% for
`ass-render`.

The container, playback, MPEG-TS, HLS and closed caption rows were checked again with the
code-size change at `4fc96a499ecc1c1a8a6a23d11485123cc4e817a7`, linked before the link
policy, on a c8g.2xlarge pinned to CPU 1: parity, 30 timing pairs and 30 kernel-bound memory pairs per case, graded with the
module loaded; the release tests and the worker tests pass. Every case passes runtime and
peak heap and is lower in total RSS. In that run, three cases had no valid memory
observation: in one pair each of `tags-binary` and `track-disabled` (Matroska segment
metadata) and `fate-witch-realtime` (closed captions), the kernel bound recorded an error
while it observed the reference. Their memory was collected again, 30 pairs each, on
another c8g.2xlarge with byte-identical workers and references: total RSS ≤0.9310, ≤0.9308
and ≤0.7770, every observation valid. The worst upper bounds of runtime and total RSS:

| Feature | Cases | Runtime | Total RSS |
|---|---:|---:|---:|
| `mp4-forward` | 39 | 0.9670 | 0.9793 |
| `ts-program` | 35 | 1.0360 | 0.9035 |
| `raw-annex-b` | 7 | 1.0897 | 0.8588 |
| `mpegts-pes` | 97 | 0.9530 | 0.9137 |
| `hls-segments` | 71 | 1.0780 | 0.8215 |
| `matroska-audio-clocks` | 10 | 1.1294 | 0.9903 |
| `matroska-startup-seek` | 44 | 1.1414 | 0.9375 |
| Matroska's six feature groups | 115 | 1.1312 | 0.9841 |
| External HDR vector | 1 | 0.9783 | 0.8676 |
| `subtitle-cc` | 201 | 0.5290 | 0.7773 |

`subtitle-cc` keeps its four open `hevc-eos-ts` cases. The ISO BMFF worker, and every
reference worker, are byte-identical to those of `4dbbd545d2d452220cd0e35ff11807f6c2fd2703`,
measured the same way on the same host: ISO BMFF passes all 972 outputs (runtime ≤0.8972,
peak heap ≤0.316, total RSS ≤0.7433, all lower).

| Area | Revision | Raw samples SHA-256 |
|---|---|---|
| Matroska / WebM | `513149a970caad5b30c61becba1bc2bda1c5e193` | `883ce5128410aefb3ec9affe3579a798ee8d1c5d082c1d129a462d67728e7af9` |
| Matroska startup and seek | `513149a970caad5b30c61becba1bc2bda1c5e193` | `883ce5128410aefb3ec9affe3579a798ee8d1c5d082c1d129a462d67728e7af9` |
| Matroska startup and seek, preceding comparison | `0e28298d038bea072629b3bad6389dcc806b4861` | `a20dcbbe804bfd21ee631cc42f61466d2400d5115abb8ea97dd243ef733ba74f` |
| ISO BMFF / MP4 | `f958c0b9624a8b7c7bd91b7d2a0c30299ae1dbed` | `fd718807e0ec1e6287bcb690226bf7056b79985796e74eba54f43544a034bc2c` |
| ISO BMFF comparison | `0a033ec3406aebd09d191298a546659e02c6b76c` | `f56d3fa78b8cc70222f58c7cfa9a17331b0924a9070c7ff6ecc6dc1f213561bc` |
| ISO BMFF open and index runtime and memory | `f99d893196908d562c897bccb20c6a972cd25d11` | `806f2f996b19370bb23350ae4a166f3b7688b3e0e499352d77c3b2610c90aa7b` |
| MPEG-TS PES | `21c9eef2f08a11bb7bd1c457997095326f2bb42d` | `8ad96ce3a9ca2b5e4b23dfcc53659fb4cc99ae0638d502e114188b3acbdad2f5` |
| MPEG-TS access units | `230d1314464d34a04f5f2614298e5b7009292f0c` | `a86e737ad8081e349a4c3a707b6b29ecfee6ae8bbeaabd9a9edc5f846e2da697` |
| MPEG-TS program playback | `21c9eef2f08a11bb7bd1c457997095326f2bb42d` | `8ad96ce3a9ca2b5e4b23dfcc53659fb4cc99ae0638d502e114188b3acbdad2f5` |
| MPEG-TS progressive playback, compact and self-contained reports | `ec6afe8635509d7e9007fd3d3c20ce2bf1c25835` | `f35c3d9e3abba7519187a6f7821b47362f5f58869637738a7490e9329d10067f` |
| MPEG-TS progressive playback, earlier report comparison | `855bfea306ee7c2f7a933505cb18cde287f91a7c` | `d144f900ef1598b8300ddc7966349268fbea62c4a726fccf4291c766273e2e6a` |
| MPEG-TS progressive playback | `21c9eef2f08a11bb7bd1c457997095326f2bb42d` | `8ad96ce3a9ca2b5e4b23dfcc53659fb4cc99ae0638d502e114188b3acbdad2f5` |
| HLS segments | `21c9eef2f08a11bb7bd1c457997095326f2bb42d` | `8ad96ce3a9ca2b5e4b23dfcc53659fb4cc99ae0638d502e114188b3acbdad2f5` |
| MPEG-TS progressive playback, preceding comparison | `23f68766543bcd57a96e88bfe5350095236ff88c` | `a372562b91bb14dce3bbd7117f71deb098f6dba49492c5f4396a70336012d6fc` |
| MPEG-TS program playback, preceding comparison | `23f68766543bcd57a96e88bfe5350095236ff88c` | `a372562b91bb14dce3bbd7117f71deb098f6dba49492c5f4396a70336012d6fc` |
| MPEG-TS progressive playback, earlier comparison | `5ec7c1f8a7816b1427631f22a8ca77ba74abeeee` | `fb8a3a3b1f7f3d687254a3f4c1849dede7a78bc3c62dbc7905b09a45f675709d` |
| MPEG-TS program playback, tight carrier comparison | `eb3279f36c19c97dd9bdd68b1e66f371f28e38c1` | `7e18edf0a340b6ff4bb296b52deffc720f706c464340c7c73392127f9c87248c` |
| MPEG-TS program playback, earlier comparison | `3125c1e61d73a575419f7b653a1dae44a3269dae` | `beb5fa935b0109d0e33dbc3f57c22481c875a4c2b065820dda247535dc431f3a` |
| HLS segments, earlier comparison | `5ec7c1f8a7816b1427631f22a8ca77ba74abeeee` | `fb8a3a3b1f7f3d687254a3f4c1849dede7a78bc3c62dbc7905b09a45f675709d` |
| Packet framing | `04512a07fbb7efdf0d6d7173db08ec1267f2fae3` | `677ce4b5aef58efd8523868cd2318d9cfde07868d2b7451d0017f013dfb3130b` |
| MP4 range playback with an initial non-sync prefix | `e35e3584f748a9dd6a4bc41e8de826b682c688e0` | `35b353d845a9a414bad2fcd862a5e22e23000c2d98711aee2cb8bfe9febc3063` |
| MP4 range playback with fragmented subtitles | `766961a481106e878705bcb43a438c24725869e0` | `7b6e9c9b7f078554882d60869a07068fcec65ce1e4271f69db3f15e9de147bcf` |
| MP4 range playback earlier fragmented subtitle comparison | `b42e33df742854373b2aba84decd086481aa8af6` | `5093aa38db9a01fb644488dafb2924b602d855825e16ff9052568f5e13abc1d5` |
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
| Codecs: the HE-AAC v1 and v2 workloads, timing, memory and instruction counts, Linux AArch64 and ARMv7 | `700c2134a123d8c9566c1cdccdeb3a0b3b0399c0` | `838ee260edf0c87fa7f1feb154d9ae83529c2a817b9d92c7c6b9ba72fae2d239` |
| Codecs and decode sessions, runtime | `add6b272cb622bae0d921dc3fbbf0812958c2023` | `ab2a174847322658159e93b2cd9bb994cdd9f4f9438926191518897aff28d929` |
| Audio codecs in files | `ccff5b5734c113c1d88160b84825157508c701c0` | `0ddcc517728bb39e2d74cd2e3bf4c00071a6f3815ad3409d5215d216af9a0a53` |
| Ogg, audio elementary streams | `ccd3090572433e79d8ab65a377fcd9896d2839d9` | `8123e0f020a9c4ccedd83aea3e665d4486d55e26612e8bc29626e9a3026e2cd1` |
| Capture, timing and memory | `ceb5076659e00ca9a2d3f3cb55ee4513d6d0710f` | `b75c8d6e6d64d294a385ec25b176e4ffb8f1a0d3c11b3ca956b3d352ac44c3b4` |
| Codec and capture timing, earlier and shared runner | `c21176e03328f26e0f49d6870dad0a55dd7c5e76` | `9a2cb6808d49462ff33e0666a44773692c44e85414f5aa9b2a21ea27a11a6bcf` |
| MP4 forward and range playback, AAC edits | `f5453ef9d88712777aa5b6383250875456e9f334` | `bb60ca1d1571ae40f6cc38e1d7f1315305eb2abd4daa371e1944f8b1c8273223` |
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
| ADTS AAC priming | `96232af7d9431c2285c6b6d248aa38a133b85fe3` | `a905aaa8c0e099f85218d2a7f097ed1aa3a3627a5adaf77c10dbdfdab063fdfe` |
| AAC access-unit decoder | `907ac3dce61c98bd05f52bfa3ff92497efdd141d` | `b56b7978e2b3824d95d71828ff8020ef87fb0f213d1ae83f5d4b3380e5688af0` |
| AAC access-unit decoder, fixed layouts and indexed rates | `3d422610316d79665c80b48ab8a27a8a57880efe` | `5b707aa011c5f32e5915a7ba3b8b3daa85ae32111842c7e91754d4de75c2c949` |
| AAC access-unit decoder with program configurations | `0a51b7c3746c882b3ca5127ad047162130c01ea6` | `86fc2ed8f95b4c4584f8ceb1ce90350c699db984d77de998bbb74bee415c49cf` |
| AAC access-unit decoder with height validation and rear pairs | `7b479ad67690a7c2bc03c15f4ff8545c05bc965a` | `7e7f3985d7d4c2f8e56d82aeb10b8627afb6e4a72df6bfc66bd1da4a5a7d019e` |
| AAC access-unit decoder with distinct program speakers | `fde36dd91d85ca700538421062ceacd45075c5b8` | `68233a92037fcb6011d7a198a1c11a735b461268fbf9d9ada0acb8f18a8f1daf` |
| MP4 range playback with configured 7.1 AAC | `09c154a3d4c8e38c34f59815ad7219df1e86ee26` | `cea6d87e41ab618d2db679b659856276788a37a6f962eb27e5e71a2b8825c48b` |
| MP4 range playback with AAC program configurations | `98ad5b82da81ba113b3bab206459efc7ede2ed8a` | `6b41bc92f4c55f6a868dc01181d152a75fe9cbb4868eebb72a6da437b4cf5579` |
| MP4 range playback with the bounded AAC program parser | `a961b3b38b083180e11a5d57bcf6ab73a15b72dc` | `1beefed497c83dcff49a8638ddbcda6f7968af67806753d727afd10ad16dad26` |
| MP4 forward and range playback, earlier RSS comparison | `733ee1582e4fce4af1d89c91171e29e41a16d294` | `23cf7cdb3de800c1ca0d03dfe937ce6fda6cc768bef5933370223a88cda78c87` |
| MP4 forward and range playback, earlier comparison | `8bbf6eab798f3d58f62dce14915fb30acd14488e` | `a3e74e11216d7769b7b167ac57cb17c043e6d92b4feb39a8422879c27493eebe` |
| MP4 forward and range, encoded edits | `2a1b26347fee56f5506db63efbc8e3e3c66ad388` | `fe95646449aa52a4a121aeecb3346308b270923aee9b0751b2125501abf36004` |
| Matroska audio clocks | `513149a970caad5b30c61becba1bc2bda1c5e193` | `883ce5128410aefb3ec9affe3579a798ee8d1c5d082c1d129a462d67728e7af9` |
| Earlier Matroska audio clocks and playback profiles | `b3579f729494a37a4dbebe03c475f7b38484046e` | `907bb7a35d70d09673ccbf0904a8b1ab85890a632454b8adf291d73a50672136` |
| Subtitles, Linux AArch64 | `f263491102d307997d40d40826a6193d8468e87e` | `9ed7263aabbc4f44910c5135fcf077419ea5b426e3a09a888e83f10153680c32` |
| Earlier ARMv7, A32: subtitles | `8513398b85d27aa093b8212c202b25c32da859bf` | `9ed7263aabbc4f44910c5135fcf077419ea5b426e3a09a888e83f10153680c32` |
| Closed captions, Linux AArch64; earlier ARMv7, A32 | `9d048c58fb30c64777f7230eb67c7037a1e1bcf1` | `2b78cc89c624e9296967d4f3ec8aa0e5713631d1d3228c63c8b96eb1c4e6cfd1` |
| Earlier: subtitles, PGS, closed captions, teletext | `956c42961f4a27d88f96d5800d59ff1eeb088018` | `61b52a272e82eeee913c9c4b4d6331caea3283a99ebad4a897aba65dab3c891d` |
| Earlier: subtitles, PGS memory | `03f3efd3a0fde0d558c68f5408c521d632802fb7` | `6c0cc3a669152b3eeba9a1189939c53b1b68e2f5cb9ab6cb81f903ec4358634d` |
| Earlier: subtitles, DVD | `531e2d04ba7386513ff7afe7b44f53b2a93ca9dc` | `01ef0a02aad45d7b754c0e409e12949437e8f7f0270e252e82a3f7ebff0e6391` |
| Earlier: subtitles, DVB | `0d70bebfc0799f65df14f170ed97e68c5a35daa7` | `06d1a46a967e052ce365795827803ddbc5ae8ff0e2452ae83428f53744949077` |
| Subtitles: text, Linux AArch64; earlier ARMv7, A32 | `6126b759b9b983afe488c3c362c6e37c2d1425a2` | `aec763325453d4174a1d70764d4e863a77ade9315ee607b14cfe44d1eb8a7676` |
| ARMv7: subtitles, DVD, PGS and teletext, the 8 crafted size, fragment and trimming cases, parity, runtime and kernel-bound memory | `da6545639aa4cb878ca62bfb384ea3aeff7f8309` | `2445c829a23ebd335a86ad4f3d946ed78053617d40f25db1c680612ed8eec145` |
| Subtitles: DVD, PGS and teletext, the 8 crafted size, fragment and trimming cases, runtime and memory on Linux AArch64, parity and instruction counts on Linux AArch64 and ARMv7, A32 | `c79ae2da8b8a0d004d37936100eedd7b46e0a589` | `a5afebfe3a1c86829b0768823e7aa6518d008fd450104a72ca95b812113019a9` |
| Subtitles: DVD, PGS and teletext, the review commit's gates, traces and instruction counts on Linux AArch64 and parity and instruction counts on ARMv7, A32 | `821900952ba46678d07300ea7e4bb04ec517f324` | `6fc99861c2fc41d8b1fe161148c3c916f84425142d437c0eb09ed5b1b3936731` |
| Subtitles: DVD, PGS and teletext, worker traces against main on Linux AArch64 and ARMv7, A32 | `92d1a1c5c83b8b39b12ae1ed11b999badbeddc23` | `425c08ba299c60e13c0690b16ef7599bf4fdaade2cfb8ce32b75c547edc7767b` |
| Subtitles: text, the `mov-text-tags` sessions, runtime and memory on Linux AArch64, parity and instruction counts on Linux AArch64 and ARMv7 (A32) | `96f660ba99fa33fbc1a2019cda9e73bec6de287d` | `03a2ee2f02daf3c5371c5c44e183c45b4bc655c30714bc12594fc58c72a56311` |
| ARMv7: subtitles, text, the `mov-text-tags` sessions, parity, runtime and kernel-bound memory | `dd395b41add1b51859be0104ad865de111d7b401` | `f3a4c240aa2177b1af1ec6557318208bd417fc983adde2e742de5d54bf0a5713` |
| Earlier: subtitles, text, the `mov-text-tags` sessions, parity and instruction counts, Linux AArch64 and ARMv7 (A32) | `30fe564a562f0ed73de7c076fe7f4eb0b12f21d6` | `1b73087069b838cfd16bc22c5f8d29dcafbe9c5674c3c775d53519f02fcf08bc` |
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
| AArch64: the s16 conversion's instruction counts on every audio workload against `b89e5844`, equivalence and the `aac-lc-stereo` profiles | `f0744418776dd2e01795c24d06716f54a0aefc2e` | `ad8eb8a8a898a065df3e43fd62c74f9d988977bfe88515aab1c3447e0ec7d1a6` |
| ARMv7: the AAC codec timings (AAC-LC, HE-AAC v1 and v2), their instruction counts against `b89e5844`, equivalence and the `aac-he-stereo` profiles | `0b3c2db5239da81ab38d7a33f73b70a9f77df464` | `bf142970609ef8d465066f57c2010f8e876524b0b7e334628390d59bfc5fb43a` |
| ARMv7: the AC-3, E-AC-3 and DTS codec timings, the s16 conversion's instruction counts against `f3d4935b`, equivalence and the `aac-lc-stereo` profiles | `ddb02a59a7ac8bf1974c14439194430e2b245504` | `52e5576be35489a871fea3b89cdb13389cb5da8dc3ef465d23315556d41e8977` |
| Linux AArch64: the container, playback, MPEG-TS, HLS and closed caption rows, checked again with the code-size change | `4fc96a499ecc1c1a8a6a23d11485123cc4e817a7` | `78255f4d6b7a140b9992db206fc9978716fa70457b389bd24990a5536eb3ad29` |
| Linux AArch64: the memory of `tags-binary`, `track-disabled` and `fate-witch-realtime`, collected again with the code-size change | `4fc96a499ecc1c1a8a6a23d11485123cc4e817a7` | `606dad5b6af16fbf3611bfb0669a084a560976ad5bf4ea142fcaac62e5ea8192` |
| Linux AArch64: ISO BMFF with the code-size change | `4dbbd545d2d452220cd0e35ff11807f6c2fd2703` | `da350a13c27d6b8a2ed6b6ead710808abf4f3224041a0128bae9c339cf25ff8f` |
| ARMv7: the Matroska, startup and seek, HLS, ISO BMFF, packet framing, audio clock, MPEG-TS, playback and closed caption rows of the ARMv7 table, the third collection of `charset-cp950-nomapping-a240` and `long-movie-mkv` | `4fc96a499ecc1c1a8a6a23d11485123cc4e817a7` | `9328926f64b4a90ef5d4d8161059c5dae72105aa02856bb5f74c221ad9b64bda` |
| ARMv7: every other row of the ARMv7 table and the earlier bounds of the rows above, the video codec timings | `3df7ae6d504d67e054ffe9a54901620c134ba2c6` | `65707cc2d5ece5ed7d907509b9f2cd4f438ad380be3109be368244b04d3f12fd` |
| Earlier ARMv7, before the page-cache eviction: the container, playback, ASS rendering and capture rows; the A32 and NEON codec comparisons | `22ed8232b28a285f1439772b91713df3aa64ea91` | `abb62b29518be601df25d36c54235cbd4c9f36f7255470a1f89f02eadaeeed1e` |
| Earlier ARMv7, A32: Matroska / WebM, startup and seek, HLS, codec timing, capture | `78a873dd983c878c861a7b5549639a58d5a599c1` | `773f9f077ec4e556f8692132c35d73b8256f24c9c26a5e407aa11df32820a116` |
| Earlier ARMv7, A32: ISO BMFF / MP4 | `f99d893196908d562c897bccb20c6a972cd25d11` | `bb7a7df1df72022fd2cdb617e6f0876327405c2833a0e056d319ff97d5fcbfe1` |
| Earlier ARMv7, A32: packet framing, codec conformance, tests | `9621f6063a2e517fecc1c28757e6bed50b5228a8` | `9eaa9347010b3c4aaf5c35bda715ee4ed1b2c7a14d3329b21976a2414455439e` |
| Earlier ARMv7, A32: kernel-bound memory of the container and playback rows; MP4 forward, TS program, Matroska audio clocks and raw Annex B runtime | `afb9049c681916c97311fc9f495e27ceaa69f4df` | `4567e8ba906099070b64ac43868bdd9695d350049f25645078e35c0f334ce8b1` |
| Earlier ARMv7, A32: MP4 forward, TS program | `4c823f7f74419cbfb9c8220a72b4684931c621f9` | `fe4098d3c3812710cc17b7b78155f2ee87eba366ce3bbe3d9de9569631bd7106` |
| Earlier ARMv7, A32: MPEG-TS runtime; earlier Matroska audio clocks and raw Annex B | `7be886648e47f711b0f258412df82d214158a709` | `b1df5b6f79fa2644106b8e87c61799b5c6fa9075c33096017bf4c50d89ef95a0` |
| Rasterising, Linux x86_64 | `a7b6635e20cc2dc9d4592a874ba0eecab24f2870` | `1edfe455ee5a8ba42c9197bd9e8e97f42e4325eeb3f8abd6d13f0b479345ee3c` |
| Rasterising, Linux AArch64 | `a7b6635e20cc2dc9d4592a874ba0eecab24f2870` | `0ba86c152cef51adc4b57d37f7d9eeeab29a2137425aec95c9b7198b0245a44f` |
| Earlier ARMv7, A32: rasterising | `a7b6635e20cc2dc9d4592a874ba0eecab24f2870` | `d328061b906a28f523eddd7aeaa778b62d13b7c7750c68347659a457497465d7` |
| ASS parsing, Linux AArch64 | `21642833a1c1f4d8e587a5c5bc903910f215428e` | `c039ca3e2cb63255140973528ddd22cc7e96b32e5254d22ea4f954011822a35d` |
| Earlier ARMv7, A32: ASS parsing | `21642833a1c1f4d8e587a5c5bc903910f215428e` | `c039ca3e2cb63255140973528ddd22cc7e96b32e5254d22ea4f954011822a35d` |
| Earlier ARMv7, A32: ASS rendering kernel-bound memory, the Thumb-2 playback experiment, the AArch64 proof of the kernel bound's 32-bit entry | `afb9049c681916c97311fc9f495e27ceaa69f4df` | `d6df03ff861f3259d837bfb93273a440f30a8240ab52c95891d2065e594c242a` |
| ASS rendering runtime and memory, Linux AArch64; earlier ARMv7, A32 | `5b3e03f0e9410853152043d033b4f4d757e98a40` | `632964773b8ca4cbd5b2b3c09a2b6ddeafdaeeaff56588ea345bae8550012bb7` |
| Earlier: ASS rendering memory against the instrumented libass build, Linux AArch64 | `e6de7e70827bb930a8ef9e5cb4d30b1e4e7d0727` | `799386520d27ae7be5eab6c827fcbe0608950ddb4c32c93be7ea3cf660d4b0e3` |
| Bidi from UAX #9: comparison, conformance, generated texts, equivalence and instruction counts, Linux AArch64 | `81a17b4edf5e24611eab70ac1ddf9b3cf4c37603` | `63104dd7b693a2c559906ca279a2350b4a273083c03fabe4fe097d49e56b961f` |
| ARMv7: bidi from UAX #9, comparison, conformance, one seed of generated texts and instruction counts | `81a17b4edf5e24611eab70ac1ddf9b3cf4c37603` | `18211802f6b7d171f06852a6f38459957ae6049a7c65930f1c96900cc33edb36` |
| Earlier: bidi from UAX #9, Linux AArch64 and ARMv7 | `e44bfacb38a19598eca07ea96a9aa83a05a9b1f2` | `dbf448c1ada891ab4122396818350e39df1171ff58e7cd77de0c354abb22c943` |
| ASS collision order on boxes 2^31 rows apart (`collision-far-margins.ass`), Linux AArch64 and ARMv7 | `02e3a75a86bd5c22900d972c02055e23a37103ab` | `736748aa549fec74078fa33d31dba3e0a123290e652278c3359d37a0a9737db2` |
| ASS collision order: comparison, generated scripts and equivalence, Linux AArch64 and ARMv7 | `1d2ae4a0287972ac3402bd2e717e5b7fdaa086ff` | `e4e97db001bf538840edc2cbfb8db80c126aeb97df3edeb68cf7185b2fe4bd04` |
| ASS rendering comparison and generated scripts, Linux AArch64 and ARMv7 | `01ccb49792cf1d4d05b74addf35dc616763b88e0` | `c510f31add480041fa7197ddbca26fe143926cf84106890b061b3b99897749be` |
| Earlier: ASS rendering parity, Linux AArch64 and ARMv7 | `28e5ccf0ecb5419f7105d52c91ebe8d6c09f053c` | `bc26dfc21b141cc79b5bb4f8cb3d636dcfaa3f64fa24eec2d200409eff0c32f0` |
| Earlier: ASS rendering comparison and generated scripts, Linux AArch64 and ARMv7 | `fedfb790bfb52131389cb7c5134bab7ab813b55f` | `f10a94cf995ed49ac8e9befda3441efcf3c900a984105b1385c3d34444418d1f` |
| The auto-hinter's CJK writing system: font, shaping and ASS rendering comparison against main, and instruction counts, Linux AArch64 and ARMv7 | `204d212285ace2400a36774544188b687b06eff6` | `6aa341a5864a8f4b535d2d9383ebcd7a4726f5889f1258f91a9c17d4c7136973` |
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
| AAC Parametric Stereo from publications and probes: aac lane gates, open cases, table generator runs and instruction counts, Linux AArch64 and ARMv7 | `560f222594f3b8767d7454ee1fa5752573020112` | `b3e04129ab40a86bea032839869680e53907db23ff44aabade68373929d9c708` |
| AAC Parametric Stereo review: the transient ducking's re-derivation by probes, aac lane gates, open cases, table generator checks, fuzzing and instruction counts, Linux AArch64 and ARMv7 (`thumbv7neon`) | `81cb8bf9ac9499fe036d2f186a8fdca365e13820` | `25cd8fa2e8d359ae096328c21d46ab0322654a7d93016b17f822134db25d8560` |
| ARMv7: subtitles, DVB palettes, the 12 crafted cases and the three `sequence` cases, parity, runtime and kernel-bound memory | `74155089881341aa7aae67e5f754b35a86e4898b` | `63453697efcbf39e4af583c5f546d1f366e2e49c4b6a9c97df3e867ca1293646` |
| Subtitles, DVB palettes: the 10 new cases' runtime and memory, instructions on both targets | `aebbe5827b1b2ae8f1dcf53a1be67053c850a279` | `d856570b6485dc339bc9fdcb2dbf76824ffd9e2e760b8d3efab36ca7ef291dc3` |

## Platforms

Qualification runs on native AArch64 Linux and, where the next section says so, on
32-bit ARMv7 Linux. x86_64 has no performance runs yet.

### 32-bit ARMv7

The gate build for `thumbv7neon-unknown-linux-gnueabihf` (Rust 1.99.1, default flags:
Thumb-2, assuming NEON; linked before the link policy) at `3df7ae6d`, with every FFmpeg, FreeType, HarfBuzz, FriBidi and
libass reference built from the same pins in the same user space (Thumb-2, the armhf
compiler's default; FFmpeg selects NEON at run time). The Ubuntu 24.04 armhf user space
archived as `armv7-userland` ([how to repeat it](quality.md#32-bit-armv7)), with the 15
packages libzvbi's build adds (autoconf, automake, autopoint, libtool, gettext and what they
depend on), on two AWS c6g.4xlarge hosts (Graviton2, Neoverse N1 r3p1), kernel
`7.0.0-1014-aws`, that built byte-identical workers from the same source and package list.
Each feature's runs come from one host, pinned to CPU 1: the tests, codecs, container,
playback, ISO BMFF and framing rows from one, and the fonts, shaping, rasterising, ASS,
capture and subtitle rows from the other. The Matroska, startup and seek, HLS, ISO BMFF,
packet framing, audio clock, MPEG-TS, playback and closed caption rows are at `4fc96a49`
instead, also linked before the link policy, run on a third c6g.4xlarge with the same user
space and kernel, with the references built there from the same pins. Every case has 30
timing pairs and 30 memory pairs; the contract features are graded by `cargo xtask grade`.
Memory is kernel-bound in every row but capture: measured under the kernel RSS bound,
through its 32-bit entry point, with both workers' cached pages dropped before every run
([quality](quality.md#measuring-performance)), so total RSS is the candidate's proven
resident peak over the reference's observed peak. The capture row uses the standard
bounds: its total RSS is the candidate's virtual peak over the reference's observed
resident peak, a ceiling that already lies below the reference. The codec timings are
diagnostics: no codec contract is gated on this target.

| Area | Verified against the reference | Runtime | Memory |
|---|---|---|---|
| Core | 51,235 exact-arithmetic cases; the workspace tests (4,326 in debug, 4,325 in release) and the worker tests pass | via dependents | via dependents |
| Matroska / WebM | 116/116 (six feature groups and the external HDR vector) | 1.1438 (116/116 pass) | heap ≤0.775 (116/116 lower); total RSS fails: ≤1.0748, 16/116 lower |
| Matroska startup and seek | 44/44 | 0.8449 (44/44 pass) | heap ≤0.396 (44/44 lower); total RSS fails: ≤1.0342, 17/44 lower |
| HLS segments | 71/71 | 0.5883 (71/71 pass) | heap ≤0.020, total RSS ≤0.8669 (71/71 lower) |
| ISO BMFF / MP4 | 1,020/1,020 open and index comparisons (972 matching outputs, 48 matching refusals) | 1.0009 (972/972 pass) | heap ≤0.274, total RSS ≤0.8155 (972/972 lower) |
| Packet framing | 125/125 AVC/HEVC reference comparisons | 1.0706 (125/125 pass) | heap ≤0.766 (125/125 lower); total RSS fails: ≤1.0591, 0/125 lower |
| Codecs | 347/347 comparable H.264 and HEVC conformance vectors (of 383 run, 31 are outside the supported profiles and 5 are refused by both); the 71 derived audio streams were not run, because nothing in the repository generates them; the AAC lane's 36 FATE and conformance vectors pass 35/36, as on AArch64, at the Parametric Stereo revision; the six video timing workloads decode to FFmpeg's digests | diagnostic, not gated: 2.0698 (10/20 within the ceiling; AAC at `0b3c2db5`, other audio at `ddb02a59`) | no repository runner for the gate; the three HE-AAC workloads at `700c2134`, not gated: heap ≤0.944, total RSS ≤0.890 (3/3 lower) |
| Capture | 0/15: mux and encoder pass on all 15, conversion fails on all 15 (below) | 2.2962 (3/15 pass) | standard bounds: heap ≤0.569, total RSS ≤0.982 (15/15 lower) |
| Matroska audio clocks | 10/10 packet folds (`matroska-audio-clocks`) | 0.7809 (10/10 pass) | heap ≤0.733 (10/10 lower); total RSS fails: ≤1.1913, 0/10 lower |
| MPEG-TS | 97/97 compat-view operations (`mpegts-pes`) | 0.8834 (97/97 pass) | heap ≤0.091, total RSS ≤0.8923 (97/97 lower) |
| Playback: MP4 forward | 39/39 packet folds (`mp4-forward`) | 1.0480 (39/39 pass) | heap ≤0.405 (39/39 lower); total RSS fails: ≤1.0795, 0/39 lower |
| Playback: TS program | 35/35 packet folds (`ts-program`) | 0.8700 (35/35 pass) | heap ≤0.118, total RSS ≤0.9855 (35/35 lower) |
| Playback: raw Annex B | 7/7 packet folds (`raw-annex-b`) | 0.7153 (7/7 pass) | heap ≤0.127, total RSS ≤0.9181 (7/7 lower) |
| Rasterising | `raster-glyphs` 12/12, `raster-outlines` 2/2 and `raster-bitmaps` 2/2 pass; `raster-sweep` 301/303 (below); the crate's tests pass | not measured | not measured |
| ASS parsing | `ass-parse-scripts` 48/48, `ass-parse-chunks` 4/4 and `ass-parse-events` 21/21 pass; `ass-parse-host` 13/15 (2 open); the crate's tests pass | not measured | not measured |
| Fonts | `font-outlines` 53/53, `font-hinting-none` 159/159 and `font-hinting-default` 9/53 (the same open cases as on AArch64); 10/53 with the CJK writing system at `204d212285ace2400a36774544188b687b06eff6`, as on AArch64 | not measured | not measured |
| Shaping | `shape-libass` 195/201 (201/201 with the CJK writing system at `204d212285ace2400a36774544188b687b06eff6`) and `shape-text` 200/201 (the same open cases as on AArch64), `shape-bidi` 24/24, `shape-simple` 6/6, `shape-unicode` 1/1 | not measured | not measured |
| ASS rendering | `ass-render` 78/78 and `ass-render-host` 4/4 pass; the crate's tests pass | 0.7782 (82/82 pass) | heap ≤0.929, total RSS ≤0.911 (82/82 lower) |
| Subtitles, text | `subtitle-text` 367/367 (the 13 `mov-text-tags` cases at `dd395b41`) and `subtitle-text-host` 23/23 pass | `subtitle-text` 0.9184 (354/354 pass), the 13 `mov-text-tags` cases at `dd395b41` 0.9682 (13/13 pass; worst `mov-text-tags-ranges`), `subtitle-text-host` 0.0783 (23/23 pass); instructions at `96f660ba`, built for A32, against main at `339ec261`: 0.984 of main's in sum, `subtitle-text-host` 1.000, no case above main beyond the run spread but `mov-text-tags-nul-transitions` (1.0075), whose output is longer | heap ≤0.336 (377/377 lower); total RSS fails: ≤1.0045, 376/377 lower (below); the 13 `mov-text-tags` cases at `dd395b41`: heap ≤0.333, total RSS ≤0.907 (13/13 lower, kernel-bound) |
| Subtitles, PGS | 317/317 (`subtitle-pgs`); with the 2 cases added at `c79ae2da`, 319/319 on A32 (`armv7-unknown-linux-gnueabihf`) at `c79ae2da` and `82190095`, and the 2 on the gate target at `da654563` | 1.0021 (317/317 pass; the 2 cases added at `c79ae2da`, at `da654563`: 0.2759 (2/2 pass; worst `rx-canvas-bounds-ts`)); instructions on A32 (`armv7-unknown-linux-gnueabihf`) at `c79ae2da` against main at `339ec261`: 0.99991 of main's in sum, no case above main beyond the run spread | heap ≤0.968 (317/317 lower); total RSS fails: ≤1.0051, 316/317 lower (below); the 2 cases added at `c79ae2da`, at `da654563`: heap ≤0.0995, total RSS ≤0.7756 (2/2 lower, kernel-bound) |
| Subtitles, DVB | 241/241 (`subtitle-dvb`, at `74155089`) | 0.8021 (229/229 pass); the 12 cases added since and the three `sequence` cases at `74155089`: 0.9127 (15/15 pass; worst `automatic-large-exposure-ts`, the `sequence` cases 0.3225 to 0.3489); instructions at `74155089` against main at `2de40661`: 0.8460 of main's in sum, median of the cases 1.0000; above main beyond its run spread of 0.029% are the three `sequence` cases (`sequence-mkv`, `sequence-ts`, `sequence--compute_edt-1`), by 639 to 787 instructions of about 1.99 million (+0.033% to +0.040%) | heap ≤0.992, total RSS ≤0.864 (229/229 lower); the 12 cases added since and the three `sequence` cases at `74155089`: heap ≤0.459, total RSS ≤0.839 (15/15 lower, kernel-bound) |
| Subtitles, DVD | 224/224 (`subtitle-dvd`) and 5/5 (`subtitle-dvd-host`, gated on this target); with the 4 cases added at `c79ae2da`, 228/228 on A32 (`armv7-unknown-linux-gnueabihf`) at `c79ae2da` and `82190095`, and the 4 on the gate target at `da654563` | 1.0389 (229/229 pass; the 4 cases added at `c79ae2da`, at `da654563`: 0.1340 (4/4 pass; worst `vobsub-idx-size-widest`)); instructions on A32 (`armv7-unknown-linux-gnueabihf`) at `c79ae2da` against main at `339ec261`: 1.00000 of main's in sum, `subtitle-dvd-host` 1.00002, no case above main beyond the run spread | heap ≤0.966, total RSS ≤0.887 (229/229 lower); the 4 cases added at `c79ae2da`, at `da654563`: heap ≤0.0028, total RSS ≤0.8252 (4/4 lower, kernel-bound) |
| Closed captions | 201/205 (`subtitle-cc`; the 4 open `hevc-eos-ts` cases, as on AArch64) | 0.8253 (201/201 pass); instructions at `4d81cde0` against main at `339ec261`: 0.9941 of main's in sum, median of the cases 0.9986, no case above main but `hevc-cra-ts` (1.0013) and `hevc-reserved-ts` (1.0012), whose difference follows the input path's length in the ARMv7 chroot, not the code: main's count steps up with a longer path and the candidate's stays flat | heap ≤0.127, total RSS ≤0.7984 (201/201 lower; below) |
| Teletext | 70/70 (`subtitle-teletext`); with the 2 cases added at `c79ae2da`, 72/72 on A32 (`armv7-unknown-linux-gnueabihf`) at `c79ae2da` and `82190095`, and the 2 on the gate target at `da654563` | 0.5255 (70/70 pass; the 2 cases added at `c79ae2da`, at `da654563`: 0.4879 (2/2 pass; worst `teletext-chop-ass`)); instructions on A32 (`armv7-unknown-linux-gnueabihf`) at `c79ae2da` against main at `339ec261`: 0.99754 of main's in sum, no case above main beyond the run spread | heap ≤0.626, total RSS ≤0.804 (70/70 lower); the 2 cases added at `c79ae2da`, at `da654563`: heap ≤0.1276, total RSS ≤0.7512 (2/2 lower, kernel-bound) |

The runtime and memory of fonts and shaping are not verified on ARMv7. The subtitle,
caption and teletext rows are timed and measured against `ffmpeg-subtitles-plain-native`,
the subtitles reference built without the wrappers that flag target-dependent results, so
no flagging code runs in the reference's timed span or memory window.

The 13 `mov-text-tags` text cases were measured at `dd395b41` against `subtitle-text`'s
reference, `ffmpeg-formats-native`, on a c6g.4xlarge pinned to CPU 1, as a slice of the
feature: 30 timing pairs, then 30 memory pairs under a kernel RSS proof made in that
checkout. `cargo xtask grade` does not join a slice, so their bounds come from each run's
report.

At `3df7ae6d` the total RSS gate failed in three subtitle features, in the cases above the
reference. The four `mpeg2-a53-limit-ts` captions were above it in all 120 of their pairs
(1.0720 to 1.0822): the candidate started the operation with about 1,223 resident pages for
the transport stream, against about 838 for the same video as an elementary stream, and grew
by 8 to 22 pages; the reference started with about 1,157 and 1,090 and did not grow.
Preparation read the transport stream's whole video stream (1.57 MB of PES payload) into
memory before extracting its captions. It now extracts them as the packets are cut, as it
does for the elementary stream: at `4dbbd545` the transport stream case starts the
operation with 857 resident pages, against 1,207 before and 853 for the elementary stream,
and at `4fc96a49` the four cases
measure 0.7784 to 0.7882 and all eight `mpeg2-a53-limit` cases at most 0.7984. The text case
`charset-cp950-nomapping-a240` (1.0045) and the PGS case `long-movie-mkv` (1.0051) are above
it in 9 and 16 of their 30 pairs; their median peaks are within 16 pages of the reference's
(534 against 550, and 1,680 against 1,677). Collected again alone, 30 pairs each on the same
host from an empty page cache, they give 0.9752 (above in 3 pairs) and 1.0012 (above in 8);
the host reclaimed no memory during either collection. At `4fc96a49`, collected after the
closed captions on the third host, they give 0.9713 (above in 4 pairs) and 0.9908 (above in
3). The causes are not established; the table keeps the first collection, and the raw
samples hold all three.

The 12 cases added to `subtitle-dvb`, with the three `sequence` cases, were measured on the
gate target at `74155089` on a c6g.4xlarge pinned to CPU 1, as a slice of the feature: 30
timing pairs, then 30 memory pairs under a kernel RSS proof made in that checkout. `cargo
xtask grade` does not join a slice, so their bounds come from each run's report.

The DVD decoder gives the numbers of a VobSub configuration and of the `palette` option the
values observed in FFmpeg built for ARMv7: a colour list's numbers (the `palette` option and
a `palette:` line) wrap modulo 2³², and one beyond 32 bits is white (`0xffffff`); a `size:`
number beyond 2³¹ − 1 or below −2³¹ refuses the size; and a `.idx` `delay:` line is a `-`
after spaces negating the whole delay, then up to four fields that saturate at 2³¹ − 1 and
−2³¹, missing fields zero. FFmpeg built for AArch64 keeps the low 32 bits of such numbers, or
all 64 for a colour, instead. The probes these values come from are in the raw samples of
`8513398b`. `subtitle-dvd-host` pins the cases
that reach them per target and holds four of them open on AArch64, each target-dependent
by design: `vobsub-palette-option`, which `subtitle-dvd` held before and which passed there,
and the cases `vobsub-idx-size-beyond-32-bits`, `vobsub-idx-size-negative-beyond-32-bits`
and `vobsub-idx-delay-beyond-32-bits`; its fifth, `vobsub-idx-size-int-min`, both targets
refuse. A decoder that refuses to open is a result both workers fold and compare.

The 8 cases added to `subtitle-dvd`, `subtitle-pgs` and `subtitle-teletext` were measured on
the gate target at `da654563` on a c6g.4xlarge pinned to CPU 1, as a slice of each feature:
30 timing pairs, then 30 memory pairs under a kernel RSS proof made in that checkout. `cargo
xtask grade` does not join a slice, so their bounds come from each run's report.

Above the ceiling on ARMv7: capture `c07-ui-static-480-bottomup` 2.2962,
`c01-ui-static-960` 2.2117, `c10-ui-transition-480-topdown` 2.0288, `c03-ui-transition-960`
1.9567, `c08-ui-scroll-480-bottomup` 1.3932, `c13-motion-960-6m` 1.3757,
`c14-motion-960-rate255` 1.3709, `c02-ui-scroll-960` 1.3351, `c04-motion-960` 1.3273,
`c06-ui-scroll-960-bottomup` 1.3189, `c09-motion-480-bottomup` 1.3055, `c12-motion-960-1m`
1.2281. The closest container, playback and subtitle cases are `info-date-latest` (Matroska
segment metadata) 1.1438, `hevc-irap_repeat` (packet framing) 1.0706,
`progressive-ten-sequential-batches` (MP4 forward) 1.0480, `vob-split` (DVD) 1.0389,
`rle-long-rows-sup` (PGS) 1.0021 and the ISO BMFF output
`order27-u64-last-scale1000-cat21-native-index-tiny` 1.0009. The raw samples'
`report.json` lists each bound.

The codec timings are not gated. On a c6g.4xlarge pinned to CPU 1, in the same user
space and against the same pinned reference, the AC-3, E-AC-3 and DTS workloads were
timed at `ddb02a59` and the five AAC workloads at `0b3c2db5`. Ten of the fourteen audio
workloads are above the ceiling: `dts-core-7.1-master` 2.0698, `dts-core-es-6.1` 2.0338,
`aac-he-5.1` 1.7656, `aac-he-stereo` 1.7092, `aac-hev2-stereo` 1.6377, `ac3-5.1-448`
1.3764, `ac3-2.0-192` 1.3716, `eac3-stereo-spx` 1.3684, `eac3-5.1-spx` 1.3292 and
`eac3-7.1` 1.2933, while `aac-lc-5.1` 1.1423, `ac3-4.0` 1.0948, `eac3-5.1-1536` 0.9794
and `aac-lc-stereo` 0.9761 are within it. The six video workloads, at `3df7ae6d`, are
0.5945 to 1.1236. Before `ddb02a59` the instruction set moved audio and video in opposite
directions. At `22ed8232`, built for
`armv7-unknown-linux-gnueabihf` (A32 without NEON; a comparison build) and timed against
the same FFmpeg reference, the codec worker's upper bounds are `h264-high-1080p` 1.5231,
`aac-lc-stereo` 1.1610, `ac3-2.0-192` 1.5995,
`eac3-stereo-spx` 1.4897 and `dts-core-es-6.1` 1.7571, against 1.1214, 1.7653, 2.0521,
2.2204 and 2.0400 in the gate build at that commit: the video decoders gain from the gate
target, the audio decoders lose. A32 with NEON (`-C target-feature=+neon`) gives 1.1035,
1.0730, 1.5231, 1.3929 and 1.6803 on the same workloads, and Thumb-2 without NEON 1.5587,
1.7915, 2.0801, 2.2523 and 1.9084: NEON brings the video gain and the Thumb-2 encoding the
audio loss. No Rust target compiles A32 with NEON under default flags.

Most of that audio loss was the downmix's conversion to s16. ARMv7 has no instruction
for `f32::floor`, so the conversion called the `floorf` linked into the worker, which
moves each sample from the VFP registers to the core registers and back and, as Thumb-2,
mispredicts its branches: at `f3d4935b`, 8 decodes of `aac-lc-stereo` miss 7.9 M branches
in the gate build and 0.83 M in the A32 with NEON build. `mix::to_s16` now converts toward
zero, steps down where that rounded up and saturates in integers. It gives the floor's
result for every one of the 2^32 `f32` values, and `cargo xtask codecs equivalence`
records the same 574 decoder outputs as at `f3d4935b`. On the same host, the gate build
at `ddb02a59` against `f3d4935b` (instruction and cycle counts with `cargo xtask count`,
runtime as the upper bound over 30 pairs); built as A32 with NEON, `ddb02a59` likewise
runs 2.5 to 18.5% fewer instructions than `f3d4935b` on every workload:

| Workload | Instructions | Cycles | Runtime, `f3d4935b` | Runtime, `ddb02a59` |
|---|---:|---:|---:|---:|
| `aac-lc-stereo` | −20.88% | −45.72% | 1.7788 | 0.9644 |
| `aac-lc-5.1` | −8.90% | −22.04% | 1.4580 | 1.1380 |
| `ac3-2.0-192` | −14.26% | −31.47% | 2.0115 | 1.3716 |
| `ac3-4.0` | −8.82% | −24.34% | 1.4443 | 1.0948 |
| `ac3-5.1-448` | −5.29% | −8.04% | 1.4936 | 1.3764 |
| `eac3-stereo-spx` | −15.73% | −37.27% | 2.1990 | 1.3684 |
| `eac3-5.1-spx` | −6.27% | −17.26% | 1.6029 | 1.3292 |
| `eac3-7.1` | −3.85% | −11.07% | 1.4502 | 1.2933 |
| `eac3-5.1-1536` | −3.61% | −3.33% | 1.0104 | 0.9794 |
| `dts-core-es-6.1` | −2.89% | −6.05% | 2.2072 | 2.0338 |
| `dts-core-7.1-master` | −3.20% | −7.86% | 2.2418 | 2.0698 |

200 decodes of `aac-lc-stereo` take 4.35 s in the gate build, against 7.57 s at
`f3d4935b`, and 3.91 s in the A32 with NEON build, against 4.79 s, all with the same
digest. Recorded with `perf` (user-mode cycles and instructions), `floorf` and
`Downmix::mix` took 4,555 M and 2,077 M cycles in the gate build at `f3d4935b`;
`Downmix::mix` now takes 852 M, against 719 M in the A32 with NEON build. The gate
build takes 8.54 G cycles in all, 11% more than the A32 with NEON build's 7.67 G, against
14.89 G and 9.50 G before; the rest of the difference is spread over the decoder's
functions. The raw samples hold the recordings and the per-symbol reports.

The five AAC workloads, HE-AAC v1 and v2 among them, were measured again at `0b3c2db5`,
which merges `b89e5844` into the change, against `b89e5844` on one host. The 577 decoder
outputs, which now include the HE-AAC workloads' mixes, are byte-identical. Every audio
workload runs 2.2 to 20.9% fewer instructions in the gate build, and 1.8 to 18.5% fewer
built as A32 with NEON. HE-AAC gains less than AAC-LC because SBR is most of its work:

| Workload | Instructions | Cycles | Runtime, `b89e5844` | Runtime, `0b3c2db5` |
|---|---:|---:|---:|---:|
| `aac-lc-stereo` | −20.88% | −45.52% | 1.7838 | 0.9761 |
| `aac-lc-5.1` | −8.90% | −22.01% | 1.4625 | 1.1423 |
| `aac-he-stereo` | −6.10% | −18.96% | 2.1114 | 1.7092 |
| `aac-he-5.1` | −2.17% | −6.90% | 1.8979 | 1.7656 |
| `aac-hev2-stereo` | −6.74% | −20.36% | 2.0549 | 1.6377 |

Recorded with `perf`, 50 decodes of `aac-he-stereo` in the gate build take 13.59 s
against 16.65 s at `b89e5844`, with the same digest. There `floorf` and `Downmix::mix`
took 4,620 M and 2,175 M of 32.65 G cycles; `Downmix::mix` now takes 692 M of 26.57 G,
and `Sbr::process` with the QMF synthesis takes 47%.

Capture conversion must stay within ±1 of FFmpeg's on every sample. On
`c01-ui-static-960`, Triergon's conversion output is byte-identical on ARMv7 and
AArch64, while FFmpeg's ARMv7 conversion differs from its AArch64 output. Across the 15
cases FFmpeg's ARMv7 conversion is up to 36 from Triergon's (on the noise cases), so all
15 fail the rule on this target; the encoder and mux pass.

`raster-sweep` fails `outlines-71` and `outlines-83`: libass silently draws nothing
when its composite allocation fails; the crate returns `Allocation`. Both cases draw
glyph and border bitmaps of 1 to 2 GB each, so their composite does not fit a 32-bit
address space; both sides record the same 265 and 267 packets
and differ only in that record.
Both are listed as open cases on `thumbv7neon-unknown-linux-gnueabihf` only, so they are
compared there and must pass on every other target.
Its heaviest case, `bitmaps-24`, takes about 50 s on each side, within
`raster-sweep`'s 120 s `timeout_seconds`.

The total RSS gate fails on ARMv7 in five container and playback rows: Matroska, startup and seek, packet framing, Matroska audio clocks and MP4 forward.
It is lower in every ISO BMFF output, MPEG-TS operation, HLS segment, TS program and raw
Annex B case, in 15 of the 44 startup and seek cases and in 8 of the 116 Matroska cases;
it is above in every packet framing, MP4 forward and Matroska audio clocks case. Peak heap
is lower than the reference in every case. The difference is the workers' own code. The playback worker is 2,099,860 bytes with 1,430,354 bytes of `.text`; the
FFmpeg worker it is compared with, which links 12 demuxers (`ass`, `h264`, `hevc`,
`matroska`, `mov`, `mpegps`, `mpegts`, `mpegtsraw`, `srt`, `sup`, `vobsub`, `webvtt`), is
1,248,636 bytes with 781,288 bytes of `.text`. One playback worker holds every runner, and
`Engine` links every container backend.

These are the first ARMv7 collections with the page-cache eviction. At `22ed8232`, without
it, how much of each executable was resident at the start of the operation varied between
runs: the playback worker started at a median of about 2.3 MiB, up to 2.8 MiB, where it now
starts at about 1.8 MiB within 1.73 to 1.88 MiB, and the Matroska reference started at about
2.0 MiB, where it now starts at about 1.7 MiB. MP4 forward, TS program, raw Annex B and HLS
are therefore lower than they were, and Matroska higher (8 of 116 cases below the
reference, where 110 were).

The TS program and ISO BMFF memory was collected a second time, on the same host from an
empty page cache. In the first collection the kernel bound recorded, in one run each of two
TS program cases and five ISO BMFF outputs, an update of the measured process's resident
counters by another task, which makes the observation invalid. That host's kernel had
reclaimed about 7.8 million pages through `kswapd` by then; it reclaimed none during the
second collection. The table gives the second collection; both are in the raw samples.

The [measured revisions](#measured-revisions) table names the revision and raw samples
behind each ARMv7 row.

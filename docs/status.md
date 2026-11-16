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
Unless a row names another revision, its runtime and memory come from the full gate run
at `a73350ca` on both targets, and AAC in MOV, which the gate leaves to
`codecs aac-access-units`, from the same measurements at that revision on a host of its
own per target: all on the qualified host class, the AWS c6g.4xlarge (Graviton2). The run
predates the gate's `codecs-audio` item, whose AAC, AC-3, E-AC-3 and DTS correctness lanes
are graded separately at `bbdefa70`. Workers and references link under the
[link policy](quality.md#measuring-performance) `static-toolchain-runtime-trained-order-v1`,
which builds on `static-toolchain-runtime-v1`: both sides link their toolchain runtime
support libraries from their static archives, which changes every Rust worker and the
HarfBuzz and libass references, while the FFmpeg and FreeType references link as before.
A row that names another revision names its host class too
([measured revisions](#measured-revisions)). The trained-order policy links the media, playback and Matroska
playback workers, the framing worker on Linux AArch64 and the transport worker on ARMv7
with the toolchain's `lld` instead of its default linker, the functions their trained
orders list first and the rest in module order, so their results reflect the linker,
the module order and the trained order together. Every other worker and every
reference links as under `static-toolchain-runtime-v1`, byte for byte as before. The
other workers keep their link because a trained order made a row worse or would not
help: the codec worker's file workloads for FLAC, MP3, MP2, Vorbis and Opus took more
resident memory with an order trained on the corpus's AAC, H.264 and HEVC operations
(worst total RSS 0.5416 to 0.5607 on Linux AArch64, 0.6158 to 0.6380 on ARMv7); on
ARMv7 the framing worker's cases kept their resident peaks and two reached the
reference's; on Linux AArch64 three malformed PSI cases of the transport worker, whose
code the corpus does not run, held one more 64 KiB window of code; and the ISO BMFF
worker's operations read only the raw-video and timed-text movies of its comparison.

| Area | Implemented | Verified against the reference | Runtime | Memory | Open |
|---|---|---|---|---|---|
| Core | Charged storage, exact time, input windows, work budgets, neutral container model with one codec identity | 51,235 exact-arithmetic cases against an independent oracle | via dependents | via dependents | fuzzing |
| Matroska / WebM | Incremental reader: lacing, header removal, bounded zlib, Cues and preroll, colour and static HDR, chapters, tags, attachments, side data | 116 workloads in six feature groups, including an external HDR sample, and four host FATE demux comparisons | 1.0495 (116/116 pass: the six feature groups and the external vector) | heap ≤0.6262, total RSS ≤0.8791 (116/116 lower) | full EBML/Matroska scope |
| Matroska legacy AAC-LC | MPEG-2 and MPEG-4 legacy AAC-LC tags, derived initialization, ADTS feed and packet clocks | 4/4 packet folds and 32 framed packets, including a Xiph lace and declared or omitted duration (`matroska-legacy-aac`) | 0.9653 (4/4 pass) | heap ≤0.5227, total RSS ≤0.8085 (4/4 lower) | FATE coverage |
| Matroska startup and seek | Startup and seek with lazy scan replay | 44 cases, 1,344 packets | 0.9094 (44/44 pass) | heap ≤0.4037, total RSS ≤0.6775 (44/44 lower), on `playback-matroska-worker` | FATE mapping |
| Matroska audio clocks | AAC, AC-3, E-AC-3 startup and forward clocks | 10/10 packet folds (`matroska-audio-clocks`) | 0.8700 (10/10 pass) | heap ≤0.8103, total RSS ≤0.7931 (10/10 lower), on `playback-matroska-worker` | audio seek and FATE coverage |
| ISO BMFF / MP4 | Regular and fragmented files, edit lists, seek and range plans, neutral `ContainerReader` over seekable sources, QuickTime compatibility including legacy data handlers and dependency flags in recovery mode, bounded AAC configuration and regular table recovery, timing, sync and terminal media prefixes; AVC, HEVC, AAC, AC-3, E-AC-3, DTS core, AV1, VP9 and Dolby Vision sample entry identification | 255 inputs, 1,020 open and index comparisons (972 matching outputs and 48 matching refusal outcomes with compatibility policies); the neutral `Reader` against the formats `ffprobe` (`cargo xtask codecs isobmff-reader`): 648 cases, 522 inputs read through and 126 seeks, 333 identical, 3 refused by both, 312 differing only by 17 reviewed dispositions, 12 of them FFmpeg behaviours and 5 the reader's own documented refusals of inputs that break ISO/IEC 14496-12 ([rules](../harness/features/isobmff-reader/rules.json), [frozen differences](../harness/features/isobmff-reader/differences.json)) | 0.7978 (956/956 timed operations pass) | heap ≤0.2841, total RSS ≤0.7415 (956/956 lower) | neutral reader: FATE mov/mp4 coverage beyond one sample, codec metadata not compared; seeks and range plans in their own row |
| ISO BMFF seek and range plans | Backward seek on one track after open or after reading to the end, a bounded read after it; recovery-aware source range plans read from their decode start | 476 cases (`isobmff-seek`): 378 seeks with byte-identical traces, 94 range plans with the same source packets, 4 matching refusals, over 8 authored movies of up to 1,800 samples per track and 36 open and index inputs | 0.9062 (472/472 pass) | heap ≤0.9032, total RSS ≤0.7541 (472/472 lower) | seeks the compatibility seek refuses (unequal clocks, overlapping or null index clocks) and those that differ from FFmpeg on 47 open and index inputs (decode times below the presentation start, trimmed edits); range plans on multi-track inputs or where movie-time planning selects other samples |
| MP4 range playback | Regular AVC/AAC and AVC/Opus startup and backward seek, including regular AVC with an initial non-sync prefix or a leading empty edit, fragmented AVC/AAC with open-ended edits, timed text and DVD subpictures including a leading empty subpicture edit, dedicated HEVC and AV1 sample entries, source-declared Opus seek preroll, QuickTime AAC with fixed and program configurations, including regular version-2 descriptions and fragmented version-1 descriptions, and media playback in the presence of RTP hint tracks | 64 matching open and backward-seek comparisons (`mp4-range`), 9,229 packets on AArch64 Linux | 0.9557 (64/64 pass) | heap ≤0.8877, total RSS ≤0.8174 (64/64 lower) | Additional range playback profiles |
| AAC in MOV | Raw AAC-LC access units with 1024 samples per plane and all thirteen indexed rates. Supports fixed configurations 1–7, 11, 12 and 14 and out-of-band programs assigning up to eight distinct speaker positions. | The [configured decoder comparison](../harness/features/aac-access-units/contract.json) covers 34 clips, 3,544 access units and 15,575,040 PCM samples. Two fixed-layout QuickTime clips compare the surround pair in the same plane positions with side or back labels. Every other clip compares exact speaker order. | 0.8534 (34/34 pass) | heap ≤0.6729, total RSS ≤0.4561 (34/34 lower) | 960-sample frames and SBR in raw access units |
| Ogg | Pages with CRC and resync, multiplexed and chained streams, Vorbis, Opus, FLAC, Theora and Speex mappings with canonical configurations, granule timing (Theora split granules, keyframe random access), pre-skip and end trims, bisection seeking across chain links (Theora to the keyframe) | Packets against the pinned `ffprobe` (`cargo xtask codecs readers`, gate item `ogg-readers`, at `4d31df88` on both targets) on 45 files (21 FATE Vorbis, 3 FATE Theora, 3 FATE chained, 13 generated Opus, Vorbis and FLAC, 5 generated Speex: multiplexed, chained, damaged, truncated, with Skeleton), in two views: `compat::clock` 40/45 pass, 5 differ only by reviewed FFmpeg behaviours; the reader's own timing 10/45 pass, 31 differ only where `compat::clock` reproduces FFmpeg, 4 by reviewed behaviours; and 26 seeks (each link of chained files, each track of multiplexed ones, both policies): `compat::clock` 11/26 pass, 15 differ only by FFmpeg seeking within one link of a chained file ([reader rules](../harness/codecs/reader-rules.json), [seek cases](../harness/codecs/reader-seeks.json), [frozen differences](../harness/codecs/reader-differences.json)) | within the codec workloads | within the codec workloads | `b_vorbis.ogg`, `chain_vorbis.ogg`: a negative last duration FFmpeg prints modulo 2^32; `crc_bad.opus`: the first packet after a page that fails its CRC, which FFmpeg times from its own page and `compat::clock` from the stream start; `bear.ogv`: leading Vorbis pages stating granule position 0, which FFmpeg times from the next page; `chain_speex.spx`: a chained link reusing its predecessor's serial, which FFmpeg reads as the same stream; chained seeks: FFmpeg delivers one link only; VP8, Daala and Kate in Ogg untimed |
| Audio elementary streams | Native FLAC (with or without `fLaC`), MPEG audio with ID3v2/ID3v1/APE tags, Xing/Info/VBRI and LAME gapless trims, ADTS, AC-3/E-AC-3; one shared frame-stream engine | Packets as the readers emit them against the pinned `ffprobe` on 271 FLAC, MPEG audio, ADTS, AC-3 and E-AC-3 files (the IETF FLAC testbench, MP3 conformance, generated and crafted damaged streams): 253/271 pass, 16 differ only by reviewed FFmpeg behaviours, 2 refused by both (testbench faulty `06 - missing streaminfo metadata block`, `07 - other metadata blocks preceding streaminfo metadata block`) | within the codec workloads | within the codec workloads | FLAC `flac_s20_rate768k`, `06 - samplerate 768kHz` (no durations at 768 kHz), `01 - changing samplerate`, `27 - old format variable blocksize file created with Flake 0.11`, `11 - file starting with unparsable data`, `11 - incorrect metadata block length`, `08 - blocksize 65536`, `09 - blocksize 1`; crafted `ac3_tags`, `adts_mid_junk`, `flac_corrupt`, `flac_id3`, `flac_mid_junk`, `mp3_foreign_frame`, `mp3_mid_junk_falsesync`; MP3 `he_free` (free format, refused by FFmpeg); E-AC-3 dependent substreams on real files; ADTS and AC-3 seek cost |
| MPEG-TS | PSI, continuity, PES assembly and ordered TS188/M2TS192 entities with control-only and bounded fault completion, with format-owned track classification | 97/97 PES compatibility-view comparisons across all 272 requests | 0.7217 (97/97 pass) | heap ≤0.0653, total RSS ≤0.8651 (97/97 lower) | strict source clocks, full FATE mapping |
| MPEG-TS access units | `ContainerReader` and `AnyReader` for TS188 and M2TS192, with AVC, HEVC, ADTS AAC-LC, LATM in LOAS, signalled AC-3 and E-AC-3, DVB subtitle segments and ordered entities; `Reader` can frame selected PIDs while retaining all source events | 169/169 transport compatibility folds for all tracks in the chosen program, including LATM PES and opaque observations, with all 272 original request associations; explicit PID selection has separate reader tests | 0.9075 (169/169 pass) | heap ≤0.1067, total RSS ≤0.8812 (169/169 lower) | additional carriages, seeking and full FATE mapping |
| MPEG-TS program playback | Selected TS188 program and M2TS192 PID with packet payloads and clock projection | 35/35 packet folds | 0.8518 (35/35 pass) | heap ≤0.0877, total RSS ≤0.7956 (35/35 lower) | additional program and carriage profiles |
| MPEG-TS progressive playback | TS188 with AVC and AAC-LC, bounded sequential input and access units before EOF | 3 packet folds and all 5 functional inputs with 10 literal outcomes | 1.0435 (3/3 pass) | heap ≤0.0151, total RSS ≤0.6197 (3/3 lower) | broader transport and codec profiles |
| HLS segments | Independent TS188 segments with contiguous AVC or HEVC video and AAC, AC-3 or E-AC-3 audio | 71 packet comparisons, 71 measured workloads | 0.8026 (71/71 pass) | heap ≤0.0207, total RSS ≤0.7318 (71/71 lower) | other profiles |
| Packet framing | AVC/HEVC configuration and NAL framing, AV1, VP9, AC-3, E-AC-3, DTS core | 135/135 AVC/HEVC reference comparisons; DTS core byte-exact on 37,125 projections and 1,172 FATE packets, recorded without a timing runner | 1.0962 (135/135 pass for AVC/HEVC) at `d915aa19` | heap ≤0.8367 (135/135 lower), total RSS fails at 1.0267 (134/135 lower; `avc-bad-update`) at `d915aa19` | AArch64 total RSS, ARMv7 total RSS, full AVC/HEVC/ADTS grammar |
| Subtitles, text | SubRip and WebVTT documents as FFmpeg's `srt` and `webvtt` demuxers read them, ASS packets as its `ass` decoder decodes them, with `sub_charenc` charsets, and mov_text and tx3g to ASS | 367/367 cases (`subtitle-text`: 82 captured mov_text cases, 13 authored mov_text sessions for the override tags written where a style record or the highlight starts or ends, the samples of the 79 tx3g tests, 80 charset cases, 17 ASS packets, 96 SubRip and WebVTT documents) on Linux AArch64 and [32-bit ARMv7](#32-bit-armv7); `subtitle-text-host` pins per target the 23 SubRip and WebVTT documents whose time fields leave the 32-bit range: 23/23 pass on ARMv7, 0/23 on AArch64 (open) | 1.0271 (367/367 pass; worst `charset-windows-1251-text`) | heap ≤0.3360, total RSS ≤0.9311 (367/367 lower) | on AArch64, `subtitle-text-host`'s 23 cases, a deliberate divergence: FFmpeg for AArch64 reads a SubRip time field beyond the 32-bit range as its value, held to the 64-bit signed range, modulo 2^32, and a WebVTT field whose digits exceed 2^32 − 1 as those digits, held to 2^64 − 1, modulo 2^32, where on 32-bit ARM both saturate; the reader follows 32-bit ARM, so these cases are not timed or measured on AArch64; FATE coverage |
| Subtitles, PGS | Presentation, palette and object segments to subtitle rectangles, with FFmpeg's recovery from damaged and incomplete segments | 319/319 cases (`subtitle-pgs`: TS, SUP and Matroska), on Linux AArch64 and [32-bit ARMv7](#32-bit-armv7) | 0.8098 (319/319 pass) at `99f1027b` | heap ≤0.9654, total RSS ≤0.9596 (319/319 lower) at `99f1027b` | FATE coverage |
| Subtitles, PGS capsules | Recorded PGS packet sessions through `compat::pgs::ReceiverDecoder`, with raw PTS, DTS and time base, the forced-only option and hard resets that reopen the decoder | 229/229 cases (`subtitle-pgs-capsules`: capsules of up to 64 complete PGS packets against FFmpeg's `pgssub` decoder), on Linux AArch64 and [32-bit ARMv7](#32-bit-armv7) | 0.7248 (229/229 pass) | heap ≤0.5788, total RSS ≤0.7373 (229/229 lower) | — |
| Subtitles, DVB | Display definition, page, region, CLUT and object segments to bitmap rectangles, including `compute_clut` (−2 keeps a region's first automatic palette until a new epoch) | 241/241 cases (`subtitle-dvb`: TS and Matroska), on Linux AArch64 and [32-bit ARMv7](#32-bit-armv7) | 0.9036 (241/241 pass; worst `long-text-mkv`) | heap ≤0.7116, total RSS ≤0.8029 (241/241 lower) | FATE coverage |
| Subtitles, DVD | SPU assembly and control sequences, run-length raster, palettes from VobSub `.idx`, Matroska and MP4 private data, DVD program streams, HD SPU | 233 cases: `subtitle-dvd` 228/228 on Linux AArch64 and [32-bit ARMv7](#32-bit-armv7), and `subtitle-dvd-host`'s 5, pinned per target, 5/5 on ARMv7 and 1/5 on AArch64 | 0.7898 (228/228 pass) | heap ≤0.9590, total RSS ≤0.8565 (228/228 lower) | FATE coverage; on AArch64, `subtitle-dvd-host`'s 4 open cases, target-dependent by design: FFmpeg there reads numbers of a VobSub configuration and of the `palette` option beyond 32 bits with 64, and the decoder follows FFmpeg on 32-bit ARM |
| Closed captions | CEA-608 captions from A/53 and SCTE 20 user data in MPEG-2 video, A/53 SEI in H.264 and HEVC, and SCC files, to ASS; H.264 and HEVC pictures in decoded-picture output order (`triergon_framing::picture_output`) | 461/461 cases (`subtitle-cc`, the fold and the prepared packets' times; H.264 and HEVC in MP4 and Matroska with parameter sets only in `avcC` or `hvcC` among them), on Linux AArch64 and [32-bit ARMv7](#32-bit-armv7) | 0.6555 (461/461 pass); whole worker runs over the 112 inputs at `adaa4bc7` take 0.921 of main's instructions in sum (main at `03cc8d3f`) | heap ≤0.1227, total RSS ≤0.7473 (461/461 lower) | FATE coverage; not reproduced: H.264 durations in Matroska without durations when the SPS states a bitstream restriction, after a start hold of depth 3 or more, after a second B picture dropped as late, and at ticks between 1 and 2 ms where the reference picks a multiple of the frame rate; H.264 durations from VUI `pic_struct`; H.264 recovery with `recovery_frame_cnt` 5 in B pyramids; the H.264 start hold when the depth reaches 3 from 1; H.264 `frame_num` going back without an IDR picture; a packet with neither PTS nor DTS; H.264 in MP4 with PTS equal to DTS on every packet while the stream reorders (`h264-hold-depth1` remuxed by FFmpeg, not in the corpus), where the reference prepares 299 caption packets to the extractor's 301, with or without in-band parameter sets, while the same stream in Matroska matches |
| Teletext | DVB teletext pages to text and ASS, as FFmpeg's libzvbi 0.2.46 decoder emits them | 72/72 cases (`subtitle-teletext`), on Linux AArch64 and [32-bit ARMv7](#32-bit-armv7) | 0.4261 (72/72 pass) | heap ≤0.6212, total RSS ≤0.7032 (72/72 lower) | FATE coverage |
| Playback | Matroska, MP4, TS and raw AVC/HEVC feeds; seeking; end-of-file continuation | MP4 terminal-media matrix against recorded FFprobe 9.0.2 output (evidence set `playback-terminal-matrix-ffprobe`): 39 inputs in 13 source geometries (12 nonseekable for every input, 1 seekable for the 31 original inputs), 499 rows, each run in debug and in release. `Mp4Open::new_recovery` 499/499 pass: 497 identical packet lists, and 2 seekable reads of a zero-size fragment sample that the opener refuses in the header, where FFprobe also refuses. `Mp4Open::new` 446/499 pass. Packet folds: MP4 forward 51/51 (`mp4-forward`), 769 packets, TS program 35/35 (`ts-program`), raw Annex B 7/7 (`raw-annex-b`) | MP4 forward 0.8043 (51/51 pass), TS program 0.8518 (35/35 pass), raw Annex B 1.0021 (7/7 pass) | MP4 forward heap ≤0.6755, total RSS ≤0.8501 (51/51 lower); TS program heap ≤0.0877, total RSS ≤0.7956 (35/35 lower); raw Annex B heap ≤0.1274, total RSS ≤0.6986 (7/7 lower) | other profiles; legacy Matroska `A_AAC/MPEG4/*` codec IDs are not read as AAC |
| Codecs | H.264 8-bit 4:2:0; HEVC Main and Main10, both with the decoded picture buffer of `triergon_framing::picture_output`; AAC LC, HE, HEv2 (Parametric Stereo from open publications and probes); AC-3 and E-AC-3; DTS core with XCh and X96; storage charged to a budget | 417/417 comparable conformance and FATE vectors; the AAC lane's three crafted Parametric Stereo streams of paths no FATE vector takes (a 20/34-band change registered while PS is off, IPD/OPD across a change, a PS frame without its SBR element); the audio correctness lanes as gate item `codecs-audio` at `bbdefa70` on both targets, the 71 derived streams among them reproduced byte for byte by `cargo xtask codecs derive`: AAC 38/39 and one open case, AC-3 6/6, E-AC-3 6/6, DTS core 22/22, and `xll_51_24_48_none` out of set (no core substream) | 1.0322 (20/20 pass; worst `aac-he-stereo`) | heap ≤0.9538, total RSS ≤0.8337 (20/20 lower) | error concealment, unverified tool paths; `aac-sce-in-stereo`, open: FFmpeg leaves the FR plane of single-SCE frames unwritten (FR RMS 0.0853, FL equal; the stereo output passes); four open AAC Parametric Stereo and SBR cases (`harness/codecs/aac/ps-open-manifest.json`); AAC charges in-band reconfiguration when it appears, and a refused access unit leaves the decoder unchanged unless an earlier raw data block or Main-prediction element of it was decoded |
| Audio codecs in files | FLAC (RFC 9639, 4 to 32 bits, 1 to 8 channels); Opus (RFC 6716 and 8251: SILK, CELT, hybrid, families 0, 1 and 255); Vorbis I; MPEG-1, 2 and 2.5 audio Layer I, II and III | 315/321 pass through the container readers (FATE Vorbis, Opus and MP3 conformance, the IETF FLAC testbench, generated streams): FLAC bit-exact, the others within the [numeric rules](../harness/codecs/numeric-rules.json) | 1.1051 (10/10 pass; worst `opus-5.1-256`) | heap ≤0.9041, total RSS ≤0.7164 (10/10 lower) | FLAC `01 - changing samplerate`, `02 - increasing number of channels`, `03 - decreasing number of channels` and MP3 `he_mode` change format mid-stream, which the CLI converts; FLAC `flac_3ch_odd_mask`, whose layout FFmpeg refuses on its stereo output; MP3 `he_free` (free format: FFmpeg finds no two consecutive frames, and the decoder refuses free-format frames), refused by both; runtime toward the target for Opus and Vorbis (at `ccff5b57` on a c8g.2xlarge: Opus music 1.0301 and 5.1 1.0834, Vorbis 5.1 0.9356 and stereo 0.8423, Opus speech 0.8719); Opus families 2 and 3 |
| Decode sessions | Bounded queues, clock-gated output, flush and seek, RGBA letterbox, stereo mix; container track decoding with trims and start delays | with the codecs | with the codecs | with the codecs | — |
| Fonts | sfnt and collections; `head`, `hhea`, `maxp`, `hmtx`, `vhea`, `vmtx`, `OS/2`, `name`; `cmap` formats 0, 2, 4, 6, 8, 10, 12, 13, and the glyph-name and encoding charmaps FreeType adds to CFF faces, in its charmap order; `glyf`/`loca`; CFF and CFF2 at the default instance; `kern` format 0; loading at a size as libass does with hinting off (unhinted outlines, synthetic bold and oblique, size metrics, kerning, decoration bands, default-load advances where hinting cannot move them, and the vertical advances libass draws and shapes the glyphs of `@` fonts with); FreeType's default load of CFF, CFF2 and CID-keyed glyphs, hinted by Adobe's hinter as FreeType's CFF driver hints them, of TrueType glyphs without bytecode, through FreeType's auto-hinter with its Latin and dummy writing systems, and of instructed TrueType glyphs, through a TrueType interpreter running their programs as FreeType's does in backward compatibility mode, with the grid-fitted metrics of each; face matching with libass's rules (compared through the renderer's `ass-render` feature) | The gate covers libass's default configuration (hinting off). All 70 faces of 53 corpus files (TrueType including Arabic, Hebrew, Indic and Thai fonts, name-keyed and CID-keyed CFF, CFF2, a 10-face collection, six authored fonts, among them a face for each way FreeType reads vertical metrics and a CFF face with each charmap FreeType adds; 836,345 glyphs) identical to FreeType: names, every charmap FreeType lists with its mappings, libass's text charmap, the kerning of every listed pair, advances and unscaled outlines (`font-outlines` 53/53); at 256 pixels, size metrics, kerning, every glyph's unhinted outline with its synthetic styles, default-load advances, and its linear and scaled vertical advances unhinted and of the default load (`font-hinting-none` 159/159); at that size and at 15.625 and 10 pixels (26.6 heights 1000 and 640), every glyph's default-load metrics and hinted outline, which libass's shaper reads for advances, extents and contour points (`font-hinting-default` 159/159 at `69d4d0f8`: every CFF, CFF2 and CID-keyed face, the auto-hinted authored-cmap-formats and DejaVuMathTeXGyre, and the 43 instructed TrueType files); Linux AArch64 and [32-bit ARMv7](#32-bit-armv7), each with its own FreeType build, and macOS AArch64 before the vertical cases | not measured | not measured | libass's hinting-enabled modes (NATIVE, LIGHT, NORMAL): their nine other sizes are measured, not gated (`font-hinting-modes` 954/954 at `9d14e98c` on both targets, unhinted outlines and default-load advances), and the hinted outlines those modes draw are not compared; the auto-hinter's Indic writing system, for the hinted outlines of the glyphs it styles; the TrueType interpreter's unit vectors set along a line or from the stack (`SPVTL`, `SFVTL`, `SDPVTL`, `SPVFS`, `SFVFS`), whose normalisation matches FreeType's for most directions but not all: FreeType's carry an error of about 2⁻¹⁶ that depends on the direction and on the vector's magnitude within its power of two, which no fixed-point rule tried reproduces (`vectors-normalization`); no corpus glyph differs for it at the compared sizes; the default load at sizes other than 256, 15.625 and 10 pixels (not compared); tricky fonts and programs that leave backward compatibility (`INSTCTRL` selector 3); `cmap` format 14; runtime and memory |
| Shaping | OpenType layout (GDEF, every GSUB and GPOS lookup type, feature variations at the default instance, `kern` format 0 fallback, fallback mark positioning) with HarfBuzz's default, Arabic, Hebrew, Thai/Lao, Hangul, Indic, Khmer, Myanmar and Universal shapers for horizontal runs; every BCP 47 language mapped to OpenType language systems as HarfBuzz maps it; libass's FreeType-backed font functions over a sized face, with the rotated advances of `@` fonts, the empty extents of glyphs without points, and the default-load extents and contour points of CFF and TrueType glyphs; the CFF and CFF2 glyph extents of HarfBuzz's own font functions; bidi from UAX #9 with FriBidi's results, resolved and reordered as libass does; libass's simple shaping level; Unicode properties generated from pinned UCD files; planning, shaping and bidi bounded by work limits | Identical to HarfBuzz and FriBidi on Linux AArch64 and [32-bit ARMv7](#32-bit-armv7), and on macOS AArch64 before the charmap face joined the shaping corpus. Gating: `shape-libass` 209/209 at `69d4d0f8`, 3,453 runs over 27 corpus files and 22 scripts shaped as libass does at 256 pixels with hinting off; `shape-bidi` 24/24; `shape-simple` 6/6; `shape-unicode` 1/1 (every code point); Unicode 14.0.0's `BidiTest.txt` (770,241 cases) and `BidiCharacterTest.txt` (91,707) all pass. Coverage at units per em: `shape-text` 209/209 and, on demand, `shape-text-all` 1,790/1,790 at `69d4d0f8` (104,796 runs over every corpus face), with CFF and CFF2 extents as HarfBuzz's own charstring reading gives them, and `shape-extents` 9/9, every glyph's extents of every face of the corpus's CFF, CFF2 and collection files (699,045 glyphs) | not measured | not measured | the hinted glyph extents and contour points of glyphs FreeType's auto-hinter gives an Indic style, which libass reads from FreeType to place marks; vertical directions (libass shapes `@` fonts horizontally); AAT tables and Apple `kern`, and OpenType `kern` formats 1, 2 and 3 and cross-stream kerning, which HarfBuzz applies (faces that need them are refused); `stch` stretching; variable instances other than the default; runtime and memory |
| Rasterising | libass's outline and bitmap stages: conversion from font outlines, rotation by 90°, affine and perspective transforms, border stroking, the 16×16 tile rasteriser, the glyph transform and its quantisation against a run with borders and opaque boxes, the karaoke leftmost x, `\blur` and `\be`, composition of fill, border and shadow, the additive and multiplying blends, shadow shifts and rectangle clipping with the karaoke split; storage charged to a budget, work to a `WorkBudget`, bitmaps bounded by libass's own size limit | Identical to libass's plain C bitmap engine (what libass runs on 32-bit ARMv7, pinned on every host), with glyphs loaded by `triergon-font` as libass loads them with hinting off. Gating: `raster-glyphs` 12/12 pass (576 operations over 8 glyphs of a file of each outline format and an authored font at 13.33 and 256 pixels) and `raster-outlines` 2/2 pass (1,019 operations on 200 random outlines), both with transforms of moderate reach, and `raster-bitmaps` 2/2 pass (560 operations on 80 random bitmap sets, a tenth of their blurs and `\be` passes beyond libass's clamps, among them radii it still blurs); on demand, `raster-sweep` 303/303 pass (72,445 operations: every corpus file at three heights with 30 glyphs each, 4,500 random outlines, 3,000 bitmap sets, with transforms as far as libass draws them). Platforms: macOS AArch64, Linux x86_64 and Linux AArch64 all four pass (12/12, 2/2, 2/2, 303/303); [32-bit ARMv7](#32-bit-armv7) passes the three gating features (12/12, 2/2, 2/2) and `raster-sweep` 301/303: on `outlines-71` and `outlines-83` libass silently draws nothing when its composite allocation fails; the crate returns `Allocation` (open cases on ARMv7, compared and passing elsewhere). `raster-simd`, the required cases against the host's SIMD engine, records where it departs: NEON on macOS and Linux AArch64 14/16 pass (4 of the 80 blur cases depart, all with `\be` passes); AVX2 on Linux x86_64 0/16 (AVX2: every bitmap differs in row padding; visible-pixel comparison needs its own fold) | not measured | not measured | the overlap and allocation logic of vector clips at image-list level, which belongs to the renderer; libass's large-tile build and its SIMD engines (NEON blurs `\be` over whole 16-byte rows into the padding; AVX2 pads rows to 32 bytes); runtime and memory; `outlines-71` and `outlines-83` on ARMv7, open there: libass silently draws nothing when its composite allocation fails, the crate returns `Allocation`; a visible-pixel fold for `raster-simd` |
| ASS parsing | Whole ASS and SSA scripts and Matroska packet streams as libass reads them: headers, styles, events, style overrides, embedded fonts; codec private, packets with ReadOrder deduplication, flushing and pruning; typed override tags with libass's argument rules; every glyph's state at a time (animation, movement, fades, clips, karaoke tags, transition effects); karaoke syllables from laid-out positions; the drawing language into `triergon-raster` outlines; storage charged to a budget, work to a `WorkBudget` | Identical to libass on 54 authored fixtures, on macOS AArch64 and, with each target's own libass build, on aarch64-unknown-linux-gnu and thumbv7neon-unknown-linux-gnueabihf: `ass-parse-scripts` 48/48 pass (every track, style and event field, 87 styles, 222 events, embedded fonts, one script without a track), `ass-parse-chunks` 4/4 pass (10 track states, ReadOrder deduplication by bitmap and by scan, flushes, prunes, fonts in a codec private and in more data), `ass-parse-events` 21/21 pass (695 event walks, 6,976 glyph states and karaoke results, 48 drawing and clip outlines, 3 of them rejected as libass rejects them). `ass-parse-host` holds the 15 operations whose libass result differs by target, each pinned per target: on thumbv7neon-unknown-linux-gnueabihf 13/15 pass with 2 open, on aarch64-unknown-linux-gnu 11/15 pass with 4 open; elsewhere all 15 are reported. The reference flags every conversion from `double` to `int` out of range or for NaN (drawing coordinates, `\pbo`, an animated `\clip` or `\iclip`, `\fade`'s alpha, a `Banner` or `Scroll` delay) and a drawing coordinate on `INT_MIN`, and a flagged case fails any other feature. 2,728 mutated fixtures (`fuzz --contract`, 30 rounds of each feature, with the reference for all but `ass-parse-host`) gave identical work in both workers | not measured | not measured | `long`-width behaviour follows each target's libass, and out-of-range drawing coordinates are rejected as libass does on 32-bit ARM with GCC; every other out-of-range conversion from `double` to `int` saturates as there. Open as deliberate divergences: a coordinate landing exactly on `INT_MIN`, which libass built with GCC accepts and the crate rejects (`drawings-int-min-x` and `drawings-int-min-y`, on both targets), and `drawings-infinite` and `drawings-int-min` on aarch64-unknown-linux-gnu, whose coordinates libass converts with a 64-bit `long` there; the font description a glyph asks for (`GlyphState::font`) and karaoke run starts from style runs, shaping and wrapping are compared through the renderer (`ass-render`), not here; selective style overrides, track features, `ass_step_sub`; runtime and memory |
| ASS rendering | `Renderer`: the events shown at a time to positioned alpha images as libass's `ass_render_frame` draws them with hinting off and complex shaping: font selection and fallback over attached, embedded and default fonts, shaping and bidi, wrapping, alignment, positioning, scrolling, collisions kept across frames, change detection, glyph and border bitmaps, blur and shadows, clips, karaoke and the image list; storage charged to a budget, and all the work of a frame (evaluation, font selection, shaping, outlines, layout, drawing and collisions) to one per-frame bound, `Settings::work` | Identical to libass on Linux AArch64 and [32-bit ARMv7](#32-bit-armv7), each with its own libass build, and on macOS AArch64 before the cases of `@` fonts, unregistered languages, glyphs without points, the charmap retry, two `@` font styles and vector clips without a contour or beyond libass's reach (macOS was not run with them), configured as a player embeds it (attached fonts, a default family and font file, no system font provider, no hinting, storage smaller than the frame): `ass-render` 100/100 pass over 47 authored scripts and 3 Matroska packet streams (ReadOrder deduplication, flushes, prunes, an embedded font) at 1920×1080 over 1280×720 storage and 1280×720 over anamorphic 720×576 storage, at each fixture's frame times; every image's kind, position, size, colour and alpha bytes, and every frame's change value as `ass_detect_change` reports it (802 frames, including underline and strike-out lines on TrueType, CFF and CFF2 faces with synthetic styles, borders and `@` fonts, frames after a flush and after pruning every event, opaque boxes around different glyphs that draw nothing, vector clips whose drawing has no contour, which empty their event's images, and one whose drawing libass cannot place, which it does not apply, frames whose caches overflow and are cut, C1 controls drawn through a fallback face's Macintosh charmap that the face keeps for later lookups, a style font size of 0, whose glyphs FreeType loads unscaled, `@` fonts drawn rotated and shaped with their vertical advances, a `Language` header HarfBuzz does not register, fallback mark positioning that measures a glyph without points, and a fallback face whose characters libass finds by trying its format 2 and 8 subtables, the Unicode charmap FreeType builds from its glyph names and its CFF encoding in FreeType's order, and two styles of one `@` font that select the same face, whose rotation decisions libass keeps apart); the box behind a `BorderStyle` 4 event, whose type libass leaves uninitialised, is told by its own buffer, also after a collision shift cuts rows off its top, and compared as a type of its own; colliding events whose boxes lie 2^31 or more rows apart, each within `int`'s range (`collision-far-margins.ass`), placed as glibc's merge sort orders them under libass's overflowing comparison, with the far event clipped away, whose 26.6 position leaves `int`'s range and which libass for AArch64 alone draws inside the frame; with libass's cache limits at 1000 glyphs and 24 MiB; `ass-render-host` pins per target the rendering whose libass result C leaves to the target (`border-x-overflow.ass`, a box width that doubles a huge border in wrapping `int`, and `collision-far-tops.ass`, colliding events whose boxes end up 2^31 or more rows apart, sorted as glibc's merge sort sorts them with libass's overflowing comparison): 4/4 pass on ARMv7 | 1.0767 (100/100 pass) | heap ≤0.9485, total RSS ≤0.9972 (100/100 lower) | on AArch64, `ass-render-host`'s 4 cases, a deliberate divergence: libass built with GCC for aarch64-unknown-linux-gnu gives an event box value outside the signed 32-bit range (its top, height, left or width) its low 32 bits, where libass on 32-bit ARM saturates it, and the renderer follows 32-bit ARM, so they are not timed or measured there; settings other than frame and storage size are unverified; libass's simple shaping and hinting modes, selective overrides and track features; of 5,182 generated scripts and 540 fixture mutations rendered frame by frame against libass for ARMv7, 48 scripts and no mutation differ, each for a stated cause: the renderer stops at the default `Settings::work` bound (17), which on ARMv7 is about 9 s of rendering for one frame (a median 33 ns per unit); with the bound lifted, 34 of those frames take 7 to 21 s each there and 110 run out of the 32-bit address space, so the bound refuses only frames no player could show in time; it runs out of the 32-bit address space (19 allocations refused, 8 at the worker's budget) where libass for ARMv7 peaks at 2.6 to 4.1 GB; and in 4, and in earlier frames of 2 of the above, libass for ARMv7 alone departs from libass for AArch64 at 3.7 to 4.0 GB of its 4 GB address space, and the renderer matches libass for AArch64 (`random-2258` from frame 46). Against libass for AArch64, 113 scripts and 1 mutation differ: 79, the mutation among them, that are identical on ARMv7 and differ there through libass's 64-bit `long` narrowing and its low 32 bits of event box values outside the signed 32-bit range, both deliberate divergences; 34 at the work bound, where libass peaks at 3.5 to 23 GB; and `random-1428`, where libass is killed for memory and the renderer refuses at its budget |
| Capture | RGBA to YUV 4:2:0 as FFmpeg's ARMv7 build converts it, MPEG-1 video with budgeted storage, MPEG-TS mux | at `576f088f`: mux and encoder pass 15/15 against FFmpeg 9.0.2; conversion fails on all 15, open on this target; the short filtered conversion agrees within one sample on 84 probes at `cfcb1c97` on AArch64 and 42 at `a62c8eab` on ARMv7 | 1.1473 at `576f088f` on c6g.4xlarge (15/15 pass; worst `c04-motion-960`) | heap ≤0.5904, total RSS ≤0.9701 (15/15 lower, kernel-bound) at `576f088f` and `6d7a0f3c` | on `aarch64-unknown-linux-gnu`, the 15 cases' conversion: FFmpeg for AArch64 filters chroma vertically where its ARMv7 build averages each 2×2 block ([pixel](../crates/pixel/README.md#conformance)); the converter follows 32-bit ARM; on every target, widths of 2 modulo 4 from 18 up, where FFmpeg's ARMv7 build faults and the converter takes the filtered path |

## Measured revisions

Each row names the source revision a result above was measured on and, where the raw
samples are archived, their SHA-256. Update the row in the same change as the result.
A gate run that [reuses](quality.md#reuse) a cited result changes no row: the row keeps
the revision and set it was measured with, and the run's `gate-result.json` names that
set as reused.

The rows at `a73350ca` come from one full sharded gate run per target
([running the gates](quality.md#running-the-gates)), each on eight c6g.4xlarge hosts
(Graviton2) with kernel `7.0.0-1014-aws` and every worker and reference linked under the
link policy `static-toolchain-runtime-trained-order-v1`: each item timed alone on CPU 1 with
the RSS module unloaded, its memory then collected under the kernel RSS proof on CPUs 1–15,
and graded with the module loaded; no result was reused. Linux AArch64 ran 153 steps and
ARMv7 150; 3 and 2 failed, each only on its contract's or list's listed open cases. A grade
then failed on parity alone, so the memory bounds above 1.0 of packet framing and of the
four `long-movie` PGS cases were in the graded reports' acceptance, not in the step results.
A grade now fails on a case whose bound fails ([bounds](quality.md#reference-and-corpus)): graded
again, the PGS grades fail on both targets, and packet framing's total RSS fails as listed
in its contract's `open_bounds`. The PGS rows have since been measured again at `99f1027b`
(below). AAC
in MOV, which the gate does not run, was measured at the same revision on one more
c6g.4xlarge host per target as the gate measures a codec workload list. Sets
`gate-run-TARGET-a73350ca-*-shard-1` to `-8` hold the shards' outputs, build identities,
plans, step records and logs, `aac-access-units-a73350ca-TARGET` the AAC in MOV
measurements, and `closing-run-ratios-a73350ca` every timed case's paired median and 95%
bounds with, per feature and target, the cases whose median or bound is above 1.0 and whose
bound is above the [ceiling](quality.md#gates), 1.10 and 1.05, and the median of the case
medians. The run predates the `codecs-audio` item, graded at `bbdefa70`.

The PGS rows at `99f1027b` come from one partial gate run of the `subtitle-pgs` item per
target, each on one c6g.4xlarge (Graviton2) from the gate image, kernel `7.0.0-1014-aws`,
under the link policy `static-toolchain-runtime-trained-order-v1`: timed alone on CPU 1,
memory collected under the kernel RSS proof on CPUs 1–15, and graded with the module
loaded; every step passes 319 of 319 cases. Sets
`gate-run-aarch64-99f1027b-20261116-050209-shard-1` and
`gate-run-armv7-99f1027b-20261116-050516-shard-1` hold each target's outputs, build
identities, plan, step records and logs.

The packet framing rows at `d915aa19` come from one partial gate run of the `framing` item
per target, measured the same way; every step passes 135 of 135 cases, and only the total
RSS bounds listed in the contract's `open_bounds` fail. No case's runtime bound is above
1.10 on either target, against 28 on Linux AArch64 and 5 on ARMv7 at `a73350ca`; the
median of the case medians is 0.9630 on Linux AArch64 (1.0411 at `a73350ca`) and 0.8100 on
ARMv7 (0.8989). Sets `gate-run-aarch64-d915aa19-20261116-064738-shard-1` and
`gate-run-armv7-d915aa19-20261116-064928-shard-1` hold each target's outputs, build
identities, plan, step records and logs.

The rows at `79e0ae13` come from one full sharded gate run per target
([running the gates](quality.md#running-the-gates)), each on ten c6g.4xlarge hosts
(Graviton2) with kernel `7.0.0-1014-aws` and every worker and reference linked under the
link policy `static-toolchain-runtime-v1`: each feature timed alone on CPU 1 with the RSS
module unloaded, its memory then collected under the kernel RSS proof, and graded with the
module loaded. The memory of both codec workload lists on both targets comes from a partial
run of those items at `8154812e` on four c6g.4xlarge hosts per target, under each list's memory run limit for the target. Each
feature's evidence set, `gate-FEATURE-79e0ae13`, holds its timing, memory and grade outputs
with every shard's build identities, plan, step records and logs; `gate-run-TARGET-79e0ae13`
and `gate-run-TARGET-8154812e` hold each run's summary and host times. The rows of MP4
range, MP4 forward, Matroska startup and seek, Matroska audio clocks and the six Matroska
feature groups that name `9be327a2` come from a partial run of those items on one
c6g.4xlarge host per target, the same kernel and link policy, timed on CPU 1 and their
memory collected under the kernel RSS proof; evidence set
`gate-playback-containers-9be327a2` holds both targets' graded reports, build identities,
step records, logs, summaries and host times, and a bundle of the measured source. The
ARMv7 rows of HLS segments, TS program and raw Annex B that name `359a90b4` come from a
partial run of those items on one host of the same class, kernel and link policy
(evidence set `gate-playback-ts-359a90b4`), which measures the playback worker these
changes left in place of the one measured at `79e0ae13`.

The rows and cells at `4d31df88` come from one partial gate run per target, each on one
c6g.4xlarge (Graviton2) from the gate image, kernel `7.0.0-1014-aws`, under the same link
policy and the same stages: the codecs' parity, both codec workload lists' timing and
memory, the ISO BMFF reader comparison and the Ogg reader comparison, with the
`ogg-reader-inputs` set unpacked at the repository root; the run also measured the three
ISO BMFF open and index parts.
Sets `gate-run-aarch64-4d31df88` and `gate-run-armv7-4d31df88` hold each run's outputs,
summary and host log.
The ISO BMFF results at `e2d4a6a3` come from one partial run per
target on the same host class and image, by `tools/gate-aws.sh`, of every item whose
worker links the ISO BMFF crate (the three ISO BMFF parts, the seek suite, the reader
comparison, MP4 forward and range, DVD subtitles and closed captions, and the codecs'
parity); sets `gate-run-TARGET-e2d4a6a3-*-shard-1` hold them. The playback and subtitle
rows keep their later runs at `9be327a2` and earlier, whose playback worker this run
predates; in it each keeps its row's verdicts.
The MP4 forward, MP4 range and ISO BMFF seek rows at `d7a6d8f7` come from one partial run
of those items per target on the same host class and image, after the comparison worker
again resets its input at a seek's landing; sets `gate-run-TARGET-d7a6d8f7-*-shard-1`
hold them.
The rows at `d2c391b8` and `6dc6440d` come from partial runs per target on c6g.4xlarge
hosts from the gate images built at `f71495ea`, under the trained-order link policy. The
six Matroska feature groups, and on ARMv7 MPEG-TS and MPEG-TS access units, at
`d2c391b8`, ran on five hosts per target (sets
`gate-run-aarch64-d2c391b8-20261115-143518-shard-1`, `-2`, `-4` and `-5`, and
`gate-run-armv7-d2c391b8-20261115-143448-shard-1` to `-5`); `6dc6440d` and `cd7d9d7c`
keep their media worker, and on ARMv7 their transport worker, byte for byte. The items of the
workers that main's framing changes reached, Matroska startup and seek and audio clocks,
MP4 forward and range, TS program, raw Annex B, HLS, progressive playback and, on Linux
AArch64, packet framing, ran at `6dc6440d` on three hosts per target (sets
`gate-run-TARGET-6dc6440d-20261115-184229-shard-1` to `-3`); `cd7d9d7c` keeps those
workers byte for byte. The ARMv7 packet framing row comes from a partial run of that item
at `cd7d9d7c` on one host, whose framing worker links in module order, byte for byte as
main's at `7c4057c5` (set `gate-run-armv7-cd7d9d7c-20261115-191630-shard-1`).

The codec correctness lanes of AAC, AC-3, E-AC-3 and DTS at `bbdefa70` come from a
partial run of the item `codecs-audio` on one c6g.4xlarge per target from the gate images
built at `f71495ea`, with the `codec-derived-streams` set, which the images lack, unpacked at
the repository root of each host before it got ready; sets
`gate-run-aarch64-bbdefa70-20261116-025121-shard-1` and
`gate-run-armv7-bbdefa70-20261116-025121-shard-1` hold their rows, summaries and logs.
The codec cells at `8d6f2789` come from a partial run on three c6g.4xlarge hosts per target
from the same images (the codecs' parity, the timing of `workloads.json` and its memory in
five parts, and the tests; set `gate-run-codecs-8d6f2789`). The closed caption rows come
from a partial run of `subtitle-cc` at `6d5145d7` on one host per target, measured in the
same way, with the `subtitle-cc-inputs` set, which the images lack, unpacked at the
repository root of each host before it got ready; sets
`gate-run-aarch64-6d5145d7-20261115-071404-shard-2` and
`gate-run-armv7-6d5145d7-20261115-071435-shard-2` hold them. The tests of `f9b3f138`,
which corrects a test's count of the caption cases, ran on one more host per target (sets
`gate-run-TARGET-f9b3f138-*-shard-1`). `8d6f2789` holds the decoded picture output model without the
Parametric Stereo paths of `4d31df88`, and `4d31df88` the reverse: the codec cells give the
video workloads at `8d6f2789` and the others at `4d31df88`. The tree that holds both, and
main's later changes to `BudgetVec` growth, is not measured until the next full gate run.

Most Linux AArch64 rows were measured before on c8g (Graviton4) hosts, not on the
qualified c6g.4xlarge. On each such row's worst Linux AArch64 case the candidate's
instruction count at `79e0ae13` stays within 0.4% of the build the row cited,
but for Matroska audio clocks, whose code got 9.18% cheaper, while the runtime upper bounds
moved by −0.31 to +0.18, so the moves come from the host class, not the code
([quality](quality.md#measuring-performance)). The earlier measurements follow.

The Matroska rows used the [gate build](quality.md#measuring-performance)
at `513149a970caad5b30c61becba1bc2bda1c5e193` on a c8g.large pinned to CPU 1,
with 30 timing pairs of 50 operations and 30 separate memory pairs per case.
Codec and decode-session runtime at
`add6b27` was one unpinned gate-build pass of 30 pairs on a c8g.2xlarge. Capture at
`576f088f` was measured on c6g.4xlarge on both targets. Timing ran on CPU 1
with the RSS module unloaded. Peak memory ran under the kernel RSS proof. Each
case planned 30 pairs per stage and used the [early-stop rule](quality.md#early-stop).
The AArch64 noise case's timing and memory are at `6d7a0f3c`, which gives
the memory observer the same session timeout as timing.
The audio codecs in files row used the gate build at
`ccff5b5734c113c1d88160b84825157508c701c0` on a c8g.2xlarge, unpinned, against the `decode` profile:
30 timing pairs per workload of `harness/codecs/file-workloads.json`, each demuxing,
decoding and mixing to stereo s16 on both sides (`cargo xtask codecs time`), 30 heap and
30 RSS pairs under the native observer (`cargo xtask codecs memory`), and the reader lane
against the profile's `ffprobe`; the per-workload runtimes its Open cell cites come from
that pass. The Ogg and audio elementary-stream rows rerun the reader
lane with both Ogg views at `ccd3090572433e79d8ab65a377fcd9896d2839d9` on the same instance type.
The codec, decode-session and file-audio timing passes at `add6b27` and `ccff5b57`, and
the ARMv7 codec timing passes at `78a873dd`, predate the shared paired runner and keep
their recorded results; the shared validator has not re-graded them, as their records
hold no warm-ups or module observations. Their pairs were checked pair by pair rather
than watched over the whole run. A diagnostic
comparison on a c6g.2xlarge, with the codec workload timed twice by each runner:

| Workload | Earlier runner, `642316cf`: median (upper 95%) | Shared runner, `c21176e0`: median (upper 95%) |
|---|---|---|
| `aac-lc-stereo` | 0.7350 (0.7357), 0.7339 (0.7361) | 0.7344 (0.7363), 0.7342 (0.7351) |
| `c07-ui-static-480-bottomup` | 1.3026 (1.3047) | 1.2978 (1.3001) |

At `be0eff89` the packet-framing rows compared all 135 cases with 30 timing and 30
kernel-bound memory pairs per case on c6g.4xlarge, the AArch64 worker in the trained
order recorded from its own training corpus and the ARMv7 worker in the toolchain link;
the kept sets `packet-framing-be0eff89-aarch64` and `packet-framing-f4f1c7ad-armv7`
contain those per-case results. At `d915aa19` the AArch64 worker links in its order
retrained from the same training operations.

On AArch64, `avc-bad-update` is the only total RSS miss. Its median
resident pages are 443 at entry and 459 at peak in Triergon, and 450 at
entry and 451 at peak in FFmpeg. In three mapping snapshots per worker
and marker, the candidate's resident executable text grows from 152 to
216 KiB during the malformed update. The reference stays at 320 KiB.
The separately collected mappings are in `framing-bad-update-mappings-be0eff89`.

An earlier packet-framing comparison used the gate build at
`f4f1c7ad1a4f2572e676f48f81ad15aa4e9ebdc5` on c6g.4xlarge.
Both targets compare all 135 cases with 30 timing pairs of 50 operations on CPU 1
and 30 kernel-bound memory pairs. The kept sets
`packet-framing-f4f1c7ad-aarch64` and `packet-framing-f4f1c7ad-armv7`
contain the per-case results. Both workers use the same input preparation and
output fold. For `avc-width1`, median resident pages at entry are
467 versus 450 on AArch64 and 346 versus 329 on ARMv7.

The reference input adapter at `f40c8eb0` uses descriptor reads and bounded
scratch storage. The earlier `f71495ea` adapter used stdio. In six first-write
mapping snapshots per build for `avc-width1`, resident libc text at memory entry
fell from 1088 to 896 KiB on AArch64 and from 756 to 634 KiB on ARMv7.
Both reference workers rebuilt byte-identically from the same FFmpeg archives
and compiler arguments. These diagnostic snapshots accompany the kernel-bound
measurements in `packet-framing-reference-residency-f40c8eb0`.

The earlier matched run at `62e1ad85` also had 0/131 lower total RSS on each
target. Its two ARMv7 runtime misses, `hevc-irap_repeat` and `hevc-type19`, are
within the ceiling at `f40c8eb0`. The comparisons at `6ce7109c` and
`7707d776` used different adapter work on each side.

At `64fc1b08` the ARMv7 total RSS bound tied at 1.0000 for `hevc-type23`,
with 124/125 cases strictly lower. The tie cleared at `b3d01267`. The
cause of the change in that case is unknown. The earlier AArch64
result at `79e0ae13` had a worst runtime of 1.1849 and lower total RSS
in 77/125 cases. The change to `b3d01267` spans the move to the
c6g.4xlarge gate host and updates from main.

The measured revisions include the earlier extension comparisons at
`83993880` and the framing run at `2b997c87`. At `2b997c87`, ARMv7
total RSS was lower in 125/131 cases and the worst bound was 1.0087.

The MPEG-TS PES, program playback, progressive playback and HLS rows used the
375-case gate build at `21c9eef2f08a11bb7bd1c457997095326f2bb42d` on a c8g.large pinned to CPU 1.
All 375 cases passed parity, runtime, peak heap and total RSS with 30 timing
pairs of 50 operations and 30 memory pairs under the kernel RSS proof.
All five progressive functional inputs passed their ten outcomes. Each HLS
video unit passes through `contiguous_feed` inside the measured operation.

The progressive report comparison used the gate build at
`ec6afe8635509d7e9007fd3d3c20ce2bf1c25835` on a c8g.large pinned to CPU 1.
All three cases and ten functional outcomes passed with 30 timing pairs of 50
operations and 30 memory pairs. Grading compact and self-contained reports
gave the same results.

The MPEG-TS access-unit comparison used the gate build at
`230d1314464d34a04f5f2614298e5b7009292f0c` on a c8g.xlarge pinned to CPU 1.
All 169 cases framed all tracks in the chosen program and passed parity, runtime,
peak heap and total RSS with 30 timing pairs of 50 operations and 30 memory
pairs under the kernel RSS proof.

The raw Annex B results and the earlier Matroska audio clock comparison used the gate
build at `b3579f729494a37a4dbebe03c475f7b38484046e` on a c8g.2xlarge pinned to CPU 1,
with 30 timing pairs and 30 memory pairs under the kernel RSS proof.

In the earlier audio clock comparison, `laced-fractional-cadence` reached 1.4510,
`laced-unknown-duration` 1.4089, `two-laced-before-first-known-pts` 1.3968 and
`damaged-lace-before-valid-anchor` 1.3878. Peak heap reached 1.822 on
`no-absolute-anchor` and 1.321 on `dolby-bad-first`. Total RSS ranged from 1.038 to
1.043 across all ten cases.

The subtitle rows on Linux AArch64 used the gate build at
`f263491102d307997d40d40826a6193d8468e87e` on a c8g.xlarge pinned to CPU 1, with 30 timing
pairs of 50 operations and 30 memory pairs under the kernel RSS proof, graded by `cargo xtask
grade` against `ffmpeg-subtitles-plain-native`. Its workers are byte-identical to those of
`8513398b85d27aa093b8212c202b25c32da859bf`: the commit between
changes only the memory runner, which since drops both workers' cached pages before every
run. The closed caption rows come from a partial gate run of `subtitle-cc` at
`6d5145d7f31b002c72f9815130b522964d2abb64`, one c6g.4xlarge per target from the gate images with
evidence set `subtitle-cc-inputs` unpacked ([running the gates](quality.md#running-the-gates));
their instruction counts are of whole `sub-trace` runs at `adaa4bc7`, before the video decoders
moved onto the shared picture output model, which changed the model's field pair step but no
caption output. Before the picture output model, the row used the gate build at
`9d048c58fb30c64777f7230eb67c7037a1e1bcf1` on a c8g.xlarge in the same way, where the caption receiver reads `cc_data` as three-byte
constructs and resolves the field once per packet; on all 205 cases its workers' traces and
folds are byte-identical to those of `d37da04f`. On the eight `mpeg2-a53-limit` cases, which
carry large `cc_data`, it executes 0.66 to 0.68 times the reference's instructions on
AArch64, where `d37da04f` executed 1.58 to 1.64 times, and times at 0.5525 to 0.5724 on the
c8g.xlarge. On a c6g.4xlarge (Graviton2), natively on AArch64, the same eight cases time at
0.6272 to 0.6485 in 30 pairs, where `8513398b` gave them 1.3887 to 1.4481. The earlier rows were
measured at `956c42961f4a27d88f96d5800d59ff1eeb088018` (PGS, closed captions and teletext),
`03f3efd3a0fde0d558c68f5408c521d632802fb7` (PGS memory),
`531e2d04ba7386513ff7afe7b44f53b2a93ca9dc` (DVD) and
`0d70bebfc0799f65df14f170ed97e68c5a35daa7` (DVB). The text row used the gate build at
`6126b759b9b983afe488c3c362c6e37c2d1425a2` on a c8g.xlarge, whose timing pairs are of 2,000
operations. The 8 cases added to `subtitle-dvd`, `subtitle-pgs` and `subtitle-teletext` were
first measured with the gate build at `c79ae2da8b8a0d004d37936100eedd7b46e0a589` on a c8g.xlarge pinned to CPU 1, with
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
above main beyond the run spread. The 13 `mov-text-tags` cases were first measured with the gate build at
`96f660ba99fa33fbc1a2019cda9e73bec6de287d` on a c8g.xlarge pinned to CPU 1, with 30 timing
pairs and 30 memory pairs under the kernel RSS proof, run as a slice of the feature.
`cargo xtask grade` does not join the two runs, because the slice's contract differs from
the registered feature's, so their per-case results come from each run's report.
The DVB rows' 10 cases added at `aebbe582` were first measured the same way on a c8g.xlarge
pinned to CPU 1, as a slice of `subtitle-dvb`, their instruction counts running both workers
from paths of equal length.

The ADTS AAC decoder can leave priming to a caller with an explicit container
edit. The default still applies the encoder delay signature. The stereo and
5.1 AAC-LC workloads were measured earlier with the gate build at `96232af7d9431c2285c6b6d248aa38a133b85fe3`
on a c8g.large pinned to CPU 1, with 30 timing pairs and 30 memory pairs
per case. Both matched the reference PCM within the decoder's numeric rules.
The upper runtime bound was at most 0.7587, peak heap at most 0.7034, and
total RSS at most 0.4219 of the reference.

ISO BMFF open and index measurements use `cargo xtask isobmff-bench`, with
`--subset52` selecting the 52-operation set. Packet framing uses `cargo xtask framing`.

The ISO BMFF row used the gate build at `f99d893196908d562c897bccb20c6a972cd25d11`
on a c8g.large pinned to CPU 1, 30 pairs. Runtime comes from the timing collection and
memory from a separate collection under the kernel RSS bound; both are in the raw
samples. This earlier run kept separate verdicts because its grader compared the
process addresses in the reference diagnostics.

The MP4 forward and range results used the gate build at
`f5453ef9d88712777aa5b6383250875456e9f334` on a c8g.large pinned to CPU 1, with
30 timing pairs of 50 complete operations and 30 memory pairs per case.
All 46 forward and 51 range cases matched and passed runtime, peak heap and
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
as before on both, 0.014 % fewer at most, so the AAC timing rows measured before it held.

The downmix's conversion to s16 (`mix::to_s16`) also runs fewer instructions on AArch64,
the codec gate target. There the floor was one `frintm`, but the loop clamped in `f32`,
converted and clamped again, two samples at a time; the integer form folds the scale
into a fixed-point `fcvtzs`, saturates with `sqsub` and narrows with `sqxtn`, four
samples at a time. The gate build at `f0744418` against `b89e5844` on a c6g.4xlarge
(`cargo xtask count`, median of three rounds) runs fewer instructions on every audio
workload, while `b89e5844` against itself moves by at most 0.01%. The 577 decoder
outputs are byte-identical and the conversion gives the floor's result for every
`f32`, so the AArch64 codec timing rows measured before it held:

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

The ASS rendering runtime and memory are graded against `libass-player-native`: libass linked as a
player links it, none of its sources compiled into the worker, with
`ass_set_cache_limits(renderer, 1000, 24)`. The reference replaces libass's bitmap engine
with its plain C one, and libass is built with `-O2 -ffp-contract=off
-ftrivial-auto-var-init=zero`, so a player's libass with its NEON code on AArch64 is faster
than this reference. The timed spans favour the renderer slightly: in 12 cases libass opens
the default font file inside its span, which the renderer reads before its own, and
libass's side copies the script inside its span. Both features are timed by the shared paired
runner that times the codecs and capture. Memory is measured under the `linux-glibc-v4`
observer; font files libass maps during an operation count toward its total RSS.
`ass-render-host` is timed and measured on ARMv7 only; on AArch64 its 4 cases are a
deliberate divergence. Earlier, at `5b3e03f0e9410853152043d033b4f4d757e98a40` on a
c6g.2xlarge, natively on AArch64 unpinned and in the ARMv7 user space pinned to CPU 1, with
30 timing pairs and 30 memory pairs per case, graded together by `cargo xtask grade` after
`cargo xtask ass compare --all` passed at the same commit on both targets, the median of the
per-case medians over the 76 `ass-render` cases was 0.813 on AArch64 (5 cases above 1.0,
the highest upper bound 1.0658) and 0.790 on ARMv7 (the highest upper bound 0.9427), and
`ass-render-host` on ARMv7 had a worst upper bound of 0.8688, peak heap 0.614 and total RSS
0.866.

The shaping cases that need the interpreter (Bitstream Vera 1.10 in the corpus, DejaVu Sans
and Vera Latin, and the samples `c` with U+0308 and `x` with U+0301) were compared at
`dc792ea4b25af0e64db6b73b47ce66dc89b6e045` against main at `51977abacdfe2e8086c124e37c0ad303168423f3` on the same host class and targets: `shape-libass` 209/209,
`shape-text` 208/209 and `shape-text-all` 1,789/1,790 with every other feature as on main, and
every `shape-libass` and `ass-render` case within main's instruction spread.

The TrueType interpreter was compared at `218065d60fb7520fc5e636c8451b46a4d6bda61f` against main at
`2886a3fd7b84392b75a98369e8426e0dd6f9edda` on a c6g.4xlarge, natively on AArch64 and on the ARMv7 Thumb-2 target, with
`fonts compare --all`, `shape compare --all` and `ass compare --all`: no case fails that passes
on main and every passing case folds as on main; `font-hinting-default` gains the 43 instructed
TrueType files. Every `shape-libass` and `ass-render` case runs the same number of instructions as
at `e110f29cb904c0a4f3925fc90a2d1a10a67aeff4` within that commit's own spread, so the `ass-render` runtime and kernel-bound memory
collected at `e110f29cb904c0a4f3925fc90a2d1a10a67aeff4` on both targets stood for it. Against main,
the two `hinted-extents` cases, whose glyphs are now hinted, run more instructions, and every case
runs a fixed few hundred more at start-up (the interpreter's relocations). Its probes
(`harness/fonts/probes`) are recorded on ARMv7.

The font program's single run per face, the control value program's run at each size request,
the default load at 26.6 heights 1000 and 640, zero pixels per em, each outline's orientation, the
underline and strike-out fixtures and HarfBuzz's CFF and CFF2 extents were measured at `69d4d0f8` by
a partial gate run (`font-shape-raster-ass`, `ass-render`, `ass-render-host`) on one c6g.4xlarge
per target: every comparison passes but the open cases its contract lists, `shape-extents` 9/9
among them, and `ass-render`'s 22
new cases are within the runtime ceiling and lower than libass in peak heap and total RSS on both
targets, `decorations-bands.ass` at 720p the highest in both (heap 0.9485 and total RSS 0.9971 on
AArch64, 0.9754 and 0.9981 on ARMv7). The reference matches every probe's ARMv7 record under
`harness/fonts/probes` and `harness/shape/probes` on both targets, and the candidate matches the
reference but for the three vector probes (`vectors-normalization`, `vectors-from-stack`, `projections`).
`random-3310` and `random-8005`, two generated scripts whose shaping reads hinted extents of
instructed TrueType glyphs, render as libass does on both targets at `9d14e98c`.

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

`subtitle-cc` then had 201 cases and four open `hevc-eos-ts` cases; its rows have since been
measured again. The ISO BMFF worker, and every
reference worker, are byte-identical to those of `4dbbd545d2d452220cd0e35ff11807f6c2fd2703`,
measured the same way on the same host: ISO BMFF passes all 972 outputs (runtime ≤0.8972,
peak heap ≤0.316, total RSS ≤0.7433, all lower).

| Area | Revision | Raw samples SHA-256 |
|---|---|---|
| Full gate run, Linux AArch64, shard 1 of 8 | `a73350cad69905e273984ed479cf8bbff8f54f93` | [`c83c3bf8dc9c9ea81ac8c6d9fe3404c110741c9615b062640a62b2a1ca5dd2f3`](../harness/evidence/sets/gate-run-aarch64-a73350ca-20261116-023216-shard-1.json) |
| Full gate run, Linux AArch64, shard 2 of 8 | `a73350cad69905e273984ed479cf8bbff8f54f93` | [`a96d0fda576444bd936a5b163699bf55b31eda4e2858709712488f53ce669992`](../harness/evidence/sets/gate-run-aarch64-a73350ca-20261116-023216-shard-2.json) |
| Full gate run, Linux AArch64, shard 3 of 8 | `a73350cad69905e273984ed479cf8bbff8f54f93` | [`1de5483e30e5160b7700d4f021921f08301694290ec9960765329c7635312fdb`](../harness/evidence/sets/gate-run-aarch64-a73350ca-20261116-023216-shard-3.json) |
| Full gate run, Linux AArch64, shard 4 of 8 | `a73350cad69905e273984ed479cf8bbff8f54f93` | [`d0f514b48d178f121de50824a7f19d764ef3f16d4cce16eafd455a05fa201f43`](../harness/evidence/sets/gate-run-aarch64-a73350ca-20261116-023216-shard-4.json) |
| Full gate run, Linux AArch64, shard 5 of 8 | `a73350cad69905e273984ed479cf8bbff8f54f93` | [`cbe002f80500291c87174b78704408460947fc11ec50aedbf1edfb558de68503`](../harness/evidence/sets/gate-run-aarch64-a73350ca-20261116-023216-shard-5.json) |
| Full gate run, Linux AArch64, shard 6 of 8 | `a73350cad69905e273984ed479cf8bbff8f54f93` | [`d09f3d6a7983d093ff9f3408970f7e50c7090f7cf3ac6f305cf636711c891a85`](../harness/evidence/sets/gate-run-aarch64-a73350ca-20261116-023216-shard-6.json) |
| Full gate run, Linux AArch64, shard 7 of 8 | `a73350cad69905e273984ed479cf8bbff8f54f93` | [`fff421b70073562b989a7a970b7ef3b10a9441a18ae8bf22eb7a08eae760f821`](../harness/evidence/sets/gate-run-aarch64-a73350ca-20261116-023216-shard-7.json) |
| Full gate run, Linux AArch64, shard 8 of 8 | `a73350cad69905e273984ed479cf8bbff8f54f93` | [`1ef0e0b6e5e47d5c11913dbbeac3aaea22cc6b9055ff28379ef912b348190bdd`](../harness/evidence/sets/gate-run-aarch64-a73350ca-20261116-023216-shard-8.json) |
| AAC in MOV, Linux AArch64 | `a73350cad69905e273984ed479cf8bbff8f54f93` | [`25d957a127449a8fb070b22b8d965499cbc75a66d423e887b9c9e5ec67d2ecf2`](../harness/evidence/sets/aac-access-units-a73350ca-aarch64.json) |
| Full gate run, ARMv7, shard 1 of 8 | `a73350cad69905e273984ed479cf8bbff8f54f93` | [`e64327b5a918910068a520c3eed46778246ceb1ac72455ed70c2f01c7a745585`](../harness/evidence/sets/gate-run-armv7-a73350ca-20261116-022908-shard-1.json) |
| Full gate run, ARMv7, shard 2 of 8 | `a73350cad69905e273984ed479cf8bbff8f54f93` | [`a0dd601c4b6bb684c1f36d7750d40dff56c5585b378cf52f29c92a134a465c13`](../harness/evidence/sets/gate-run-armv7-a73350ca-20261116-022908-shard-2.json) |
| Full gate run, ARMv7, shard 3 of 8 | `a73350cad69905e273984ed479cf8bbff8f54f93` | [`b937a248551068d5fb491cf51a97e307c12fed8ada4bb7fbc3890811002f2dde`](../harness/evidence/sets/gate-run-armv7-a73350ca-20261116-022908-shard-3.json) |
| Full gate run, ARMv7, shard 4 of 8 | `a73350cad69905e273984ed479cf8bbff8f54f93` | [`8eeea64425dad7d04baea0379bcdac17b45f00375e847795e9939e2a32c87cd8`](../harness/evidence/sets/gate-run-armv7-a73350ca-20261116-022908-shard-4.json) |
| Full gate run, ARMv7, shard 5 of 8 | `a73350cad69905e273984ed479cf8bbff8f54f93` | [`cbd5a0b11ac26d679b7dd8120fc9f726872cf14d3f25ba4f084d394619740e2a`](../harness/evidence/sets/gate-run-armv7-a73350ca-20261116-022908-shard-5.json) |
| Full gate run, ARMv7, shard 6 of 8 | `a73350cad69905e273984ed479cf8bbff8f54f93` | [`470fbcb63fe480abea6b145e8aa15871c6f122443124c9f35b9b2bab349062d1`](../harness/evidence/sets/gate-run-armv7-a73350ca-20261116-022908-shard-6.json) |
| Full gate run, ARMv7, shard 7 of 8 | `a73350cad69905e273984ed479cf8bbff8f54f93` | [`987f5aecd9772f85609426239029d8764cca5b3f7d6d567391c575209df77ccb`](../harness/evidence/sets/gate-run-armv7-a73350ca-20261116-022908-shard-7.json) |
| Full gate run, ARMv7, shard 8 of 8 | `a73350cad69905e273984ed479cf8bbff8f54f93` | [`2154275af63ffe7e42974a6576f416a8e8bdbe130728b24ed3670ba4a001f49d`](../harness/evidence/sets/gate-run-armv7-a73350ca-20261116-022908-shard-8.json) |
| AAC in MOV, ARMv7 | `a73350cad69905e273984ed479cf8bbff8f54f93` | [`ac664cca8d09697977201d5538a7542e925bdc4bd4ac75b106af4ee94872b698`](../harness/evidence/sets/aac-access-units-a73350ca-armv7.json) |
| Per-case runtime ratios of the full gate runs and AAC in MOV, both targets | `a73350cad69905e273984ed479cf8bbff8f54f93` | [`74bd0b90c8f4697c0e624136fcc511808a33d57158aaa27aa8a25e734ffcb3bb`](../harness/evidence/sets/closing-run-ratios-a73350ca.json) |
| Packet framing, Linux AArch64 | `d915aa196a33328e07ecfb3ccc858a04585b2f31` | [`af871e6591be476a190154e041a4f54c72609797cb86fbd2fdc374fa2ad58d09`](../harness/evidence/sets/gate-run-aarch64-d915aa19-20261116-064738-shard-1.json) |
| Packet framing, ARMv7 | `d915aa196a33328e07ecfb3ccc858a04585b2f31` | [`bd1f456614cf4e391f1235d571ea1d06ebdc3c50169653810ac546aff5d13b7b`](../harness/evidence/sets/gate-run-armv7-d915aa19-20261116-064928-shard-1.json) |
| Subtitles, PGS, Linux AArch64 | `99f1027b1e6a4bddb0b1a99b84cc79756116d8cb` | [`4cd8df9c5d0652aabf435dcb8100e4984966fcca365080e1f46c84fd973f5bd1`](../harness/evidence/sets/gate-run-aarch64-99f1027b-20261116-050209-shard-1.json) |
| Subtitles, PGS, ARMv7 | `99f1027b1e6a4bddb0b1a99b84cc79756116d8cb` | [`4c93b0902098f445ae645b18cc1df079177e3ae6bd45876e1f643f18b163b2de`](../harness/evidence/sets/gate-run-armv7-99f1027b-20261116-050516-shard-1.json) |
| ISO BMFF open and index, AArch64 part 1 of 3 | `d315f73c3b7d944442d465bed89908f230be286f` | [`e580e786653f5f4647b325ab16f4295d526814103f34fbba8e4a31fade86afa7`](../harness/evidence/sets/gate-run-aarch64-d315f73c-20261116-010313-shard-1.json) |
| ISO BMFF open and index, AArch64 part 2 of 3 | `d315f73c3b7d944442d465bed89908f230be286f` | [`997a310b2595a8490854a01a75444f78428dddc1665b61eb4ac8361f3b42919c`](../harness/evidence/sets/gate-run-aarch64-d315f73c-20261116-010313-shard-2.json) |
| ISO BMFF open and index, AArch64 part 3 of 3 | `d315f73c3b7d944442d465bed89908f230be286f` | [`a537ace87a00dfbcdbbe1b89a5d8ccd8098867d7064e15d6ab74992ed5edba67`](../harness/evidence/sets/gate-run-aarch64-d315f73c-20261116-010313-shard-3.json) |
| ISO BMFF open and index with legacy QuickTime metadata | `7b3990083d8305aa46972411f3605a483a2e4b5f` | `371475c620e1da835bba0f3fb12b6227516a5593edfef267a401bf1c3800e6fe` |
| Gate run, Linux AArch64 and ARMv7: Matroska / WebM, `matroska-demux` | `79e0ae138c71ffc57c73decc88d29ae86b1153d4` | `2ad4e9706184a803da2e6ca2fb567c915e223a9da692da801cf6c0c0dc646f79` |
| Gate run, Linux AArch64 and ARMv7: Matroska / WebM, `matroska-header-removal` | `79e0ae138c71ffc57c73decc88d29ae86b1153d4` | `51051d5173516db2f18aa374c9b2aa27583a32f69787dae96ead3a8ddac2b5af` |
| Gate run, Linux AArch64 and ARMv7: Matroska / WebM, `matroska-zlib` | `79e0ae138c71ffc57c73decc88d29ae86b1153d4` | `169ab805e391d16caddd4bb447b5c30d57f8e6278f87b04bd2bbe683eff9e9f9` |
| Gate run, Linux AArch64 and ARMv7: Matroska / WebM, `matroska-cues-preroll` | `79e0ae138c71ffc57c73decc88d29ae86b1153d4` | `ed64a0ee179cda5453e75b807668b80d2252377226d43d06da520d3e54cff891` |
| Matroska comparisons, AArch64 part 1 of 2 | `4f956899f44c6e96453279b049486baeba402ab4` | [`453fca921a42302d6815dada0832395781caea5595d907466eed36dc16bf2d25`](../harness/evidence/sets/gate-run-aarch64-4f956899-20261116-012709-shard-1.json) |
| Matroska comparisons, AArch64 part 2 of 2 | `4f956899f44c6e96453279b049486baeba402ab4` | [`3a3ce7bbe0acff134fbd53a1fc70b2b86fb3101d0d2c15091218f7ffe04b0fd7`](../harness/evidence/sets/gate-run-aarch64-4f956899-20261116-012709-shard-2.json) |
| Matroska legacy AAC-LC, audio clocks and startup/seek, AArch64 gate | `f7c2d25f91581f795202e3d0552bd0dc4d8df2a7` | `c45475c1cc5581d687e2b9bb727f707ca4648d5bbf37dc522cbfa5208bd5551e` |
| Matroska legacy AAC-LC, earlier comparison | `46efc86fdc121a0f5bfcc9129f00599c24e291dc` | `927caece428d577611be77b2da58cc5e57fff6025e903e2a814b679fec825547` |
| Matroska legacy AAC-LC, preceding comparison | `d100a1225d9fadcef101d89acffec307694e9ce9` | `176e926c723851e4a2c41bf2ddee547d4485a93e5ae28a8fd1480ed41143258b` |
| Gate run, Linux AArch64 and ARMv7: Matroska / WebM, `matroska-colour` | `79e0ae138c71ffc57c73decc88d29ae86b1153d4` | `227c28e62aa64891b437cdd66997b79fe1c6a850960e36fedf3926b49e129742` |
| Gate run, Linux AArch64 and ARMv7: Matroska / WebM, `matroska-segment-metadata` | `79e0ae138c71ffc57c73decc88d29ae86b1153d4` | `3e0db31f1772c891af7428ddb8cdb84c6f45f8a33218cae1fd2b84784cd99b3c` |
| Gate run, Linux AArch64 and ARMv7: Matroska / WebM, the external HDR vector | `79e0ae138c71ffc57c73decc88d29ae86b1153d4` | `a819cb6715a46cf8b7d2a2ad44236e02d2fb8954cbd6af6c422fe59775f6f7a4` |
| Gate run, Linux AArch64 and ARMv7: Matroska startup and seek | `79e0ae138c71ffc57c73decc88d29ae86b1153d4` | `cd00f33a0e070c68f46bd887725e7924fe80ebd46ad714779045eb3eb1f0a460` |
| Gate run, Linux AArch64 and ARMv7: Matroska audio clocks | `79e0ae138c71ffc57c73decc88d29ae86b1153d4` | `44291f259f00e429f54f2fffb7a75534cb72d54216d53ae1f49afa91a5eff47e` |
| Gate run, Linux AArch64 and ARMv7: ISO BMFF / MP4 open and index, in three parts | `79e0ae138c71ffc57c73decc88d29ae86b1153d4` | `76c81aebbb9ca11b79abc4ab0934674d750718fdec15fa88d9b0feb5e2d24880` |
| Gate run, Linux AArch64 and ARMv7: MP4 range playback | `79e0ae138c71ffc57c73decc88d29ae86b1153d4` | `9e56ed4ed4e3b5f162d61c6a05680d2346c6dae2bd8e4d86a71a06b271c4f82c` |
| Gate run, Linux AArch64 and ARMv7: playback, MP4 forward | `79e0ae138c71ffc57c73decc88d29ae86b1153d4` | `f90b882997199ec4f145bf40726100507f6533beb74479889638457629499104` |
| Gate run, Linux AArch64 and ARMv7: MPEG-TS PES | `79e0ae138c71ffc57c73decc88d29ae86b1153d4` | `336971ce5b1d94a7a1b8a7d628459c69ba093586896f4d0752c37480d00b77c7` |
| Gate run, Linux AArch64 and ARMv7: MPEG-TS access units | `79e0ae138c71ffc57c73decc88d29ae86b1153d4` | `cb05c21a8f9dfa48a25210a127f502d6ba8929b1827917cff4194fb552b02cbe` |
| Gate run, Linux AArch64 and ARMv7: MPEG-TS program playback | `79e0ae138c71ffc57c73decc88d29ae86b1153d4` | `85136ebd8227a2295ea4d04a5ae380a3aa88aa9602c00b74a4599da36e741f95` |
| Gate run, Linux AArch64: MPEG-TS progressive playback (ARMv7 measures nothing) | `79e0ae138c71ffc57c73decc88d29ae86b1153d4` | `508da696a4781a3a1efa9340622f002f93f5184dba647472017600f993be4787` |
| Gate run, Linux AArch64 and ARMv7: HLS segments | `79e0ae138c71ffc57c73decc88d29ae86b1153d4` | `61c2176506f565709670d8fb4f03dfb97dc316fb2d6fa579e4d2cf27f240e9c3` |
| Gate run, Linux AArch64 and ARMv7: playback, raw Annex B | `79e0ae138c71ffc57c73decc88d29ae86b1153d4` | `72a83617d8541bd35f4ad59dd2f7aebd108279aa195ca7f44b8a7c778d672511` |
| Gate run, Linux AArch64 and ARMv7: packet framing | `79e0ae138c71ffc57c73decc88d29ae86b1153d4` | `5bfaaaf0dc18f27473bff810c9eb5d895bd787bda9a4df5942c27f2370844e76` |
| Gate run with the trained link order, Linux AArch64: `matroska-header-removal` | `d2c391b85a1a4e844275d45df279e5d4f448b772` | `f101cab28e61ce47d9c4ea4be54afb1b090e31f5d58f1e9ec59ca713ea85eab7` |
| Gate run with the trained link order, Linux AArch64: `matroska-zlib` | `d2c391b85a1a4e844275d45df279e5d4f448b772` | `b937074bfa960037ab3d97c87fcdaa93f2b27d01bc8ec0584472d469214afc13` |
| Gate run with the trained link order, Linux AArch64: `matroska-cues-preroll` | `d2c391b85a1a4e844275d45df279e5d4f448b772` | `2030637aac6ba37ffa0ba2ce256daf449520db079d6c121dbbbcc04889b8915f` |
| Gate run with the trained link order, Linux AArch64: `matroska-colour`, `matroska-demux`, `matroska-segment-metadata` | `d2c391b85a1a4e844275d45df279e5d4f448b772` | `33791280f65217634e58054ee1db45f253b579dea0c500c1e7e7580843312436` |
| Gate run with the trained link order, Linux ARMv7: `mpegts-access-units` | `d2c391b85a1a4e844275d45df279e5d4f448b772` | `2a4ca9dd69da6e065e0c4da839858519d1e4fbf017ae630f237dc43f8909baa5` |
| Gate run with the trained link order, Linux ARMv7: `mpegts-pes` | `d2c391b85a1a4e844275d45df279e5d4f448b772` | `74e5c10b0716b0fe196609ba627bc89d2641ef11e2dde92210c60c7503033a1e` |
| Gate run with the trained link order, Linux ARMv7: `matroska-demux` | `d2c391b85a1a4e844275d45df279e5d4f448b772` | `495ca5286998fbb320d8955cd59e0672275b96e5bf217002e3809c3b1a0ba8bd` |
| Gate run with the trained link order, Linux ARMv7: `matroska-header-removal` | `d2c391b85a1a4e844275d45df279e5d4f448b772` | `65317f6a4df661b414c308c2f323d69a0a08050a71a8aaeb109e1dec7a7c02f4` |
| Gate run with the trained link order, Linux ARMv7: `matroska-colour`, `matroska-cues-preroll`, `matroska-segment-metadata`, `matroska-zlib` | `d2c391b85a1a4e844275d45df279e5d4f448b772` | `582099ddaf8da3b579a21f4ea4548aaeacdeeed209f4aa28cf877739340362b9` |
| Gate run with the trained link order, Linux AArch64: `hls-segments`, `matroska-audio-clocks`, `progressive` | `6dc6440d0439428bf61ab219fedcc3fea29924ce` | `1fb4f802b1e328883e3650b15e5936445c0a5313c2346a5223cdcf7648767bf1` |
| Partial gate run, ARMv7: MPEG-TS progressive playback | `5d8b5ee7cfc90a0e21696a242a48cbabaf342651` | `fe3019ef8bac2fcb9bd08008a66658c22eb3cd9b185085250ddb32fdf5e836b8` |
| Gate run with the trained link order, Linux AArch64: packet framing, `matroska-startup-seek`, `raw-annex-b` | `6dc6440d0439428bf61ab219fedcc3fea29924ce` | `68babd9b4d90f0b68230e3b81065c87cb466661fa12f4ec62d3888470911524a` |
| Gate run with the trained link order, Linux AArch64: `mp4-forward`, `mp4-range`, `ts-program` | `6dc6440d0439428bf61ab219fedcc3fea29924ce` | `2cef2fed47c80345d353bbc0c9d1fad33a1dde7d94bb0be514c23c806f47d289` |
| Gate run with the trained link order, Linux ARMv7: `hls-segments`, `raw-annex-b`, `ts-program` | `6dc6440d0439428bf61ab219fedcc3fea29924ce` | `261f27f436a0f19cb47627e176e556321767ffadd5b459e002868fb8049abba1` |
| Gate run with the trained link order, Linux ARMv7: `matroska-startup-seek` | `6dc6440d0439428bf61ab219fedcc3fea29924ce` | `13a05ccb7da42642a6f7a80d816b35505e8790aa8a73094b9cfb21520827baa0` |
| Gate run with the trained link order, Linux ARMv7: `matroska-audio-clocks`, `mp4-forward`, `mp4-range` | `6dc6440d0439428bf61ab219fedcc3fea29924ce` | `73f28b9110022f342a496839dd48c2d91c379d023b375bdac73f562d45c3a6c0` |
| Gate run, Linux ARMv7: packet framing, its worker linked in module order | `cd7d9d7caf433db115f0c3bec8768b4f6fab311e` | `56474beae98ef1738d20260fc8daa542b8baf3541d9c610ccba496f78ba366d6` |
| Packet framing, Linux AArch64, current | `be0eff89e4655c0a2dfae16e10bceb4709d357d7` | `8297bc5916f00325b66893a4d9bc43613c253c2aa5df9e896c33488d7d686d6a` |
| Packet framing malformed AVC update mappings, Linux AArch64 | `be0eff89e4655c0a2dfae16e10bceb4709d357d7` | `8f88b57846eac803852b35c2f2ab030a18a5476b06b91f92f7f8f3eaaa16baac` |
| Earlier packet framing, Linux AArch64 | `f4f1c7ad1a4f2572e676f48f81ad15aa4e9ebdc5` | `27551dc9b06b8a5e022e946eca4752c11c1a96bbe821c4cd606e0d6cde451c22` |
| Packet framing, Linux ARMv7, current | `f4f1c7ad1a4f2572e676f48f81ad15aa4e9ebdc5` | `5e1acd8270982b81890ad34eb69bf300f3029b908d0279f236664475a87b8805` |
| Earlier packet framing, Linux AArch64 | `f40c8eb009c15b812f131c935c6cfdfee1660df0` | `57b84793b0d187563123059018b30fd449c883f69dae1bdfde6e7ac60d074dd3` |
| Earlier packet framing, Linux ARMv7 | `f40c8eb009c15b812f131c935c6cfdfee1660df0` | `3a49c9567e82e878e8e031c0a0252198db3f1ae98cee25e3eff3cb6a7df6c925` |
| Earlier packet framing, Linux ARMv7 | `f71495ea154e95b30f8fa002637b4a6932cd32dd` | `260676ed9b8a1d39fa94ff22cb6f0f0467fbd92e7490017e943035578a424d4b` |
| Packet framing reference-input mappings, Linux AArch64 and ARMv7 | `f40c8eb009c15b812f131c935c6cfdfee1660df0` | `68675427c442f834c2dc401ed1d336f11703144c36a3778c04f54643adf345fc` |
| Earlier packet framing, Linux AArch64 | `62e1ad85011c85141ad58edfec275303a2d63b83` | `cb21eabf713fe7d55546a55ef264698436cbae0202058787890141100c824b8c` |
| Earlier packet framing, Linux ARMv7 | `62e1ad85011c85141ad58edfec275303a2d63b83` | `b7d37c676319d2cd294639a8f2f51efb5dce9fde9dc948965850952c501bac3b` |
| Earlier packet framing, Linux AArch64 | `7707d7768e1c6f1103ac333248857fba00290c94` | `4ce0fc95b2d8b3ec686a3853137619be86e99a0eb04d736ba3f945b68895ad8c` |
| Earlier packet framing, Linux ARMv7 | `7707d7768e1c6f1103ac333248857fba00290c94` | `e328d5c702f3fb527d3802aba615b107ca428121f83346c569fea1980369b3e1` |
| Earlier packet framing, Linux AArch64 | `6ce7109cd2929a7057498769768cbb2e6eb2a301` | `caa4fd0af30391d0f73dbcd255704c3a5dfe4228e034f373bb958127c50ed2ad` |
| Earlier packet framing, Linux ARMv7 | `6ce7109cd2929a7057498769768cbb2e6eb2a301` | `4375d33f486c45d75338de3f1b493c9217ae3686710793e268a04cd68185e618` |
| Packet framing, Linux AArch64 | `3f07dcc4c596cf387ce307458db0aaa88b77d14d` | `d960dc53a15ba7657a6fa63f0a44ec2d95dcbcc3dcc90eb2c8701a0d7887ccb8` |
| Packet framing, Linux ARMv7 | `3f07dcc4c596cf387ce307458db0aaa88b77d14d` | `8bb3458c1ac6af0b8527a76617cd0a7e250b44a8dfe6671b28093e8b05da92e6` |
| Packet framing, Linux AArch64 | `b3d01267428a2bf318bb05a9c02f0b9435c5f61f` | `0afb50e86cde3238b372ea31bfc248eedaf7895c4206bc8d5a7d82f0d26d2f92` |
| Packet framing, Linux ARMv7 | `b3d01267428a2bf318bb05a9c02f0b9435c5f61f` | `db1d8f73b7fb8b36d8868a78bbe56bc3c82f599d2bb7a430eff1bb520da57265` |
| Packet framing, Linux AArch64 | `2b997c87727e542fa8b92d3dc791976ee9c75f99` | `5fc70ab69e6bbaa2eadcd41e69f750973644d9bc44393292a0d72515c04c08aa` |
| Packet framing, Linux ARMv7 | `2b997c87727e542fa8b92d3dc791976ee9c75f99` | `9dafe9cb860e783ad45d25cc5fc383c5a7c8d73667fb83f8cb99775c24aade2b` |
| Packet framing, Linux AArch64 | `839938804f297a06ce40c9ff6c0f59ea30cc8863` | `94612c1c766917359cecc21162a8a9dc2a5d1830dc0518b05c5681d5ba95ab43` |
| Packet framing, Linux ARMv7 | `839938804f297a06ce40c9ff6c0f59ea30cc8863` | `54747081ae9da817301b5d8db4324c1781cf240a5ef867b1678fbdc16ef3143f` |
| Packet framing, Linux AArch64 | `64fc1b08163364dd1f9cba25dc17970e6bc9555f` | `aa9eaa38cc3463de47490dcc449b581da9d45cc8c80f407d0d97b4279751013b` |
| Packet framing, Linux ARMv7 | `64fc1b08163364dd1f9cba25dc17970e6bc9555f` | `70dee62eda8f3ab761c7eb05d445d4a913ce7ef8cbc8ca2e7a3035a1843b2f9d` |
| Gate run, Linux AArch64 and ARMv7: codecs, decode sessions and audio codecs in files, FATE parity and the timing of both workload lists | `79e0ae138c71ffc57c73decc88d29ae86b1153d4` | `5ca320791dad42f2a8a31ba7fb182752593a44a343c736b831bd8dbd82423250` |
| Gate run, Linux AArch64 and ARMv7: subtitles, text, `subtitle-text` | `79e0ae138c71ffc57c73decc88d29ae86b1153d4` | `13675118ba3b998743aaebb346da8f553992d040071a692b8aba3e67659dee77` |
| Gate run, Linux AArch64 and ARMv7: subtitles, text, `subtitle-text-host` | `79e0ae138c71ffc57c73decc88d29ae86b1153d4` | `4c8edb811be6cddf913c23125e71da746bddcc7eb514edd50304e75f97d24476` |
| Gate run, Linux AArch64 and ARMv7: subtitles, PGS | `79e0ae138c71ffc57c73decc88d29ae86b1153d4` | `a5e044f794746c06a88a18a66faf16f35766302883db8eaf7ce4dd6aea905b7b` |
| Gate run, Linux AArch64 and ARMv7: subtitles, PGS capsules | `79e0ae138c71ffc57c73decc88d29ae86b1153d4` | `a16ebfe68d3039554feb024b2b2fa9cea8cebd4223c15589b0e015c76f387c6b` |
| Gate run, Linux AArch64 and ARMv7: subtitles, DVB | `79e0ae138c71ffc57c73decc88d29ae86b1153d4` | `31e96b79d2fece24e0c0c9c8d0c1237e972927393be9075a34896a086b7585c9` |
| Gate run, Linux AArch64 and ARMv7: subtitles, DVD, `subtitle-dvd` | `79e0ae138c71ffc57c73decc88d29ae86b1153d4` | `0c8ed8d49c9fe2c907a518ac0a1a335cb7d56878baa5fe00b4a9f665966637ac` |
| Gate run, Linux AArch64 and ARMv7: subtitles, DVD, `subtitle-dvd-host` | `79e0ae138c71ffc57c73decc88d29ae86b1153d4` | `b6726eaf269f89a53d0bc34637f80d79956001487d1db25c2de9f81bf8174abf` |
| Gate run, Linux AArch64 and ARMv7: teletext | `79e0ae138c71ffc57c73decc88d29ae86b1153d4` | `3061157c384c874ae2dec8ad092a0e2814ce1df3bee79801ee67f0d457bceefc` |
| Gate run, Linux AArch64 and ARMv7: closed captions (the closed caption rows cite earlier measurements) | `79e0ae138c71ffc57c73decc88d29ae86b1153d4` | `76b7df1d34359f58a0fa967d02db6061c0470c30d663b697c69a3eb57856195d` |
| Gate run, Linux AArch64 and ARMv7: capture (the capture rows cite earlier measurements) | `79e0ae138c71ffc57c73decc88d29ae86b1153d4` | `a0bd067815114b690cdd1fe3fea5206e3c78e7bdf886886dc73971b0cd35529b` |
| Partial gate run, Linux AArch64 and ARMv7, one c6g.4xlarge per target: the font, shaping, rasterising and ASS comparisons, `ass-render` and `ass-render-host` | `69d4d0f8fb97673163d4c0496f26137d9d495efc` | `81a8db361506139c4c50d26363224b1715aef5d952950bb6fbfc420cfc37bd23` |
| Gate run, Linux AArch64 and ARMv7: font, shaping, rasterising and ASS comparisons, without the fonts on ARMv7 | `79e0ae138c71ffc57c73decc88d29ae86b1153d4` | `ada63c33c6f79e413b5a7155cb9b17171196bc7c0a17c1dc3be00ca1ee9abb98` |
| Gate run, Linux AArch64 and ARMv7: the workspace tests, checked and release, and the core time test | `79e0ae138c71ffc57c73decc88d29ae86b1153d4` | `46aa709c5f445b438ef94d589fd52979b5de8fb3c1c4605a036ae3445893b73b` |
| Gate run, Linux AArch64: plans, step records, logs, summary and host times | `79e0ae138c71ffc57c73decc88d29ae86b1153d4` | `b3e4fff20e89cc54f6404088212ba5c1eb20a61acb9a2796d304d54f61ded5a4` |
| Gate run, ARMv7: plans, step records, logs, summary and host times | `79e0ae138c71ffc57c73decc88d29ae86b1153d4` | `612813087738e85133648515bfa55a6760fc8c63338cf3fa5c9fc2f83f3aad3c` |
| Partial gate run, Linux AArch64 and ARMv7: MP4 range, MP4 forward, Matroska startup and seek, Matroska audio clocks and the six Matroska feature groups, with each run's records, summary and host times | `9be327a202b7d31c464bcc6308864a3cb66f9c25` | `a96d7d7451c0c94371c3955212fb9e3c7772a1db69667420c239dd7c3f6e0e90` |
| Partial gate run, ARMv7: HLS segments, TS program and raw Annex B, with the run's records, summary and host times | `359a90b46ba2bfa9a896b827c9bd4869ff7b7b48` | `8963a7a9ce2ef4ed8841838e4c880ccf881b05296c198bb53328d002c6587254` |
| Partial gate run, Linux AArch64 and ARMv7: the memory of both codec workload lists | `8154812e05da374a0f7c173bfd01770d5bd0ca85` | `6ad80f90b4784bea932beae4bab944354a42d34dec581faa97d3b74e5cd2e56f` |
| Partial gate run, ARMv7: the font, shaping, rasterising and ASS comparisons | `8154812e05da374a0f7c173bfd01770d5bd0ca85` | `e3561dfe596482134846bf8f629844fc0e1314fd7b8ca3ed9077eb3269b36fb6` |
| Partial gate run, Linux AArch64: plans, step records, logs, summary and host times | `8154812e05da374a0f7c173bfd01770d5bd0ca85` | `1a6d1206380ad9b81fb67352d1d90775fa301182b14e7a66d74172434fc8173d` |
| Partial gate run, Linux AArch64: codecs, codec workloads, ISO BMFF reader, Ogg readers (and ISO BMFF open and index, superseded) | `4d31df88e5805ec85b2d858f12b9ac7a0ae11a5c` | `6a55a3b0db8d15f45346510df0dde777d1aef60fab5c473f68116fe25c2c4ba5` |
| Partial gate run, ARMv7: codecs, codec workloads, ISO BMFF reader, Ogg readers (and ISO BMFF open and index, superseded) | `4d31df88e5805ec85b2d858f12b9ac7a0ae11a5c` | `840ad51a537eb0b05aea13fa2597836c4b1e5301f6eb01e884399f6003b128cb` |
| Earlier: partial gate run, Linux AArch64: ISO BMFF seek and range plans, before the sample-table walks | `001597859a0faf2508d29773627116ee33689cda` | `f4bbd658d24164f76bee744ed119f7ff8fda400f04da071e3667d8f24a05dc0e` |
| Earlier: partial gate run, ARMv7: ISO BMFF seek and range plans, before the sample-table walks | `001597859a0faf2508d29773627116ee33689cda` | `fcf9a2a4dffb550368cae52927cde3b08fd8693e48639a53c2498f120f21f339` |
| Partial gate run, Linux AArch64: ISO BMFF open and index, seeks and range plans, reader comparison, MP4 forward and range, DVD subtitles, closed captions, codec parity | `e2d4a6a372409412afb3e99eaab11f384796ebb2` | `633c9f5d3b7637c9f229e09a3a2995587e83225495203c4120d76699bc575d50` |
| Partial gate run, ARMv7: ISO BMFF open and index, seeks and range plans, reader comparison, MP4 forward and range, DVD subtitles, closed captions, codec parity | `e2d4a6a372409412afb3e99eaab11f384796ebb2` | `5a8ee161c466c84044abb9efaa1771c685585ee5e7a75d9a030428c86e09a924` |
| Partial gate run, Linux AArch64: MP4 forward and range, ISO BMFF seeks and range plans | `d7a6d8f78c7d9963455f3a0872ca935b79fa14b4` | `72005e2ef1bcf35601169f05e20573f8f48432af2bf8484384335fc605742c2e` |
| Partial gate run, ARMv7: MP4 forward and range, ISO BMFF seeks and range plans | `d7a6d8f78c7d9963455f3a0872ca935b79fa14b4` | `46d6e06cb79d76d0bc2eeb9cbfed97e045b66620645097a1c2921bdb19fbed09` |
| Partial gate run, ARMv7: plans, step records, logs, summary and host times | `8154812e05da374a0f7c173bfd01770d5bd0ca85` | `b3abda80845d0876416a37361b5b4f90204cf3107793d405a82e2d59fd5641e7` |
| Partial gate run, Linux AArch64 and ARMv7: codecs, FATE parity, the timing and memory of `workloads.json`, and the workspace tests, checked and release, with each run's plans, step records, logs and summary | `8d6f278949aed417290058f1212dee0fc2a97243` | `db33a447578b4eaa7615e55318dbd13307826de18c4a720db64b515d19a6c69e` |
| Partial gate run, ARMv7: the timing and memory of both codec workload lists, shard 1 of 3 | `4cbd1e8623fea61bad5139c63daf78938ecf18a5` | `b48c34422ff83174bb79051f7876dd4c3b4be9ef237835e66bb95c1cd8b8c7dd` |
| Partial gate run, ARMv7: the timing and memory of both codec workload lists, shard 2 of 3 | `4cbd1e8623fea61bad5139c63daf78938ecf18a5` | `38e339510fd6e78b09bb79b1eb56b7d5754e906de10876073c0d5cd10db401f1` |
| Partial gate run, ARMv7: the timing and memory of both codec workload lists, shard 3 of 3 | `4cbd1e8623fea61bad5139c63daf78938ecf18a5` | `dcb35289bd2de6bd15fc319a8aae0b37c8a22684c3fed28b678d354726cfe7fe` |
| ARMv7: the causes of the codec timing failures (A32 with NEON timing, instruction and cycle counts, profiles) | `4cbd1e8623fea61bad5139c63daf78938ecf18a5` | `464ed3a644ca440687ffd3ad7fcbf48db0657378a903faeb04ee343d651adcee` |
| Partial gate run, Linux AArch64: closed captions, `subtitle-cc`, the shard's outputs | `6d5145d7f31b002c72f9815130b522964d2abb64` | `7c9c1afadc396331987d6bee144a7e4bb28a46c36b41d012d5cde95880100497` |
| Partial gate run, ARMv7: closed captions, `subtitle-cc`, the shard's outputs | `6d5145d7f31b002c72f9815130b522964d2abb64` | `284603aa021f0ac4123d4d581e3e1804ae8ee17ce928262a69a6607c3eea7277` |
| Partial gate run, Linux AArch64: the audio codec correctness lanes, `codecs-audio` | `bbdefa706033fb0e715b4ab5dd94b2cfdced4eeb` | `046b27c09e4a14716eb4fbf2f421fed1092fe4691d91313880c5e1e259154573` |
| Partial gate run, ARMv7: the audio codec correctness lanes, `codecs-audio` | `bbdefa706033fb0e715b4ab5dd94b2cfdced4eeb` | `6e3e725f8e08b3801b0a04741c7b2c00eb74f537f5079cce55dec4dcb5610e5c` |
| Partial gate run, Linux AArch64: the workspace tests, checked and release | `f9b3f13839e31b8a1c97cb299902e9748acaf9ec` | `07715774a1e6e3d70b81912dc6498fc8d460285699a999b8351bab2d04da8479` |
| Partial gate run, ARMv7: the workspace tests, checked and release | `f9b3f13839e31b8a1c97cb299902e9748acaf9ec` | `78d171085e6d6e671d46fcd595877ffd7417ebedcb28f9b5f54d67eed52242d8` |
| Linux AArch64 and ARMv7: the H.264 and HEVC decoders on the picture output model against `985bb4d4` (equivalence, conformance, caption inputs, tests, instruction counts) | `8d6f278949aed417290058f1212dee0fc2a97243` | `8e3485b2a0b184649f87ee5ae2bbbf1562ed3f786c3865f1c898ad4dee2f30b4` |
| Earlier: Matroska / WebM | `513149a970caad5b30c61becba1bc2bda1c5e193` | `883ce5128410aefb3ec9affe3579a798ee8d1c5d082c1d129a462d67728e7af9` |
| Earlier: Matroska startup and seek | `513149a970caad5b30c61becba1bc2bda1c5e193` | `883ce5128410aefb3ec9affe3579a798ee8d1c5d082c1d129a462d67728e7af9` |
| Matroska startup and seek, preceding comparison | `0e28298d038bea072629b3bad6389dcc806b4861` | `a20dcbbe804bfd21ee631cc42f61466d2400d5115abb8ea97dd243ef733ba74f` |
| Earlier: ISO BMFF / MP4 | `f958c0b9624a8b7c7bd91b7d2a0c30299ae1dbed` | `fd718807e0ec1e6287bcb690226bf7056b79985796e74eba54f43544a034bc2c` |
| ISO BMFF comparison | `0a033ec3406aebd09d191298a546659e02c6b76c` | `f56d3fa78b8cc70222f58c7cfa9a17331b0924a9070c7ff6ecc6dc1f213561bc` |
| Earlier: ISO BMFF open and index runtime and memory | `f99d893196908d562c897bccb20c6a972cd25d11` | `806f2f996b19370bb23350ae4a166f3b7688b3e0e499352d77c3b2610c90aa7b` |
| Earlier: MPEG-TS PES | `21c9eef2f08a11bb7bd1c457997095326f2bb42d` | `8ad96ce3a9ca2b5e4b23dfcc53659fb4cc99ae0638d502e114188b3acbdad2f5` |
| Earlier: MPEG-TS access units | `230d1314464d34a04f5f2614298e5b7009292f0c` | `a86e737ad8081e349a4c3a707b6b29ecfee6ae8bbeaabd9a9edc5f846e2da697` |
| Earlier: MPEG-TS program playback | `21c9eef2f08a11bb7bd1c457997095326f2bb42d` | `8ad96ce3a9ca2b5e4b23dfcc53659fb4cc99ae0638d502e114188b3acbdad2f5` |
| Earlier: MPEG-TS progressive playback, compact and self-contained reports | `ec6afe8635509d7e9007fd3d3c20ce2bf1c25835` | `f35c3d9e3abba7519187a6f7821b47362f5f58869637738a7490e9329d10067f` |
| MPEG-TS progressive playback, earlier report comparison | `855bfea306ee7c2f7a933505cb18cde287f91a7c` | `d144f900ef1598b8300ddc7966349268fbea62c4a726fccf4291c766273e2e6a` |
| Earlier: MPEG-TS progressive playback | `21c9eef2f08a11bb7bd1c457997095326f2bb42d` | `8ad96ce3a9ca2b5e4b23dfcc53659fb4cc99ae0638d502e114188b3acbdad2f5` |
| Earlier: HLS segments | `21c9eef2f08a11bb7bd1c457997095326f2bb42d` | `8ad96ce3a9ca2b5e4b23dfcc53659fb4cc99ae0638d502e114188b3acbdad2f5` |
| MPEG-TS progressive playback, preceding comparison | `23f68766543bcd57a96e88bfe5350095236ff88c` | `a372562b91bb14dce3bbd7117f71deb098f6dba49492c5f4396a70336012d6fc` |
| MPEG-TS program playback, preceding comparison | `23f68766543bcd57a96e88bfe5350095236ff88c` | `a372562b91bb14dce3bbd7117f71deb098f6dba49492c5f4396a70336012d6fc` |
| MPEG-TS progressive playback, earlier comparison | `5ec7c1f8a7816b1427631f22a8ca77ba74abeeee` | `fb8a3a3b1f7f3d687254a3f4c1849dede7a78bc3c62dbc7905b09a45f675709d` |
| MPEG-TS program playback, tight carrier comparison | `eb3279f36c19c97dd9bdd68b1e66f371f28e38c1` | `7e18edf0a340b6ff4bb296b52deffc720f706c464340c7c73392127f9c87248c` |
| MPEG-TS program playback, earlier comparison | `3125c1e61d73a575419f7b653a1dae44a3269dae` | `beb5fa935b0109d0e33dbc3f57c22481c875a4c2b065820dda247535dc431f3a` |
| HLS segments, earlier comparison | `5ec7c1f8a7816b1427631f22a8ca77ba74abeeee` | `fb8a3a3b1f7f3d687254a3f4c1849dede7a78bc3c62dbc7905b09a45f675709d` |
| Earlier: packet framing | `04512a07fbb7efdf0d6d7173db08ec1267f2fae3` | `677ce4b5aef58efd8523868cd2318d9cfde07868d2b7451d0017f013dfb3130b` |
| MP4 forward and range playback with a leading empty AVC edit, AArch64 gate | `eb3de5af730ce00ec0dda1d4ea937470b7b5d074` | `d059bad42f84ab99acaf1ed9c1db42fd98132167abf2f451b357af0f44e7dd74` |
| MP4 forward and range playback with a leading empty AVC edit | `f16392a95cd03c7ef2b90445baa994e637ca3ead` | `f62922f9609fda3ae29dd1d3053a9e2917e5d5a94333a4caa3e0db29c993365c` |
| MP4 range playback with a track shorter than the movie, AArch64 gate | `bbe4a648b5b4c30ef735b50bd3a2cb23c5b1f6ee` | `a708ef538f9df4982529a16ae9d24751bc93bf79e7b98469b75f4ce0a239c79a` |
| MP4 range playback with a track shorter than the movie | `df8e76b816e05904db6b3c57615ef8f235dd50b3` | `c931c162d04fdbde96b7a9cc2272fb0b22134aadbf44d749c684aedd952944db` |
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
| Earlier: codecs, the HE-AAC v1 and v2 workloads, timing, memory and instruction counts, Linux AArch64 and ARMv7 | `700c2134a123d8c9566c1cdccdeb3a0b3b0399c0` | `838ee260edf0c87fa7f1feb154d9ae83529c2a817b9d92c7c6b9ba72fae2d239` |
| Earlier: codecs and decode sessions, runtime | `add6b272cb622bae0d921dc3fbbf0812958c2023` | `ab2a174847322658159e93b2cd9bb994cdd9f4f9438926191518897aff28d929` |
| Audio codecs in files: the per-workload runtimes the row's Open cell cites, and the earlier row | `ccff5b5734c113c1d88160b84825157508c701c0` | `0ddcc517728bb39e2d74cd2e3bf4c00071a6f3815ad3409d5215d216af9a0a53` |
| Ogg, audio elementary streams | `ccd3090572433e79d8ab65a377fcd9896d2839d9` | `8123e0f020a9c4ccedd83aea3e665d4486d55e26612e8bc29626e9a3026e2cd1` |
| ARMv7: capture's kernel-bound memory, collected in parallel, with its parity and timing; codec memory in parallel | `4b43bf7497c18822e96ca224fa5ba0e367088f93` | `6429b9987bcf25b09b19e6fbf7aa8af0c7e76ae54a4b73117145e5fc3c0d3040` |
| ARMv7: capture, the conversion probes, parity, runtime and standard-bound memory | `dcf51398e97c781d517c2c577194a9d92f8c7f15` | `1b48bb701d74e5bc17a0e930ecf08e1061454b94e98b519b092573d9d24e0d99` |
| Capture on Linux AArch64: parity with the conversion open, tests and instruction counts against main | `dcf51398e97c781d517c2c577194a9d92f8c7f15` | `016c8619d9526186484f68101855e338034d3d5ff13a6bf0641d34615cde21a2` |
| Capture on AArch64 Linux, [set 1 of 5](../harness/evidence/sets/gate-run-aarch64-9ce47662-20261116-033256-shard-1.json) | `9ce47662208072cf3ab14a69aef15fcc748cb81e` | `bbaab5db72eb034862167a349603e66d122d17b213825adfc58f1058e541e2f5` |
| Capture on AArch64 Linux, [set 2 of 5](../harness/evidence/sets/gate-run-aarch64-9ce47662-20261116-033256-shard-2.json) | `9ce47662208072cf3ab14a69aef15fcc748cb81e` | `e00521b17731b50c5a3a421937ad6e3a6d43eca4f90c23dcab3e7bb024639cb2` |
| Capture on AArch64 Linux, [set 3 of 5](../harness/evidence/sets/gate-run-aarch64-9ce47662-20261116-033256-shard-3.json) | `9ce47662208072cf3ab14a69aef15fcc748cb81e` | `5cdf288cd4500f4403b75cc9eedb8ab6214093e38026aba70eec867e7bffaf85` |
| Capture on AArch64 Linux, [set 4 of 5](../harness/evidence/sets/gate-run-aarch64-9ce47662-20261116-033256-shard-4.json) | `9ce47662208072cf3ab14a69aef15fcc748cb81e` | `cf1b760ac1049defdc0a510b0dc0218b2e7344f73eb403306880e1dec76b8a87` |
| Capture on AArch64 Linux, [set 5 of 5](../harness/evidence/sets/gate-run-aarch64-9ce47662-20261116-033256-shard-5.json) | `9ce47662208072cf3ab14a69aef15fcc748cb81e` | `148cea20cd4e70520438e6caef597f66c583d29210fb3f19ab5b480f94412ebf` |
| Capture on 32-bit ARMv7, [set 1 of 5](../harness/evidence/sets/gate-run-armv7-9ce47662-20261116-033329-shard-1.json) | `9ce47662208072cf3ab14a69aef15fcc748cb81e` | `b1361f7897e04cf360df70f7a4d1c6a492b576d095709abecf6ba5237fb777c7` |
| Capture on 32-bit ARMv7, [set 2 of 5](../harness/evidence/sets/gate-run-armv7-9ce47662-20261116-033329-shard-2.json) | `9ce47662208072cf3ab14a69aef15fcc748cb81e` | `8ff434aa1fbd4fb846fb462c13dc45fba9c60ed5491c46d225bb66c9ea6f3fdd` |
| Capture on 32-bit ARMv7, [set 3 of 5](../harness/evidence/sets/gate-run-armv7-9ce47662-20261116-033329-shard-3.json) | `9ce47662208072cf3ab14a69aef15fcc748cb81e` | `8e996f012c6f7448af11ca22e35d37b90c83e54aa3565dc824e41a8d3ebbeae3` |
| Capture on 32-bit ARMv7, [set 4 of 5](../harness/evidence/sets/gate-run-armv7-9ce47662-20261116-033329-shard-4.json) | `9ce47662208072cf3ab14a69aef15fcc748cb81e` | `7b5a95d7d996ae5c6b0ad39a4a70f4859d113401d9c950396fffae4548959462` |
| Capture on 32-bit ARMv7, [set 5 of 5](../harness/evidence/sets/gate-run-armv7-9ce47662-20261116-033329-shard-5.json) | `9ce47662208072cf3ab14a69aef15fcc748cb81e` | `a777a5ff679b34c9a3038d00986fe4ac94920fb092de0daca8d7c02c030f7b1a` |
| Wide short-frame conversion on AArch64 Linux | `cfcb1c97c65eb077063489b0191ff83f46049f37` | `700006d0c0f559aedcc7c6e56a4d7a36351047f63357658a4ae6e9204d9e383e` |
| [Short filtered capture conversion on Linux AArch64](../harness/evidence/sets/capture-short-narrow-aarch64.json) | `a62c8eab90b2ce87b15d8319c695f40d6c66a7be` | `fd51990053cd9355b0daf7eda09cf753c6ab9dadb47a24dda7d567de7ab0130a` |
| [Short filtered capture conversion on 32-bit ARMv7](../harness/evidence/sets/capture-short-narrow-armv7.json) | `a62c8eab90b2ce87b15d8319c695f40d6c66a7be` | `f8ef3f3b0a5a3361079650e4bc3597f59caa9c2b08f2ba9bebcbfa2ccd0e4c94` |
| Capture on Linux AArch64, [noise-case memory timeout](../harness/evidence/sets/gate-run-aarch64-576f088f-20261116-052543-shard-1.json) | `576f088fa2d463585adf87d471ba2ccc22c72c67` | `2291de167449144eaa0f1951d2f6e352acf7d0e7549b71aa3e62bd2b0609beb0` |
| Capture on Linux AArch64, [set 2 of 5](../harness/evidence/sets/gate-run-aarch64-576f088f-20261116-052543-shard-2.json) | `576f088fa2d463585adf87d471ba2ccc22c72c67` | `934dc15b5da67410fa4d4db7743feef4dad26b282d039094485ad22c6461b667` |
| Capture on Linux AArch64, [set 3 of 5](../harness/evidence/sets/gate-run-aarch64-576f088f-20261116-052543-shard-3.json) | `576f088fa2d463585adf87d471ba2ccc22c72c67` | `4092c01e475c86ca437bb29c14a1d307bacb8dbdc077c5e4fa56f2461d6e5a35` |
| Capture on Linux AArch64, [set 4 of 5](../harness/evidence/sets/gate-run-aarch64-576f088f-20261116-052543-shard-4.json) | `576f088fa2d463585adf87d471ba2ccc22c72c67` | `d1d029f92b24c849333eb438855db72730737613372ca0635bb0e7ffe4c25d2e` |
| Capture on Linux AArch64, [set 5 of 5](../harness/evidence/sets/gate-run-aarch64-576f088f-20261116-052543-shard-5.json) | `576f088fa2d463585adf87d471ba2ccc22c72c67` | `101d76180dd7922e18bd50f51468b966bf6b8155545b4d8736413a8a27fe8749` |
| Capture on Linux AArch64, [noise-case timing and memory](../harness/evidence/sets/gate-run-aarch64-6d7a0f3c-20261116-062729-shard-1.json) | `6d7a0f3c888c9cba8e6be7eddb760b5ea28b9c30` | `592b2aa3785d0eda4be492ecdc6fbc6a666906835c31c328abcaf944357bc3cb` |
| Capture on 32-bit ARMv7, [set 1 of 5](../harness/evidence/sets/gate-run-armv7-576f088f-20261116-052543-shard-1.json) | `576f088fa2d463585adf87d471ba2ccc22c72c67` | `94919c7b26a6413a6ea544f54f2dafc9b5b51bed0753b6ed530f666cf0b77920` |
| Capture on 32-bit ARMv7, [set 2 of 5](../harness/evidence/sets/gate-run-armv7-576f088f-20261116-052543-shard-2.json) | `576f088fa2d463585adf87d471ba2ccc22c72c67` | `559d8625d66f3c7a389bd38be9f47b9293a6521c184c8a652beac85b756b9748` |
| Capture on 32-bit ARMv7, [set 3 of 5](../harness/evidence/sets/gate-run-armv7-576f088f-20261116-052543-shard-3.json) | `576f088fa2d463585adf87d471ba2ccc22c72c67` | `591d0edade68c389c63646d6e09275ac648a163a780123443142749ee1d930ff` |
| Capture on 32-bit ARMv7, [set 4 of 5](../harness/evidence/sets/gate-run-armv7-576f088f-20261116-052543-shard-4.json) | `576f088fa2d463585adf87d471ba2ccc22c72c67` | `afdb2e58cf505fc31f6de184a47eeaeca1015c837d3f24d5c8d429d47bb5362e` |
| Capture on 32-bit ARMv7, [set 5 of 5](../harness/evidence/sets/gate-run-armv7-576f088f-20261116-052543-shard-5.json) | `576f088fa2d463585adf87d471ba2ccc22c72c67` | `50b4bc9bbede9dee1300f5cd0fdb2c1d430e37907b278242055fcf334da87062` |
| Capture on Linux AArch64 under the link policy: tests, parity with the conversion open, runtime and kernel-bound memory | `5aa554886320b59d220523032d9b81e998a60deb` | `c8a13c1a1388ffdcc928206334410d0d6d7556943d432f694fcbdbc881d47921` |
| Codec and capture timing, earlier and shared runner | `c21176e03328f26e0f49d6870dad0a55dd7c5e76` | `9a2cb6808d49462ff33e0666a44773692c44e85414f5aa9b2a21ea27a11a6bcf` |
| Earlier: MP4 forward and range playback, AAC edits | `f5453ef9d88712777aa5b6383250875456e9f334` | `bb60ca1d1571ae40f6cc38e1d7f1315305eb2abd4daa371e1944f8b1c8273223` |
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
| Earlier: Matroska audio clocks | `513149a970caad5b30c61becba1bc2bda1c5e193` | `883ce5128410aefb3ec9affe3579a798ee8d1c5d082c1d129a462d67728e7af9` |
| Earlier Matroska audio clocks and playback profiles | `b3579f729494a37a4dbebe03c475f7b38484046e` | `907bb7a35d70d09673ccbf0904a8b1ab85890a632454b8adf291d73a50672136` |
| Earlier: subtitles, Linux AArch64 | `f263491102d307997d40d40826a6193d8468e87e` | `9ed7263aabbc4f44910c5135fcf077419ea5b426e3a09a888e83f10153680c32` |
| Earlier ARMv7, A32: subtitles | `8513398b85d27aa093b8212c202b25c32da859bf` | `9ed7263aabbc4f44910c5135fcf077419ea5b426e3a09a888e83f10153680c32` |
| Earlier: closed captions, Linux AArch64; ARMv7, A32 | `9d048c58fb30c64777f7230eb67c7037a1e1bcf1` | `2b78cc89c624e9296967d4f3ec8aa0e5713631d1d3228c63c8b96eb1c4e6cfd1` |
| Earlier: subtitles, PGS, closed captions, teletext | `956c42961f4a27d88f96d5800d59ff1eeb088018` | `61b52a272e82eeee913c9c4b4d6331caea3283a99ebad4a897aba65dab3c891d` |
| Earlier: subtitles, PGS memory | `03f3efd3a0fde0d558c68f5408c521d632802fb7` | `6c0cc3a669152b3eeba9a1189939c53b1b68e2f5cb9ab6cb81f903ec4358634d` |
| Earlier: subtitles, DVD | `531e2d04ba7386513ff7afe7b44f53b2a93ca9dc` | `01ef0a02aad45d7b754c0e409e12949437e8f7f0270e252e82a3f7ebff0e6391` |
| Earlier: subtitles, DVB | `0d70bebfc0799f65df14f170ed97e68c5a35daa7` | `06d1a46a967e052ce365795827803ddbc5ae8ff0e2452ae83428f53744949077` |
| Earlier: subtitles, text, Linux AArch64 and ARMv7, A32 | `6126b759b9b983afe488c3c362c6e37c2d1425a2` | `aec763325453d4174a1d70764d4e863a77ade9315ee607b14cfe44d1eb8a7676` |
| Earlier ARMv7: subtitles, DVD, PGS and teletext, the 8 crafted size, fragment and trimming cases, parity, runtime and kernel-bound memory | `da6545639aa4cb878ca62bfb384ea3aeff7f8309` | `2445c829a23ebd335a86ad4f3d946ed78053617d40f25db1c680612ed8eec145` |
| Earlier: subtitles, DVD, PGS and teletext, the 8 crafted size, fragment and trimming cases, runtime and memory on Linux AArch64, parity and instruction counts on Linux AArch64 and ARMv7, A32 | `c79ae2da8b8a0d004d37936100eedd7b46e0a589` | `a5afebfe3a1c86829b0768823e7aa6518d008fd450104a72ca95b812113019a9` |
| Subtitles: DVD, PGS and teletext, the review commit's gates, traces and instruction counts on Linux AArch64 and parity and instruction counts on ARMv7, A32 | `821900952ba46678d07300ea7e4bb04ec517f324` | `6fc99861c2fc41d8b1fe161148c3c916f84425142d437c0eb09ed5b1b3936731` |
| Subtitles: DVD, PGS and teletext, worker traces against main on Linux AArch64 and ARMv7, A32 | `92d1a1c5c83b8b39b12ae1ed11b999badbeddc23` | `425c08ba299c60e13c0690b16ef7599bf4fdaade2cfb8ce32b75c547edc7767b` |
| Earlier: subtitles, text, the `mov-text-tags` sessions, runtime and memory on Linux AArch64, parity and instruction counts on Linux AArch64 and ARMv7 (A32) | `96f660ba99fa33fbc1a2019cda9e73bec6de287d` | `03a2ee2f02daf3c5371c5c44e183c45b4bc655c30714bc12594fc58c72a56311` |
| Earlier ARMv7: subtitles, text, the `mov-text-tags` sessions, parity, runtime and kernel-bound memory | `dd395b41add1b51859be0104ad865de111d7b401` | `f3a4c240aa2177b1af1ec6557318208bd417fc983adde2e742de5d54bf0a5713` |
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
| ARMv7: the resident pages of both framing workers by libc's load address, and the earlier packet framing row under the link policy | `4a932ef908ca783976c56e187c0563ce955f15b5` | `e67e0660c55ebde8ec616239c0dcde734eaa59f508160e1554b2a7df5ec32ae1` |
| ARMv7: the earlier closed caption row and Matroska, startup and seek, HLS, ISO BMFF, audio clock, MPEG-TS, playback and packet framing bounds; the third collection of `charset-cp950-nomapping-a240` and `long-movie-mkv` | `4fc96a499ecc1c1a8a6a23d11485123cc4e817a7` | `9328926f64b4a90ef5d4d8161059c5dae72105aa02856bb5f74c221ad9b64bda` |
| ARMv7: the tests, the rasterising, shaping and ASS parsing comparisons and the video codec timings; the earlier bounds of the other rows | `3df7ae6d504d67e054ffe9a54901620c134ba2c6` | `65707cc2d5ece5ed7d907509b9f2cd4f438ad380be3109be368244b04d3f12fd` |
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
| Earlier: ASS rendering runtime and memory, Linux AArch64 and ARMv7, A32 | `5b3e03f0e9410853152043d033b4f4d757e98a40` | `632964773b8ca4cbd5b2b3c09a2b6ddeafdaeeaff56588ea345bae8550012bb7` |
| Earlier: ASS rendering memory against the instrumented libass build, Linux AArch64 | `e6de7e70827bb930a8ef9e5cb4d30b1e4e7d0727` | `799386520d27ae7be5eab6c827fcbe0608950ddb4c32c93be7ea3cf660d4b0e3` |
| Bidi from UAX #9: comparison, conformance, generated texts, equivalence and instruction counts, Linux AArch64 | `81a17b4edf5e24611eab70ac1ddf9b3cf4c37603` | `63104dd7b693a2c559906ca279a2350b4a273083c03fabe4fe097d49e56b961f` |
| ARMv7: bidi from UAX #9, comparison, conformance, one seed of generated texts and instruction counts | `81a17b4edf5e24611eab70ac1ddf9b3cf4c37603` | `18211802f6b7d171f06852a6f38459957ae6049a7c65930f1c96900cc33edb36` |
| Earlier: bidi from UAX #9, Linux AArch64 and ARMv7 | `e44bfacb38a19598eca07ea96a9aa83a05a9b1f2` | `dbf448c1ada891ab4122396818350e39df1171ff58e7cd77de0c354abb22c943` |
| ASS collision order on boxes 2^31 rows apart (`collision-far-margins.ass`), Linux AArch64 and ARMv7 | `02e3a75a86bd5c22900d972c02055e23a37103ab` | `736748aa549fec74078fa33d31dba3e0a123290e652278c3359d37a0a9737db2` |
| ASS collision order: comparison, generated scripts and equivalence, Linux AArch64 and ARMv7 | `1d2ae4a0287972ac3402bd2e717e5b7fdaa086ff` | `e4e97db001bf538840edc2cbfb8db80c126aeb97df3edeb68cf7185b2fe4bd04` |
| ASS rendering comparison and generated scripts, Linux AArch64 and ARMv7 | `01ccb49792cf1d4d05b74addf35dc616763b88e0` | `c510f31add480041fa7197ddbca26fe143926cf84106890b061b3b99897749be` |
| Earlier: ASS rendering parity, Linux AArch64 and ARMv7 | `28e5ccf0ecb5419f7105d52c91ebe8d6c09f053c` | `bc26dfc21b141cc79b5bb4f8cb3d636dcfaa3f64fa24eec2d200409eff0c32f0` |
| Earlier: ASS rendering comparison and generated scripts, Linux AArch64 and ARMv7 | `fedfb790bfb52131389cb7c5134bab7ab813b55f` | `f10a94cf995ed49ac8e9befda3441efcf3c900a984105b1385c3d34444418d1f` |
| The shaping cases that need the TrueType interpreter: font, shaping and ASS rendering comparison against main, and instruction counts, Linux AArch64 and ARMv7 | `dc792ea4b25af0e64db6b73b47ce66dc89b6e045` | `45aee0c0f9681c334a2eb2e495f83e60ee60df6701b7c8961d2b961e845817bf` |
| The TrueType interpreter after review: font, shaping and ASS rendering comparison against main, and instruction counts against main and against `e110f29c`, Linux AArch64 and ARMv7 | `218065d60fb7520fc5e636c8451b46a4d6bda61f` | `e017f46cf9d4ff61e7b0615a16ee1d39eac92d92c8bafc43141a255e6ccec818` |
| The TrueType interpreter: font, shaping and ASS rendering comparison against main, instruction counts, and ASS rendering runtime and kernel-bound memory, Linux AArch64 and ARMv7 | `e110f29cb904c0a4f3925fc90a2d1a10a67aeff4` | `aa45ccdbeecec1d775a19639afae3d4559dd2cdbd91570a6bb07af9d84155033` |
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
| Earlier ARMv7: subtitles, DVB palettes, the 12 crafted cases and the three `sequence` cases, parity, runtime and kernel-bound memory | `74155089881341aa7aae67e5f754b35a86e4898b` | `63453697efcbf39e4af583c5f546d1f366e2e49c4b6a9c97df3e867ca1293646` |
| Earlier: subtitles, DVB palettes, the 10 new cases' runtime and memory, instructions on both targets | `aebbe5827b1b2ae8f1dcf53a1be67053c850a279` | `d856570b6485dc339bc9fdcb2dbf76824ffd9e2e760b8d3efab36ca7ef291dc3` |

## Platforms

Qualification runs on native AArch64 Linux and, where the next section says so, on
32-bit ARMv7 Linux. x86_64 has no performance runs yet.

### 32-bit ARMv7

The gate build for `thumbv7neon-unknown-linux-gnueabihf` (Rust 1.99.1, default flags:
Thumb-2, assuming NEON), with every FFmpeg, FreeType, HarfBuzz, FriBidi and
libass reference built from the same pins in the same user space (Thumb-2, the armhf
compiler's default; FFmpeg selects NEON at run time). The Ubuntu 24.04 armhf user space
archived as `armv7-userland` ([how to repeat it](quality.md#32-bit-armv7)), with the 15
packages libzvbi's build adds (autoconf, automake, autopoint, libtool, gettext and what they
depend on), on AWS c6g.4xlarge hosts (Graviton2, Neoverse N1 r3p1), kernel
`7.0.0-1014-aws`. Unless a row names another revision, its runtime and memory come from the
full gate run at `a73350ca`, and AAC in MOV from its measurements at that revision
([measured revisions](#measured-revisions)): each feature timed on CPU 1, then its memory
collected in parallel on CPUs 1–15 ([memory in parallel](quality.md#memory-in-parallel)).
The tests and the rasterising, shaping and ASS parsing comparisons were run at `3df7ae6d`,
and the font and shaping comparisons at `69d4d0f8`, on c6g.4xlarge hosts with the same user
space and kernel.
Memory is kernel-bound in every row: measured under the kernel RSS bound, through its
32-bit entry point, with both workers' cached pages dropped before every run
([quality](quality.md#measuring-performance)), so total RSS is the candidate's proven
resident peak over the reference's observed peak. The codec bounds are
gated as on AArch64; the workloads above the ceiling are open on this target, each with its
measured cause in its list's `open_workloads`.

| Area | Verified against the reference | Runtime | Memory |
|---|---|---|---|
| Core | 51,235 exact-arithmetic cases; the workspace tests (4,326 in debug, 4,325 in release) and the worker tests pass at `3df7ae6d` | via dependents | via dependents |
| Matroska / WebM | 116/116 (six feature groups and the external HDR vector) | 1.1518 (116/116 pass: the six feature groups and the external vector) | heap ≤0.7747, total RSS ≤0.9479 (116/116 lower) |
| Matroska legacy AAC-LC | 4/4 (`matroska-legacy-aac`) | 0.8374 (4/4 pass) | heap ≤0.4763, total RSS ≤0.9608 (4/4 lower) |
| Matroska startup and seek | 44/44 | 0.8294 (44/44 pass) | heap ≤0.3956, total RSS ≤0.8085 (44/44 lower), on `playback-matroska-worker` |
| HLS segments | 71/71 | 0.5787 (71/71 pass) | heap ≤0.0196, total RSS ≤0.8087 (71/71 lower) |
| ISO BMFF / MP4 | 1,020/1,020 open and index comparisons (972 matching outputs, 48 matching refusals); the neutral reader's 648 comparison cases, as on AArch64 | 0.9747 (956/956 timed operations pass) | heap ≤0.2467, total RSS ≤0.8605 (956/956 lower) |
| ISO BMFF seek and range plans | 476/476 (378 seeks, 94 range plans, 4 refusals) | 1.0222 (472/472 pass) | heap ≤0.8541, total RSS ≤0.8704 (472/472 lower) |
| AAC in MOV | the configured decoder comparison: 34 clips, 3,544 access units | 1.1105 (34/34 pass) | heap ≤0.6772, total RSS ≤0.5415 (34/34 lower) |
| Packet framing | 135/135 AVC/HEVC reference comparisons | 1.0431 (135/135 pass) at `d915aa19` | heap ≤0.7694 (135/135 lower), total RSS fails at 1.0866 (0/135 lower) at `d915aa19` |
| Codecs | 347/347 comparable H.264 and HEVC conformance vectors (of 383 run, 31 are outside the supported profiles and 5 are refused by both); the audio correctness lanes as gate item `codecs-audio` at `bbdefa70`, as on AArch64: AAC 38/39 and one open case (`aac-sce-in-stereo`, FFmpeg's unwritten FR plane), AC-3 6/6, E-AC-3 6/6, DTS core 22/22, and `xll_51_24_48_none` out of set (no core substream); the six video timing workloads decode to FFmpeg's digests | 10/20 pass: the six video workloads 0.6113 to 1.1410, `aac-lc-5.1` 1.1622, `ac3-4.0` 1.1407, `aac-lc-stereo` 1.0065 and `eac3-5.1-1536` 0.9934; 10 open on this target (below), worst `dts-core-7.1-master` 1.9016 | heap ≤0.9566, total RSS ≤0.8419 (20/20 lower) |
| Audio codecs in files | the 10 workloads of `file-workloads.json`, each worker's work checked equal before it is measured | 7/10 pass, ≤1.1786; 3 open on this target (below): `mp2-256-48k` 1.8211, `mp3-vbr-v2` 1.7971 and `mp3-cbr-128-joint` 1.7627 | heap ≤0.8993, total RSS ≤0.7706 (10/10 lower) |
| Capture | 15/15 at `576f088f`: conversion byte-identical to FFmpeg's on all 15, mux and encoder pass (below) | 1.1865 at `576f088f` (15/15 pass; worst `c14-motion-960-rate255`) | heap ≤0.6086, total RSS ≤0.9765 (15/15 lower, kernel-bound) at `576f088f` |
| Matroska audio clocks | 10/10 packet folds (`matroska-audio-clocks`) | 0.7710 (10/10 pass) | heap ≤0.7323, total RSS ≤0.9398 (10/10 lower), on `playback-matroska-worker` |
| MPEG-TS | 97/97 compat-view operations (`mpegts-pes`) | 0.8543 (97/97 pass) | heap ≤0.0902, total RSS ≤0.8453 (97/97 lower) |
| MPEG-TS access units | 169/169 transport compatibility folds (`mpegts-access-units`) | 1.0084 (169/169 pass) | heap ≤0.1626, total RSS ≤0.8928 (169/169 lower) |
| Playback: MP4 forward | 46/46 packet folds (`mp4-forward`) | 1.0724 (51/51 pass) | heap ≤0.5920, total RSS ≤0.9878 (51/51 lower) |
| Playback: MP4 range | 51/51 open and backward-seek comparisons (`mp4-range`) | 1.0489 (64/64 pass) | heap ≤0.7849, total RSS ≤0.9262 (64/64 lower) |
| Playback: TS program | 35/35 packet folds (`ts-program`) | 0.8859 (35/35 pass) | heap ≤0.1177, total RSS ≤0.8492 (35/35 lower) |
| MPEG-TS progressive playback | 3 packet folds and all 5 functional inputs with 10 literal outcomes | 0.7779 (3/3 pass) | heap ≤0.0150, total RSS ≤0.7358 (3/3 lower) |
| Playback: raw Annex B | 7/7 packet folds (`raw-annex-b`) | 0.7526 (7/7 pass) | heap ≤0.1265, total RSS ≤0.8003 (7/7 lower) |
| Rasterising | `raster-glyphs` 12/12, `raster-outlines` 2/2 and `raster-bitmaps` 2/2 pass; `raster-sweep` 301/303 (below); the crate's tests pass | not measured | not measured |
| ASS parsing | `ass-parse-scripts` 48/48, `ass-parse-chunks` 4/4 and `ass-parse-events` 21/21 pass; `ass-parse-host` 13/15 (2 open); the crate's tests pass | not measured | not measured |
| Fonts | `font-outlines` 53/53, `font-hinting-none` 159/159 and `font-hinting-default` 159/159 at `69d4d0f8`, as on AArch64 | not measured | not measured |
| Shaping | `shape-libass` 209/209 and `shape-text` 209/209 at `69d4d0f8`, `shape-bidi` 24/24, `shape-simple` 6/6, `shape-unicode` 1/1, `shape-extents` 9/9 | not measured | not measured |
| ASS rendering | `ass-render` 100/100 and `ass-render-host` 4/4 pass; the crate's tests pass | `ass-render` 0.7987 (100/100 pass), `ass-render-host` 0.7431 (4/4 pass) | heap ≤0.9754, total RSS ≤0.9979 (104/104 lower) |
| Subtitles, text | `subtitle-text` 367/367 and `subtitle-text-host` 23/23 pass | `subtitle-text` 0.9948 (367/367 pass); `subtitle-text-host` 0.0758 (23/23 pass) | `subtitle-text` heap ≤0.3358, total RSS ≤0.9983 (367/367 lower); `subtitle-text-host` heap ≤0.0075, total RSS ≤0.9351 (23/23 lower) |
| Subtitles, PGS | 319/319 (`subtitle-pgs`) | 1.0109 (319/319 pass) at `99f1027b` | heap ≤0.9678, total RSS ≤0.9926 (319/319 lower) at `99f1027b` |
| Subtitles, PGS capsules | 229/229 (`subtitle-pgs-capsules`) | 0.6940 (229/229 pass) | heap ≤0.5966, total RSS ≤0.8108 (229/229 lower) |
| Subtitles, DVB | 241/241 (`subtitle-dvb`) | 0.8663 (241/241 pass) | heap ≤0.7118, total RSS ≤0.8589 (241/241 lower) |
| Subtitles, DVD | 228/228 (`subtitle-dvd`) and 5/5 (`subtitle-dvd-host`, gated on this target) | `subtitle-dvd` 1.0869 (228/228 pass); `subtitle-dvd-host` 0.7825 (5/5 pass) | `subtitle-dvd` heap ≤0.9657, total RSS ≤0.8831 (228/228 lower); `subtitle-dvd-host` heap ≤0.2037, total RSS ≤0.8003 (5/5 lower) |
| Closed captions | 461/461 (`subtitle-cc`, the fold and the prepared packets' times) | 0.8130 (461/461 pass); whole worker runs over the 112 inputs at `adaa4bc7` take 0.909 of main's instructions in sum (main at `03cc8d3f`), no input above main beyond the run spread but `h264-rate-two-ts` (+0.60%) | heap ≤0.1267, total RSS ≤0.7902 (461/461 lower) |
| Teletext | 72/72 (`subtitle-teletext`) | 0.5226 (72/72 pass) | heap ≤0.6256, total RSS ≤0.7933 (72/72 lower) |

The runtime and memory of fonts and shaping are not verified on ARMv7. The subtitle,
caption and teletext rows are timed and measured against `ffmpeg-subtitles-plain-native`,
the subtitles reference built without the wrappers that flag target-dependent results, so
no flagging code runs in the reference's timed span or memory window.

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
3). The causes are not established; the raw samples of `3df7ae6d` and `4fc96a49` hold all
three collections. At `79e0ae13` both features are lower than the reference in total RSS in
every case. At `a73350ca` the four `long-movie` PGS cases are above it on both targets:
total RSS 1.1903 to 1.4305 on Linux AArch64 and 1.3049 to 1.5305 here, with heap at most
0.9563 and no memory reclaimed on either host during the collections. The reference's
figures were unchanged; the worker started the operation with about 3.8 MB more resident
memory (10.0 to 10.4 MB against 6.2 to 6.7 MB at `79e0ae13` on Linux AArch64), one
allocation more per packet. Every timed and measured run had begun to report the prepared
packets' input digest, and the PGS worker worked it out from its `sub-input` view, which
copies every packet payload out of the pool and was kept for the rest of the process: a
second copy of the 3.77 MB input. The worker now folds the digest from the packets' clocks
(`99f1027b`), its output unchanged, and starts the operation with 6.3 to 6.9 MB on Linux
AArch64 and 5.7 to 6.3 MB here. All four cases are lower again on both targets: 0.7723 to
0.9596 on Linux AArch64 and 0.8450 to 0.9926 here.

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

At `f4f1c7ad`, the ARMv7 packet-framing runtime bounds exceed the ceiling
for `hevc-type17` (1.2422) and `hevc-irap_repeat` (1.2689). Their paired
medians are 1.1090 and 1.1972. The slowest relative pairs took 4541 ns
and 6638 ns per operation in Triergon versus medians of 3448 ns and
5790 ns. The reference times in those pairs are close to its medians.
The source of the variation is unresolved. The ARMv7 runtime bound for
`hevc-update` fell from 1.2106 at `f40c8eb0` to 1.1359 at `f4f1c7ad`.

Median ARMv7 time in ns per operation (Triergon / FFmpeg) on the same
c6g.4xlarge class and image:

| Case | Main `f71495ea` | `62e1ad85` | `f4f1c7ad` |
|---|---:|---:|---:|
| `hevc-type17` | 4313.33 / 4284.05 | 3449.96 / 3092.15 | 3447.57 / 3115.18 |
| `hevc-irap_repeat` | 7364.49 / 7036.05 | 5902.78 / 4842.78 | 5789.83 / 4848.21 |
| `hevc-update` | 6139.05 / 6228.57 | 5100.73 / 4440.64 | 5044.14 / 4477.17 |

Triergon's medians fell in all three cases. The reference's fell by more
after the timed adapter adopted the matched fold and reused empty packet
shells. The library and filter calls are unchanged. The earlier reference
run at `f71495ea` used 20 pairs for `hevc-type17` and `hevc-update` and
30 pairs for `hevc-irap_repeat`. Both later runs used 30 pairs for each.

Five of MP4 range's 51 cases also exceed the ceiling on ARMv7, with a worst
bound of 1.2241. MP4 range is gated on
Linux AArch64 only. The closest below it are capture `c13-motion-960-6m` 1.1536
and `c14-motion-960-rate255` 1.1528 at `dcf51398`, then Matroska 1.1466, MP4
forward 1.0811 and DVD 1.0360.
Each feature's evidence set lists every case's bound.

The codec bounds are gated on this target. At `a73350ca`, 10 of the 20 workloads of
`workloads.json` and 7 of the 10 of `file-workloads.json` are within the ceiling, and the
13 others are open here, each with its cause, measured at `4cbd1e86`, in its list. Built as A32 with NEON
(`armv7-unknown-linux-gnueabihf`, `-C target-feature=+neon`) and timed against the same
reference, the three E-AC-3 workloads are within the ceiling (1.1231 to 1.1821): their
cause is the Thumb-2 encoding. The two AC-3 (1.2190 and 1.2657), three HE-AAC (1.4763 to
1.5747), two DTS (1.6030 and 1.6343) and three MPEG audio workloads (1.5358 to 1.6114)
stay above it. Those decoders run 1.6 to 2.3 times the reference's user-mode
instructions: SBR and its QMF synthesis take 47% of HE-AAC's cycles, the polyphase
synthesis 61 to 72% of MPEG audio's, the library `floor` 12% of AC-3's and the library
integer division 7.6% of DTS's (evidence set `codec-armv7-causes`). Earlier, on a c6g.4xlarge pinned to CPU 1, in the same user
space and against the same pinned reference, the AC-3, E-AC-3 and DTS workloads were
timed at `ddb02a59` and the five AAC workloads at `0b3c2db5`. Ten of the fourteen audio
workloads were above the ceiling: `dts-core-7.1-master` 2.0698, `dts-core-es-6.1` 2.0338,
`aac-he-5.1` 1.7656, `aac-he-stereo` 1.7092, `aac-hev2-stereo` 1.6377, `ac3-5.1-448`
1.3764, `ac3-2.0-192` 1.3716, `eac3-stereo-spx` 1.3684, `eac3-5.1-spx` 1.3292 and
`eac3-7.1` 1.2933, while `aac-lc-5.1` 1.1423, `ac3-4.0` 1.0948, `eac3-5.1-1536` 0.9794
and `aac-lc-stereo` 0.9761 were within it. The six video workloads, at `3df7ae6d`, were
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

Capture conversion must stay within ±1 of FFmpeg's on every sample. FFmpeg's ARMv7 and
AArch64 builds convert RGBA differently at the capture widths: for a width of at least 16
that is a multiple of 4, the ARMv7 build averages each 2×2 block's RGB before computing its
chroma, where the AArch64 build filters vertically
([pixel](../crates/pixel/README.md#conformance)). The converter follows the ARMv7 build on
every target. At `dcf51398` its output is byte-identical to FFmpeg's ARMv7 output on all 15
cases, and the same on AArch64; FFmpeg's AArch64 output is up to 36 from it, so the 15 cases
are listed as open on `aarch64-unknown-linux-gnu`.

`raster-sweep` fails `outlines-71` and `outlines-83`: libass silently draws nothing
when its composite allocation fails; the crate returns `Allocation`. Both cases draw
glyph and border bitmaps of 1 to 2 GB each, so their composite does not fit a 32-bit
address space; both sides record the same 265 and 267 packets
and differ only in that record.
Both are listed as open cases on `thumbv7neon-unknown-linux-gnueabihf` only, so they are
compared there and must pass on every other target.
Its heaviest case, `bitmaps-24`, takes about 50 s on each side, within
`raster-sweep`'s 120 s `timeout_seconds`.

The total RSS gate passes on ARMv7 in every container and playback row. Which of a
worker's code is resident is set by where the functions an operation runs lie among the
64 KiB windows the kernel faults in around each access: an MP4 range operation runs
about 250 KB of the playback worker's code, which module order spreads over most of its
windows. The media, playback, Matroska playback and transport workers link with `lld`
in their trained orders ([quality](quality.md#measuring-performance)), which keep the
functions the training corpus runs together. Against main's builds, linked by the
toolchain's default linker on the same images, MP4 forward's total RSS bound falls from 1.0856 (0/46 lower) at `7c4057c5`
to 0.9911 (46/46 lower) at `6dc6440d`, MP4 range's from 1.0742 (0/51 lower) to 0.9173
(51/51 lower), and the six Matroska feature groups' from 1.0464 (110/115 lower) at
`f71495ea` to 0.9498 (115/115 lower) at `d2c391b8`, whose media worker `6dc6440d`
keeps byte for byte.

At `79e0ae13` packet framing's total RSS is lower than the reference's in all 125 cases
(worst 0.9972). At `4a932ef9` it was lower in 73, with a worst bound of
1.010086455331412 (`bad-packet-good-update`) and a median of the 125 bounds of 0.9971.
Both workers' resident pages move in 64 KiB steps, the kernel's fault-around window,
depending on where libc is loaded. In 40 runs of each worker on
`bad-packet-good-update`, sorted by libc's load address modulo 64 KiB, the framing
worker holds 15 to 16 more pages of libc than the reference at the same address, and 31
to 32 more at four of the sixteen: before `main`, the Rust runtime's main-thread
stack-guard lookup reads `/proc/self/maps` through glibc's `pthread_getattr_np` and
`sscanf`, which brings in libc's printf and scanf code, and the reference never runs
that code. The worker's own binary is 15 pages smaller (53 resident pages against 68),
so the two totals overlap, and a case falls below the reference when most of the
reference's 30 runs land on an address that costs it an extra window.

The collections at `3df7ae6d` were the first ARMv7 collections with the page-cache eviction. At `22ed8232`, without
it, how much of each executable was resident at the start of the operation varied between
runs: the playback worker started at a median of about 2.3 MiB, up to 2.8 MiB, where it then
started at about 1.8 MiB within 1.73 to 1.88 MiB, and the Matroska reference started at about
2.0 MiB, where it then started at about 1.7 MiB. MP4 forward, TS program, raw Annex B and HLS
were therefore lower than before, and Matroska higher (8 of 116 cases below the
reference, where 110 had been).

At `3df7ae6d` the TS program and ISO BMFF memory was collected a second time, on the same
host from an empty page cache. In the first collection the kernel bound recorded, in one run each of two
TS program cases and five ISO BMFF outputs, an update of the measured process's resident
counters by another task, which makes the observation invalid. That host's kernel had
reclaimed about 7.8 million pages through `kswapd` by then; it reclaimed none during the
second collection. The ARMv7 table of that time gave the second collection; both are in the
raw samples.

The [measured revisions](#measured-revisions) table names the revision and raw samples
behind each ARMv7 row.

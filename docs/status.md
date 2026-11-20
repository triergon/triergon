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

The blocking facade stops reading as soon as a container signature selects a reader.
It reserves replay capacity before consuming sequential input and rewinds seekable input.
Frame-based elementary probing examines up to 64 KiB once.

The [session queue workloads](../harness/session-queues/README.md) cover five real-payload feed patterns and three queue states. Each comparison checks decoded output before measuring paired runtime, peak heap and total RSS.

| Area | Implemented | Verified against the reference | Runtime | Memory | Open |
|---|---|---|---|---|---|
| Core | Charged storage, exact time, input windows, work budgets, neutral container model with one codec identity | 51,235 exact-arithmetic cases against an independent oracle | via dependents | via dependents | fuzzing |
| Matroska / WebM | Incremental reader: lacing, header removal, bounded zlib, Cues and preroll, colour and static HDR, chapters, tags, attachments, side data | 129 workloads across six feature groups and the external HDR vector. The 21 Cues cases include 13 Opus cases for subtitle seek recovery and early indexed seeks. Four host FATE demux comparisons. | 1.0392 (129/129 pass, worst `chapters-many`) at `2fc2ebb8` | heap ≤0.6262, total RSS ≤0.8951 (129/129 lower) at `2fc2ebb8` | full EBML/Matroska scope |
| Matroska legacy AAC-LC | MPEG-2 and MPEG-4 legacy AAC-LC tags, derived initialization, ADTS feed and packet clocks | 4/4 packet folds and 32 framed packets, including a Xiph lace and declared or omitted duration (`matroska-legacy-aac`) | 1.0155 (4/4 pass, worst `mpeg2-lc-laced`) at `deb6105a` | heap ≤0.5228, total RSS ≤0.8568 (4/4 lower) at `deb6105a` | FATE coverage |
| Matroska startup and seek | Startup and seek with lazy scan replay, including matching HEVC parameter sets in CodecPrivate and in-band | 45 cases, 1,350 packets | 0.9096 (45/45 pass, worst `seek-hevc-seek-no-cues-0-fresh`) at `deb6105a` | heap ≤0.4038, total RSS ≤0.6799 (45/45 lower) at `deb6105a` | FATE mapping |
| Matroska audio clocks | AAC, AC-3, E-AC-3 startup and forward clocks | 10/10 packet folds (`matroska-audio-clocks`) | 0.8747 (10/10 pass, worst `laced-unknown-duration`) at `deb6105a` | heap ≤0.8103, total RSS ≤0.8048 (10/10 lower) at `deb6105a` | audio seek and FATE coverage |
| ISO BMFF / MP4 | Regular and fragmented files, edit lists, seek and range plans, neutral `ContainerReader` over seekable sources, shared buffered cursor for transport traces, QuickTime compatibility including legacy data handlers and dependency flags in recovery mode, bounded AAC configuration and regular table recovery, timing, sync and terminal media prefixes; AVC, HEVC, AAC, AC-3, E-AC-3, DTS core, AV1, VP9 and Dolby Vision sample entry identification | 255 inputs, 1,020 open and index comparisons (972 matching outputs and 48 matching refusal outcomes with compatibility policies); the neutral `Reader` against the formats `ffprobe` (`cargo xtask codecs isobmff-reader`): 648 cases, 522 inputs read through and 126 seeks, 333 identical, 3 refused by both, 312 differing only by 17 reviewed dispositions, 12 of them FFmpeg behaviours and 5 the reader's own documented refusals of inputs that break ISO/IEC 14496-12 ([rules](../harness/features/isobmff-reader/rules.json), [frozen differences](../harness/features/isobmff-reader/differences.json)) | 0.8139 (956/956 pass, worst `order27-u64-last-scale1000-cat21-native-index-tiny`) at `deb6105a` | heap ≤0.2841, total RSS ≤0.7395 (956/956 lower) at `deb6105a` | neutral reader: FATE mov/mp4 coverage beyond one sample, codec metadata not compared; seeks and range plans in their own row |
| ISO BMFF seek and range plans | Backward seek on one track after open or after reading to the end, a bounded read after it; recovery-aware source range plans read from their decode start | 476 cases (`isobmff-seek`): 378 seeks with byte-identical traces, 94 range plans with the same source packets, 4 matching refusals, over 8 authored movies of up to 1,800 samples per track and 36 open and index inputs | 0.9142 (472/472 pass, worst `regular-front-1800-t1-sync-eof-seek-ordinary`) at `deb6105a` | heap ≤0.9032, total RSS ≤0.7500 (472/472 lower) at `deb6105a` | seeks the compatibility seek refuses (unequal clocks, overlapping or null index clocks) and those that differ from FFmpeg on 47 open and index inputs (decode times below the presentation start, trimmed edits); range plans on multi-track inputs or where movie-time planning selects other samples |
| MP4 range playback | Regular AVC/AAC and AVC/Opus startup and backward seek, including regular AVC with an initial non-sync prefix or a leading empty edit, fragmented AVC/AAC with open-ended edits, timed text and DVD subpictures including a leading empty subpicture edit, dedicated HEVC and AV1 sample entries, source-declared Opus seek preroll, QuickTime AAC with fixed and program configurations, including regular version-2 descriptions and fragmented version-1 descriptions, and media playback in the presence of RTP hint tracks | 64 matching open and backward-seek comparisons (`mp4-range`), 9,229 packets on AArch64 Linux | 0.9202 (64/64 pass, worst `fragmented-subtitles-seek-start`) at `deb6105a` | heap ≤0.8877, total RSS ≤0.8193 (64/64 lower) at `deb6105a` | Additional range playback profiles |
| AAC in MOV | Raw AAC-LC access units with 1024 samples per plane and all thirteen indexed rates. Supports fixed configurations 1–7, 11, 12 and 14 and out-of-band programs assigning up to eight distinct speaker positions. | The [configured decoder comparison](../harness/features/aac-access-units/contract.json) covers 34 clips, 3,544 access units and 15,575,040 PCM samples. Two fixed-layout QuickTime clips compare the surround pair in the same plane positions with side or back labels. Every other clip compares exact speaker order. | 0.8695 (34/34 pass) at `414b4cb5` | heap ≤0.6721, total RSS ≤0.4513 (34/34 lower, kernel-bound) at `414b4cb5` | 960-sample frames and SBR in raw access units |
| Ogg | Pages with CRC and resync, multiplexed and chained streams, Vorbis, Opus, FLAC, Theora and Speex mappings with canonical configurations, granule timing (Theora split granules, keyframe random access), pre-skip and end trims, bisection seeking across chain links (Theora to the keyframe) | Packets against the pinned `ffprobe` (`cargo xtask codecs readers`, gate item `ogg-readers`, at `4d31df88` on both targets) on 45 files (21 FATE Vorbis, 3 FATE Theora, 3 FATE chained, 13 generated Opus, Vorbis and FLAC, 5 generated Speex: multiplexed, chained, damaged, truncated, with Skeleton), in two views: `compat::clock` 40/45 pass, 5 differ only by reviewed FFmpeg behaviours; the reader's own timing 10/45 pass, 31 differ only where `compat::clock` reproduces FFmpeg, 4 by reviewed behaviours; and 26 seeks (each link of chained files, each track of multiplexed ones, both policies): `compat::clock` 11/26 pass, 15 differ only by FFmpeg seeking within one link of a chained file ([reader rules](../harness/codecs/reader-rules.json), [seek cases](../harness/codecs/reader-seeks.json), [frozen differences](../harness/codecs/reader-differences.json)) | within the codec workloads | within the codec workloads | `b_vorbis.ogg`, `chain_vorbis.ogg`: a negative last duration FFmpeg prints modulo 2^32; `crc_bad.opus`: the first packet after a page that fails its CRC, which FFmpeg times from its own page and `compat::clock` from the stream start; `bear.ogv`: leading Vorbis pages stating granule position 0, which FFmpeg times from the next page; `chain_speex.spx`: a chained link reusing its predecessor's serial, which FFmpeg reads as the same stream; chained seeks: FFmpeg delivers one link only; VP8, Daala and Kate in Ogg untimed |
| Audio elementary streams | Native FLAC (with or without `fLaC`), MPEG audio with ID3v2/ID3v1/APE tags, Xing/Info/VBRI and LAME gapless trims, ADTS, AC-3/E-AC-3; one shared frame-stream engine | Packets as the readers emit them against the pinned `ffprobe` on 271 FLAC, MPEG audio, ADTS, AC-3 and E-AC-3 files (the IETF FLAC testbench, MP3 conformance, generated and crafted damaged streams): 253/271 pass, 16 differ only by reviewed FFmpeg behaviours, 2 refused by both (testbench faulty `06 - missing streaminfo metadata block`, `07 - other metadata blocks preceding streaminfo metadata block`) | within the codec workloads | within the codec workloads | FLAC `flac_s20_rate768k`, `06 - samplerate 768kHz` (no durations at 768 kHz), `01 - changing samplerate`, `27 - old format variable blocksize file created with Flake 0.11`, `11 - file starting with unparsable data`, `11 - incorrect metadata block length`, `08 - blocksize 65536`, `09 - blocksize 1`; crafted `ac3_tags`, `adts_mid_junk`, `flac_corrupt`, `flac_id3`, `flac_mid_junk`, `mp3_foreign_frame`, `mp3_mid_junk_falsesync`; MP3 `he_free` (free format, refused by FFmpeg); E-AC-3 dependent substreams on real files; ADTS and AC-3 seek cost |
| MPEG-TS | PSI, continuity, PES assembly and ordered TS188/M2TS192 entities with control-only and bounded fault completion, with format-owned track classification | 97/97 PES compatibility-view comparisons across all 272 requests | 0.7295 (97/97 pass) at `273f6992` | heap ≤0.0653, total RSS ≤0.9134 (97/97 lower) at `273f6992` | strict source clocks, full FATE mapping |
| MPEG-TS access units | `ContainerReader` and `AnyReader` for TS188 and M2TS192, with AVC, HEVC, ADTS AAC-LC, LATM in LOAS, signalled AC-3 and E-AC-3, DVB subtitle segments and ordered entities; `Reader` can frame selected PIDs while retaining all source events | 169/169 transport compatibility folds for all tracks in the chosen program, including LATM PES and opaque observations, with all 272 original request associations; explicit PID selection has separate reader tests | 0.9149 (169/169 pass, worst `transport-sync-loss`) at `deb6105a` | heap ≤0.1068, total RSS ≤0.9281 (169/169 lower) at `deb6105a` | additional carriages, seeking and full FATE mapping |
| MPEG-TS program playback | Selected TS188 program and M2TS192 PID with packet payloads and clock projection | 35/35 packet folds | 0.7805 (35/35 pass, worst `m2ts-entry-hevc-aac-44100-01-video`) at `deb6105a` | heap ≤0.0880, total RSS ≤0.7983 (35/35 lower) at `deb6105a` | additional program and carriage profiles |
| MPEG-TS progressive playback | TS188 with AVC and AAC-LC, bounded sequential input and access units before EOF | 3 packet folds and all 5 functional inputs with 10 literal outcomes | 0.9655 (3/3 pass, worst `reordered-48000`) at `deb6105a` | heap ≤0.0156, total RSS ≤0.6367 (3/3 lower) at `deb6105a` | broader transport and codec profiles |
| HLS segments | Independent TS188 segments with contiguous AVC or HEVC video and AAC, AC-3 or E-AC-3 audio | 71 packet comparisons, 71 measured workloads | 0.7929 (71/71 pass, worst `dvb-hevc-eac3-32000`) at `deb6105a` | heap ≤0.0208, total RSS ≤0.7427 (71/71 lower) at `deb6105a` | other profiles |
| Packet framing | AVC/HEVC configuration and NAL framing, AV1, VP9, AC-3, E-AC-3, DTS core with consistent coded-window diagnostics from owned and packed inspectors | 135/135 AVC/HEVC reference comparisons. At `7c730ff8` on AArch64, both DTS inspectors agreed on all 27 fields and every core matched its reference length and bytes across seven inputs. [DTS corpus and instruction counts](#dts-core-header-inspection). | 1.1556 (133/135 pass for AVC/HEVC, worst `flush-update`) at `deb6105a` | heap ≤0.3300, total RSS ≤0.9889 (135/135 lower) at `deb6105a` | AArch64 runtime for `flush-update` and `avc-same-update-continuation`, ARMv7 total RSS, full AVC/HEVC/ADTS grammar |
| Subtitles, text | SubRip and WebVTT documents as FFmpeg's `srt` and `webvtt` demuxers read them, ASS packets as its `ass` decoder decodes them, with `sub_charenc` charsets including EUC-KR receiver policies, and mov_text and tx3g to ASS | 367/367 cases (`subtitle-text`: 82 captured mov_text cases, 13 authored mov_text sessions for the override tags written where a style record or the highlight starts or ends, the samples of the 79 tx3g tests, 80 charset cases, 17 ASS packets, 96 SubRip and WebVTT documents) on Linux AArch64 and [32-bit ARMv7](#32-bit-armv7). `subtitle-text-host` matches 23/23 SubRip and WebVTT documents with oversized timestamp fields on both targets. | `subtitle-text` 1.0385 (367/367 pass)<br>`subtitle-text-host` 0.0915 (23/23 pass) at `97759e39` | `subtitle-text` heap ≤0.3360, total RSS ≤0.9298 (367/367 lower)<br>`subtitle-text-host` heap ≤0.0120, total RSS ≤0.7924 (23/23 lower) at `97759e39` | FATE coverage |
| Subtitles, PGS | Presentation, palette and object segments to subtitle rectangles, with FFmpeg's recovery from damaged and incomplete segments | 319/319 cases (`subtitle-pgs`: TS, SUP and Matroska), on Linux AArch64 and [32-bit ARMv7](#32-bit-armv7) | 0.7806 (319/319 pass) at `546045db` | heap ≤0.9654, total RSS ≤0.9682 (319/319 lower) at `546045db` | FATE coverage |
| Subtitles, PGS capsules | Recorded PGS packet sessions through `compat::pgs::ReceiverDecoder`, with raw PTS, DTS and time base, the forced-only option and hard resets that reopen the decoder | 229/229 cases (`subtitle-pgs-capsules`: capsules of up to 64 complete PGS packets against FFmpeg's `pgssub` decoder), on Linux AArch64 and [32-bit ARMv7](#32-bit-armv7) | 0.7221 (229/229 pass) at `546045db` | heap ≤0.5788, total RSS ≤0.7381 (229/229 lower) at `546045db` | — |
| Subtitles, DVB | Display definition, page, region, CLUT and object segments to bitmap rectangles, including `compute_clut` (−2 keeps a region's first automatic palette until a new epoch) | 241/241 cases (`subtitle-dvb`: TS and Matroska), on Linux AArch64 and [32-bit ARMv7](#32-bit-armv7) | 0.8995 (241/241 pass; worst `long-text-mkv`) at `f8e1c4d9` | heap ≤0.7116, total RSS ≤0.7901 (241/241 lower) at `f8e1c4d9` | FATE coverage |
| Subtitles, DVD | SPU assembly and control sequences, run-length raster, palettes from VobSub `.idx`, Matroska and MP4 private data, DVD program streams, HD SPU | 233 cases: `subtitle-dvd` 228/228 on Linux AArch64 and [32-bit ARMv7](#32-bit-armv7), and `subtitle-dvd-host`'s 5, pinned per target, 5/5 on ARMv7 and 1/5 on AArch64 | 0.8191 (228/228 pass) at `f8e1c4d9` | heap ≤0.9590, total RSS ≤0.8565 (228/228 lower) at `f8e1c4d9` | FATE coverage; on AArch64, `subtitle-dvd-host`'s 4 open cases, target-dependent by design: FFmpeg there reads numbers of a VobSub configuration and of the `palette` option beyond 32 bits with 64, and the decoder follows FFmpeg on 32-bit ARM |
| Closed captions | CEA-608 captions from A/53 and SCTE 20 user data in MPEG-2 video, A/53 SEI in H.264 and HEVC, and SCC files, to ASS; H.264 and HEVC pictures in decoded-picture output order (`triergon_framing::picture_output`) | 461/461 cases (`subtitle-cc`, the fold and the prepared packets' times; H.264 and HEVC in MP4 and Matroska with parameter sets only in `avcC` or `hvcC` among them), on Linux AArch64 and [32-bit ARMv7](#32-bit-armv7) | 0.6578 (461/461 pass) at `f8e1c4d9`; whole worker runs over the 112 inputs at `adaa4bc7` take 0.921 of main's instructions in sum (main at `03cc8d3f`) | heap ≤0.1227, total RSS ≤0.7360 (461/461 lower) at `f8e1c4d9` | FATE coverage; not reproduced: H.264 durations in Matroska without durations when the SPS states a bitstream restriction, after a start hold of depth 3 or more, after a second B picture dropped as late, and at ticks between 1 and 2 ms where the reference picks a multiple of the frame rate; H.264 durations from VUI `pic_struct`; H.264 recovery with `recovery_frame_cnt` 5 in B pyramids; the H.264 start hold when the depth reaches 3 from 1; H.264 `frame_num` going back without an IDR picture; a packet with neither PTS nor DTS; H.264 in MP4 with PTS equal to DTS on every packet while the stream reorders (`h264-hold-depth1` remuxed by FFmpeg, not in the corpus), where the reference prepares 299 caption packets to the extractor's 301, with or without in-band parameter sets, while the same stream in Matroska matches |
| Teletext | DVB teletext pages to text and ASS, as FFmpeg's libzvbi 0.2.46 decoder emits them | 72/72 cases (`subtitle-teletext`), on Linux AArch64 and [32-bit ARMv7](#32-bit-armv7) | 0.4288 (72/72 pass) at `f8e1c4d9` | heap ≤0.6212, total RSS ≤0.7035 (72/72 lower) at `f8e1c4d9` | FATE coverage |
| Playback | Matroska, MP4, TS and raw AVC/HEVC feeds, including TS188 private audio through `compat::pes`; seeking and end-of-file continuation | MP4 terminal-media matrix against recorded FFprobe 9.0.2 output (evidence set `playback-terminal-matrix-ffprobe`): 39 inputs in 13 source geometries (12 nonseekable for every input, 1 seekable for the 31 original inputs), 499 rows, each run in debug and in release. `Mp4Open::new_recovery` 499/499 pass: 497 identical packet lists, and 2 seekable reads of a zero-size fragment sample that the opener refuses in the header, where FFprobe also refuses. `Mp4Open::new` 446/499 pass. Packet folds: MP4 forward 51/51 (`mp4-forward`), 769 packets, TS program 35/35 (`ts-program`), raw Annex B 7/7 (`raw-annex-b`) | MP4 forward 0.7730 (51/51 pass, worst `regular-encoded-avc-aac-indexed-seek`), TS program 0.7805 (35/35 pass, worst `m2ts-entry-hevc-aac-44100-01-video`), raw Annex B 0.9957 (7/7 pass, worst `hevc-aud1`) at `deb6105a` | MP4 forward heap ≤0.6755, total RSS ≤0.8616 (51/51 lower), TS program heap ≤0.0880, total RSS ≤0.7983 (35/35 lower), raw Annex B heap ≤0.1274, total RSS ≤0.7224 (7/7 lower) at `deb6105a` | other profiles, including additional legacy Matroska AAC profiles |
| Codecs | H.264 8-bit 4:2:0; HEVC Main and Main10, both with the decoded picture buffer of `triergon_framing::picture_output`; AAC LC, HE, HEv2 (Parametric Stereo from open publications and probes); AC-3 and E-AC-3; DTS core with XCh and X96; storage charged to a budget | 417/417 comparable conformance and FATE vectors; the AAC lane's three crafted Parametric Stereo streams of paths no FATE vector takes (a 20/34-band change registered while PS is off, IPD/OPD across a change, a PS frame without its SBR element); the audio correctness lanes as gate item `codecs-audio` at `81edf4c0` on both targets, the 71 derived streams among them reproduced byte for byte by `cargo xtask codecs derive`: AAC 38/39 and one open case, AC-3 6/6, E-AC-3 6/6, DTS core 22/22, and `xll_51_24_48_none` out of set (no core substream) | 0.9534 (20/20 pass; worst `h264-high-1080p`) at `81edf4c0` | heap ≤0.9397, total RSS ≤0.8334 (20/20 lower) at `81edf4c0` | error concealment, unverified tool paths; `aac-sce-in-stereo`, open: FFmpeg leaves the FR plane of single-SCE frames unwritten (FR RMS 0.0853, FL equal; the stereo output passes); four open AAC Parametric Stereo and SBR cases (`harness/codecs/aac/ps-open-manifest.json`); AAC charges in-band reconfiguration when it appears, and a refused access unit leaves the decoder unchanged unless an earlier raw data block or Main-prediction element of it was decoded |
| Audio codecs in files | FLAC (RFC 9639, 4 to 32 bits, 1 to 8 channels, bit-limited Rice residual partitions); Opus (RFC 6716 and 8251: SILK, CELT, hybrid, families 0, 1 and 255); Vorbis I; MPEG-1, 2 and 2.5 audio Layer I, II and III | 315/321 pass through the container readers (FATE Vorbis, Opus and MP3 conformance, the IETF FLAC testbench, generated streams): FLAC bit-exact, the others within the [numeric rules](../harness/codecs/numeric-rules.json) | 1.0068 (10/10 pass, worst `opus-5.1-256`) at `deb6105a` | heap ≤0.9043, total RSS ≤0.7165 (10/10 lower) at `deb6105a` | FLAC `01 - changing samplerate`, `02 - increasing number of channels`, `03 - decreasing number of channels` and MP3 `he_mode` change format mid-stream, which the CLI converts; FLAC `flac_3ch_odd_mask`, whose layout FFmpeg refuses on its stereo output; MP3 `he_free` (free format: FFmpeg finds no two consecutive frames, and the decoder refuses free-format frames), refused by both; runtime toward the target for Opus (at `deb6105a`: Opus 5.1 1.0068, music 0.9584 and speech 0.9149); Opus families 2 and 3 |
| Decode sessions | Bounded queues, clock-gated output, flush and seek, RGBA letterbox, stereo mix; container track decoding with trims and start delays | with the codecs | with the codecs | with the codecs | — |
| Fonts | sfnt and collections; `head`, `hhea`, `maxp`, `hmtx`, `vhea`, `vmtx`, `OS/2`, `name`; `cmap` formats 0, 2, 4, 6, 8, 10, 12, 13, and the glyph-name and encoding charmaps FreeType adds to CFF faces, in its charmap order; `glyf`/`loca`; CFF and CFF2 at the default instance; `kern` format 0; loading at a size as libass does with hinting off (unhinted outlines, synthetic bold and oblique, size metrics, kerning, decoration bands, default-load advances where hinting cannot move them, and the vertical advances libass draws and shapes the glyphs of `@` fonts with); FreeType's default load of CFF, CFF2 and CID-keyed glyphs, hinted by Adobe's hinter as FreeType's CFF driver hints them, of TrueType glyphs without bytecode, through FreeType's auto-hinter with its Latin and dummy writing systems, and of instructed TrueType glyphs, through a TrueType interpreter running their programs as FreeType's does in backward compatibility mode, with the grid-fitted metrics of each; face matching with libass's rules (compared through the renderer's `ass-render` feature) | The gate covers libass's default configuration (hinting off). All 70 faces of 53 corpus files (TrueType including Arabic, Hebrew, Indic and Thai fonts, name-keyed and CID-keyed CFF, CFF2, a 10-face collection, six authored fonts, among them a face for each way FreeType reads vertical metrics and a CFF face with each charmap FreeType adds; 836,345 glyphs) identical to FreeType: names, every charmap FreeType lists with its mappings, libass's text charmap, the kerning of every listed pair, advances and unscaled outlines (`font-outlines` 53/53); at 256 pixels, size metrics, kerning, every glyph's unhinted outline with its synthetic styles, default-load advances, and its linear and scaled vertical advances unhinted and of the default load (`font-hinting-none` 159/159); at that size and at 15.625 and 10 pixels (26.6 heights 1000 and 640), every glyph's default-load metrics and hinted outline, which libass's shaper reads for advances, extents and contour points (`font-hinting-default` 159/159 at `69d4d0f8`: every CFF, CFF2 and CID-keyed face, the auto-hinted authored-cmap-formats and DejaVuMathTeXGyre, and the 43 instructed TrueType files); Linux AArch64 and [32-bit ARMv7](#32-bit-armv7), each with its own FreeType build, and macOS AArch64 before the vertical cases | not measured | not measured | libass's hinting-enabled modes (NATIVE, LIGHT, NORMAL): their nine other sizes are measured, not gated (`font-hinting-modes` 954/954 at `9d14e98c` on both targets, unhinted outlines and default-load advances), and the hinted outlines those modes draw are not compared; the auto-hinter's Indic writing system, for the hinted outlines of the glyphs it styles; the TrueType interpreter's unit vectors set along a line or from the stack (`SPVTL`, `SFVTL`, `SDPVTL`, `SPVFS`, `SFVFS`), whose normalisation matches FreeType's for most directions but not all: FreeType's carry an error of about 2⁻¹⁶ that depends on the direction and on the vector's magnitude within its power of two, which no fixed-point rule tried reproduces (`vectors-normalization`); no corpus glyph differs for it at the compared sizes; the default load at sizes other than 256, 15.625 and 10 pixels (not compared); tricky fonts and programs that leave backward compatibility (`INSTCTRL` selector 3); `cmap` format 14; runtime and memory |
| Shaping | OpenType layout (GDEF, every GSUB and GPOS lookup type, feature variations at the default instance, `kern` format 0 fallback, fallback mark positioning) with HarfBuzz's default, Arabic, Hebrew, Thai/Lao, Hangul, Indic, Khmer, Myanmar and Universal shapers for horizontal runs; every BCP 47 language mapped to OpenType language systems as HarfBuzz maps it; libass's FreeType-backed font functions over a sized face, with the rotated advances of `@` fonts, the empty extents of glyphs without points, and the default-load extents and contour points of CFF and TrueType glyphs; the CFF and CFF2 glyph extents of HarfBuzz's own font functions, with the same StandardEncoding string IDs as the outline reader for accented CFF glyphs; bidi from UAX #9 with FriBidi's results, resolved and reordered as libass does; libass's simple shaping level; Unicode properties generated from pinned UCD files; planning, shaping and bidi bounded by work limits | Identical to HarfBuzz and FriBidi on Linux AArch64 and [32-bit ARMv7](#32-bit-armv7), and on macOS AArch64 before the charmap face joined the shaping corpus. Gating: `shape-libass` 209/209 at `69d4d0f8`, 3,453 runs over 27 corpus files and 22 scripts shaped as libass does at 256 pixels with hinting off; `shape-bidi` 24/24; `shape-simple` 6/6; `shape-unicode` 1/1 (every code point); Unicode 14.0.0's `BidiTest.txt` (770,241 cases) and `BidiCharacterTest.txt` (91,707) all pass. Coverage at units per em: `shape-text` 209/209 and, on demand, `shape-text-all` 1,790/1,790 at `69d4d0f8` (104,796 runs over every corpus face), with CFF and CFF2 extents as HarfBuzz's own charstring reading gives them, and `shape-extents` 9/9, every glyph's extents of every face of the corpus's CFF, CFF2 and collection files (699,045 glyphs) | `shape-extents` 1.1724 (7/9 pass, worst `authored-charmaps-otf-extents`) at `deb6105a`; other shaping workloads unmeasured against their references | `shape-extents` heap ≤14.4937 (2/9 lower), total RSS ≤0.9999 (9/9 lower) at `deb6105a`; other shaping workloads unmeasured against their references | the hinted glyph extents and contour points of glyphs FreeType's auto-hinter gives an Indic style, which libass reads from FreeType to place marks; vertical directions (libass shapes `@` fonts horizontally); AAT tables and Apple `kern`, and OpenType `kern` formats 1, 2 and 3 and cross-stream kerning, which HarfBuzz applies (faces that need them are refused); `stch` stretching; variable instances other than the default; runtime and memory |
| Rasterising | libass's outline and bitmap stages: conversion from font outlines, rotation by 90°, affine and perspective transforms, border stroking, the 16×16 tile rasteriser, the glyph transform and its quantisation against a run with borders and opaque boxes, the karaoke leftmost x, `\blur` and `\be`, composition of fill, border and shadow, the additive and multiplying blends, shadow shifts and rectangle clipping with the karaoke split; storage charged to a budget, work to a `WorkBudget`, bitmaps bounded by libass's own size limit | Identical to libass's plain C bitmap engine (what libass runs on 32-bit ARMv7, pinned on every host), with glyphs loaded by `triergon-font` as libass loads them with hinting off. Gating: `raster-glyphs` 12/12 pass (576 operations over 8 glyphs of a file of each outline format and an authored font at 13.33 and 256 pixels) and `raster-outlines` 2/2 pass (1,019 operations on 200 random outlines), both with transforms of moderate reach, and `raster-bitmaps` 2/2 pass (560 operations on 80 random bitmap sets, a tenth of their blurs and `\be` passes beyond libass's clamps, among them radii it still blurs); on demand, `raster-sweep` 303/303 pass (72,445 operations: every corpus file at three heights with 30 glyphs each, 4,500 random outlines, 3,000 bitmap sets, with transforms as far as libass draws them). Platforms: macOS AArch64, Linux x86_64 and Linux AArch64 all four pass (12/12, 2/2, 2/2, 303/303); [32-bit ARMv7](#32-bit-armv7) passes the three gating features (12/12, 2/2, 2/2) and `raster-sweep` 301/303: on `outlines-71` and `outlines-83` libass silently draws nothing when its composite allocation fails; the crate returns `Allocation` (open cases on ARMv7, compared and passing elsewhere). `raster-simd`, the required cases against the host's SIMD engine, records where it departs: NEON on macOS and Linux AArch64 14/16 pass (4 of the 80 blur cases depart, all with `\be` passes); AVX2 on Linux x86_64 0/16 (AVX2: every bitmap differs in row padding; visible-pixel comparison needs its own fold) | not measured | not measured | the overlap and allocation logic of vector clips at image-list level, which belongs to the renderer; libass's large-tile build and its SIMD engines (NEON blurs `\be` over whole 16-byte rows into the padding; AVX2 pads rows to 32 bytes); runtime and memory; `outlines-71` and `outlines-83` on ARMv7, open there: libass silently draws nothing when its composite allocation fails, the crate returns `Allocation`; a visible-pixel fold for `raster-simd` |
| ASS parsing | Whole ASS and SSA scripts and Matroska packet streams as libass reads them: headers, styles, events, style overrides, embedded fonts; codec private, packets with ReadOrder deduplication, flushing and pruning; typed override tags with libass's argument rules; every glyph's state at a time (animation, movement, fades, clips, karaoke tags, transition effects); karaoke syllables from laid-out positions; the drawing language into `triergon-raster` outlines; storage charged to a budget, work to a `WorkBudget` | Identical to libass on 54 authored fixtures, on macOS AArch64 and, with each target's own libass build, on aarch64-unknown-linux-gnu and thumbv7neon-unknown-linux-gnueabihf: `ass-parse-scripts` 48/48 pass (every track, style and event field, 87 styles, 222 events, embedded fonts, one script without a track), `ass-parse-chunks` 4/4 pass (10 track states, ReadOrder deduplication by bitmap and by scan, flushes, prunes, fonts in a codec private and in more data), `ass-parse-events` 21/21 pass (695 event walks, 6,976 glyph states and karaoke results, 48 drawing and clip outlines, 3 of them rejected as libass rejects them). `ass-parse-host` holds the 15 operations whose libass result differs by target, each pinned per target: on thumbv7neon-unknown-linux-gnueabihf 13/15 pass with 2 open, on aarch64-unknown-linux-gnu 11/15 pass with 4 open; elsewhere all 15 are reported. The reference flags every conversion from `double` to `int` out of range or for NaN (drawing coordinates, `\pbo`, an animated `\clip` or `\iclip`, `\fade`'s alpha, a `Banner` or `Scroll` delay) and a drawing coordinate on `INT_MIN`, and a flagged case fails any other feature. 2,728 mutated fixtures (`fuzz --contract`, 30 rounds of each feature, with the reference for all but `ass-parse-host`) gave identical work in both workers | not measured | not measured | `long`-width behaviour follows each target's libass, and out-of-range drawing coordinates are rejected as libass does on 32-bit ARM with GCC; every other out-of-range conversion from `double` to `int` saturates as there. Open as deliberate divergences: a coordinate landing exactly on `INT_MIN`, which libass built with GCC accepts and the crate rejects (`drawings-int-min-x` and `drawings-int-min-y`, on both targets), and `drawings-infinite` and `drawings-int-min` on aarch64-unknown-linux-gnu, whose coordinates libass converts with a 64-bit `long` there; the font description a glyph asks for (`GlyphState::font`) and karaoke run starts from style runs, shaping and wrapping are compared through the renderer (`ass-render`), not here; selective style overrides, track features, `ass_step_sub`; runtime and memory |
| ASS rendering | `Renderer`: the events shown at a time to positioned alpha images as libass's `ass_render_frame` draws them with hinting off and complex shaping: font selection and fallback over attached, embedded and default fonts, shaping and bidi, wrapping, alignment, positioning, scrolling, collisions kept across frames, change detection, glyph and border bitmaps, blur and shadows, clips, karaoke and the image list; storage charged to a budget, and all the work of a frame (evaluation, font selection, shaping, outlines, layout, drawing and collisions) to one per-frame bound, `Settings::work`. Final vector clipping charges for copied and multiplied row spans on every frame. A style-4 background fills one row and reuses it across the visible height | Identical to libass on Linux AArch64 and [32-bit ARMv7](#32-bit-armv7), each with its own libass build, and on macOS AArch64 before the cases of `@` fonts, unregistered languages, glyphs without points, the charmap retry, two `@` font styles and vector clips without a contour or beyond libass's reach (macOS was not run with them), configured as a player embeds it (attached fonts, a default family and font file, no system font provider, no hinting, storage smaller than the frame): `ass-render` 100/100 pass over 47 authored scripts and 3 Matroska packet streams (ReadOrder deduplication, flushes, prunes, an embedded font) at 1920×1080 over 1280×720 storage and 1280×720 over anamorphic 720×576 storage, at each fixture's frame times; every image's kind, position, size, colour and alpha bytes, and every frame's change value as `ass_detect_change` reports it (802 frames, including underline and strike-out lines on TrueType, CFF and CFF2 faces with synthetic styles, borders and `@` fonts, frames after a flush and after pruning every event, opaque boxes around different glyphs that draw nothing, vector clips whose drawing has no contour, which empty their event's images, and one whose drawing libass cannot place, which it does not apply, frames whose caches overflow and are cut, C1 controls drawn through a fallback face's Macintosh charmap that the face keeps for later lookups, a style font size of 0, whose glyphs FreeType loads unscaled, `@` fonts drawn rotated and shaped with their vertical advances, a `Language` header HarfBuzz does not register, fallback mark positioning that measures a glyph without points, and a fallback face whose characters libass finds by trying its format 2 and 8 subtables, the Unicode charmap FreeType builds from its glyph names and its CFF encoding in FreeType's order, and two styles of one `@` font that select the same face, whose rotation decisions libass keeps apart); the box behind a `BorderStyle` 4 event, whose type libass leaves uninitialised, is told by its own buffer, also after a collision shift cuts rows off its top, and compared as a type of its own; colliding events whose boxes lie 2^31 or more rows apart, each within `int`'s range (`collision-far-margins.ass`), placed as glibc's merge sort orders them under libass's overflowing comparison, with the far event clipped away, whose 26.6 position leaves `int`'s range and which libass for AArch64 alone draws inside the frame; with libass's cache limits at 1000 glyphs and 24 MiB; `ass-render-host` pins per target the rendering whose libass result C leaves to the target (`border-x-overflow.ass`, a box width that doubles a huge border in wrapping `int`, and `collision-far-tops.ass`, colliding events whose boxes end up 2^31 or more rows apart, sorted as glibc's merge sort sorts them with libass's overflowing comparison): 4/4 pass on ARMv7 | 1.0256 (100/100 pass) at `eaff16ba` | heap ≤0.9485, total RSS ≤0.9972 (100/100 lower) at `eaff16ba` | on AArch64, `ass-render-host`'s 4 cases, a deliberate divergence: libass built with GCC for aarch64-unknown-linux-gnu gives an event box value outside the signed 32-bit range (its top, height, left or width) its low 32 bits, where libass on 32-bit ARM saturates it, and the renderer follows 32-bit ARM, so they are not timed or measured there; settings other than frame and storage size are unverified; libass's simple shaping and hinting modes, selective overrides and track features; of 5,182 generated scripts and 540 fixture mutations rendered frame by frame against libass for ARMv7, 48 scripts and no mutation differ, each for a stated cause: the renderer stops at the default `Settings::work` bound (17), which on ARMv7 is about 9 s of rendering for one frame (a median 33 ns per unit); with the bound lifted, 34 of those frames take 7 to 21 s each there and 110 run out of the 32-bit address space, so the bound refuses only frames no player could show in time; it runs out of the 32-bit address space (19 allocations refused, 8 at the worker's budget) where libass for ARMv7 peaks at 2.6 to 4.1 GB; and in 4, and in earlier frames of 2 of the above, libass for ARMv7 alone departs from libass for AArch64 at 3.7 to 4.0 GB of its 4 GB address space, and the renderer matches libass for AArch64 (`random-2258` from frame 46). Against libass for AArch64, 113 scripts and 1 mutation differ: 79, the mutation among them, that are identical on ARMv7 and differ there through libass's 64-bit `long` narrowing and its low 32 bits of event box values outside the signed 32-bit range, both deliberate divergences; 34 at the work bound, where libass peaks at 3.5 to 23 GB; and `random-1428`, where libass is killed for memory and the renderer refuses at its budget |
| Capture | RGBA to YUV 4:2:0, MPEG-1 video with retryable motion-search allocation, MPEG-TS mux | 15/15 conversion, encoder and mux comparisons at `deb6105a`. Conversion is byte-identical to FFmpeg 9.0.2 using `Session::open_filtered` on AArch64 and `Session::open` on ARMv7. | 0.9797 (15/15 pass, worst `c04-motion-960`) at `deb6105a` | heap ≤0.5904, total RSS ≤0.9706 (15/15 lower) at `deb6105a` | FFmpeg's ARMv7 build faults at widths of 2 modulo 4 from 18 up. The converter uses the filtered path at those widths. |

## DTS core header inspection

At `7c730ff8`, both DTS inspectors agreed on all 27 fields for 41,813 packets
on Linux AArch64. FFmpeg 9.0.2’s `dca_core` filter independently supplied
each expected core length and its bytes. All three authored containers passed
(12,375 packets each), as did the FATE DTS-HD MA stream and its three remuxes
(1,172 packets each).

The native controls below compare release test executables at reviewed main
`3e3ba929` and `7c730ff8`. Each entry is the minimum user-mode instruction
count across five runs of the whole test process on CPU 1. The owned inspector
reads each completed field or final partial field once.

| Control | AArch64 main | AArch64 candidate | Change | ARMv7 main | ARMv7 candidate | Change |
|---|---:|---:|---:|---:|---:|---:|
| Packed inspector, all 27 fields | 383,812 | 379,681 | -1.08% | 458,553 | 451,469 | -1.54% |
| Packed inspector, every exact-bit prefix | 1,050,925 | 743,708 | -29.23% | 1,318,869 | 978,501 | -25.81% |
| Owned inspector and strict parser, 396 source inputs | 5,437,197 | 4,319,714 | -20.55% | 7,261,046 | 6,206,697 | -14.52% |

DTS wall time, peak heap and total RSS remain unmeasured. The 135 AVC/HEVC
packet-framing comparisons have their own reference timing and memory results.

## Measured revisions

Each row names the source revision a result above was measured on and, where the raw
samples are archived, their SHA-256. Update the row in the same change as the result.
A gate run that [reuses](quality.md#reuse) a cited result changes no row: the row keeps
the revision and set it was measured with, and the run's `gate-result.json` names that
set as reused.

At `deb6105a` all 15 capture cases met the conversion, encoding, mux, time and
memory requirements on each c6g.4xlarge target. The packet-framing worker was
byte-identical to `499305f1` on each target. Its AArch64 time bounds against
FFmpeg were 1.15564 for `flush-update` and 1.08509 for
`avc-same-update-continuation`, with medians of 0.96215 and 0.96135. On ARMv7
all 135 time bounds passed and all 135 total resident bounds exceeded 1.0.

At `deb6105a` the FLAC decoder and sequential reader were measured together on c6g.4xlarge. Against FFmpeg the upper time ratios for
16-bit stereo and 24-bit six-channel files were 0.44132 and 0.47770 on AArch64,
and 0.64485 and 0.73675 on 32-bit ARMv7. Both used less peak heap and total RSS.
For all ten audio file workloads on both targets, decoded PCM and stereo
output passed their numerical comparisons. The largest absolute stereo sample
difference was zero for 16-bit FLAC and one s16 unit for each of the other
nine inputs. Both FLAC files decoded bit-exactly before mixing.
The 187-input FLAC comparison on each target matched 182 packet streams, with
five previously documented timestamp or recovery differences. Decoded PCM and
stereo output both passed on 183 inputs. Three streams change format during
decoding. The remaining stream decodes exactly but FFmpeg produces no stereo
output with its channel mask.

On c6g.4xlarge, the two registered FLAC files were decoded at `0230c88e` and
`3e3ba929` in 30 paired runs per target. For 16-bit stereo and 24-bit six-channel
audio, respectively, median decode time on ARMv7 fell by 16.9% and 7.1%
(upper time ratios 0.83128 and 0.92973). On AArch64 it rose by 0.34% and 0.20%
(upper ratios 1.00387 and 1.00237). Both output digests matched the preceding build. The file-codec
rows above use FFmpeg as their reference.

At `414b4cb5`, all 418 Matroska and linked playback cases pass on each c6g.4xlarge
target, including HLS, MP4 forward and range, progressive TS188, raw Annex B and
MPEG-TS program playback. Runtime uses paired timing and memory is kernel-bound.
At `be14c7e6`, all 21 Cues cases pass with 252 compatibility and 252 normal
playback traces per target. Extra preroll retains its source packet type and clock.

At `2fc2ebb8`, all 418 Matroska and playback cases pass per target on c6g.4xlarge,
including 252 compatibility and 252 normal playback seek probes. The highest runtime
upper bounds are 1.0392 on Linux AArch64 and 0.9986 on 32-bit ARMv7.
Peak heap and kernel-bound total peak RSS are lower in every case. All 10 audio file
workloads pass per target. `opus-5.1-256` measures 1.0092 on Linux AArch64 and
0.8575 on ARMv7, with byte-identical output before and after the CELT change.
On ARMv7 all 89 steps pass. On Linux AArch64 87 pass, with 23 listed
`subtitle-text-host` and four listed `subtitle-dvd-host` cases still open.

With the PGS fill and clock changes at `47ffada4`, 53 of 53 linked worker steps pass
on ARMv7 and 51 of 53 on Linux AArch64, with the same open subtitle cases.
The two subtitle workers change and every other candidate and reference hash matches
`2fc2ebb8`.

The Opus CELT profile includes `5bacc138` with the CELT changes applied. That build's
codec worker matches the hash of the Linux AArch64 gate build at `2fc2ebb8`.

At `5bacc138`, all 418 Matroska and playback cases pass on each c6g.4xlarge
target, including 252 compatibility and 252 normal playback seek probes per target.
The largest runtime upper bounds are 1.0404 on Linux AArch64 and 0.9935 on 32-bit ARMv7.
Peak heap and kernel-bound total peak RSS are lower in every case.
The wider selection passes all 89 steps on ARMv7. On Linux AArch64, 87 pass
and the other two grades fail only on the listed `subtitle-text-host` and
`subtitle-dvd-host` cases. The AArch64 `opus-5.1-256` bound
was 1.0593 before the CELT change.

At `97759e39`, both c6g.4xlarge targets pass all 367 text subtitle cases
and all 23 oversized timestamp cases. The subtitle worker selects the
reference target’s decimal field width. At `f8e1c4d9`, that comparison passed
23/23 on 32-bit ARMv7 and 0/23 on Linux AArch64 with the 32-bit field policy.

At `ce55904b`, all 290 cases across nine playback items pass parity, runtime,
peak heap and kernel-bound total peak RSS on c6g.4xlarge hosts for both targets.
Runtime upper bounds peak at 0.9945 on Linux AArch64
and 0.9942 on 32-bit ARMv7. The Matroska comparison includes
a six-packet HEVC input with repeated parameter sets in CodecPrivate.

At `5514c6a0`, all 290 cases across nine playback items pass parity, runtime,
peak heap and kernel-bound total peak RSS on c6g.4xlarge hosts for both targets.
The Matroska startup comparison includes a six-packet HEVC input with repeated
parameter sets in CodecPrivate. Runtime upper bounds peak at 1.0444 on Linux
AArch64 and 1.0009 on 32-bit ARMv7. The four playback startup and continuation
archives contain the per-case results and build identities.

At `bc488ac0`, the 16 Matroska and playback items passed all 418 cases on both
targets, including the 21 Cues cases and 252 seek probes per target.
The worst runtime upper bounds were 1.0480 on Linux AArch64 and 0.9967 on 32-bit ARMv7.
Peak live heap and kernel-bound total peak RSS were lower in every case.

At `414b4cb5`, the linked codec, audio reader, ISO BMFF reader and subtitle items
cover 42 gate steps on six c6g.4xlarge hosts per target. All 42 steps pass on ARMv7.
On Linux AArch64, 40 pass and the other two fail only on their listed open cases:
23 text-subtitle timestamp overflow cases and four DVD host cases. The separate
configured AAC access-unit comparison passes all 34 cases on each target for
parity, paired runtime, peak heap and kernel-bound total peak RSS.

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
and with the Matroska / WebM and other subtitle rows at `f8e1c4d9` (below).
The capture rows are measured at `c30b9c80`. AAC
in MOV, which the gate does not run, was measured at the same revision on one more
c6g.4xlarge host per target as the gate measures a codec workload list. Sets
`gate-run-TARGET-a73350ca-*-shard-1` to `-8` hold the shards' outputs, build identities,
plans, step records and logs, `aac-access-units-a73350ca-TARGET` the AAC in MOV
measurements, and `closing-run-ratios-a73350ca` every timed case's paired median and 95%
bounds with, per feature and target, the cases whose median or bound is above 1.0 and whose
bound is above 1.20, 1.10 and the present [ceiling](quality.md#gates), and the median of the
case medians. The run predates the `codecs-audio` item, graded at `bbdefa70`.

The runtime [ceiling](quality.md#gates) is set on the full sharded gate run per target at
`31d29945` described below with the PES list and unit reuse, the newest full measurement of
every item, and on AAC in MOV measured at `4213d042` (0.8717 on Linux AArch64, 1.0062 on
ARMv7). On Linux AArch64 no timed case has a runtime bound above the ceiling; the highest is
Matroska segment metadata's `chapters-many` at 1.0352. On ARMv7 two `subtitle-pgs` cases were
above it, `rle-long-rows-sup` at 1.0591 and `state-c0-and-reserved-sup` at 1.0550 (1.0490 and
1.0496 in a same-day run of that item at `1ebcdadc`); they were not listed as open. The full
run per target at `546045db` described below puts them at 0.7256 and 0.6435, and no timed case
there is above the ceiling on either target: the highest are `chapters-many` at 1.0377 on
Linux AArch64 and ISO BMFF seek's `origin18-nonzero-anchor-second-absent-t1-mid-seek-tiny` at
1.0221 on ARMv7.

The previous ceiling, 1.10, was set on one more full sharded gate run per target
at `1c64b775`, measured as the `a73350ca` run was, with set `codec-derived-streams` unpacked
on its hosts, and on AAC in MOV measured at that revision on one more c6g.4xlarge per
target; no result was reused. Linux AArch64 ran 156 steps and ARMv7 153. On Linux AArch64
the grades of `ass-render-host`, `subtitle-dvd-host` and `subtitle-text-host` fail, each only
on its contract's listed open cases; no step fails on ARMv7. Of 4,316 timed cases on Linux
AArch64 and 4,362 on ARMv7, none has a runtime bound above 1.10. The highest are
`ass-render`'s `cache-keys-ass-720p` at 1.0817 (median 1.0782) on Linux AArch64 and
`aac-hev2-stereo` at 1.0755 (median 1.0711) on ARMv7, where `h264-high-1080p` reaches
1.0426, `hevc-irap_repeat` 1.0133 and AAC in MOV 1.0176. The only memory bounds above 1.0
are packet framing's total RSS, listed in its contract's `open_bounds`: one case at 1.0290
on Linux AArch64, all 135 up to 1.0823 on ARMv7. Sets
`gate-run-aarch64-1c64b775-20261116-230956-shard-1` to `-8` and
`gate-run-armv7-1c64b775-20261116-230958-shard-1` to `-8` hold the shards' outputs, build
identities, plans, step records and logs, `aac-access-units-1c64b775-TARGET` the AAC in MOV
measurements, and `closing-run-ratios-1c64b775` every timed case's paired median and 95%
bounds, the cases with a bound above the present [ceiling](quality.md#gates), and per feature and target the worst runtime,
heap and total RSS bounds. The rows above keep the revisions they name.

At `2b196e9c`, which keeps `Time`'s 256-bit fallbacks out of line, one more full sharded gate
run per target was measured the same way. It measured every item again but the capture
cases' timing, which it reused from `1c64b775`: the change leaves the capture worker byte
for byte unchanged, as it does the ASS, font, packet framing, rasterising and shaping
workers. On Linux AArch64 the grades of `ass-render-host`, `subtitle-dvd-host` and
`subtitle-text-host` fail, each only on its contract's listed open cases; no grade fails
on ARMv7. The capture memory steps fail on both targets without measuring:
`capture qualify --stage memory` reads the records of its timing step, which a reused
timing step did not restore then. No runtime bound crosses 1.10, the ceiling then, and no memory
bound crosses its limit; per feature,
the median change of the case medians against `1c64b775` lies within −1.6% and +1.9%
(+8.1% for the five `subtitle-dvd-host` cases, whose operations take microseconds), as do
the features whose workers are unchanged, and the changed workers' user-mode instruction
counts on the link-order training operations are within 0.05% of `2a6a07a7`'s. Sets
`gate-run-aarch64-2b196e9c-20261117-192110-shard-1` to `-8` and
`gate-run-armv7-2b196e9c-20261117-192033-shard-1` to `-8` hold the shards' outputs, and
`link-order-code-size` the instruction counts and the per-feature comparisons. A reused
timing step now restores those records from its kept set: one partial run per target of the
15 capture items on five hosts, at `22b82550` on Linux AArch64 and `01ba2375` on ARMv7,
reused every timing step from the `a5837d38` run (sets
`gate-run-aarch64-a5837d38-20261118-163537-shard-1` to `-5` and
`gate-run-armv7-a5837d38-20261118-163631-shard-1` to `-5`, which measured all 30 steps per
target) and measured every memory step from the restored records, 30 of 30 steps passing on
both targets (heap ≤0.5904 and total RSS ≤0.9704 on AArch64, ≤0.6086 and ≤0.9766 on ARMv7;
sets `gate-run-aarch64-22b82550-20261118-172738-shard-1` to `-5` and
`gate-run-armv7-01ba2375-20261118-191656-shard-1` to `-5`).

The playback rows at `419a8493` (HLS segments, Matroska startup and seek, audio clocks and
legacy AAC-LC, MP4 forward and range, TS program, MPEG-TS progressive playback and raw
Annex B) come from one partial gate run per target of the nine items whose workers link
`triergon-playback`, each on one c6g.4xlarge host from the gate images, measured as the
`1c64b775` run was; no result was reused. All 26 steps pass on both targets, and every
case is lower on both heap and total RSS. Sets
`gate-run-aarch64-419a8493-20261117-190817-shard-1` and
`gate-run-armv7-419a8493-20261117-190751-shard-1` hold the outputs, build identities,
plans, step records and logs.

The playback tests cover HEVC startup with exact parameter-set repeats and
matching truncations of each parameter set that prevent startup. At `6bbb2cb7`,
all 289 cases in the nine playback gate groups passed parity, runtime and
kernel-bound heap and total RSS on both targets. Linux AArch64's largest runtime
bound is 1.0528 for progressive `reordered-48000`, with worst heap 0.8877 and
total RSS 0.8499. The 32-bit ARMv7 bounds are 0.9999, 0.7849 and 0.9670,
respectively. The four playback archives in the measured revisions table hold
the corresponding build identities and per-case evidence.

At `fd22d790`, reusing the admitted AAC payload view keeps the complete
progressive traces and work counts byte-identical. The nine playback groups
and the two MPEG-TS groups passed parity, runtime, peak heap and kernel-bound
total RSS on 555 cases per target on c6g.4xlarge. The largest bounds on Linux AArch64
were 1.0398 for runtime, 0.8877 for heap and 0.8801 for RSS.
The 32-bit ARMv7 bounds were 1.0101, 0.7849 and 0.9790, respectively.
Every runtime upper bound is within the present [ceiling](quality.md#gates). The four AAC payload view archives
list the corresponding build identities and per-case results.

At `29cb8c54`, each AVC access unit is published in exact storage while the
reader keeps its assembly buffer, and the progressive traces and work counts stay
byte-identical. All 289 cases in the nine playback groups passed parity, runtime,
peak heap and kernel-bound total RSS on both targets, from one partial gate run
per target on c6g.4xlarge. Linux AArch64's largest runtime bound is 1.0161 for
progressive `reordered-48000`, with worst heap 0.8877 and total RSS 0.8701. The
32-bit ARMv7 bounds are 1.0044 (MP4 range `fragmented-before-sync`), 0.7849 and
0.9825. The two AVC unit storage archives list the build identities and per-case
results.

At `c4643d6c`, AVC publication transfers the completed assembly buffer and
copies only the delimiter lookahead. All 289 cases in the nine playback items
passed exact parity, runtime, peak heap and kernel-bound total RSS on both
targets on c6g.4xlarge. The worst runtime upper bounds were 1.0386 on Linux
AArch64 (`reordered-48000`) and 0.9950 on 32-bit ARMv7
(`fragmented-before-sync`). The AVC buffer transfer archives hold the
per-case results and build identities.

At `31d29945`, the MPEG-TS observer reuses each consumed PES's span and header
lists, the progressive reader reuses retired AVC units' storage under the 2×
size rule, the hottest progressive paths keep their cold branches out of line,
and the trained orders are recorded again. The progressive and HLS traces and
work counts stay byte-identical. One full gate run per target on c6g.4xlarge
passed every step except those on the contract's listed open cases. Progressive
`reordered-48000` has runtime upper bounds of 1.0064 on Linux AArch64 and 0.7495
on 32-bit ARMv7. Heap and kernel-bound total RSS are lower than the reference in
every case except ARMv7 packet framing, whose total RSS stays open at 1.0858. The
ARMv7 PGS cases at 1.0591 and 1.0550 match a same-day run of the PGS item at
`1ebcdadc` (1.0490 and 1.0496, same medians). The PES list and unit reuse
archives hold the per-case results and build identities.

At `273f6992`, each transport feed loop variant is a function of its own and the
progressive reader keeps table events and owned payloads out of line, so its hot
path spans fewer instruction-cache lines. Traces and work counts stay
byte-identical. One full gate run per target on c6g.4xlarge passed every step
except those on the contract's listed open cases. Progressive `reordered-48000`
has runtime upper bounds of 0.9652 on Linux AArch64 and 0.7306 on 32-bit ARMv7,
and no case exceeds the runtime ceiling on either target. Heap and kernel-bound total RSS are
lower than the reference in every case except ARMv7 packet framing, whose total
RSS stays open at 1.0835. The run's credentials expired as its shards finished,
so each shard's output tree was packed, archived and indexed afterwards with the
gate's own steps; no file in any tree is newer than its shard's end. The feed
loop split archives hold the per-case results and build identities.

At `546045db`, the PGS receiver writes a run of up to 64 pixels as whole blocks of one
colour, each as long as the target fills inline: two 32-byte blocks on 32-bit ARM, where a
longer fill of a non-zero byte becomes a call to the C library's `memset`, and one 64-byte
block elsewhere. It rescales packet clocks to microseconds in 64-bit arithmetic while the
product fits, where 128-bit division is a software routine on 32-bit ARM. Traces and work
counts stay byte-identical; on ARMv7 the RLE-heavy PGS sessions run 21 to 27% fewer
user-mode instructions and 29 to 38% fewer cycles, and on Linux AArch64 0.3 to 1.5% fewer
instructions and about 4% fewer cycles. One full gate run per target on c6g.4xlarge, Linux
AArch64 156 steps and ARMv7 153, passed every step except, on Linux AArch64, the grades of
`ass-render-host`, `subtitle-dvd-host` and `subtitle-text-host`, each only on its contract's
listed open cases. On ARMv7 `rle-long-rows-sup` and `state-c0-and-reserved-sup` have
runtime upper bounds of 0.7256 and 0.6435 (medians 0.7123 and 0.6352), and the highest PGS
bound is 0.7775 there and 0.7806 on Linux AArch64. Heap and kernel-bound total RSS are lower
than the reference in every case except ARMv7 packet framing, whose total RSS stays open at
1.0800. The PGS run fill archives hold the per-case results and build identities.

The Matroska / WebM and subtitle rows, earlier capture comparisons and ARMv7 workspace
tests at `f8e1c4d9` come from one partial gate run per target of 33 items: the
15 capture cases, the six Matroska feature groups and the external HDR vector, every
subtitle feature, the workspace tests in both profiles with the worker tests, each on five
c6g.4xlarge hosts (Graviton2) from the gate images, kernel `7.0.0-1014-aws`, under the link
policy `static-toolchain-runtime-trained-order-v1`, measured as the full run is; no result
was reused. On the shard where the release tests ran first, they rebuilt the capture worker
outside the gate build, so capture `c06`, `c08` and `c11` were timed on that build and
their memory steps refused the timing; a second partial run of those three items at the
same revision, on three hosts per target, measured them. Apart from those three steps,
every step passes on both targets but `subtitle-text-host` and `subtitle-dvd-host` on Linux
AArch64, which fail only on their listed open cases. A partial run of the same items on
ARMv7 at `78596bf0`, before `main`'s capture, framing and AAC changes were merged, gave
worst bounds within 0.006 of these (capture 1.0618, Matroska 0.9725, DVD 0.9753). Sets
`gate-run-TARGET-f8e1c4d9-*` hold the shards' outputs, build identities, plans, step
records and logs, `gate-run-armv7-78596bf0-20261116-070135-shard-1` to `-5` the earlier
ARMv7 run, and `capture-matroska-subtitle-ratios-f8e1c4d9` every case's paired runtime
ratio and memory bounds with, per feature, the worst bounds and the cases above the present
[ceiling](quality.md#gates).

The PGS rows were measured before that at `99f1027b`, from one partial gate run of the `subtitle-pgs` item per
target, each on one c6g.4xlarge (Graviton2) from the gate image, kernel `7.0.0-1014-aws`,
under the link policy `static-toolchain-runtime-trained-order-v1`: timed alone on CPU 1,
memory collected under the kernel RSS proof on CPUs 1–15, and graded with the module
loaded; every step passes 319 of 319 cases. Sets
`gate-run-aarch64-99f1027b-20261116-050209-shard-1` and
`gate-run-armv7-99f1027b-20261116-050516-shard-1` hold each target's outputs, build
identities, plan, step records and logs.

The codec rows at `81edf4c0` come from one partial gate run per target of the items that
link the AAC decoder or the stereo downmix (`codecs`, `codecs-audio`, both codec workload
lists' timing and memory, `isobmff-reader`, `ogg-readers` and the tests) on four
c6g.4xlarge hosts, built and measured as at `f9fd4030`, with set `codec-derived-streams`
unpacked on their hosts. Every step passes on both targets. Sets
`gate-run-aarch64-81edf4c0-20261117-225329-shard-1` to `-4` and
`gate-run-armv7-81edf4c0-20261117-225155-shard-1` to `-4` hold the outputs, build
identities, plans, step records and logs.

The codec rows' workload timings and memory at `906d0a36` come from one partial gate run per
target of the H.264-linked items (`codecs`, both codec workload lists' timing and memory and
the tests) on four c6g.4xlarge hosts, built and measured as at `f9fd4030`. Every step passes
on both targets. Sets `gate-run-aarch64-906d0a36-20261116-213413-shard-1` to `-4` and
`gate-run-armv7-906d0a36-20261116-213256-shard-1` to `-4` hold the outputs, build
identities, plans, step records and logs. The audio, Ogg and ISO BMFF correctness lanes
were not part of that run and stand at `f9fd4030`.

The codec rows at `f9fd4030` come from one partial gate run per target of the codec items
(`codecs`, `codecs-audio`, `ogg-readers`, both codec workload lists' timing and memory,
`isobmff-reader` and the tests), measured as at `c68c9667` below, with `codec-aac-worker`
and the AAC decode example built by the pinned nightly with codec-aac's `simd` feature and
every other worker, `codec-worker` included, by stable. Every step passes on both targets.
Sets `gate-run-aarch64-f9fd4030-20261116-141046-shard-1` to `-3` and
`gate-run-armv7-f9fd4030-20261116-140926-shard-1` to `-3` hold the outputs, build
identities, plans, step records and logs. The same items ran at `7f32411b` with the whole
codec worker built by the nightly; its workload timings are in sets
`gate-run-aarch64-7f32411b-20261116-112713-shard-1` and
`gate-run-armv7-7f32411b-20261116-112546-shard-1`.

The codec rows at `c68c9667` come from one partial gate run per target of the codec items
(`codecs`, `codecs-audio`, `ogg-readers`, both codec workload lists' timing and memory,
`isobmff-reader` and the tests), on three c6g.4xlarge hosts (Graviton2) per target from
the gate image, kernel `7.0.0-1014-aws`, under the link policy
`static-toolchain-runtime-trained-order-v1`, with set `codec-derived-streams` unpacked on
its hosts. Every step passes on both targets but the ARMv7 timing of `workloads.json`,
which fails only on its three listed HE-AAC workloads. Sets
`gate-run-aarch64-c68c9667-20261116-082713-shard-1` to `-3` and
`gate-run-armv7-c68c9667-20261116-082759-shard-1` to `-3` hold the outputs, build
identities, plans, step records and logs. The same items were run at `0ac17828`, before
the AAC decoder's speed changes were merged (sets
`gate-run-aarch64-0ac17828-20261116-062732-shard-1` to `-3` and
`gate-run-armv7-0ac17828-20261116-062728-shard-1` to `-3`).

The codec and AAC rows at `4213d042` come from two partial gate runs per target of the
items that link the AAC decoder and the downmix (`codecs`, `codecs-audio`, both codec
workload lists' timing and memory, `isobmff-reader`, `ogg-readers` and the tests), on
c6g.4xlarge hosts (Graviton2) from the gate image, kernel `7.0.0-1014-aws`, under the
link policy `static-toolchain-runtime-trained-order-v1`. The first run's shard holding
`codecs-audio` stopped at its preflight, as the gate image lacks the derived audio
streams; the second run measured that shard's items with set `codec-derived-streams`
unpacked on its hosts. Every step passes on both targets but the two codec lists' ARMv7
timing, which fails only on their listed open workloads. Sets
`gate-run-aarch64-4213d042-20261116-061706-shard-2` and `-shard-3`,
`gate-run-aarch64-4213d042-20261116-064825-shard-1` and `-shard-2`, and on ARMv7 the same
shards of runs `061623` and `064649`, hold the outputs, build identities, plans, step
records and logs. AAC in MOV, which the gate does not run, was measured at the same
revision on one more c6g.4xlarge per target as the gate measures a codec workload list
(sets `aac-access-units-4213d042-TARGET`).

The packet framing rows come from the `framing` item on each target at `217fef73`.
All 135 cases on each c6g.4xlarge host match the reference, with 30 timing pairs on
CPU 1 and 30 kernel-bound memory pairs per case. The highest paired runtime upper
bounds are 0.9964 on Linux AArch64 (`avc-multi_idr`) and
0.9130 on 32-bit ARMv7 (`hevc-irap_repeat`). Peak heap is lower in every case.
All AArch64 total RSS bounds are lower. All 135 ARMv7 total RSS bounds are open,
from 1.0429 to 1.0840 (`rearm-type3`).

At `f6de9851` the same `framing` item was measured on both targets. Each run measured all 135 cases on c6g.4xlarge, with 30 timing pairs of
50 operations on CPU 1 and 30 kernel-bound memory pairs per case. All cases match the
reference and pass the runtime and heap bounds. The worst runtime bounds are 1.0176
on AArch64 for `avc-multi_idr` and 0.9643 on ARMv7 for `hevc-irap_repeat`. Across
cases, the median of the ratios of median times is 0.9004 on AArch64 and 0.7486 on ARMv7.
All AArch64 total RSS bounds are lower, including `avc-bad-update` at 0.9844.
All 135 ARMv7 total RSS bounds were above 1.0, with `avc-header-only-idr` highest at
1.0836. Sets
`gate-run-aarch64-f6de9851-20261117-230647-shard-1` and
`gate-run-armv7-f6de9851-20261117-230647-shard-1` hold the reports, build identities,
plans and logs.

ARMv7 debugger snapshots of the same two executables cover `avc-header-only-idr`,
`avc-high-flags` and `hevc-config-unknown_empty`, with 32 runs per executable and case.
Rust's stack-guard lookup adds a median of 31.5 to 32 resident pages, of which 30.5 to
31 are libc executable pages. Each shared libc load-address bucket (modulo 64 KiB)
compares the candidate's median residency before input open with the median of the
reference's per-run maxima over three stops (before input open, operation start and
operation end). The median excess across shared buckets is 16.25, 14.5 and 13.5 pages,
respectively. The packet-framing gate uses the kernel-bound peaks recorded above.

Every gated feature's contract requires both Linux AArch64 and ARMv7 since `47aa0451`
([quality](quality.md#reference-and-corpus)). One partial gate run at that revision on three
c6g.4xlarge hosts ran the 26 items those contracts belong to on ARMv7, measured the same
way: 72 steps, none failed. Every case of each of those features passes; the only failures
are the listed open cases of `raster-sweep` (`outlines-71`, `outlines-83`) and
`ass-parse-host`. The rows above keep the revisions they were measured at. Sets
`gate-run-armv7-47aa0451-20261118-040605-shard-1` to `-shard-3` hold the outputs, build
identities, plans, step records and logs.

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
In that earlier comparison, the AArch64 noise case used timing and memory at
`6d7a0f3c`, which gave the memory observer the same session timeout as timing.
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
contain those per-case results. At `384de6c9` the AArch64 worker links in its order
retrained from the same training operations.

At `be0eff89`, `avc-bad-update` was the only AArch64 total RSS miss. Its median
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

The ASS rendering rows at `eaff16ba` come from one partial gate run per target of the items
the rasteriser reaches (`ass-render`, `ass-render-host`, `font-shape-raster-ass` and the
tests) on two c6g.4xlarge hosts each, built and measured as the full runs at `1c64b775`. Every
step passes on both targets but, on Linux AArch64, `ass-render-host`'s grade, on its contract's
four listed open cases. At `1c64b775` the two `cache-keys` cases on Linux AArch64 ran fewer
user-mode instructions than libass (0.91 to 0.92 of them) in more cycles: the last pass of the
generic 16×16 tile fill, which turns each row's coverage into bytes, compiled to a scalar store
per pixel where it is inlined, about a quarter of the fill's cycles and 6% of the case's,
while libass's C fill is vectorised. That pass now maps each row as a whole array, which
compiles to vector code on AArch64 and to the same code on ARMv7, and draws the same bytes. On
Linux AArch64 `cache-keys-ass-720p` has a bound of 1.0256 (median 1.0241, from 1.0782) and
`cache-keys-ass-1080p` 1.0118 (median 1.0094, from 1.0647), both stopped after 20 pairs; no
case's median rose, the median of the case medians fell from 0.8096 to 0.7594, and 2 cases
have a bound above 1.0, from 8. On ARMv7 the median of the case medians is 0.6081, against
0.6076 at `1c64b775`, and the largest move of a median is `charmap-retry-ass-720p`'s, from
0.6815 to 0.7008, a case whose own bound is 0.7881. Set `ass-cache-keys-stage-eaff16ba` holds
the counts and cycle profiles of both renderer builds and libass on the `cache-keys` cases and
the fold digests of every case from both builds, all identical; set
`ass-render-ratios-eaff16ba` every case's runtime and memory bounds on both targets with the
comparison against `1c64b775`; sets `gate-run-aarch64-eaff16ba-20261117-185337-shard-1` and
`-2` and `gate-run-armv7-eaff16ba-20261117-190254-shard-1` and `-2` the outputs, build
identities, plans, step records and logs.

The shaping cases that need the interpreter (the corpus's Bitstream Vera, DejaVu Sans
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
| MPEG-1 capture, first eight inputs, AArch64 Linux | `deb6105a9644aa95adf6bea2b71c96845d3c6dfb` | [`1b53faf1a7d7f83e50a90e22fbac8903f7961c544ef48b3a0f58c0091c4d5bce`](../harness/evidence/sets/public-capture-low-deb6105a-aarch64.json) |
| MPEG-1 capture, remaining seven inputs, packet framing and MPEG-TS access units, AArch64 Linux | `deb6105a9644aa95adf6bea2b71c96845d3c6dfb` | [`a1f5937bb9e0163be50dc1a24900695b5207b3b07daf6e8c2056ea1bad06a535`](../harness/evidence/sets/public-capture-framing-deb6105a-aarch64.json) |
| MPEG-1 capture, first eight inputs, ARMv7 Linux | `deb6105a9644aa95adf6bea2b71c96845d3c6dfb` | [`f87e971033c19cf288c3194dd0104ddcd5d4a2ba09502b39d9fa999a6ed8e380`](../harness/evidence/sets/public-capture-low-deb6105a-armv7.json) |
| MPEG-1 capture, remaining seven inputs, packet framing and MPEG-TS access units, ARMv7 Linux | `deb6105a9644aa95adf6bea2b71c96845d3c6dfb` | [`8455e62a79a0cb6fe176fcfd63ad0c6ffc45faf6519b14bdab5c752efce52e0d`](../harness/evidence/sets/public-capture-framing-deb6105a-armv7.json) |
| Decoded samples and stereo output for the ten file workloads, AArch64 Linux | `deb6105a9644aa95adf6bea2b71c96845d3c6dfb` | [`e631aad20bf1545bfbc14704e33fbc916761b5c5885ac5d8a6d316d75257b554`](../harness/evidence/sets/codec-file-accuracy-deb6105a-aarch64.json) |
| Decoded samples and stereo output for the ten file workloads, ARMv7 Linux | `deb6105a9644aa95adf6bea2b71c96845d3c6dfb` | [`134c96bdecb862c724a3d99b13cde88a3775d8980cf4f0ebc05b9d19590c49f5`](../harness/evidence/sets/codec-file-accuracy-deb6105a-armv7.json) |
| ISO BMFF open, index, seek, range and forward playback, AArch64 Linux | `deb6105a9644aa95adf6bea2b71c96845d3c6dfb` | [`4e0d6ffff6bc3e1e1aee5c2c07a1d21afe172aa0d512fd948983f54310f087c5`](../harness/evidence/sets/public-isobmff-deb6105a-aarch64.json) |
| ISO BMFF open, index, seek, range and forward playback, ARMv7 Linux | `deb6105a9644aa95adf6bea2b71c96845d3c6dfb` | [`fffa1799a6458b7122157ccdaa3cc8be86333ef68c4b280442765299b9d85ec6`](../harness/evidence/sets/public-isobmff-deb6105a-armv7.json) |
| Transport, HLS, Matroska and elementary playback, fonts and shaping, AArch64 Linux | `deb6105a9644aa95adf6bea2b71c96845d3c6dfb` | [`28dd92402d28a4204649361cc42ee28230b3fa00a0a9426525e0261d6a6e0b94`](../harness/evidence/sets/public-playback-graphics-deb6105a-aarch64.json) |
| Transport, HLS, Matroska and elementary playback, fonts and shaping, ARMv7 Linux | `deb6105a9644aa95adf6bea2b71c96845d3c6dfb` | [`7c1f241f500b8d2b54a516aa41ba992cdffb4fb67db18fa39ded669ad2530862`](../harness/evidence/sets/public-playback-graphics-deb6105a-armv7.json) |
| File audio runtime and memory, AArch64 Linux | `deb6105a9644aa95adf6bea2b71c96845d3c6dfb` | [`e5d323cd62b44cbb848f0d4a0550005e8f7cde2a27ba479f5fb82d1b04d3f9a5`](../harness/evidence/sets/public-file-codecs-deb6105a-aarch64.json) |
| FLAC packet and PCM comparison, AArch64 Linux | `deb6105a9644aa95adf6bea2b71c96845d3c6dfb` | [`c943cd76153aa5df738fbedf5fc9625f42cd48b0a86f1bf9b678fae495953d40`](../harness/evidence/sets/flac-187-deb6105a-aarch64.json) |
| File audio runtime and memory, 32-bit ARMv7 Linux | `deb6105a9644aa95adf6bea2b71c96845d3c6dfb` | [`67e67b4a39dc3abf436cc1437c064efa0d7c368655bd2b6e19b3fe9956e2b347`](../harness/evidence/sets/public-file-codecs-deb6105a-armv7.json) |
| FLAC packet and PCM comparison, 32-bit ARMv7 Linux | `deb6105a9644aa95adf6bea2b71c96845d3c6dfb` | [`5b89fd0b6c25642da5aaa77b49989180665e09588fecbc54a6e3a8bc7b73a169`](../harness/evidence/sets/flac-187-deb6105a-armv7.json) |
| FLAC file decoding compared with `3e3ba929`, AArch64 Linux | `0230c88e4bb78677484d7b7d00a9c80e0be66d37` | [`b3320c7ff3c9d0fd3c9d6f665c7fa41499cf87992c4e12e41277907b4617a49a`](../harness/evidence/sets/flac-rice-0230c88e-aarch64.json) |
| FLAC file decoding compared with `3e3ba929`, 32-bit ARMv7 Linux | `0230c88e4bb78677484d7b7d00a9c80e0be66d37` | [`06e9f4afbc18def659c6b3f58dedd095529deeeb557d5215986385b12364d567`](../harness/evidence/sets/flac-rice-0230c88e-armv7.json) |
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
| Ceiling gate run, Linux AArch64, shard 1 of 8 | `1c64b77588fe3ec635b2bb31519dc23e2f365db7` | [`29a8bac6d12ea636d8d35b10ba625f01ee7795b8f37ab4db73aef4d4ff36df83`](../harness/evidence/sets/gate-run-aarch64-1c64b775-20261116-230956-shard-1.json) |
| Ceiling gate run, Linux AArch64, shard 2 of 8 | `1c64b77588fe3ec635b2bb31519dc23e2f365db7` | [`c7f8e3cb7911c140afaf02e00cdcc162e9dd257f6e56f06b1daab39ed732d540`](../harness/evidence/sets/gate-run-aarch64-1c64b775-20261116-230956-shard-2.json) |
| Ceiling gate run, Linux AArch64, shard 3 of 8 | `1c64b77588fe3ec635b2bb31519dc23e2f365db7` | [`7d449810df566d09c1b8f3cc1c5331542a370c6e0f90bcb6dc9041c7763843b4`](../harness/evidence/sets/gate-run-aarch64-1c64b775-20261116-230956-shard-3.json) |
| Ceiling gate run, Linux AArch64, shard 4 of 8 | `1c64b77588fe3ec635b2bb31519dc23e2f365db7` | [`575ddca2287dec5f1b3a97946fce12c70432bc4c2c5813506c8efd7ea43c099f`](../harness/evidence/sets/gate-run-aarch64-1c64b775-20261116-230956-shard-4.json) |
| Ceiling gate run, Linux AArch64, shard 5 of 8 | `1c64b77588fe3ec635b2bb31519dc23e2f365db7` | [`2885668abec21175b65a7ab47de111e5b3e37696bd4a9a7daa9444902624e592`](../harness/evidence/sets/gate-run-aarch64-1c64b775-20261116-230956-shard-5.json) |
| Ceiling gate run, Linux AArch64, shard 6 of 8 | `1c64b77588fe3ec635b2bb31519dc23e2f365db7` | [`c48836af7f466c6f48d28afd3140eab6ad9569de3509317643d2088cf87d7168`](../harness/evidence/sets/gate-run-aarch64-1c64b775-20261116-230956-shard-6.json) |
| Ceiling gate run, Linux AArch64, shard 7 of 8 | `1c64b77588fe3ec635b2bb31519dc23e2f365db7` | [`5764ddc3aacb6d2206fd55a893bab332f1e4a2ac8a1ec8b95ad7f95eb86e8722`](../harness/evidence/sets/gate-run-aarch64-1c64b775-20261116-230956-shard-7.json) |
| Ceiling gate run, Linux AArch64, shard 8 of 8 | `1c64b77588fe3ec635b2bb31519dc23e2f365db7` | [`b5946a929528bfb53c240041041925ad14926315ff5aa91580375be68e4cf73d`](../harness/evidence/sets/gate-run-aarch64-1c64b775-20261116-230956-shard-8.json) |
| Ceiling run's AAC in MOV, Linux AArch64 | `1c64b77588fe3ec635b2bb31519dc23e2f365db7` | [`4090aaedc07e54cc64f24438aca0e1a5cde600386cf7b4ea349a1a69774b710b`](../harness/evidence/sets/aac-access-units-1c64b775-aarch64.json) |
| Ceiling gate run, ARMv7, shard 1 of 8 | `1c64b77588fe3ec635b2bb31519dc23e2f365db7` | [`c3876f378296f68061674d99222b830a87c04f71af8642ca7339ddaaa9a99dbb`](../harness/evidence/sets/gate-run-armv7-1c64b775-20261116-230958-shard-1.json) |
| Ceiling gate run, ARMv7, shard 2 of 8 | `1c64b77588fe3ec635b2bb31519dc23e2f365db7` | [`b16e7152f357c901a435198e43c16f0c5d75568f8b7ec17341311359eb0dd473`](../harness/evidence/sets/gate-run-armv7-1c64b775-20261116-230958-shard-2.json) |
| Ceiling gate run, ARMv7, shard 3 of 8 | `1c64b77588fe3ec635b2bb31519dc23e2f365db7` | [`fce6358f83e7551a250ebebba63976ced5441f270613abb8b5304ec8a1bdcc9c`](../harness/evidence/sets/gate-run-armv7-1c64b775-20261116-230958-shard-3.json) |
| Ceiling gate run, ARMv7, shard 4 of 8 | `1c64b77588fe3ec635b2bb31519dc23e2f365db7` | [`bccff9c8634f1174f6ab0ea4f0e11f90d41862e79cd73b77abe7feed30e6f323`](../harness/evidence/sets/gate-run-armv7-1c64b775-20261116-230958-shard-4.json) |
| Ceiling gate run, ARMv7, shard 5 of 8 | `1c64b77588fe3ec635b2bb31519dc23e2f365db7` | [`93c5d316272d28ce7e676c34ca3fb0ebdac35051bc4ade5e70c10f01a25321a6`](../harness/evidence/sets/gate-run-armv7-1c64b775-20261116-230958-shard-5.json) |
| Ceiling gate run, ARMv7, shard 6 of 8 | `1c64b77588fe3ec635b2bb31519dc23e2f365db7` | [`73120b0c40c5a581fdcb5b93dae4953491688375f87591a60e1a544be796c664`](../harness/evidence/sets/gate-run-armv7-1c64b775-20261116-230958-shard-6.json) |
| Ceiling gate run, ARMv7, shard 7 of 8 | `1c64b77588fe3ec635b2bb31519dc23e2f365db7` | [`3caea6a25bc74221c36c28a946ed9d4808c3f3b88d75e83d588821f3f6f70784`](../harness/evidence/sets/gate-run-armv7-1c64b775-20261116-230958-shard-7.json) |
| Ceiling gate run, ARMv7, shard 8 of 8 | `1c64b77588fe3ec635b2bb31519dc23e2f365db7` | [`f1f605eb8482b8d0bdaa3e5219b417e61739ba1cdf00c82abdde7f940ce39561`](../harness/evidence/sets/gate-run-armv7-1c64b775-20261116-230958-shard-8.json) |
| Ceiling run's AAC in MOV, ARMv7 | `1c64b77588fe3ec635b2bb31519dc23e2f365db7` | [`5c39ee676cf55ec28958f97ddde6a6246c0a331e93ad10f8c3156471013f6056`](../harness/evidence/sets/aac-access-units-1c64b775-armv7.json) |
| Per-case runtime ratios of the ceiling gate runs and AAC in MOV, both targets | `1c64b77588fe3ec635b2bb31519dc23e2f365db7` | [`3fa6a4bb337c9d4a8931582f73f714b6d3d3a8d910430e1b7ec14cd5d7513131`](../harness/evidence/sets/closing-run-ratios-1c64b775.json) |
| Code-size gate run, Linux AArch64, shard 1 of 8 | `2b196e9c94f6341af49ef5acd46b08ace399e551` | [`dec8229f2f41eaffe5cd5d4110d5306f3418ac8b4842cd6b764a3100b14b01dd`](../harness/evidence/sets/gate-run-aarch64-2b196e9c-20261117-192110-shard-1.json) |
| Code-size gate run, Linux AArch64, shard 2 of 8 | `2b196e9c94f6341af49ef5acd46b08ace399e551` | [`d486987aeabfedbe7ed61ae3b347d8859f021cae83bd5c0be951bad1d0100ca9`](../harness/evidence/sets/gate-run-aarch64-2b196e9c-20261117-192110-shard-2.json) |
| Code-size gate run, Linux AArch64, shard 3 of 8 | `2b196e9c94f6341af49ef5acd46b08ace399e551` | [`7bfaefc0df192c120f1ccc531ad77691ac112e0ee2171735ce53e60fa811273a`](../harness/evidence/sets/gate-run-aarch64-2b196e9c-20261117-192110-shard-3.json) |
| Code-size gate run, Linux AArch64, shard 4 of 8 | `2b196e9c94f6341af49ef5acd46b08ace399e551` | [`b1b480fd6d66a2adca83874b2176fc70d84eb7e04edf8ec9bc6cd9fca4bbdb6f`](../harness/evidence/sets/gate-run-aarch64-2b196e9c-20261117-192110-shard-4.json) |
| Code-size gate run, Linux AArch64, shard 5 of 8 | `2b196e9c94f6341af49ef5acd46b08ace399e551` | [`3ece32ad060ae9518974c255fdbf466a6aba26ec4bdaf0ebf20cda25e679e2f9`](../harness/evidence/sets/gate-run-aarch64-2b196e9c-20261117-192110-shard-5.json) |
| Code-size gate run, Linux AArch64, shard 6 of 8 | `2b196e9c94f6341af49ef5acd46b08ace399e551` | [`64d267f51a12827ccfbc5eb4e23bfab72890d5b3cfffa4475d6a524b0552f196`](../harness/evidence/sets/gate-run-aarch64-2b196e9c-20261117-192110-shard-6.json) |
| Code-size gate run, Linux AArch64, shard 7 of 8 | `2b196e9c94f6341af49ef5acd46b08ace399e551` | [`a106230d2c951bc7d27d03c5b3163f3dd6614f382a8201c1389ca42b43537e0d`](../harness/evidence/sets/gate-run-aarch64-2b196e9c-20261117-192110-shard-7.json) |
| Code-size gate run, Linux AArch64, shard 8 of 8 | `2b196e9c94f6341af49ef5acd46b08ace399e551` | [`e9ad8b02bad07b86c41b02204b3c28a66f21ad8a581dc0a6bea2fea2caa3161b`](../harness/evidence/sets/gate-run-aarch64-2b196e9c-20261117-192110-shard-8.json) |
| Code-size gate run, ARMv7, shard 1 of 8 | `2b196e9c94f6341af49ef5acd46b08ace399e551` | [`d15127d61f8848c1b3b89dbc2e7745dc0107556539bb83e0bdbfe2087b8f94c8`](../harness/evidence/sets/gate-run-armv7-2b196e9c-20261117-192033-shard-1.json) |
| Code-size gate run, ARMv7, shard 2 of 8 | `2b196e9c94f6341af49ef5acd46b08ace399e551` | [`84489028480912bdba965a2a33a3972b0e9c0bc7e16f593af9b7ffbf293cfffc`](../harness/evidence/sets/gate-run-armv7-2b196e9c-20261117-192033-shard-2.json) |
| Code-size gate run, ARMv7, shard 3 of 8 | `2b196e9c94f6341af49ef5acd46b08ace399e551` | [`63fb39c1c8422e3ecce943638dddad69895314762c62ad1a3b2c0084905926b3`](../harness/evidence/sets/gate-run-armv7-2b196e9c-20261117-192033-shard-3.json) |
| Code-size gate run, ARMv7, shard 4 of 8 | `2b196e9c94f6341af49ef5acd46b08ace399e551` | [`538bd6c347b76aae54a60f85783f40720f149e31ae5a2c22c532ff1f025d2559`](../harness/evidence/sets/gate-run-armv7-2b196e9c-20261117-192033-shard-4.json) |
| Code-size gate run, ARMv7, shard 5 of 8 | `2b196e9c94f6341af49ef5acd46b08ace399e551` | [`f6f3ac9d66a7a42c271a8b482d05773a413ca44cdf245422b4f07372e94e0392`](../harness/evidence/sets/gate-run-armv7-2b196e9c-20261117-192033-shard-5.json) |
| Code-size gate run, ARMv7, shard 6 of 8 | `2b196e9c94f6341af49ef5acd46b08ace399e551` | [`200b7139822f8fc9edccf0efb5dd5d5cf1ed4488bdfef90b5a96d3c6e072180e`](../harness/evidence/sets/gate-run-armv7-2b196e9c-20261117-192033-shard-6.json) |
| Code-size gate run, ARMv7, shard 7 of 8 | `2b196e9c94f6341af49ef5acd46b08ace399e551` | [`421106aa25e7b3ab431fcafd27a0099711f5f6f1c1f6ac8692015dca9fb59cfa`](../harness/evidence/sets/gate-run-armv7-2b196e9c-20261117-192033-shard-7.json) |
| Code-size gate run, ARMv7, shard 8 of 8 | `2b196e9c94f6341af49ef5acd46b08ace399e551` | [`f2f78eb52b940ea67cf87b8ae185327614e3df5cf1c1477a03526380f571cfd8`](../harness/evidence/sets/gate-run-armv7-2b196e9c-20261117-192033-shard-8.json) |
| Ordered workers without their trained orders, Linux AArch64 | `e21f800e152edda037157d121512988f0184d5f3` | [`f92932325ff24c751c92992084d4eb60b60169daa6ae4c2988045f226ef3add3`](../harness/evidence/sets/gate-run-aarch64-e21f800e-20261117-184759-shard-1.json) |
| Ordered workers without their trained orders, ARMv7 | `e21f800e152edda037157d121512988f0184d5f3` | [`f6694d99b2beec8bd4696c7da415512e5d9a016ba69153151cf21d83a60ed701`](../harness/evidence/sets/gate-run-armv7-e21f800e-20261117-184730-shard-1.json) |
| Ordered workers without their trained orders after the code-size change, Linux AArch64 | `d18904f950fee2d00da37901cf9d36b6cf9d46a6` | [`8d2cfccfd6f0cffe114bc7b9990264ec592a247ea432937a4ad1d919349afdcc`](../harness/evidence/sets/gate-run-aarch64-d18904f9-20261117-192309-shard-1.json) |
| Ordered workers without their trained orders after the code-size change, ARMv7 | `d18904f950fee2d00da37901cf9d36b6cf9d46a6` | [`f1d1cc8dabc6cfec0d885ba7e779b2501f9620fda5f43783a58698fd69e6faf6`](../harness/evidence/sets/gate-run-armv7-d18904f9-20261117-192245-shard-1.json) |
| Code size against the trained orders: sizes, resident probes, instruction counts and per-case comparisons, both targets | `2b196e9c94f6341af49ef5acd46b08ace399e551` | [`39ea31fe8e47d1250d407aea7d2c2163bec09d29ee1449373f5e4144078f2834`](../harness/evidence/sets/link-order-code-size.json) |
| AAC payload views, Linux AArch64, shard 1 of 2 | `fd22d7906da027afb91fb8c486814a90d3da1c38` | [`e92ac9ba2e23db5a7f719cf3401c6eb0f16006c5ca5acfd43f2d9b43d8b1236b`](../harness/evidence/sets/gate-run-aarch64-fd22d790-20261118-154842-shard-1.json) |
| AAC payload views, Linux AArch64, shard 2 of 2 | `fd22d7906da027afb91fb8c486814a90d3da1c38` | [`cda3273b5f0fa481ef3506d3240c0cb836e38a7618728145ddd747e5a6169609`](../harness/evidence/sets/gate-run-aarch64-fd22d790-20261118-154842-shard-2.json) |
| AAC payload views, ARMv7, shard 1 of 2 | `fd22d7906da027afb91fb8c486814a90d3da1c38` | [`7c5bf2e1e8f629da90ba6b1ab17e0e10683e4099971c554a55dfe8ef0a407b2c`](../harness/evidence/sets/gate-run-armv7-fd22d790-20261118-155439-shard-1.json) |
| AAC payload views, ARMv7, shard 2 of 2 | `fd22d7906da027afb91fb8c486814a90d3da1c38` | [`1b0f99bb79e4a7a6c60a30080482f6c1b96f25e250b08cedbcbd8fcc7edc0b9b`](../harness/evidence/sets/gate-run-armv7-fd22d790-20261118-155439-shard-2.json) |
| AVC unit storage, Linux AArch64 | `29cb8c545900be1197c18499721f7dc78e48c478` | [`287061df55a8a896a7ffe143755a9d1d0559b319bfc75a27cbc3c572872b3c25`](../harness/evidence/sets/gate-run-aarch64-29cb8c54-20261118-212336-shard-1.json) |
| AVC unit storage, ARMv7 | `29cb8c545900be1197c18499721f7dc78e48c478` | [`57da19b7af4e7f324a9a612d04c28635b2acd5c0471c59814ec0814786674f07`](../harness/evidence/sets/gate-run-armv7-29cb8c54-20261118-212453-shard-1.json) |
| AVC buffer transfer, Linux AArch64, shard 1 of 2 | `c4643d6cee49e4c512ab41f0a9dd178da58d8386` | [`4156e1e93329d879a17107d923da5bc55065c79f291bb49fa5e5592ac0108325`](../harness/evidence/sets/gate-run-aarch64-c4643d6c-20261119-005613-shard-1.json) |
| AVC buffer transfer, Linux AArch64, shard 2 of 2 | `c4643d6cee49e4c512ab41f0a9dd178da58d8386` | [`6f7a9f1b311ae506e5ae90d20fb9260101226835a4d245de4716b0d9e7f08ff4`](../harness/evidence/sets/gate-run-aarch64-c4643d6c-20261119-005613-shard-2.json) |
| AVC buffer transfer, 32-bit ARMv7, shard 1 of 2 | `c4643d6cee49e4c512ab41f0a9dd178da58d8386` | [`f2c27476551109fbbc578c35ca7432b8cfc6e90a9c4ca4c6a870775fd96e081b`](../harness/evidence/sets/gate-run-armv7-c4643d6c-20261119-005613-shard-1.json) |
| AVC buffer transfer, 32-bit ARMv7, shard 2 of 2 | `c4643d6cee49e4c512ab41f0a9dd178da58d8386` | [`b2844044440b4a3a62c272f0c1cdef07df50d796bf8dea9ec8f822ff81d55f4f`](../harness/evidence/sets/gate-run-armv7-c4643d6c-20261119-005613-shard-2.json) |
| PES list and unit reuse, Linux AArch64, shard 1 of 8 | `31d2994522f116b4404916cb51f82b03a29c158f` | [`94312501392918702d08c450ab72f4f4fa7101e7d99c588a21978f251bb153e6`](../harness/evidence/sets/gate-run-aarch64-31d29945-20261119-061108-shard-1.json) |
| PES list and unit reuse, Linux AArch64, shard 2 of 8 | `31d2994522f116b4404916cb51f82b03a29c158f` | [`2068dcedbd43a007298725bb0b30ce6f404c7ae9cb91e66181baf980e4181e8c`](../harness/evidence/sets/gate-run-aarch64-31d29945-20261119-061108-shard-2.json) |
| PES list and unit reuse, Linux AArch64, shard 3 of 8 | `31d2994522f116b4404916cb51f82b03a29c158f` | [`8761019bc8897fdcb5b6e18ebced23c904db9bb92a0a65be7616fd5f6c3c474f`](../harness/evidence/sets/gate-run-aarch64-31d29945-20261119-061108-shard-3.json) |
| PES list and unit reuse, Linux AArch64, shard 4 of 8 | `31d2994522f116b4404916cb51f82b03a29c158f` | [`92ca479401b247c36393491e2e81d1f2d4aa64af33033f02d8946704cfea4f62`](../harness/evidence/sets/gate-run-aarch64-31d29945-20261119-061108-shard-4.json) |
| PES list and unit reuse, Linux AArch64, shard 5 of 8 | `31d2994522f116b4404916cb51f82b03a29c158f` | [`f6376e66880cd70d9bb05ef1cbb81fc0972f7a7d611b2fea2031a81fbc742a74`](../harness/evidence/sets/gate-run-aarch64-31d29945-20261119-061108-shard-5.json) |
| PES list and unit reuse, Linux AArch64, shard 6 of 8 | `31d2994522f116b4404916cb51f82b03a29c158f` | [`ed7071063f338a4f9fbcc975b43e06a3950d23b96997c33d7681a33da09fcb51`](../harness/evidence/sets/gate-run-aarch64-31d29945-20261119-061108-shard-6.json) |
| PES list and unit reuse, Linux AArch64, shard 7 of 8 | `31d2994522f116b4404916cb51f82b03a29c158f` | [`ebb4de7d44a06b379fbc86364a006a02798f07df905023bb63908b7b63ec58e0`](../harness/evidence/sets/gate-run-aarch64-31d29945-20261119-061108-shard-7.json) |
| PES list and unit reuse, Linux AArch64, shard 8 of 8 | `31d2994522f116b4404916cb51f82b03a29c158f` | [`d8642ba2fc6834c027179df3f79d3ef06a5419c24cf6381056d28173c2f07c38`](../harness/evidence/sets/gate-run-aarch64-31d29945-20261119-061108-shard-8.json) |
| PES list and unit reuse, 32-bit ARMv7, shard 1 of 8 | `31d2994522f116b4404916cb51f82b03a29c158f` | [`cdfab034d8cf69d3ca912b4885edfadda1e177caca1001b7e5e341c69be612dc`](../harness/evidence/sets/gate-run-armv7-31d29945-20261119-061148-shard-1.json) |
| PES list and unit reuse, 32-bit ARMv7, shard 2 of 8 | `31d2994522f116b4404916cb51f82b03a29c158f` | [`d90a5fc167c545451e42acee9e9c1cbbbfb6bfe60473036f7f7fc8f6f00e79b6`](../harness/evidence/sets/gate-run-armv7-31d29945-20261119-061148-shard-2.json) |
| PES list and unit reuse, 32-bit ARMv7, shard 3 of 8 | `31d2994522f116b4404916cb51f82b03a29c158f` | [`1df57c98b794b10e66aa979c4ac4bb0600ef0e75eb71e300e81633e805ea1736`](../harness/evidence/sets/gate-run-armv7-31d29945-20261119-061148-shard-3.json) |
| PES list and unit reuse, 32-bit ARMv7, shard 4 of 8 | `31d2994522f116b4404916cb51f82b03a29c158f` | [`a6cae1a8529c6717b3fe1356eb424e96f5703b306815af64bcf1c7889af8de98`](../harness/evidence/sets/gate-run-armv7-31d29945-20261119-061148-shard-4.json) |
| PES list and unit reuse, 32-bit ARMv7, shard 5 of 8 | `31d2994522f116b4404916cb51f82b03a29c158f` | [`f6a0da077ddbf709f03885b371c89895d8ba91e844242c4fc5a16fdc01a13214`](../harness/evidence/sets/gate-run-armv7-31d29945-20261119-061148-shard-5.json) |
| PES list and unit reuse, 32-bit ARMv7, shard 6 of 8 | `31d2994522f116b4404916cb51f82b03a29c158f` | [`df45e5dec57be8214116c988d726215f8269af6d0a921e75a47a53bca8203f9b`](../harness/evidence/sets/gate-run-armv7-31d29945-20261119-061148-shard-6.json) |
| PES list and unit reuse, 32-bit ARMv7, shard 7 of 8 | `31d2994522f116b4404916cb51f82b03a29c158f` | [`1aa5e28305f4991a29de7745afacd7cbedb33cc2b749f95e62f9c011d60ce7f5`](../harness/evidence/sets/gate-run-armv7-31d29945-20261119-061148-shard-7.json) |
| PES list and unit reuse, 32-bit ARMv7, shard 8 of 8 | `31d2994522f116b4404916cb51f82b03a29c158f` | [`25231e5c43054ebc0585162903f9d564e182b83dbda7240c41758104aadfce60`](../harness/evidence/sets/gate-run-armv7-31d29945-20261119-061148-shard-8.json) |
| PGS run fills, Linux AArch64, shard 1 of 8 | `546045db0473d0e39d70f67ec0e247e21e18e68b` | [`829049b804ee25b9da4231683749fe8a838e705c21223ed4d52a0e0646d783aa`](../harness/evidence/sets/gate-run-aarch64-546045db-20261119-143720-shard-1.json) |
| PGS run fills, Linux AArch64, shard 2 of 8 | `546045db0473d0e39d70f67ec0e247e21e18e68b` | [`079bee7b498f36331dfc2a2e8a457e5e7ba97ee4e9e24ba66a7cc46112f70512`](../harness/evidence/sets/gate-run-aarch64-546045db-20261119-143720-shard-2.json) |
| PGS run fills, Linux AArch64, shard 3 of 8 | `546045db0473d0e39d70f67ec0e247e21e18e68b` | [`3e25d4bf6b5d1334bbf150a32a52326d2f6154b29f9bac6167997ae97b49798e`](../harness/evidence/sets/gate-run-aarch64-546045db-20261119-143720-shard-3.json) |
| PGS run fills, Linux AArch64, shard 4 of 8 | `546045db0473d0e39d70f67ec0e247e21e18e68b` | [`ee906c9782755fe6b7f9dec1009cd6bef34eae358ffa3ef6b7cd32070c17cbd8`](../harness/evidence/sets/gate-run-aarch64-546045db-20261119-143720-shard-4.json) |
| PGS run fills, Linux AArch64, shard 5 of 8 | `546045db0473d0e39d70f67ec0e247e21e18e68b` | [`f8bf53732874cd37445a534c6012b0172ef15294ad26ea86b3c80c419118b896`](../harness/evidence/sets/gate-run-aarch64-546045db-20261119-143720-shard-5.json) |
| PGS run fills, Linux AArch64, shard 6 of 8 | `546045db0473d0e39d70f67ec0e247e21e18e68b` | [`98bf0e62dde68d938dc8ca7414354804a059548e8b44f417da22926f2eada3f7`](../harness/evidence/sets/gate-run-aarch64-546045db-20261119-143720-shard-6.json) |
| PGS run fills, Linux AArch64, shard 7 of 8 | `546045db0473d0e39d70f67ec0e247e21e18e68b` | [`66521765aaa0b261539d01b59d160f1b70bd815b53f0ad41a212347fbe73ab83`](../harness/evidence/sets/gate-run-aarch64-546045db-20261119-143720-shard-7.json) |
| PGS run fills, Linux AArch64, shard 8 of 8 | `546045db0473d0e39d70f67ec0e247e21e18e68b` | [`1ae18925b4abdbdf3df6638e807fdf3dd7c1cb2daf45af0f46371cdac6531981`](../harness/evidence/sets/gate-run-aarch64-546045db-20261119-143720-shard-8.json) |
| PGS run fills, 32-bit ARMv7, shard 1 of 8 | `546045db0473d0e39d70f67ec0e247e21e18e68b` | [`3673680df08c4579d4893d6ae36a04fba45900737a76461ed05d5c37aef90e57`](../harness/evidence/sets/gate-run-armv7-546045db-20261119-143555-shard-1.json) |
| PGS run fills, 32-bit ARMv7, shard 2 of 8 | `546045db0473d0e39d70f67ec0e247e21e18e68b` | [`c402c069aecf09f32e61b703de43fc34edbb22ecb8da24bc2827f30f5972c938`](../harness/evidence/sets/gate-run-armv7-546045db-20261119-143555-shard-2.json) |
| PGS run fills, 32-bit ARMv7, shard 3 of 8 | `546045db0473d0e39d70f67ec0e247e21e18e68b` | [`055c51e38c967337f13edf8a724faba926faccb5a53df747f5c0607c387fa04f`](../harness/evidence/sets/gate-run-armv7-546045db-20261119-143555-shard-3.json) |
| PGS run fills, 32-bit ARMv7, shard 4 of 8 | `546045db0473d0e39d70f67ec0e247e21e18e68b` | [`e346832f3d8ffb3747c5cac665d584c19d82b8ed2a445e4fd130c5d07c862d4e`](../harness/evidence/sets/gate-run-armv7-546045db-20261119-143555-shard-4.json) |
| PGS run fills, 32-bit ARMv7, shard 5 of 8 | `546045db0473d0e39d70f67ec0e247e21e18e68b` | [`71c0c2c32f5524ab5df67c95a9690bee26987dd117495a06f7c88d25bef053c3`](../harness/evidence/sets/gate-run-armv7-546045db-20261119-143555-shard-5.json) |
| PGS run fills, 32-bit ARMv7, shard 6 of 8 | `546045db0473d0e39d70f67ec0e247e21e18e68b` | [`a4395aaf0db6687906d184a42546881294d825050c4d06ff3171b1048d062a21`](../harness/evidence/sets/gate-run-armv7-546045db-20261119-143555-shard-6.json) |
| PGS run fills, 32-bit ARMv7, shard 7 of 8 | `546045db0473d0e39d70f67ec0e247e21e18e68b` | [`9f95eff4fc2d0d63b35f7c5b192ced4aafb78dea6817b9a40b105030ef77a852`](../harness/evidence/sets/gate-run-armv7-546045db-20261119-143555-shard-7.json) |
| PGS run fills, 32-bit ARMv7, shard 8 of 8 | `546045db0473d0e39d70f67ec0e247e21e18e68b` | [`f5c5f6484851cd024b784dfee82030263ece79990d8da06fd89c14158fff2cfb`](../harness/evidence/sets/gate-run-armv7-546045db-20261119-143555-shard-8.json) |
| Earlier: Subtitles, PGS at main, 32-bit ARMv7 | `1ebcdadca39b3d62f9c1686e01688ef01b94e62d` | [`8ca1c34ed6e80efa190b70dec57c6d88ad8d01d966b41481382fdff385c5e309`](../harness/evidence/sets/gate-run-armv7-1ebcdadc-20261119-060922-shard-1.json) |
| Feed loop split, Linux AArch64, shard 1 of 8 | `273f6992b83532651d66f95ac58810e9baa8be9f` | [`82a83c448f353b2f28d66cfa72466f08eebfb3e8126177f24a7148836ddba6e2`](../harness/evidence/sets/gate-run-aarch64-273f6992-20261119-095305-shard-1.json) |
| Feed loop split, Linux AArch64, shard 2 of 8 | `273f6992b83532651d66f95ac58810e9baa8be9f` | [`7d49ac1b31c7543a738f787186591d4f170285b23218d2e03ba08ee793d8395f`](../harness/evidence/sets/gate-run-aarch64-273f6992-20261119-095305-shard-2.json) |
| Feed loop split, Linux AArch64, shard 3 of 8 | `273f6992b83532651d66f95ac58810e9baa8be9f` | [`39456f3e6163c8932b120e969efacbb94488c94a6cfcb0c52e73f891e76835e8`](../harness/evidence/sets/gate-run-aarch64-273f6992-20261119-095305-shard-3.json) |
| Feed loop split, Linux AArch64, shard 4 of 8 | `273f6992b83532651d66f95ac58810e9baa8be9f` | [`c7f221cdb68b3c64ff9b3f5a0d6ec4dbd825f0ecd0342c82ab9452c6906198e4`](../harness/evidence/sets/gate-run-aarch64-273f6992-20261119-095305-shard-4.json) |
| Feed loop split, Linux AArch64, shard 5 of 8 | `273f6992b83532651d66f95ac58810e9baa8be9f` | [`f557842fa476c39ac46c298289e60dfb3944a2991649b6b8765feb180e794e2a`](../harness/evidence/sets/gate-run-aarch64-273f6992-20261119-095305-shard-5.json) |
| Feed loop split, Linux AArch64, shard 6 of 8 | `273f6992b83532651d66f95ac58810e9baa8be9f` | [`1b896074ecb26f2deed7c61ba7881e0e638c323837410c4d66428121a9c1528e`](../harness/evidence/sets/gate-run-aarch64-273f6992-20261119-095305-shard-6.json) |
| Feed loop split, Linux AArch64, shard 7 of 8 | `273f6992b83532651d66f95ac58810e9baa8be9f` | [`f58bacce2b341f9eeed3967d4d400ee63da0cf40268136f2a8fdcf23fe30394f`](../harness/evidence/sets/gate-run-aarch64-273f6992-20261119-095305-shard-7.json) |
| Feed loop split, Linux AArch64, shard 8 of 8 | `273f6992b83532651d66f95ac58810e9baa8be9f` | [`731c35848c13aef82f8eed521c720bb8d79c5d74f2b1e79d99a839b7392e5264`](../harness/evidence/sets/gate-run-aarch64-273f6992-20261119-095305-shard-8.json) |
| Feed loop split, 32-bit ARMv7, shard 1 of 8 | `273f6992b83532651d66f95ac58810e9baa8be9f` | [`581a16592dbc31c63b333652fe05b1502f0b0f735f1a4f7d9826b4b7f34ad5fc`](../harness/evidence/sets/gate-run-armv7-273f6992-20261119-095344-shard-1.json) |
| Feed loop split, 32-bit ARMv7, shard 2 of 8 | `273f6992b83532651d66f95ac58810e9baa8be9f` | [`5b7cdd3622a5f0f1814aa28ca2e5bebae6ef853714450cd94afbbb21e39a120a`](../harness/evidence/sets/gate-run-armv7-273f6992-20261119-095344-shard-2.json) |
| Feed loop split, 32-bit ARMv7, shard 3 of 8 | `273f6992b83532651d66f95ac58810e9baa8be9f` | [`ed846b7059002c16d4a95212acf5d12edd05108caa6899c61fbba14bc8fc3d42`](../harness/evidence/sets/gate-run-armv7-273f6992-20261119-095344-shard-3.json) |
| Feed loop split, 32-bit ARMv7, shard 4 of 8 | `273f6992b83532651d66f95ac58810e9baa8be9f` | [`9177f8ae8f8425a4fd46c26c4e16d171ebda57717ea30f15dec0c9741479046b`](../harness/evidence/sets/gate-run-armv7-273f6992-20261119-095344-shard-4.json) |
| Feed loop split, 32-bit ARMv7, shard 5 of 8 | `273f6992b83532651d66f95ac58810e9baa8be9f` | [`fa283a74b169f2bdd4fc52da14c0f955ce448d0b08ff37f6cfbe935878eaf731`](../harness/evidence/sets/gate-run-armv7-273f6992-20261119-095344-shard-5.json) |
| Feed loop split, 32-bit ARMv7, shard 6 of 8 | `273f6992b83532651d66f95ac58810e9baa8be9f` | [`e5cddccb848aeb5690b7d752762e971f6d2ba5cb45cf62db08c5c14e61037caf`](../harness/evidence/sets/gate-run-armv7-273f6992-20261119-095344-shard-6.json) |
| Feed loop split, 32-bit ARMv7, shard 7 of 8 | `273f6992b83532651d66f95ac58810e9baa8be9f` | [`b4c7a6aabd3be4cd24289ca93bd32e56842ba731ac4b63b9a30ce7101239a0ed`](../harness/evidence/sets/gate-run-armv7-273f6992-20261119-095344-shard-7.json) |
| Feed loop split, 32-bit ARMv7, shard 8 of 8 | `273f6992b83532651d66f95ac58810e9baa8be9f` | [`5ca5e94d4b82714d1d6f85ab4ac6606ebf6d1ed4cd76d112e21c16df35220926`](../harness/evidence/sets/gate-run-armv7-273f6992-20261119-095344-shard-8.json) |
| Playback with HEVC parameter-set repeats, Linux AArch64, shard 1 of 2 | `6bbb2cb7887d08585a86440dcb4722506a7e1cb6` | [`a69853ba25e9b5efcdf0b2623ce4b1e4a391f0915c3b7eda3fac3ceff01d19d7`](../harness/evidence/sets/gate-run-aarch64-6bbb2cb7-20261118-104943-shard-1.json) |
| Playback with HEVC parameter-set repeats, Linux AArch64, shard 2 of 2 | `6bbb2cb7887d08585a86440dcb4722506a7e1cb6` | [`2ab05fbb4a99c88d03cec697104d3360ee92dd45f398fcb8949a2c3e17125d4b`](../harness/evidence/sets/gate-run-aarch64-6bbb2cb7-20261118-104943-shard-2.json) |
| Playback with HEVC parameter-set repeats, ARMv7, shard 1 of 2 | `6bbb2cb7887d08585a86440dcb4722506a7e1cb6` | [`caa758a1e3348d4fccfdca6414ac504a39e8457a5fce1774402dce5bc645889e`](../harness/evidence/sets/gate-run-armv7-6bbb2cb7-20261118-105014-shard-1.json) |
| Playback with HEVC parameter-set repeats, ARMv7, shard 2 of 2 | `6bbb2cb7887d08585a86440dcb4722506a7e1cb6` | [`43ee65711daba9295f1f963771cd120531eace54aba94b9032e08fb06232b470`](../harness/evidence/sets/gate-run-armv7-6bbb2cb7-20261118-105014-shard-2.json) |
| Packet framing, Linux AArch64 | `217fef732d50953afaa89ed2b9a5422735f92936` | [`b497f5a3e66933a99802ebc821e93c680aadf9a8eec37546db0d46c85650fcfe`](../harness/evidence/sets/gate-run-aarch64-217fef73-20261119-191957-shard-5.json) |
| Packet framing, 32-bit ARMv7 | `217fef732d50953afaa89ed2b9a5422735f92936` | [`03721d8f4223237572e1d5832315767b70bfcb220a491813768d5af80cd1ae9f`](../harness/evidence/sets/gate-run-armv7-217fef73-20261119-192042-shard-5.json) |
| Packet framing, Linux AArch64 | `f6de9851d99ff3ebe621c95fabf7c89cb617a329` | [`5bc206763e41494030e26cd61cfac84d66a0fad4416ac5d5f7feeb81b8d25009`](../harness/evidence/sets/gate-run-aarch64-f6de9851-20261117-230647-shard-1.json) |
| Packet framing, ARMv7 | `f6de9851d99ff3ebe621c95fabf7c89cb617a329` | [`2c3625a5b709f4402496798fe95ddec86d59216fddf2a5e78acb12512ffac28b`](../harness/evidence/sets/gate-run-armv7-f6de9851-20261117-230647-shard-1.json) |
| Packet framing startup mappings, ARMv7 | `f6de9851d99ff3ebe621c95fabf7c89cb617a329` | [`9941dac4835e3107d1fb9a60804ca7cd3bf552635cd8b2f91b106442e87fec1f`](../harness/evidence/sets/framing-armv7-startup-residency-f6de9851.json) |
| Earlier: Packet framing, Linux AArch64 | `384de6c9347bee64e797bc3cdc49a5801ddd3f66` | [`457b9b996902aa4b5b51bc40497ffc6d02404b890f2dd3d370f1654af49fc796`](../harness/evidence/sets/gate-run-aarch64-384de6c9-20261117-193302-shard-1.json) |
| Earlier: Packet framing, ARMv7 | `384de6c9347bee64e797bc3cdc49a5801ddd3f66` | [`f804fddba7d18b70b21ace6c59465beb00f443d1514c2706caf19b8746fb1170`](../harness/evidence/sets/gate-run-armv7-384de6c9-20261117-193351-shard-1.json) |
| Gated features on both targets, ARMv7 run, shard 1 of 3 | `47aa0451e0c9511a7f75c6213a7f33d3401fc2b7` | [`ea6fc3b79c890f2907ecf887c894c1411b719747e2b90a4a8a5c426f665b87b4`](../harness/evidence/sets/gate-run-armv7-47aa0451-20261118-040605-shard-1.json) |
| Gated features on both targets, ARMv7 run, shard 2 of 3 | `47aa0451e0c9511a7f75c6213a7f33d3401fc2b7` | [`5c4fb2235ce492670904c24637d445f2905ffbb623ead2e73ffec57eb1a66e5b`](../harness/evidence/sets/gate-run-armv7-47aa0451-20261118-040605-shard-2.json) |
| Gated features on both targets, ARMv7 run, shard 3 of 3 | `47aa0451e0c9511a7f75c6213a7f33d3401fc2b7` | [`8d21689ebde93edd870a10821c623c511dbadbf725e0e778529d4105f2b3dc55`](../harness/evidence/sets/gate-run-armv7-47aa0451-20261118-040605-shard-3.json) |
| Earlier: Packet framing, Linux AArch64 | `d79071d48bc45ca5f66a922849daa15ad5a02d1c` | [`f34c5dcfd258842207b7579e0035f40e099910851ebf5814e966e1db0038a211`](../harness/evidence/sets/gate-run-aarch64-d79071d4-20261116-204903-shard-1.json) |
| Earlier: Packet framing, ARMv7 | `9870b6940b733dffcc5b63826fb785dbcfcd0027` | [`d04220edf917c952af02eaa5289777d9a6053dc4f22a9d1ff975a4158150342f`](../harness/evidence/sets/gate-run-armv7-9870b694-20261116-202556-shard-1.json) |
| Earlier: Subtitles, PGS, Linux AArch64 | `99f1027b1e6a4bddb0b1a99b84cc79756116d8cb` | [`4cd8df9c5d0652aabf435dcb8100e4984966fcca365080e1f46c84fd973f5bd1`](../harness/evidence/sets/gate-run-aarch64-99f1027b-20261116-050209-shard-1.json) |
| Earlier: Subtitles, PGS, ARMv7 | `99f1027b1e6a4bddb0b1a99b84cc79756116d8cb` | [`4c93b0902098f445ae645b18cc1df079177e3ae6bd45876e1f643f18b163b2de`](../harness/evidence/sets/gate-run-armv7-99f1027b-20261116-050516-shard-1.json) |
| Earlier: Capture, Linux AArch64, shard 1 of 5 | `001d40b60d0347fcfa74a90adf09b314fe42a13c` | [`81ffc0a2829fad7e5229dc2c0c3e4cfe7de7977e266404938d38de763751fa2e`](../harness/evidence/sets/gate-run-aarch64-001d40b6-20261118-112457-shard-1.json) |
| Earlier: Capture, Linux AArch64, shard 2 of 5 | `001d40b60d0347fcfa74a90adf09b314fe42a13c` | [`ed4a56d3052d08d73896bee340a247daa259d69cad45046a6738e210c1cb31bf`](../harness/evidence/sets/gate-run-aarch64-001d40b6-20261118-112457-shard-2.json) |
| Earlier: Capture, Linux AArch64, shard 3 of 5 | `001d40b60d0347fcfa74a90adf09b314fe42a13c` | [`7cdacefe8e434bdd6bb6340fa027203a46911ee287be5a65843249db6305ae8b`](../harness/evidence/sets/gate-run-aarch64-001d40b6-20261118-112457-shard-3.json) |
| Earlier: Capture, Linux AArch64, shard 4 of 5 | `001d40b60d0347fcfa74a90adf09b314fe42a13c` | [`27f8ab1c910345a8103e276e47e27869ed3a7f8e07b0854773a5de8df65437ab`](../harness/evidence/sets/gate-run-aarch64-001d40b6-20261118-112457-shard-4.json) |
| Earlier: Capture, Linux AArch64, shard 5 of 5 | `001d40b60d0347fcfa74a90adf09b314fe42a13c` | [`cf7c8f0b8e56f77bac684513514e66391630cf48a2915acc021e75f5c5942f79`](../harness/evidence/sets/gate-run-aarch64-001d40b6-20261118-112457-shard-5.json) |
| Earlier: Capture, 32-bit ARMv7, shard 1 of 5 | `001d40b60d0347fcfa74a90adf09b314fe42a13c` | [`b7e435a22e6a982992975bbbedeed794583983e688d68efb084760fee204b9a6`](../harness/evidence/sets/gate-run-armv7-001d40b6-20261118-112458-shard-1.json) |
| Earlier: Capture, 32-bit ARMv7, shard 2 of 5 | `001d40b60d0347fcfa74a90adf09b314fe42a13c` | [`dd24de3c318835d031849a24a25ff2ad4bde1a9baf0cfe92f1ba2856ed1dfe77`](../harness/evidence/sets/gate-run-armv7-001d40b6-20261118-112458-shard-2.json) |
| Earlier: Capture, 32-bit ARMv7, shard 3 of 5 | `001d40b60d0347fcfa74a90adf09b314fe42a13c` | [`cf20a1ad5895d44aefa373ecf42ebe4d8681a06dcaf3bd86b49e02d87aab72ac`](../harness/evidence/sets/gate-run-armv7-001d40b6-20261118-112458-shard-3.json) |
| Earlier: Capture, 32-bit ARMv7, shard 4 of 5 | `001d40b60d0347fcfa74a90adf09b314fe42a13c` | [`7d954ad14d914cbcc407b060f2e9ab7cb4bb6c6d6d427c36d1a6fc8dd705ab7e`](../harness/evidence/sets/gate-run-armv7-001d40b6-20261118-112458-shard-4.json) |
| Earlier: Capture, 32-bit ARMv7, shard 5 of 5 | `001d40b60d0347fcfa74a90adf09b314fe42a13c` | [`ac3ba1dbde7174dbc9409ad510a23c273c8d3632a6d655d775bdf91debd6bb20`](../harness/evidence/sets/gate-run-armv7-001d40b6-20261118-112458-shard-5.json) |
| Earlier: Capture, Linux AArch64, shard 1 of 5 | `a34b98d0f3a83f23a6472e7b32f4a650a62740b2` | [`cd74c2d3d4f8e58f1c350a3ab44d3a87a7708e5ef1de8a59635cf1d5497cbb51`](../harness/evidence/sets/gate-run-aarch64-a34b98d0-20261118-172006-shard-1.json) |
| Earlier: Capture, Linux AArch64, shard 2 of 5 | `a34b98d0f3a83f23a6472e7b32f4a650a62740b2` | [`61767625008844862c7385058026ee1f40d4500af41fc58beea3f4183a840f64`](../harness/evidence/sets/gate-run-aarch64-a34b98d0-20261118-172006-shard-2.json) |
| Earlier: Capture, Linux AArch64, shard 3 of 5 | `a34b98d0f3a83f23a6472e7b32f4a650a62740b2` | [`c2e203a7191af12aada5069fa9690f52d09fc57b76a3a05f3e7d6e2726d680a0`](../harness/evidence/sets/gate-run-aarch64-a34b98d0-20261118-172006-shard-3.json) |
| Earlier: Capture, Linux AArch64, shard 4 of 5 | `a34b98d0f3a83f23a6472e7b32f4a650a62740b2` | [`177e26dae0d079f070971200dcb25d831b114227e09892406314c4c965c5cd27`](../harness/evidence/sets/gate-run-aarch64-a34b98d0-20261118-172006-shard-4.json) |
| Earlier: Capture, Linux AArch64, shard 5 of 5 | `a34b98d0f3a83f23a6472e7b32f4a650a62740b2` | [`85647dfcc795cc479b7d069dffb2a53751d17c92cf6d4f91bff22cea9572ffbe`](../harness/evidence/sets/gate-run-aarch64-a34b98d0-20261118-172006-shard-5.json) |
| Earlier: Capture, 32-bit ARMv7, shard 1 of 5 | `a34b98d0f3a83f23a6472e7b32f4a650a62740b2` | [`46da8c298f4cb8fc0db538616e958f1e1ff41aa6827622a3574e6852b76b345d`](../harness/evidence/sets/gate-run-armv7-a34b98d0-20261118-172123-shard-1.json) |
| Earlier: Capture, 32-bit ARMv7, shard 2 of 5 | `a34b98d0f3a83f23a6472e7b32f4a650a62740b2` | [`446db811a403c6b0292c5206b0e4f27bf818348848fd2cb79c151eaf0c48b5e2`](../harness/evidence/sets/gate-run-armv7-a34b98d0-20261118-172123-shard-2.json) |
| Earlier: Capture, 32-bit ARMv7, shard 3 of 5 | `a34b98d0f3a83f23a6472e7b32f4a650a62740b2` | [`7e845eb83a4ecf2509164becda8a2a923cb650f8b2951cc937cc323f2faf1f88`](../harness/evidence/sets/gate-run-armv7-a34b98d0-20261118-172123-shard-3.json) |
| Earlier: Capture, 32-bit ARMv7, shard 4 of 5 | `a34b98d0f3a83f23a6472e7b32f4a650a62740b2` | [`4f8490d45c0fdcb3771c7be473b18c41dee1795ae5c8db4d3642c3de2bdf39cf`](../harness/evidence/sets/gate-run-armv7-a34b98d0-20261118-172123-shard-4.json) |
| Earlier: Capture, 32-bit ARMv7, shard 5 of 5 | `a34b98d0f3a83f23a6472e7b32f4a650a62740b2` | [`69cafbfa7ff7f62248c2ef8ad1386fcd3c4c629e419716a28d6e3885d5e72474`](../harness/evidence/sets/gate-run-armv7-a34b98d0-20261118-172123-shard-5.json) |
| Capture, Linux AArch64, shard 1 of 5 | `c30b9c80f4828eb1409e1a59bce0fe6d49668ffa` | [`4ed88bf63e5046861f80c95eaee0eb76643a9a8c12be42ac4c11d935cb09b448`](../harness/evidence/sets/gate-run-aarch64-c30b9c80-20261118-221041-shard-1.json) |
| Capture, Linux AArch64, shard 2 of 5 | `c30b9c80f4828eb1409e1a59bce0fe6d49668ffa` | [`37cc551edb5825c9b18e8fe93a40e83c08a07732aafa470106fc749ffe6a66ba`](../harness/evidence/sets/gate-run-aarch64-c30b9c80-20261118-221041-shard-2.json) |
| Capture, Linux AArch64, shard 3 of 5 | `c30b9c80f4828eb1409e1a59bce0fe6d49668ffa` | [`79ae21f4196a575160b6ebc20aed74db2934e5c2f8792d8e86e88827561f6391`](../harness/evidence/sets/gate-run-aarch64-c30b9c80-20261118-221041-shard-3.json) |
| Capture, Linux AArch64, shard 4 of 5 | `c30b9c80f4828eb1409e1a59bce0fe6d49668ffa` | [`066d38148faf6c24080b3bd98875fbde4b3a911adb9519148829700bd06ece7f`](../harness/evidence/sets/gate-run-aarch64-c30b9c80-20261118-221041-shard-4.json) |
| Capture, Linux AArch64, shard 5 of 5 | `c30b9c80f4828eb1409e1a59bce0fe6d49668ffa` | [`bb77427b1d64c94626e3e0c51c776308b8b5df22ba9ab39cbd03cf2688045949`](../harness/evidence/sets/gate-run-aarch64-c30b9c80-20261118-221041-shard-5.json) |
| Capture, 32-bit ARMv7, shard 1 of 5 | `c30b9c80f4828eb1409e1a59bce0fe6d49668ffa` | [`286619af9978dcecfa24f9d3a60de275b90eed9a172b962e682f82cd67fc50de`](../harness/evidence/sets/gate-run-armv7-c30b9c80-20261118-221127-shard-1.json) |
| Capture, 32-bit ARMv7, shard 2 of 5 | `c30b9c80f4828eb1409e1a59bce0fe6d49668ffa` | [`67ab53b52c56656d3aa2ed73745619e18bda78f8b77727ddc8d2e1eab752d685`](../harness/evidence/sets/gate-run-armv7-c30b9c80-20261118-221127-shard-2.json) |
| Capture, 32-bit ARMv7, shard 3 of 5 | `c30b9c80f4828eb1409e1a59bce0fe6d49668ffa` | [`cf199f5e369f6e94844407a931a57b3f42abee268e18acbeac7463381ce5d528`](../harness/evidence/sets/gate-run-armv7-c30b9c80-20261118-221127-shard-3.json) |
| Capture, 32-bit ARMv7, shard 4 of 5 | `c30b9c80f4828eb1409e1a59bce0fe6d49668ffa` | [`5d92b8d778cff9dd03cd9179a2eec9802f0766a14b8f1da75a34aeb270c1c215`](../harness/evidence/sets/gate-run-armv7-c30b9c80-20261118-221127-shard-4.json) |
| Capture, 32-bit ARMv7, shard 5 of 5 | `c30b9c80f4828eb1409e1a59bce0fe6d49668ffa` | [`9e6bc2f6fe5acc6cb3e794d675b68864f8f5946c067b52a656edfc160dd3e59f`](../harness/evidence/sets/gate-run-armv7-c30b9c80-20261118-221127-shard-5.json) |
| Capture, Linux AArch64, shard 1 of 5 | `e5f6c971e14b8ef43ac71edc802168926b7b0091` | [`b18f0b5e79d061fe03ac9e71ecd1cf18a29f7857d6f3a7041fa5c44e8d14c341`](../harness/evidence/sets/gate-run-aarch64-e5f6c971-20261118-172954-shard-1.json) |
| Capture, Linux AArch64, shard 4 of 5 | `e5f6c971e14b8ef43ac71edc802168926b7b0091` | [`b07cfbdd79151a8359cd9b5d3e10a141ede87559972b2bb85c5b80433a92bec0`](../harness/evidence/sets/gate-run-aarch64-e5f6c971-20261118-172954-shard-4.json) |
| Capture, Linux AArch64, shard 5 of 5 | `e5f6c971e14b8ef43ac71edc802168926b7b0091` | [`1f1e5b86572b312841749df4e6759f22028574095b74123eb8d241c9357a7519`](../harness/evidence/sets/gate-run-aarch64-e5f6c971-20261118-172954-shard-5.json) |
| Capture, Linux AArch64, shard 1 of 2 | `e5f6c971e14b8ef43ac71edc802168926b7b0091` | [`cf1c358e58f949990bff912cf3e9ab8e418db02fc37e90db86750290a9d57520`](../harness/evidence/sets/gate-run-aarch64-e5f6c971-20261118-185150-shard-1.json) |
| Capture, Linux AArch64, shard 2 of 2 | `e5f6c971e14b8ef43ac71edc802168926b7b0091` | [`23b33f6e92d06e85655799c968e14e28fe8f5865eea6383e06fcbc60ccebe5a3`](../harness/evidence/sets/gate-run-aarch64-e5f6c971-20261118-185150-shard-2.json) |
| Capture, 32-bit ARMv7, shard 1 of 5 | `e5f6c971e14b8ef43ac71edc802168926b7b0091` | [`4b1430fe845035aa34d433c8ba28a4a85676f50e3077a87aee00b2f4ccfe0572`](../harness/evidence/sets/gate-run-armv7-e5f6c971-20261118-185012-shard-1.json) |
| Capture, 32-bit ARMv7, shard 2 of 5 | `e5f6c971e14b8ef43ac71edc802168926b7b0091` | [`4046625b77739a7cbeabb3f739894da3f7938a7f0e348662bb2197a4e40659c6`](../harness/evidence/sets/gate-run-armv7-e5f6c971-20261118-185012-shard-2.json) |
| Capture, 32-bit ARMv7, shard 3 of 5 | `e5f6c971e14b8ef43ac71edc802168926b7b0091` | [`2c66fce85fc42c2a6f88dd7e231bebfb9c7e2509acbb1cdc3dc31afe745dc75b`](../harness/evidence/sets/gate-run-armv7-e5f6c971-20261118-185012-shard-3.json) |
| Capture, 32-bit ARMv7, shard 4 of 5 | `e5f6c971e14b8ef43ac71edc802168926b7b0091` | [`4737cad0d6a1be8d69b32793b66aca4e495036680d23008c48e4105c2fd672a9`](../harness/evidence/sets/gate-run-armv7-e5f6c971-20261118-185012-shard-4.json) |
| Capture, 32-bit ARMv7, shard 5 of 5 | `e5f6c971e14b8ef43ac71edc802168926b7b0091` | [`a23829f76d2e21b63dda17d13856fd9c4965aae78d875ea6608d6d24b6f01604`](../harness/evidence/sets/gate-run-armv7-e5f6c971-20261118-185012-shard-5.json) |
| Capture instruction and cycle counts, Linux AArch64 | `e5f6c971e14b8ef43ac71edc802168926b7b0091`, `a34b98d0f3a83f23a6472e7b32f4a650a62740b2` | [`8fc8c16fc49d2b47e806af0b0b1a12b21cd7486eaf2d4708d93618247d6a6c77`](../harness/evidence/sets/capture-motion-count-aarch64-e5f6c971.json) |
| Filtered capture conversion probes, Linux AArch64 | `e12863c88f5050781e20254fc8efd8cee12fac9b` | [`809f94049ee31d7b0f0ca4a619abd5842828798982ce8e406216dd742dc15c55`](../harness/evidence/sets/capture-filtered-probes-aarch64.json) |
| Capture conversion probes, 32-bit ARMv7 | `a34b98d0f3a83f23a6472e7b32f4a650a62740b2` | [`e3dfbf4a9da1d68be300175dff29845dd8ccbb36b3f09d458eb8b9c7f9ebf7a3`](../harness/evidence/sets/capture-native-widths-armv7.json) |
| Capture, Matroska and subtitle gate run, Linux AArch64, shard 1 of 5 | `f8e1c4d9136d5a43661d8f069ca0908e07cb2acc` | [`1d1edc8d28e39be46f7fd81ce3341ba288ed476569d1fd467bf0af1026cc67d8`](../harness/evidence/sets/gate-run-aarch64-f8e1c4d9-20261116-083951-shard-1.json) |
| Capture, Matroska and subtitle gate run, Linux AArch64, shard 2 of 5 | `f8e1c4d9136d5a43661d8f069ca0908e07cb2acc` | [`6a2448ea7b893cfa1f37fae38890d30139ab64ca251bc76ca1056ce4248ab35f`](../harness/evidence/sets/gate-run-aarch64-f8e1c4d9-20261116-083951-shard-2.json) |
| Capture, Matroska and subtitle gate run, Linux AArch64, shard 3 of 5 | `f8e1c4d9136d5a43661d8f069ca0908e07cb2acc` | [`d03906a29e384369a704de659412a4ce65e8039a3b2bc280acfa66c07b58681c`](../harness/evidence/sets/gate-run-aarch64-f8e1c4d9-20261116-083951-shard-3.json) |
| Capture, Matroska and subtitle gate run, Linux AArch64, shard 4 of 5 | `f8e1c4d9136d5a43661d8f069ca0908e07cb2acc` | [`797fa86301fa4d9b8ac42e826c6e1fa91873bc9a7eb9ad29aa476e7c52ded730`](../harness/evidence/sets/gate-run-aarch64-f8e1c4d9-20261116-083951-shard-4.json) |
| Capture, Matroska and subtitle gate run, Linux AArch64, shard 5 of 5 | `f8e1c4d9136d5a43661d8f069ca0908e07cb2acc` | [`2b596cc87144af2d43994fd15fb87fefc63eff4062b5e0227e4032807f999212`](../harness/evidence/sets/gate-run-aarch64-f8e1c4d9-20261116-083951-shard-5.json) |
| Capture `c06`, `c08` and `c11` gate run, Linux AArch64, shard 1 of 3 | `f8e1c4d9136d5a43661d8f069ca0908e07cb2acc` | [`677258d51aa37c14d3fdddb31089667a2c35d8ca7e30c194ab137720e1c6a47a`](../harness/evidence/sets/gate-run-aarch64-f8e1c4d9-20261116-095854-shard-1.json) |
| Capture `c06`, `c08` and `c11` gate run, Linux AArch64, shard 2 of 3 | `f8e1c4d9136d5a43661d8f069ca0908e07cb2acc` | [`3f6038c74e3357cc0730af234a1714b767b56d03bff426b64b5cee65cc4315a6`](../harness/evidence/sets/gate-run-aarch64-f8e1c4d9-20261116-095854-shard-2.json) |
| Capture `c06`, `c08` and `c11` gate run, Linux AArch64, shard 3 of 3 | `f8e1c4d9136d5a43661d8f069ca0908e07cb2acc` | [`aceab88313543f06d2c8672e62f558bf94b77270d52556fa8577786e4fff0e9e`](../harness/evidence/sets/gate-run-aarch64-f8e1c4d9-20261116-095854-shard-3.json) |
| Capture, Matroska and subtitle gate run, ARMv7, shard 1 of 5 | `f8e1c4d9136d5a43661d8f069ca0908e07cb2acc` | [`9ade3e5d4d28416e7018561d31a61989968947b301419a640623ba3effdf0e5e`](../harness/evidence/sets/gate-run-armv7-f8e1c4d9-20261116-084029-shard-1.json) |
| Capture, Matroska and subtitle gate run, ARMv7, shard 2 of 5 | `f8e1c4d9136d5a43661d8f069ca0908e07cb2acc` | [`1bbb3710a27b8b8d245327724edb5d9763380df3f7387749c9d15206c096d566`](../harness/evidence/sets/gate-run-armv7-f8e1c4d9-20261116-084029-shard-2.json) |
| Capture, Matroska and subtitle gate run, ARMv7, shard 3 of 5 | `f8e1c4d9136d5a43661d8f069ca0908e07cb2acc` | [`93267497404a699167cff41b70f85f6726cd9067f6e8820c6f29f3f616b5ed05`](../harness/evidence/sets/gate-run-armv7-f8e1c4d9-20261116-084029-shard-3.json) |
| Capture, Matroska and subtitle gate run, ARMv7, shard 4 of 5 | `f8e1c4d9136d5a43661d8f069ca0908e07cb2acc` | [`24516907992f725e77971ff6c682bb64d3a37309c827f73fd998eec0436de5b9`](../harness/evidence/sets/gate-run-armv7-f8e1c4d9-20261116-084029-shard-4.json) |
| Capture, Matroska and subtitle gate run, ARMv7, shard 5 of 5 | `f8e1c4d9136d5a43661d8f069ca0908e07cb2acc` | [`88ba32aeb33339f1bbd2c263a013382bbdaaa9e151422ae09d8d6d68b80ebd94`](../harness/evidence/sets/gate-run-armv7-f8e1c4d9-20261116-084029-shard-5.json) |
| Capture `c06`, `c08` and `c11` gate run, ARMv7, shard 1 of 3 | `f8e1c4d9136d5a43661d8f069ca0908e07cb2acc` | [`d4348b4c015bca95e3a9f5b213b2b8dbb1966c2a31489d773c07ac32755112cc`](../harness/evidence/sets/gate-run-armv7-f8e1c4d9-20261116-095918-shard-1.json) |
| Capture `c06`, `c08` and `c11` gate run, ARMv7, shard 2 of 3 | `f8e1c4d9136d5a43661d8f069ca0908e07cb2acc` | [`e386a84c79f86fdfc64aacad01d7d9554911e50e116e3501d2577b8258eae0e2`](../harness/evidence/sets/gate-run-armv7-f8e1c4d9-20261116-095918-shard-2.json) |
| Capture `c06`, `c08` and `c11` gate run, ARMv7, shard 3 of 3 | `f8e1c4d9136d5a43661d8f069ca0908e07cb2acc` | [`1664812cb40a330c02ac61963ab76d2bbb9f88f0bcbf7de4e74941af606c2085`](../harness/evidence/sets/gate-run-armv7-f8e1c4d9-20261116-095918-shard-3.json) |
| Per-case runtime and memory bounds of the capture, Matroska and subtitle gate runs, both targets | `f8e1c4d9136d5a43661d8f069ca0908e07cb2acc` | [`85c8cb5d20eb157dd4b619d7d7b9e072ca70352d9cf5ca0b257cd7948de094e5`](../harness/evidence/sets/capture-matroska-subtitle-ratios-f8e1c4d9.json) |
| Earlier: capture, Matroska and subtitle gate run, ARMv7, shard 1 of 5 | `78596bf036b30fd57217023878bda507adc2ae6d` | [`ea3791a25c7b36bf999b6bbfb578f47ad35c643bf8d9155185e2e50c11ecf131`](../harness/evidence/sets/gate-run-armv7-78596bf0-20261116-070135-shard-1.json) |
| Earlier: capture, Matroska and subtitle gate run, ARMv7, shard 2 of 5 | `78596bf036b30fd57217023878bda507adc2ae6d` | [`fce48fd70b9b74e5ab895cba3d5d942fffe0bda437b2b80d9c748c305e0305d0`](../harness/evidence/sets/gate-run-armv7-78596bf0-20261116-070135-shard-2.json) |
| Earlier: capture, Matroska and subtitle gate run, ARMv7, shard 3 of 5 | `78596bf036b30fd57217023878bda507adc2ae6d` | [`2fca2e3f9e47e0b668533f7a08b9813a63999239a33bcf6b95e9eba618eba4b4`](../harness/evidence/sets/gate-run-armv7-78596bf0-20261116-070135-shard-3.json) |
| Earlier: capture, Matroska and subtitle gate run, ARMv7, shard 4 of 5 | `78596bf036b30fd57217023878bda507adc2ae6d` | [`b3afac20737174370800fdbec4ec82dff602bfa554d4b30b029b615f32765e5f`](../harness/evidence/sets/gate-run-armv7-78596bf0-20261116-070135-shard-4.json) |
| Earlier: capture, Matroska and subtitle gate run, ARMv7, shard 5 of 5 | `78596bf036b30fd57217023878bda507adc2ae6d` | [`070e47d5a4696aedc52f353caa815e9cbdbe39126169fb06ad3fa97fc847949e`](../harness/evidence/sets/gate-run-armv7-78596bf0-20261116-070135-shard-5.json) |
| Codec and AAC items, Linux AArch64, run `061706` shard 2 | `4213d0422df8d9caae433f9340287b8a342e2824` | [`d1aef6d60dfc5011bf5051d384ab145c5211964e74d3a253c323a0d7bfd85160`](../harness/evidence/sets/gate-run-aarch64-4213d042-20261116-061706-shard-2.json) |
| Codec and AAC items, Linux AArch64, run `061706` shard 3 | `4213d0422df8d9caae433f9340287b8a342e2824` | [`bf8c813616d9e9e58351ebec251708b5f15828ecb7c64b1f1b2bbb0bb68fbe23`](../harness/evidence/sets/gate-run-aarch64-4213d042-20261116-061706-shard-3.json) |
| Codec and AAC items, Linux AArch64, run `064825` shard 1 | `4213d0422df8d9caae433f9340287b8a342e2824` | [`0fdcf9ba85fe48b3daa17b16f9e19e4de9d8dc8c5e4501b6772cb978ef912fff`](../harness/evidence/sets/gate-run-aarch64-4213d042-20261116-064825-shard-1.json) |
| Codec and AAC items, Linux AArch64, run `064825` shard 2 | `4213d0422df8d9caae433f9340287b8a342e2824` | [`10c01c39d44be7c40096817b0e9984679034d2c1d3166a1bdb01d5339ebf4419`](../harness/evidence/sets/gate-run-aarch64-4213d042-20261116-064825-shard-2.json) |
| AAC in MOV, Linux AArch64 | `4213d0422df8d9caae433f9340287b8a342e2824` | [`afe226d426979ce0574bcabcd35be00c9dde46f0539ea378a56eaca8d2923918`](../harness/evidence/sets/aac-access-units-4213d042-aarch64.json) |
| Linux AArch64: the AAC decoder's instruction counts per commit, profiles against the reference, the audio lanes' grades and errors at `e43f7a4f` and `bbe30b3b` | `bbe30b3b38cda4a8638a889b4e02ea12d2f74c30` | [`baad4cc978b41d2ad8d9bd025f199528ec9b976122a304362cce3870587bffca`](../harness/evidence/sets/aac-thumb2-speed-aarch64.json) |
| Codec and AAC items, ARMv7, run `061623` shard 2 | `4213d0422df8d9caae433f9340287b8a342e2824` | [`1bfa8d67f787a06dfbd8795927a284e08d2f7b96302f63f58d6f91abd523e71b`](../harness/evidence/sets/gate-run-armv7-4213d042-20261116-061623-shard-2.json) |
| Codec and AAC items, ARMv7, run `061623` shard 3 | `4213d0422df8d9caae433f9340287b8a342e2824` | [`1d05f3109da0dab5330fd623128411a17cf32aedc00d6c9ead8a1597e32c5fb7`](../harness/evidence/sets/gate-run-armv7-4213d042-20261116-061623-shard-3.json) |
| Codec and AAC items, ARMv7, run `064649` shard 1 | `4213d0422df8d9caae433f9340287b8a342e2824` | [`d828677c363a24d563dba8729a529e895fba855ec29999ef881b68eb99fee1ae`](../harness/evidence/sets/gate-run-armv7-4213d042-20261116-064649-shard-1.json) |
| Codec and AAC items, ARMv7, run `064649` shard 2 | `4213d0422df8d9caae433f9340287b8a342e2824` | [`4f3abc02cf65967b539edec53430981ff62c01bd19950843e3491dec234bd2ab`](../harness/evidence/sets/gate-run-armv7-4213d042-20261116-064649-shard-2.json) |
| AAC in MOV, ARMv7 | `4213d0422df8d9caae433f9340287b8a342e2824` | [`8692c15e77b04bf82168cbc560d4368b9b5048d62f50229385fa7b147492bc76`](../harness/evidence/sets/aac-access-units-4213d042-armv7.json) |
| ARMv7: the AAC decoder's instruction counts per commit, profiles against the reference, the audio lanes' grades and errors at `e43f7a4f` and `bbe30b3b` | `bbe30b3b38cda4a8638a889b4e02ea12d2f74c30` | [`d3a516818ca87b8bdedab2048ae9b96715fbd8a22c6b18c269fdfc227a72a642`](../harness/evidence/sets/aac-thumb2-speed-armv7.json) |
| Codec items, Linux AArch64, shard 1 of 3 | `c68c9667b9ae49601190c3d710fb53e0c69fd97a` | [`961d1ea064c05b40c858c0ac65414b8304c02ebde09d5e600b1325162ffc6217`](../harness/evidence/sets/gate-run-aarch64-c68c9667-20261116-082713-shard-1.json) |
| Codec items, Linux AArch64, shard 2 of 3 | `c68c9667b9ae49601190c3d710fb53e0c69fd97a` | [`e36f6ce721719369deef438ca98a69cb3d814a6638df78f116a6814ee86a0a20`](../harness/evidence/sets/gate-run-aarch64-c68c9667-20261116-082713-shard-2.json) |
| Codec items, Linux AArch64, shard 3 of 3 | `c68c9667b9ae49601190c3d710fb53e0c69fd97a` | [`9876389ab3da0ccd16482818d3a59d53de9843d63272bb73e395efa81e514c0c`](../harness/evidence/sets/gate-run-aarch64-c68c9667-20261116-082713-shard-3.json) |
| Codec items, ARMv7, shard 1 of 3 | `c68c9667b9ae49601190c3d710fb53e0c69fd97a` | [`0cdbc1629a044d5e268e9a62478e84f278cf465ba6894806fdd0e4781e8039f9`](../harness/evidence/sets/gate-run-armv7-c68c9667-20261116-082759-shard-1.json) |
| Codec items, ARMv7, shard 2 of 3 | `c68c9667b9ae49601190c3d710fb53e0c69fd97a` | [`0498bb42d62a0feba2ce6c80eed17794a76e651699bbb6e815d78da44aaa0f48`](../harness/evidence/sets/gate-run-armv7-c68c9667-20261116-082759-shard-2.json) |
| Codec items, ARMv7, shard 3 of 3 | `c68c9667b9ae49601190c3d710fb53e0c69fd97a` | [`6a0575ac1a81c332f7bda4c06fad0b5c54b4c9285d4f07d232d503129206e8ad`](../harness/evidence/sets/gate-run-armv7-c68c9667-20261116-082759-shard-3.json) |
| Codec items, Linux AArch64, shard 1 of 3 | `f9fd40300b0a14890c02ac79f14dee5aac178b03` | [`df3a5affd95c8ed204b011372184b3d62e5df5ba02182841fb4e7ee6c26b9322`](../harness/evidence/sets/gate-run-aarch64-f9fd4030-20261116-141046-shard-1.json) |
| Codec items, Linux AArch64, shard 2 of 3 | `f9fd40300b0a14890c02ac79f14dee5aac178b03` | [`48fd4261093646d5d75b9ce15b45f2ed39ab50366285c68a69e5c581a9e4437e`](../harness/evidence/sets/gate-run-aarch64-f9fd4030-20261116-141046-shard-2.json) |
| Codec items, Linux AArch64, shard 3 of 3 | `f9fd40300b0a14890c02ac79f14dee5aac178b03` | [`6b6c578a82bb826556708ad8ce741b0a395c2419a31fd2ad6add7b6f040d54a6`](../harness/evidence/sets/gate-run-aarch64-f9fd4030-20261116-141046-shard-3.json) |
| Codec items, ARMv7, shard 1 of 3 | `f9fd40300b0a14890c02ac79f14dee5aac178b03` | [`73c1fe28183e31d9bfd6e4384c4948080ead55ed0a0c7533ba50400861bf3626`](../harness/evidence/sets/gate-run-armv7-f9fd4030-20261116-140926-shard-1.json) |
| Codec items, ARMv7, shard 2 of 3 | `f9fd40300b0a14890c02ac79f14dee5aac178b03` | [`d4362b360f7e830a90b1ee49a6bef27ecabb523675d82580ab3eff31241138f1`](../harness/evidence/sets/gate-run-armv7-f9fd4030-20261116-140926-shard-2.json) |
| Codec items, ARMv7, shard 3 of 3 | `f9fd40300b0a14890c02ac79f14dee5aac178b03` | [`6a3034e5ba6228d621c5d36709d78d09dccb3a62c2cb495180425a8d8f0f6dde`](../harness/evidence/sets/gate-run-armv7-f9fd4030-20261116-140926-shard-3.json) |
| Codec items, Linux AArch64, shard 1 of 3 (nightly codec worker) | `7f32411b5519e7fb0cae303ae8eea73450b24c03` | [`dcb77231d013509b8846fa22a437b419a3bf77816ab33c38346ee19ec7016e5d`](../harness/evidence/sets/gate-run-aarch64-7f32411b-20261116-112713-shard-1.json) |
| Codec items, ARMv7, shard 1 of 3 (nightly codec worker) | `7f32411b5519e7fb0cae303ae8eea73450b24c03` | [`67c992456f5f77a91cc12fbe1faa931365276cc24141747f2dc14bde6d7f435b`](../harness/evidence/sets/gate-run-armv7-7f32411b-20261116-112546-shard-1.json) |
| H.264-linked items, Linux AArch64, shard 1 of 4 | `906d0a36a98d379412d43b5c24f5ecb304313450` | [`79e0542e81dc7a9078113ce886cd0f9f17b7d462e737efade1130df020ce5028`](../harness/evidence/sets/gate-run-aarch64-906d0a36-20261116-213413-shard-1.json) |
| H.264-linked items, Linux AArch64, shard 2 of 4 | `906d0a36a98d379412d43b5c24f5ecb304313450` | [`6c3806734509c6103e8005188a1909000b3c3ee77728c0ccc3300919213e6e04`](../harness/evidence/sets/gate-run-aarch64-906d0a36-20261116-213413-shard-2.json) |
| H.264-linked items, Linux AArch64, shard 3 of 4 | `906d0a36a98d379412d43b5c24f5ecb304313450` | [`99098fc81fd310c2358817e9a1629fe67f3e856e673af0ba9883829931ce9468`](../harness/evidence/sets/gate-run-aarch64-906d0a36-20261116-213413-shard-3.json) |
| H.264-linked items, Linux AArch64, shard 4 of 4 | `906d0a36a98d379412d43b5c24f5ecb304313450` | [`0b6ae1a935223378ec816490175f5e53b3287c9485594a4ada56617e18ed256a`](../harness/evidence/sets/gate-run-aarch64-906d0a36-20261116-213413-shard-4.json) |
| H.264-linked items, ARMv7, shard 1 of 4 | `906d0a36a98d379412d43b5c24f5ecb304313450` | [`f1a57863559b7364b8a54661ef9c392b58ddab853767adc127de8120b192aecd`](../harness/evidence/sets/gate-run-armv7-906d0a36-20261116-213256-shard-1.json) |
| H.264-linked items, ARMv7, shard 2 of 4 | `906d0a36a98d379412d43b5c24f5ecb304313450` | [`09543c8a55753a0c97dcf9469e98af314e0cab50fae00abb5fd7d492d715b287`](../harness/evidence/sets/gate-run-armv7-906d0a36-20261116-213256-shard-2.json) |
| H.264-linked items, ARMv7, shard 3 of 4 | `906d0a36a98d379412d43b5c24f5ecb304313450` | [`97bcd3aec0bdfea0643bf58c7e7886913df425eeec9629ba903fef3dafd6831c`](../harness/evidence/sets/gate-run-armv7-906d0a36-20261116-213256-shard-3.json) |
| H.264-linked items, ARMv7, shard 4 of 4 | `906d0a36a98d379412d43b5c24f5ecb304313450` | [`5d59d5f08934a38da54ce0253a414486f8b4df0ae277de4579b47467531aa511`](../harness/evidence/sets/gate-run-armv7-906d0a36-20261116-213256-shard-4.json) |
| ARMv7 and Linux AArch64: the H.264 decoder's stage account at `256aa348`, counts, profiles and tests at `7c534afb` | `7c534afbadcc2aebb31314e5281c7d5d7e0c83a1` | [`6e96b8c53eead71c0d36bbd3ce600ee87d3e8ad667317e2984453390e69ca667`](../harness/evidence/sets/h264-division-7c534afb.json) |
| AAC and downmix items, Linux AArch64, shard 1 of 4 | `81edf4c0dac9563f099a50327cab0b19f5eed01f` | [`ab4204a22e7c05d5268f579436f4d017f31bd22c68beb7e980a7a53dc78af3a9`](../harness/evidence/sets/gate-run-aarch64-81edf4c0-20261117-225329-shard-1.json) |
| AAC and downmix items, Linux AArch64, shard 2 of 4 | `81edf4c0dac9563f099a50327cab0b19f5eed01f` | [`87216601111cfbeac37ff68068bc0b07ea094973b6a518ce55b274b417ba4fc9`](../harness/evidence/sets/gate-run-aarch64-81edf4c0-20261117-225329-shard-2.json) |
| AAC and downmix items, Linux AArch64, shard 3 of 4 | `81edf4c0dac9563f099a50327cab0b19f5eed01f` | [`7164bb61eed7104978c4b85cbca0980c1027d6563680cbabd8e533fb31af5b1a`](../harness/evidence/sets/gate-run-aarch64-81edf4c0-20261117-225329-shard-3.json) |
| AAC and downmix items, Linux AArch64, shard 4 of 4 | `81edf4c0dac9563f099a50327cab0b19f5eed01f` | [`aa9e246d9f238c9c6965666d9f177541c1c690e91222337c606ddf5ca2c3a8a9`](../harness/evidence/sets/gate-run-aarch64-81edf4c0-20261117-225329-shard-4.json) |
| AAC and downmix items, ARMv7, shard 1 of 4 | `81edf4c0dac9563f099a50327cab0b19f5eed01f` | [`6b10ea23ec102660ca5761b44bbacafb7293bdbe1a4bac1a52532d7312b63ebf`](../harness/evidence/sets/gate-run-armv7-81edf4c0-20261117-225155-shard-1.json) |
| AAC and downmix items, ARMv7, shard 2 of 4 | `81edf4c0dac9563f099a50327cab0b19f5eed01f` | [`823683cdad9f0da2a439857f923df5407c406ab2066b17ebb8a7df4814b272e9`](../harness/evidence/sets/gate-run-armv7-81edf4c0-20261117-225155-shard-2.json) |
| AAC and downmix items, ARMv7, shard 3 of 4 | `81edf4c0dac9563f099a50327cab0b19f5eed01f` | [`1b6e89b55e4a7e1e40971fe72d62fcb6a22ce8fcdf065a3c1cb09047104ab371`](../harness/evidence/sets/gate-run-armv7-81edf4c0-20261117-225155-shard-3.json) |
| AAC and downmix items, ARMv7, shard 4 of 4 | `81edf4c0dac9563f099a50327cab0b19f5eed01f` | [`bcd4616b3ebe6de203e2dbb3d7587f97fd62c9dae9eae570a7f8b2d120f2c42e`](../harness/evidence/sets/gate-run-armv7-81edf4c0-20261117-225155-shard-4.json) |
| ARMv7: the HE-AAC stage account at `2a6a07a7`, counts of every commit, profiles, tests and the byte-for-byte check at `9d70b166` | `9d70b1660675a59f85e724db659a88a6da3c71f0` | [`363a5e8f251c6881072cfe165a8c0053550556b01d29594a64590164f4ada892`](../harness/evidence/sets/aac-sbr-ps-simd-armv7.json) |
| Linux AArch64: the same counts, profiles, tests and byte-for-byte check at `9d70b166` | `9d70b1660675a59f85e724db659a88a6da3c71f0` | [`6bc6f725fea8a4eba6b4a297d0a30de838ade7bc4aece09f3f31267480b7c254`](../harness/evidence/sets/aac-sbr-ps-simd-aarch64.json) |
| ARMv7: the other audio decoders' counts at `2a6a07a7` and `9d70b166`, and `vorbis-q3-5.1` profiles | `9d70b1660675a59f85e724db659a88a6da3c71f0` | [`89791000c33713eefab8eb83eb23779150c677b41cba2c728e053fe43b11467c`](../harness/evidence/sets/codec-worker-placement-armv7.json) |
| Rasteriser-linked items, Linux AArch64, shard 1 of 2 | `eaff16ba93a73b4a45499eeaa18bc4cba5c6d2c6` | [`1be93ba48986e114175381e9862831a0983090437db9ea88f43359dc80f10730`](../harness/evidence/sets/gate-run-aarch64-eaff16ba-20261117-185337-shard-1.json) |
| Rasteriser-linked items, Linux AArch64, shard 2 of 2 | `eaff16ba93a73b4a45499eeaa18bc4cba5c6d2c6` | [`8a4b9385692228384beae8cf1ae240ceb34ca7d117a26901ed08d3a5a2453486`](../harness/evidence/sets/gate-run-aarch64-eaff16ba-20261117-185337-shard-2.json) |
| Rasteriser-linked items, ARMv7, shard 1 of 2 | `eaff16ba93a73b4a45499eeaa18bc4cba5c6d2c6` | [`eccc19b86a4bc99d8fca9306ccc1555fdced1cd8150acc75b0aabd0b7c3432a4`](../harness/evidence/sets/gate-run-armv7-eaff16ba-20261117-190254-shard-1.json) |
| Rasteriser-linked items, ARMv7, shard 2 of 2 | `eaff16ba93a73b4a45499eeaa18bc4cba5c6d2c6` | [`e9aba1e10dcec702793aa07d446d763705ade2cc33e3ea9ed74a02fb5983ee78`](../harness/evidence/sets/gate-run-armv7-eaff16ba-20261117-190254-shard-2.json) |
| Linux AArch64: the `cache-keys` stage account at `2a6a07a7` and `eaff16ba` | `eaff16ba93a73b4a45499eeaa18bc4cba5c6d2c6` | [`9341333fdf92ca22f395d91ff587514d51fd68c5f6c1402793f87cf17391e95e`](../harness/evidence/sets/ass-cache-keys-stage-eaff16ba.json) |
| Linux AArch64 and ARMv7: `ass-render` and `ass-render-host` per-case runtime and memory at `eaff16ba` | `eaff16ba93a73b4a45499eeaa18bc4cba5c6d2c6` | [`bdc6aa490d98d1b1ece191719ee94454d1358ead0ac83b0eefb6f17be25331d2`](../harness/evidence/sets/ass-render-ratios-eaff16ba.json) |
| Playback items, Linux AArch64, shard 1 of 1 | `419a84935d085536b01ee174c11cbca39ba20ef6` | [`4a994c9f2cd907b4f77050933720cfa7d4f933226c781ff684ccdd79b158d12a`](../harness/evidence/sets/gate-run-aarch64-419a8493-20261117-190817-shard-1.json) |
| Playback items, ARMv7, shard 1 of 1 | `419a84935d085536b01ee174c11cbca39ba20ef6` | [`f84b48507f9e47e9e5bb4be997a9daf9e20135062c15c9c1a7367ad8b7fa8c39`](../harness/evidence/sets/gate-run-armv7-419a8493-20261117-190751-shard-1.json) |
| ARMv7 and Linux AArch64: the MP4 forward stage account at `2a6a07a7`, counts, profiles and trace identities at `419a8493` | `419a84935d085536b01ee174c11cbca39ba20ef6` | [`96aeea926560974d0ce1cd531021888aa5c84fd9c2ebfbefa8d6038d5cfa64ed`](../harness/evidence/sets/mp4-forward-step-in-place-419a8493.json) |
| Codec items, Linux AArch64, shard 1 of 3 | `0ac17828e7e7839c6779fcded191102aad09fd0b` | [`6e44268a1550ad8e66ebbe636a2fb678a8024c7315e4955f7ffdae819c3db344`](../harness/evidence/sets/gate-run-aarch64-0ac17828-20261116-062732-shard-1.json) |
| Codec items, Linux AArch64, shard 2 of 3 | `0ac17828e7e7839c6779fcded191102aad09fd0b` | [`873e03879c0fa3b269bf426f60fa6a345a938d953f50755a220e2d36793c8ef8`](../harness/evidence/sets/gate-run-aarch64-0ac17828-20261116-062732-shard-2.json) |
| Codec items, Linux AArch64, shard 3 of 3 | `0ac17828e7e7839c6779fcded191102aad09fd0b` | [`643b30a85ee3ae6f42751875f240a8a69d2954252032f4108885773d9a2126dc`](../harness/evidence/sets/gate-run-aarch64-0ac17828-20261116-062732-shard-3.json) |
| Codec items, ARMv7, shard 1 of 3 | `0ac17828e7e7839c6779fcded191102aad09fd0b` | [`6da1ebe15208fc13a73b8baa38dddcd581d325ed397d604249f407f68e6fba61`](../harness/evidence/sets/gate-run-armv7-0ac17828-20261116-062728-shard-1.json) |
| Codec items, ARMv7, shard 2 of 3 | `0ac17828e7e7839c6779fcded191102aad09fd0b` | [`137605e0efec41389c30b1cbaf8231f1cbf7876bfc1d365cf4a84f306cf30f48`](../harness/evidence/sets/gate-run-armv7-0ac17828-20261116-062728-shard-2.json) |
| Codec items, ARMv7, shard 3 of 3 | `0ac17828e7e7839c6779fcded191102aad09fd0b` | [`21f3017a2ef3916a2d3ff6f960c8dd459c78c34584497da47e976edeaef2a50f`](../harness/evidence/sets/gate-run-armv7-0ac17828-20261116-062728-shard-3.json) |
| Linux AArch64: per-vector errors against FFmpeg of the audio decoders before and after the speed changes, profiles and notes | `0ac17828e7e7839c6779fcded191102aad09fd0b` | [`3ab9b0d7fd520b687b4ffc52e23941f079bad20f8194ecf443f2fdeea4d6b917`](../harness/evidence/sets/audio-speed-0ac17828-aarch64.json) |
| ARMv7: per-vector errors against FFmpeg of the audio decoders before and after the speed changes, profiles and notes | `0ac17828e7e7839c6779fcded191102aad09fd0b` | [`2988054fd06b3213f14908b5105a956764404e2fdd399c16f38f7b36b70217b3`](../harness/evidence/sets/audio-speed-0ac17828-armv7.json) |
| ISO BMFF open and index, AArch64 part 1 of 3 | `d315f73c3b7d944442d465bed89908f230be286f` | [`e580e786653f5f4647b325ab16f4295d526814103f34fbba8e4a31fade86afa7`](../harness/evidence/sets/gate-run-aarch64-d315f73c-20261116-010313-shard-1.json) |
| ISO BMFF open and index, AArch64 part 2 of 3 | `d315f73c3b7d944442d465bed89908f230be286f` | [`997a310b2595a8490854a01a75444f78428dddc1665b61eb4ac8361f3b42919c`](../harness/evidence/sets/gate-run-aarch64-d315f73c-20261116-010313-shard-2.json) |
| ISO BMFF open and index, AArch64 part 3 of 3 | `d315f73c3b7d944442d465bed89908f230be286f` | [`a537ace87a00dfbcdbbe1b89a5d8ccd8098867d7064e15d6ab74992ed5edba67`](../harness/evidence/sets/gate-run-aarch64-d315f73c-20261116-010313-shard-3.json) |
| ISO BMFF open and index with legacy QuickTime metadata | `7b3990083d8305aa46972411f3605a483a2e4b5f` | `371475c620e1da835bba0f3fb12b6227516a5593edfef267a401bf1c3800e6fe` |
| Gate run, Linux AArch64 and ARMv7: Matroska / WebM, `matroska-demux` | `79e0ae138c71ffc57c73decc88d29ae86b1153d4` | `2ad4e9706184a803da2e6ca2fb567c915e223a9da692da801cf6c0c0dc646f79` |
| Gate run, Linux AArch64 and ARMv7: Matroska / WebM, `matroska-header-removal` | `79e0ae138c71ffc57c73decc88d29ae86b1153d4` | `51051d5173516db2f18aa374c9b2aa27583a32f69787dae96ead3a8ddac2b5af` |
| Gate run, Linux AArch64 and ARMv7: Matroska / WebM, `matroska-zlib` | `79e0ae138c71ffc57c73decc88d29ae86b1153d4` | `169ab805e391d16caddd4bb447b5c30d57f8e6278f87b04bd2bbe683eff9e9f9` |
| Gate run, Linux AArch64 and ARMv7: Matroska / WebM, `matroska-cues-preroll` | `79e0ae138c71ffc57c73decc88d29ae86b1153d4` | `ed64a0ee179cda5453e75b807668b80d2252377226d43d06da520d3e54cff891` |
| Linked codec, reader and subtitle items, Linux AArch64, shard 1 of 6 | `414b4cb5753ff0fae70fcc8194cf27e87cba1f9d` | [`e0efdf9c0795f36d0781e399a587769667d5be31cef6feb3834a1cbe013154e2`](../harness/evidence/sets/gate-run-aarch64-414b4cb5-20261118-205844-shard-1.json) |
| Linked codec, reader and subtitle items, Linux AArch64, shard 2 of 6 | `414b4cb5753ff0fae70fcc8194cf27e87cba1f9d` | [`620cb668b4bb2cee97d100e3e97450491b90448163c5093ab662ad670b6e1069`](../harness/evidence/sets/gate-run-aarch64-414b4cb5-20261118-205844-shard-2.json) |
| Linked codec, reader and subtitle items, Linux AArch64, shard 3 of 6 | `414b4cb5753ff0fae70fcc8194cf27e87cba1f9d` | [`910168a113bdb2353ed0ec1c8496297b742e1fbc4a4515f9d72351ba3239cf06`](../harness/evidence/sets/gate-run-aarch64-414b4cb5-20261118-205844-shard-3.json) |
| Linked codec, reader and subtitle items, Linux AArch64, shard 4 of 6 | `414b4cb5753ff0fae70fcc8194cf27e87cba1f9d` | [`10a1080b9751543451dce9b12a4af79109d5f59a3d45751afef371711c0ce13f`](../harness/evidence/sets/gate-run-aarch64-414b4cb5-20261118-205844-shard-4.json) |
| Linked codec, reader and subtitle items, Linux AArch64, shard 5 of 6 | `414b4cb5753ff0fae70fcc8194cf27e87cba1f9d` | [`701e598b6f0c07405c6f0c4c95a4ab4791209896ad719456cad56868554a61a6`](../harness/evidence/sets/gate-run-aarch64-414b4cb5-20261118-205844-shard-5.json) |
| Linked codec, reader and subtitle items, Linux AArch64, shard 6 of 6 | `414b4cb5753ff0fae70fcc8194cf27e87cba1f9d` | [`01d9d7ffbbfde217797787b24e2c54b1f912b63a3acf4fb0b9eeebc73ae59d25`](../harness/evidence/sets/gate-run-aarch64-414b4cb5-20261118-205844-shard-6.json) |
| AAC in MOV, Linux AArch64 | `414b4cb5753ff0fae70fcc8194cf27e87cba1f9d` | [`06c6e637c6730956ba9de5f5bdbcfc25ba84010c829b04b4b0acbec5eca94724`](../harness/evidence/sets/aac-access-units-414b4cb5-aarch64.json) |
| Linked codec, reader and subtitle items, 32-bit ARMv7, shard 1 of 6 | `414b4cb5753ff0fae70fcc8194cf27e87cba1f9d` | [`81b7a7fa491a491efec7c1c2b5349b771719c199a9fd328a5e0bf69d64ec524a`](../harness/evidence/sets/gate-run-armv7-414b4cb5-20261118-205950-shard-1.json) |
| Linked codec, reader and subtitle items, 32-bit ARMv7, shard 2 of 6 | `414b4cb5753ff0fae70fcc8194cf27e87cba1f9d` | [`027b2941ad14cc7fd7bcb12e4eb7bb928d54c7c7563606d97c88a58f723f3ee0`](../harness/evidence/sets/gate-run-armv7-414b4cb5-20261118-205950-shard-2.json) |
| Linked codec, reader and subtitle items, 32-bit ARMv7, shard 3 of 6 | `414b4cb5753ff0fae70fcc8194cf27e87cba1f9d` | [`3e49b75fd2fdc84bfc445d31ccb86138ba9f431102382090c238dc401477579f`](../harness/evidence/sets/gate-run-armv7-414b4cb5-20261118-205950-shard-3.json) |
| Linked codec, reader and subtitle items, 32-bit ARMv7, shard 4 of 6 | `414b4cb5753ff0fae70fcc8194cf27e87cba1f9d` | [`2b1c9b81b9e374ed4ff99bc3f466aafc345b6d2080cec5269967e03552d7f8b0`](../harness/evidence/sets/gate-run-armv7-414b4cb5-20261118-205950-shard-4.json) |
| Linked codec, reader and subtitle items, 32-bit ARMv7, shard 5 of 6 | `414b4cb5753ff0fae70fcc8194cf27e87cba1f9d` | [`d4a5ba1f9ea3690cc9cbd9ebf186f8f57a88bc4ab77b72002b53744ec63d4a3b`](../harness/evidence/sets/gate-run-armv7-414b4cb5-20261118-205950-shard-5.json) |
| Linked codec, reader and subtitle items, 32-bit ARMv7, shard 6 of 6 | `414b4cb5753ff0fae70fcc8194cf27e87cba1f9d` | [`f8535266a3706e538e12ea8fd14960ba3c3718c1f3e3209912ded2c1f8114145`](../harness/evidence/sets/gate-run-armv7-414b4cb5-20261118-205950-shard-6.json) |
| AAC in MOV, 32-bit ARMv7 | `414b4cb5753ff0fae70fcc8194cf27e87cba1f9d` | [`f9a66d2f6e0c4aad6f7f80078e8be84b78e617908045ff04414f28a2fb67202d`](../harness/evidence/sets/aac-access-units-414b4cb5-armv7.json) |
| Matroska and linked playback worker trace comparison | `414b4cb5753ff0fae70fcc8194cf27e87cba1f9d` | [`639e932bc47c07c96592f3e184c22c9beb0fb8a3acc151b949d6bcdcbf2c8401`](../harness/evidence/sets/matroska-playback-trace-comparison-414b4cb5.json) |
| Matroska and linked playback, Linux AArch64, shard 1 of 7 | `414b4cb5753ff0fae70fcc8194cf27e87cba1f9d` | [`41e91d53643e0369a5133cfe98ac3f5cc0bbcc26df2d9fbc76b771b235506fdf`](../harness/evidence/sets/gate-run-aarch64-414b4cb5-20261118-173154-shard-1.json) |
| Matroska and linked playback, Linux AArch64, shard 2 of 7 | `414b4cb5753ff0fae70fcc8194cf27e87cba1f9d` | [`bb76c6fbef97a181da4d13d84576b7d38ef986b19d91359ab9151207c9198a5e`](../harness/evidence/sets/gate-run-aarch64-414b4cb5-20261118-173154-shard-2.json) |
| Matroska and linked playback, Linux AArch64, shard 3 of 7 | `414b4cb5753ff0fae70fcc8194cf27e87cba1f9d` | [`a30d2a3bab92e226e45fcfe05343ef460a4abbbb6ef272a93de5f0dc57f93cac`](../harness/evidence/sets/gate-run-aarch64-414b4cb5-20261118-173154-shard-3.json) |
| Matroska and linked playback, Linux AArch64, shard 4 of 7 | `414b4cb5753ff0fae70fcc8194cf27e87cba1f9d` | [`9e162c394d0b076253ef9408887d1662ec2f8b3f8fcf01e3af324e061e80912e`](../harness/evidence/sets/gate-run-aarch64-414b4cb5-20261118-173154-shard-4.json) |
| Matroska and linked playback, Linux AArch64, shard 5 of 7 | `414b4cb5753ff0fae70fcc8194cf27e87cba1f9d` | [`b7c991c7a1c094f30c6b0f0bfcad74736225f4ac545182214feb92362d651bfa`](../harness/evidence/sets/gate-run-aarch64-414b4cb5-20261118-173154-shard-5.json) |
| Matroska and linked playback, Linux AArch64, shard 6 of 7 | `414b4cb5753ff0fae70fcc8194cf27e87cba1f9d` | [`2b6faf76aa84f72ea2a4d60289cf6756f3cb81cc6087a6708cb3c3e1e0146aae`](../harness/evidence/sets/gate-run-aarch64-414b4cb5-20261118-173154-shard-6.json) |
| Matroska and linked playback, Linux AArch64, shard 7 of 7 | `414b4cb5753ff0fae70fcc8194cf27e87cba1f9d` | [`1ea71a06504e4a06723702504d3e61b4ccbad6cbe29339be6c15b65a42eb0a0a`](../harness/evidence/sets/gate-run-aarch64-414b4cb5-20261118-173154-shard-7.json) |
| Matroska and linked playback, 32-bit ARMv7, shard 1 of 7 | `414b4cb5753ff0fae70fcc8194cf27e87cba1f9d` | [`feb554580aaaec3c5af794a671a40e2dcf0fbd614da0e27510f4e253e312355c`](../harness/evidence/sets/gate-run-armv7-414b4cb5-20261118-173135-shard-1.json) |
| Matroska and linked playback, 32-bit ARMv7, shard 2 of 7 | `414b4cb5753ff0fae70fcc8194cf27e87cba1f9d` | [`ad702f7ecf649a375777c9462c1cc6cc20c4ccd3b93dee42949bcc5e93dbc003`](../harness/evidence/sets/gate-run-armv7-414b4cb5-20261118-173135-shard-2.json) |
| Matroska and linked playback, 32-bit ARMv7, shard 3 of 7 | `414b4cb5753ff0fae70fcc8194cf27e87cba1f9d` | [`475e6933b20d5c9d35203f790d5b13437825d9f30b61b9f27091f138be79a138`](../harness/evidence/sets/gate-run-armv7-414b4cb5-20261118-173135-shard-3.json) |
| Matroska and linked playback, 32-bit ARMv7, shard 4 of 7 | `414b4cb5753ff0fae70fcc8194cf27e87cba1f9d` | [`78df7eba10d4f96c4a37999a2c40fcef0adac0d7d57a5cdbaae289b5b6462ba8`](../harness/evidence/sets/gate-run-armv7-414b4cb5-20261118-173135-shard-4.json) |
| Matroska and linked playback, 32-bit ARMv7, shard 5 of 7 | `414b4cb5753ff0fae70fcc8194cf27e87cba1f9d` | [`f0662ae2e5345d1e0d93065bac79460bb145315dbf95b1233731d5fbc17c49dd`](../harness/evidence/sets/gate-run-armv7-414b4cb5-20261118-173135-shard-5.json) |
| Matroska and linked playback, 32-bit ARMv7, shard 6 of 7 | `414b4cb5753ff0fae70fcc8194cf27e87cba1f9d` | [`e2f8aa4375971577ad3417df0858ec278c5c803069128e57dd423185429bec79`](../harness/evidence/sets/gate-run-armv7-414b4cb5-20261118-173135-shard-6.json) |
| Matroska and linked playback, 32-bit ARMv7, shard 7 of 7 | `414b4cb5753ff0fae70fcc8194cf27e87cba1f9d` | [`fb9a4722e48c33bf960d9c49bdf77d62dd0bcf8a96a0440056ef20144d2d4785`](../harness/evidence/sets/gate-run-armv7-414b4cb5-20261118-173135-shard-7.json) |
| Matroska and playback, Linux AArch64, shard 1 of 3 | `bc488ac098e95b4ab2ea81c0b07c7ffe8424ccff` | [`18fb7bac21b8dfc0bbf9c2c9be6315c0e80f0abd43a5b56b08003afe689b2cfa`](../harness/evidence/sets/gate-run-aarch64-bc488ac0-20261119-010800-shard-1.json) |
| Matroska and playback, Linux AArch64, shard 2 of 3 | `bc488ac098e95b4ab2ea81c0b07c7ffe8424ccff` | [`f7731a5249af3ffcc92ef9df429996f9df3e514cad7dd57e882f9dc27ef56229`](../harness/evidence/sets/gate-run-aarch64-bc488ac0-20261119-010800-shard-2.json) |
| Matroska and playback, Linux AArch64, shard 3 of 3 | `bc488ac098e95b4ab2ea81c0b07c7ffe8424ccff` | [`55479af8bb9975f3a64dd527d4e8d75ad814d9860882b885485d1e59fb6a2d41`](../harness/evidence/sets/gate-run-aarch64-bc488ac0-20261119-010800-shard-3.json) |
| Matroska and playback, 32-bit ARMv7, shard 1 of 3 | `bc488ac098e95b4ab2ea81c0b07c7ffe8424ccff` | [`efe90987a89e7ba92b4e15943d01aa7d01800ffa9bdba81b0848513be1b51f7b`](../harness/evidence/sets/gate-run-armv7-bc488ac0-20261119-010800-shard-1.json) |
| Matroska and playback, 32-bit ARMv7, shard 2 of 3 | `bc488ac098e95b4ab2ea81c0b07c7ffe8424ccff` | [`4df409c2e9c20df8866849f439413953215d997151f91d6b75c7647e4b79ec8a`](../harness/evidence/sets/gate-run-armv7-bc488ac0-20261119-010800-shard-2.json) |
| Matroska and playback, 32-bit ARMv7, shard 3 of 3 | `bc488ac098e95b4ab2ea81c0b07c7ffe8424ccff` | [`b36dcc6c48c5679554475a0cf9fee1f004c9f91a535a35aaec356fdc0945543a`](../harness/evidence/sets/gate-run-armv7-bc488ac0-20261119-010800-shard-3.json) |
| Playback startup and continuation, Linux AArch64, shard 1 of 2 | `5514c6a0f0118bcbaecabd1e8ac35f9b8597d094` | [`99aca49de0e58a0a686626b53f99f4072e561760cb48e82c3ac4ccb1d71b8843`](../harness/evidence/sets/gate-run-aarch64-5514c6a0-20261119-030834-shard-1.json) |
| Playback startup and continuation, Linux AArch64, shard 2 of 2 | `5514c6a0f0118bcbaecabd1e8ac35f9b8597d094` | [`39e6de5f164f2348b02f79c22ba1359691f2f390d9cafa1c4b4a658c728f2c6b`](../harness/evidence/sets/gate-run-aarch64-5514c6a0-20261119-030834-shard-2.json) |
| Playback startup and continuation, 32-bit ARMv7, shard 1 of 2 | `5514c6a0f0118bcbaecabd1e8ac35f9b8597d094` | [`d38be11c82dfa02587f849970ce4d0822ea477ac9047ac464607183cf3bbffd2`](../harness/evidence/sets/gate-run-armv7-5514c6a0-20261119-030651-shard-1.json) |
| Playback startup and continuation, 32-bit ARMv7, shard 2 of 2 | `5514c6a0f0118bcbaecabd1e8ac35f9b8597d094` | [`c18114051ed24c0ccfb1312bf639fe123d062bc52e07a695304692265759038b`](../harness/evidence/sets/gate-run-armv7-5514c6a0-20261119-030651-shard-2.json) |
| Matroska and linked worker gates with the CELT changes, Linux AArch64, shard 1 of 8 | `2fc2ebb8b30ba97544c9a07a4111fecaa0695c13` | [`3554e145a92af3bedfdf0e3df38e94430cec2f3e0c4ed0834a357f8b8b8e9e27`](../harness/evidence/sets/gate-run-aarch64-2fc2ebb8-20261119-164526-shard-1.json) |
| Matroska and linked worker gates with the CELT changes, Linux AArch64, shard 2 of 8 | `2fc2ebb8b30ba97544c9a07a4111fecaa0695c13` | [`183c439e8adcd51efe2f93be5727ea90c78bdb2b27551e2354f3841d45036df7`](../harness/evidence/sets/gate-run-aarch64-2fc2ebb8-20261119-164526-shard-2.json) |
| Matroska and linked worker gates with the CELT changes, Linux AArch64, shard 3 of 8 | `2fc2ebb8b30ba97544c9a07a4111fecaa0695c13` | [`2ee2890c10e6ec7f8ff905c2cf39f6c02d1870c5554efcda615135a50aa19f07`](../harness/evidence/sets/gate-run-aarch64-2fc2ebb8-20261119-164526-shard-3.json) |
| Matroska and linked worker gates with the CELT changes, Linux AArch64, shard 4 of 8 | `2fc2ebb8b30ba97544c9a07a4111fecaa0695c13` | [`a9a31549a23e41749ba8b9c4d6a897d15e73485182b2eae1f1a6530e61227343`](../harness/evidence/sets/gate-run-aarch64-2fc2ebb8-20261119-164526-shard-4.json) |
| Matroska and linked worker gates with the CELT changes, Linux AArch64, shard 5 of 8 | `2fc2ebb8b30ba97544c9a07a4111fecaa0695c13` | [`b74d4b5a14743cc10ad850488c019387f23c763c2e3c1f66a23681c56ee1bc8d`](../harness/evidence/sets/gate-run-aarch64-2fc2ebb8-20261119-164526-shard-5.json) |
| Matroska and linked worker gates with the CELT changes, Linux AArch64, shard 6 of 8 | `2fc2ebb8b30ba97544c9a07a4111fecaa0695c13` | [`53154ed1690fd27a4e654345c0c4e8bdb94c03c923a25e6c8a322aebd1d6c28b`](../harness/evidence/sets/gate-run-aarch64-2fc2ebb8-20261119-164526-shard-6.json) |
| Matroska and linked worker gates with the CELT changes, Linux AArch64, shard 7 of 8 | `2fc2ebb8b30ba97544c9a07a4111fecaa0695c13` | [`6cdcfb78c2eee65d360df423c2a753e28d5ec06d6e68fe792c9ce820daf93bcb`](../harness/evidence/sets/gate-run-aarch64-2fc2ebb8-20261119-164526-shard-7.json) |
| Matroska and linked worker gates with the CELT changes, Linux AArch64, shard 8 of 8 | `2fc2ebb8b30ba97544c9a07a4111fecaa0695c13` | [`6ef4db523328f2418f9bcfc4dc3e8aa68f0faf1e16e0017c53ce2dd403fc4ed0`](../harness/evidence/sets/gate-run-aarch64-2fc2ebb8-20261119-164526-shard-8.json) |
| Matroska and linked worker gates with the CELT changes, 32-bit ARMv7, shard 1 of 8 | `2fc2ebb8b30ba97544c9a07a4111fecaa0695c13` | [`fc80320ae545c9a1b15e3b252ba2fd3e09d2ff9bc9a477ce636c1826472ced4b`](../harness/evidence/sets/gate-run-armv7-2fc2ebb8-20261119-164357-shard-1.json) |
| Matroska and linked worker gates with the CELT changes, 32-bit ARMv7, shard 2 of 8 | `2fc2ebb8b30ba97544c9a07a4111fecaa0695c13` | [`bde19dadb546c143c6b6723460f6b02fe8227dd6919ca265493ec93ca521ea74`](../harness/evidence/sets/gate-run-armv7-2fc2ebb8-20261119-164357-shard-2.json) |
| Matroska and linked worker gates with the CELT changes, 32-bit ARMv7, shard 3 of 8 | `2fc2ebb8b30ba97544c9a07a4111fecaa0695c13` | [`c8f0159a8595390ac0a701684d2ab6fc6e210c55f1f1c7bcd4ea2f1ffa141a29`](../harness/evidence/sets/gate-run-armv7-2fc2ebb8-20261119-164357-shard-3.json) |
| Matroska and linked worker gates with the CELT changes, 32-bit ARMv7, shard 4 of 8 | `2fc2ebb8b30ba97544c9a07a4111fecaa0695c13` | [`613bb9049848d455a43b34183b6a7ad850024842b76ea3e2fda8af6251daa985`](../harness/evidence/sets/gate-run-armv7-2fc2ebb8-20261119-164357-shard-4.json) |
| Matroska and linked worker gates with the CELT changes, 32-bit ARMv7, shard 5 of 8 | `2fc2ebb8b30ba97544c9a07a4111fecaa0695c13` | [`172a9b6a13e07ba357c7c18d7acb2836c3c836a22b0c79a843809a7201675fc9`](../harness/evidence/sets/gate-run-armv7-2fc2ebb8-20261119-164357-shard-5.json) |
| Matroska and linked worker gates with the CELT changes, 32-bit ARMv7, shard 6 of 8 | `2fc2ebb8b30ba97544c9a07a4111fecaa0695c13` | [`70061eaea219173acee5d6c540d7dafaaf347003f12e710b190efb505239bf8c`](../harness/evidence/sets/gate-run-armv7-2fc2ebb8-20261119-164357-shard-6.json) |
| Matroska and linked worker gates with the CELT changes, 32-bit ARMv7, shard 7 of 8 | `2fc2ebb8b30ba97544c9a07a4111fecaa0695c13` | [`914074689464e88c8e28c74f45969860a108e06e7c3ecd0688cef7600f973d7f`](../harness/evidence/sets/gate-run-armv7-2fc2ebb8-20261119-164357-shard-7.json) |
| Matroska and linked worker gates with the CELT changes, 32-bit ARMv7, shard 8 of 8 | `2fc2ebb8b30ba97544c9a07a4111fecaa0695c13` | [`1f8225e238e0ce543d069df8e0b8ddab8d28f0c282d9564afd0ef35ce32f0f5a`](../harness/evidence/sets/gate-run-armv7-2fc2ebb8-20261119-164357-shard-8.json) |
| Linked worker gates with the PGS changes, Linux AArch64, shard 1 of 4 | `47ffada46db5406b434e37ff4e5ab01a031365a9` | [`a009a844ff01354b5d7efdcea03412d7df012cff0edcca194d5bb339c8e387a6`](../harness/evidence/sets/gate-run-aarch64-47ffada4-20261119-171530-shard-1.json) |
| Linked worker gates with the PGS changes, Linux AArch64, shard 2 of 4 | `47ffada46db5406b434e37ff4e5ab01a031365a9` | [`d5cd3f2af53935afd3f6a3126f2bada9e410ca5ddf1ffa9d426b6271dd2142cd`](../harness/evidence/sets/gate-run-aarch64-47ffada4-20261119-171530-shard-2.json) |
| Linked worker gates with the PGS changes, Linux AArch64, shard 3 of 4 | `47ffada46db5406b434e37ff4e5ab01a031365a9` | [`b4af68e163203cd9e1415e69b85df075100e33386e3efc56b034cf34e16cf772`](../harness/evidence/sets/gate-run-aarch64-47ffada4-20261119-171530-shard-3.json) |
| Linked worker gates with the PGS changes, Linux AArch64, shard 4 of 4 | `47ffada46db5406b434e37ff4e5ab01a031365a9` | [`dd8ac35f34cf6906f5fc1cbb79f771eb8060190e3c078fd5a03f815636f4ef66`](../harness/evidence/sets/gate-run-aarch64-47ffada4-20261119-171530-shard-4.json) |
| Linked worker gates with the PGS changes, 32-bit ARMv7, shard 1 of 4 | `47ffada46db5406b434e37ff4e5ab01a031365a9` | [`ee3ac21004334fa58b6910a413ee1450732e6ba60b924effd463782122fd6b0c`](../harness/evidence/sets/gate-run-armv7-47ffada4-20261119-171532-shard-1.json) |
| Linked worker gates with the PGS changes, 32-bit ARMv7, shard 2 of 4 | `47ffada46db5406b434e37ff4e5ab01a031365a9` | [`47a1d2641925a0901435e48bc512143a2a8e525d0b27f0915bedf00d4f55d438`](../harness/evidence/sets/gate-run-armv7-47ffada4-20261119-171532-shard-2.json) |
| Linked worker gates with the PGS changes, 32-bit ARMv7, shard 3 of 4 | `47ffada46db5406b434e37ff4e5ab01a031365a9` | [`0bbae8d22096cba9b07e5632342590114b5477e9402337cce51cd663eca29dbb`](../harness/evidence/sets/gate-run-armv7-47ffada4-20261119-171532-shard-3.json) |
| Linked worker gates with the PGS changes, 32-bit ARMv7, shard 4 of 4 | `47ffada46db5406b434e37ff4e5ab01a031365a9` | [`6f380a46f3a2461d4a6a0c17c3b39b6f82745a7e68df1f494a19d05cdb4103be`](../harness/evidence/sets/gate-run-armv7-47ffada4-20261119-171532-shard-4.json) |
| Playback startup and continuation, Linux AArch64, shard 1 of 4 | `ce55904b842a6ddad77e90688a8ec4528438f576` | [`1d9f5ed54f07d6b9e0c75bf05e63b5358d0b3464993f628b661fa9ffbea512a1`](../harness/evidence/sets/gate-run-aarch64-ce55904b-20261119-194114-shard-1.json) |
| Playback startup and continuation, Linux AArch64, shard 2 of 4 | `ce55904b842a6ddad77e90688a8ec4528438f576` | [`e5df70e8ec1b06c3c3fdfbc92d0d9fdca1e3e54b252c6c86ca2ff869883763e8`](../harness/evidence/sets/gate-run-aarch64-ce55904b-20261119-194114-shard-2.json) |
| Playback startup and continuation, Linux AArch64, shard 3 of 4 | `ce55904b842a6ddad77e90688a8ec4528438f576` | [`8d7f7af8ed4ba98344ee6206ca3f2358d4a4d907faabd4203233f5a2057307e8`](../harness/evidence/sets/gate-run-aarch64-ce55904b-20261119-194114-shard-3.json) |
| Playback startup and continuation, Linux AArch64, shard 4 of 4 | `ce55904b842a6ddad77e90688a8ec4528438f576` | [`72054b872308268d1e15d63fa903fa92d6d1ab09f0a2b3039e4b3375a0d749ed`](../harness/evidence/sets/gate-run-aarch64-ce55904b-20261119-194114-shard-4.json) |
| Playback startup and continuation, 32-bit ARMv7, shard 1 of 4 | `ce55904b842a6ddad77e90688a8ec4528438f576` | [`aa851af69e1b3514da32811692fb2c01fbe3ccc512b2ce3056f3e85f7edcfaf9`](../harness/evidence/sets/gate-run-armv7-ce55904b-20261119-194218-shard-1.json) |
| Playback startup and continuation, 32-bit ARMv7, shard 2 of 4 | `ce55904b842a6ddad77e90688a8ec4528438f576` | [`7eb7106330c6cada3fbf690f3299497f1571d1a955b8525bd8b4458e7add5dc9`](../harness/evidence/sets/gate-run-armv7-ce55904b-20261119-194218-shard-2.json) |
| Playback startup and continuation, 32-bit ARMv7, shard 3 of 4 | `ce55904b842a6ddad77e90688a8ec4528438f576` | [`7b27620324781b51b163b02e6725ac59b350195c8d313fa3322bb50a20db1df7`](../harness/evidence/sets/gate-run-armv7-ce55904b-20261119-194218-shard-3.json) |
| Playback startup and continuation, 32-bit ARMv7, shard 4 of 4 | `ce55904b842a6ddad77e90688a8ec4528438f576` | [`b883a2013704a84d68ae1482699b9e599eb051b76167633266673aae33aa2d1f`](../harness/evidence/sets/gate-run-armv7-ce55904b-20261119-194218-shard-4.json) |
| Opus CELT counts, PCM comparisons and profiles on Linux AArch64 | `31d2994522f116b4404916cb51f82b03a29c158f`, `5bacc1387b475e35a7babb98664cba227db69fe4`, `19ad7c0cb92dcd8f20cf2bb2e459dd785ba3fa2f` | [`1b1e2a6965261ad3310ef4101a0e57a1a913d6bf771c2f533b3005bbf8fcc73e`](../harness/evidence/sets/opus-celt-profile.json) |
| Matroska, playback and linked workers, Linux AArch64, shard 1 of 6 | `5bacc1387b475e35a7babb98664cba227db69fe4` | [`32a4dc5580031b94a510d9103a5e9475458126e87369dbf922275f2c2f92a4d9`](../harness/evidence/sets/gate-run-aarch64-5bacc138-20261119-095551-shard-1.json) |
| Matroska, playback and linked workers, Linux AArch64, shard 2 of 6 | `5bacc1387b475e35a7babb98664cba227db69fe4` | [`d30c4a0c1d0b6ed02caf9c1bbacb5a2e63d8c6476911c81ea3a1db60436f5897`](../harness/evidence/sets/gate-run-aarch64-5bacc138-20261119-095551-shard-2.json) |
| Matroska, playback and linked workers, Linux AArch64, shard 3 of 6 | `5bacc1387b475e35a7babb98664cba227db69fe4` | [`e6053f887a03a8c1703c30fdbedbcfbf1f4207687c269ea93b30b05fca13ec81`](../harness/evidence/sets/gate-run-aarch64-5bacc138-20261119-095551-shard-3.json) |
| Matroska, playback and linked workers, Linux AArch64, shard 4 of 6 | `5bacc1387b475e35a7babb98664cba227db69fe4` | [`f305431f00fd3a98851fc4d1d62b9933fb40cd538170c8fa186e7beb932d2323`](../harness/evidence/sets/gate-run-aarch64-5bacc138-20261119-095551-shard-4.json) |
| Matroska, playback and linked workers, Linux AArch64, shard 5 of 6 | `5bacc1387b475e35a7babb98664cba227db69fe4` | [`1d437fc0133460108105087741cbf16b0cbc04cc3b2e8f3a31d5a1478ac5dc5f`](../harness/evidence/sets/gate-run-aarch64-5bacc138-20261119-095551-shard-5.json) |
| Matroska, playback and linked workers, Linux AArch64, shard 6 of 6 | `5bacc1387b475e35a7babb98664cba227db69fe4` | [`93cb3c17e93a0d89a983fa76e5baaa757bcc431b68f00099b1fd492025d814ba`](../harness/evidence/sets/gate-run-aarch64-5bacc138-20261119-095551-shard-6.json) |
| Matroska, playback and linked workers, 32-bit ARMv7, shard 1 of 6 | `5bacc1387b475e35a7babb98664cba227db69fe4` | [`1780fc5e54a0c1ad0f0e46bfe99b45d5c4a04cf8ad907faddb5fc53d8cde42ca`](../harness/evidence/sets/gate-run-armv7-5bacc138-20261119-095542-shard-1.json) |
| Matroska, playback and linked workers, 32-bit ARMv7, shard 2 of 6 | `5bacc1387b475e35a7babb98664cba227db69fe4` | [`30bec5c399636f7c8897258b3f81a536219cad9922ebd93f5385958456bad2c5`](../harness/evidence/sets/gate-run-armv7-5bacc138-20261119-095542-shard-2.json) |
| Matroska, playback and linked workers, 32-bit ARMv7, shard 3 of 6 | `5bacc1387b475e35a7babb98664cba227db69fe4` | [`70365797dbeb19e725876c00b2ff2c9a1c4df1199e288a1a825f920f398a5417`](../harness/evidence/sets/gate-run-armv7-5bacc138-20261119-095542-shard-3.json) |
| Matroska, playback and linked workers, 32-bit ARMv7, shard 4 of 6 | `5bacc1387b475e35a7babb98664cba227db69fe4` | [`c93158be57ea9a3c45d81cd25f1517f38b6d2d223bd98b6109d656ac538874fc`](../harness/evidence/sets/gate-run-armv7-5bacc138-20261119-095542-shard-4.json) |
| Matroska, playback and linked workers, 32-bit ARMv7, shard 5 of 6 | `5bacc1387b475e35a7babb98664cba227db69fe4` | [`1a13e0348e4cdd11d51c508ae3d5fd2585eb899c69592fbbe4f5508d413f4af8`](../harness/evidence/sets/gate-run-armv7-5bacc138-20261119-095542-shard-5.json) |
| Matroska, playback and linked workers, 32-bit ARMv7, shard 6 of 6 | `5bacc1387b475e35a7babb98664cba227db69fe4` | [`412c09c412df5fb642e5aa76090273f8b7089e84e14febe62917dc4402278136`](../harness/evidence/sets/gate-run-armv7-5bacc138-20261119-095542-shard-6.json) |
| Matroska Cues and preroll, Linux AArch64 | `be14c7e6ce5c08c46d13d42f7f23e4b30289cef2` | [`74d75e4440f94e8ec1c3847cee3bf089ae09f0208871bb48bc5edfd627c08ab9`](../harness/evidence/sets/gate-run-aarch64-be14c7e6-20261118-174935-shard-1.json) |
| Matroska Cues and preroll, 32-bit ARMv7 | `be14c7e6ce5c08c46d13d42f7f23e4b30289cef2` | [`93cdf4d1f8c7cc71c051abac41477985a1a526f4c175f2e8f396a7cd50ec0acf`](../harness/evidence/sets/gate-run-armv7-be14c7e6-20261118-174904-shard-1.json) |
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
| DTS header instruction counts and seven-input packet comparison, Linux AArch64 | `7c730ff883cc4e50f43d5e34fed779716b0319d2` | [`c3c6541a5415dfb973a8f81df88a6b188b0e5010e151472a1f1e93aeeeefe377`](../harness/evidence/sets/dts-owned-count-7c730ff8-aarch64.json) |
| DTS header instruction counts, 32-bit ARMv7 | `7c730ff883cc4e50f43d5e34fed779716b0319d2` | [`a79951e5e280b5435d52a4d8e51ef2bb431501a03b8527a36be140de82db5f43`](../harness/evidence/sets/dts-owned-count-7c730ff8-armv7.json) |
| DTS header fields, per-packet core lengths and bytes, Linux AArch64 | `24ca7441413fc69b5d755d38300c2f3afb0561f9` | `43a23f4f819fc44df873353e142a4738d4a8d5a4450aed9c3050523c123974cd` |
| DTS header fields and concatenated core bytes, Linux AArch64 | `9784a307d8b8e89c7a80d47f97d397d23a760768` | `ff1adcc68ed11f0beb8386d0d927810a974de530b099e24067a61aea6b69df10` |
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
| Gate run, Linux AArch64: subtitle worker, part 1 | `97759e39b2f02611b5d22ae39f97547303198cac` | `876bd12f93e348403d04f8ce68c6a89f656cd9b9b42748c0b29b67f68e93d96b` |
| Gate run, Linux AArch64: subtitle worker, part 2 | `97759e39b2f02611b5d22ae39f97547303198cac` | `a72dc94e64a249464ab0d8853b2ac9d60d6feff66d8f598d7077cf5ebc79c2ad` |
| Gate run, Linux AArch64: subtitle worker, part 3 | `97759e39b2f02611b5d22ae39f97547303198cac` | `83f2cbcb5efeeddd48c1f1742eee9d1a0a9375f591ad0b4afb3be180327f6b79` |
| Gate run, 32-bit ARMv7: subtitle worker, part 1 | `97759e39b2f02611b5d22ae39f97547303198cac` | `6ae08e51ae03e9bb324a62ea1868f690ab15a9eb958df6085e0dc69797c886b0` |
| Gate run, 32-bit ARMv7: subtitle worker, part 2 | `97759e39b2f02611b5d22ae39f97547303198cac` | `8add82a4f8b0f7597ca7db5779add68f6ad75ef45156282ea602ac11c834904a` |
| Gate run, 32-bit ARMv7: subtitle worker, part 3 | `97759e39b2f02611b5d22ae39f97547303198cac` | `50a150d439da7cbf53656ddc7eddd1004ad28c9fb7347b257dc6432480a8b121` |
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

Qualification runs on native AArch64 Linux and on 32-bit ARMv7 Linux: every feature the
gate runs requires both, and the next section says what is measured on ARMv7. x86_64 has
no performance runs yet.

### 32-bit ARMv7

The gate build for `thumbv7neon-unknown-linux-gnueabihf` (Rust 1.99.1, default flags:
Thumb-2, assuming NEON; `codec-aac-worker` and the AAC decode example by the pinned
nightly with codec-aac's `simd` feature, as on Linux AArch64), with every FFmpeg, FreeType, HarfBuzz, FriBidi and
libass reference built from the same pins in the same user space (Thumb-2, the armhf
compiler's default; FFmpeg selects NEON at run time). The Ubuntu 24.04 armhf user space
archived as `armv7-userland` ([how to repeat it](quality.md#32-bit-armv7)), with the 15
packages libzvbi's build adds (autoconf, automake, autopoint, libtool, gettext and what they
depend on), on AWS c6g.4xlarge hosts (Graviton2, Neoverse N1 r3p1), kernel
`7.0.0-1014-aws`. Unless a row names another revision, its runtime and memory come from the
full gate run at `a73350ca`, and AAC in MOV from its measurements at that revision
([measured revisions](#measured-revisions)): each feature timed on CPU 1, then its memory
collected in parallel on CPUs 1–15 ([memory in parallel](quality.md#memory-in-parallel)).
The workspace and worker tests were run at `f8e1c4d9`, the rasterising, shaping and ASS
parsing comparisons at `3df7ae6d`, and the font and shaping comparisons at `69d4d0f8`, on c6g.4xlarge hosts with the same user
space and kernel.
Memory is kernel-bound in every row: measured under the kernel RSS bound, through its
32-bit entry point, with both workers' cached pages dropped before every run
([quality](quality.md#measuring-performance)), so total RSS is the candidate's proven
resident peak over the reference's observed peak. The codec bounds are
gated as on AArch64, and no codec workload is open on this target.

| Area | Verified against the reference | Runtime | Memory |
|---|---|---|---|
| Core | 51,235 exact-arithmetic cases; the workspace tests (4,719 checked, 4,718 in release) and the worker tests pass at `f8e1c4d9` | via dependents | via dependents |
| Matroska / WebM | 129/129 across six feature groups and the external HDR vector, including 21/21 Cues cases | 0.9706 (129/129 pass, worst `cues-sparse`) at `2fc2ebb8` | heap ≤0.7747, total RSS ≤0.9463 (129/129 lower) at `2fc2ebb8`
| Matroska legacy AAC-LC | 4/4 (`matroska-legacy-aac`) | 0.8463 (4/4 pass, worst `mpeg2-lc-laced`) at `deb6105a` | heap ≤0.4763, total RSS ≤0.9726 (4/4 lower) at `deb6105a` |
| Matroska startup and seek | 45/45 | 0.8340 (45/45 pass, worst `seek-hevc-seek-no-cues-0-fresh`) at `deb6105a` | heap ≤0.3957, total RSS ≤0.7827 (45/45 lower) at `deb6105a` |
| HLS segments | 71/71 | 0.5690 (71/71 pass, worst `dvb-hevc-eac3-32000`) at `deb6105a` | heap ≤0.0197, total RSS ≤0.8006 (71/71 lower) at `deb6105a` |
| ISO BMFF / MP4 | 1,020/1,020 open and index comparisons (972 matching outputs, 48 matching refusals); the neutral reader's 648 comparison cases, as on AArch64 | 0.9674 (956/956 pass, worst `order27-u64-last-scale1000-cat21-native-index-tiny`) at `deb6105a` | heap ≤0.2467, total RSS ≤0.8190 (956/956 lower) at `deb6105a` |
| ISO BMFF seek and range plans | 476/476 (378 seeks, 94 range plans, 4 refusals) | 1.0179 (472/472 pass, worst `origin18-zero-anchor-second-absent-t1-mid-seek-tiny`) at `deb6105a` | heap ≤0.8542, total RSS ≤0.8263 (472/472 lower) at `deb6105a` |
| AAC in MOV | the configured decoder comparison: 34 clips, 3,544 access units | 1.0074 (34/34 pass) at `414b4cb5` | heap ≤0.6756, total RSS ≤0.5230 (34/34 lower, kernel-bound) at `414b4cb5` |
| Packet framing | 135/135 AVC/HEVC reference comparisons. [DTS header instruction counts](#dts-core-header-inspection) at `7c730ff8`. | 0.9100 (135/135 pass for AVC/HEVC, worst `hevc-irap_repeat`) at `deb6105a` | heap ≤0.2633 (135/135 lower), total RSS ≤1.0888 (0/135 lower) at `deb6105a` |
| Codecs | 347/347 comparable H.264 and HEVC conformance vectors (of 383 run, 31 are outside the supported profiles and 5 are refused by both); the audio correctness lanes as gate item `codecs-audio` at `81edf4c0`, as on AArch64: AAC 38/39 and one open case (`aac-sce-in-stereo`, FFmpeg's unwritten FR plane), AC-3 6/6, E-AC-3 6/6, DTS core 22/22, and `xll_51_24_48_none` out of set (no core substream); the six video timing workloads decode to FFmpeg's digests | 20/20 pass at `81edf4c0`: the six video workloads 0.6035 to 1.0447 (`h264-high-1080p` 1.0447), the fourteen audio workloads 0.7665 to 1.0182 (worst `aac-he-5.1`; `aac-hev2-stereo` 1.0163, `aac-he-stereo` 1.0032) | heap ≤0.9421, total RSS ≤0.8384 (20/20 lower) at `81edf4c0` |
| Audio codecs in files | the 10 workloads of `file-workloads.json`, each worker's work checked equal before it is measured | 0.9820 (10/10 pass, worst `vorbis-q3-5.1`) at `deb6105a` | heap ≤0.8995, total RSS ≤0.7725 (10/10 lower) at `deb6105a` |
| Capture | 15/15 at `deb6105a`. Conversion is byte-identical to FFmpeg's on all 15, and mux and encoder pass. | 0.9988 (15/15 pass, worst `c14-motion-960-rate255`) at `deb6105a` | heap ≤0.6087, total RSS ≤0.9766 (15/15 lower) at `deb6105a` |
| Matroska audio clocks | 10/10 packet folds (`matroska-audio-clocks`) | 0.7932 (10/10 pass, worst `laced-unknown-duration`) at `deb6105a` | heap ≤0.7324, total RSS ≤0.9434 (10/10 lower) at `deb6105a` |
| MPEG-TS | 97/97 compat-view operations (`mpegts-pes`) | 0.8620 (97/97 pass) at `273f6992` | heap ≤0.0902, total RSS ≤0.8716 (97/97 lower) at `273f6992` |
| MPEG-TS access units | 169/169 transport compatibility folds (`mpegts-access-units`) | 1.0148 (169/169 pass, worst `au-dvb-eac3-32000`) at `deb6105a` | heap ≤0.1626, total RSS ≤0.8930 (169/169 lower) at `deb6105a` |
| Playback: MP4 forward | 46/46 packet folds (`mp4-forward`) | 0.9116 (51/51 pass, worst `progressive-ten-sequential-batches`) at `deb6105a` | heap ≤0.5920, total RSS ≤0.9773 (51/51 lower) at `deb6105a` |
| Playback: MP4 range | 51/51 open and backward-seek comparisons (`mp4-range`) | 0.9976 (64/64 pass, worst `fragmented-before-sync`) at `deb6105a` | heap ≤0.7849, total RSS ≤0.9186 (64/64 lower) at `deb6105a` |
| Playback: TS program | 35/35 packet folds (`ts-program`) | 0.7692 (35/35 pass, worst `entry-hevc-aac-44100-00`) at `deb6105a` | heap ≤0.1180, total RSS ≤0.8606 (35/35 lower) at `deb6105a` |
| MPEG-TS progressive playback | 3 packet folds and all 5 functional inputs with 10 literal outcomes | 0.7495 (3/3 pass, worst `reordered-48000`) at `deb6105a` | heap ≤0.0155, total RSS ≤0.6948 (3/3 lower) at `deb6105a` |
| Playback: raw Annex B | 7/7 packet folds (`raw-annex-b`) | 0.7482 (7/7 pass, worst `hevc-aud1`) at `deb6105a` | heap ≤0.1265, total RSS ≤0.7870 (7/7 lower) at `deb6105a` |
| Rasterising | `raster-glyphs` 12/12, `raster-outlines` 2/2 and `raster-bitmaps` 2/2 pass; `raster-sweep` 301/303 (below); the crate's tests pass | not measured | not measured |
| ASS parsing | `ass-parse-scripts` 48/48, `ass-parse-chunks` 4/4 and `ass-parse-events` 21/21 pass; `ass-parse-host` 13/15 (2 open); the crate's tests pass | not measured | not measured |
| Fonts | `font-outlines` 53/53, `font-hinting-none` 159/159 and `font-hinting-default` 159/159 at `69d4d0f8`, as on AArch64 | not measured | not measured |
| Shaping | `shape-libass` 209/209 and `shape-text` 209/209 at `69d4d0f8`, `shape-bidi` 24/24, `shape-simple` 6/6, `shape-unicode` 1/1, `shape-extents` 9/9 | `shape-extents` 1.0627 (7/9 pass, worst `notosansjp-regular-otf-extents`) at `deb6105a`; other shaping workloads unmeasured against their references | `shape-extents` heap ≤19.9978 (1/9 lower), total RSS ≤0.9916 (9/9 lower) at `deb6105a`; other shaping workloads unmeasured against their references |
| ASS rendering | `ass-render` 100/100 and `ass-render-host` 4/4 pass; the crate's tests pass | `ass-render` 0.8002 (100/100 pass), `ass-render-host` 0.7315 (4/4 pass) at `eaff16ba` | heap ≤0.9754, total RSS ≤0.9975 (104/104 lower) at `eaff16ba` |
| Subtitles, text | `subtitle-text` 367/367 and `subtitle-text-host` 23/23 pass | `subtitle-text` 1.0018 (367/367 pass)<br>`subtitle-text-host` 0.0800 (23/23 pass) at `97759e39` | `subtitle-text` heap ≤0.3358, total RSS ≤0.9982 (367/367 lower)<br>`subtitle-text-host` heap ≤0.0075, total RSS ≤0.9198 (23/23 lower) at `97759e39` |
| Subtitles, PGS | 319/319 (`subtitle-pgs`) | 0.7775 (319/319 pass) at `546045db` | heap ≤0.9678, total RSS ≤0.9971 (319/319 lower) at `546045db` |
| Subtitles, PGS capsules | 229/229 (`subtitle-pgs-capsules`) | 0.6719 (229/229 pass) at `546045db` | heap ≤0.5966, total RSS ≤0.7996 (229/229 lower) at `546045db` |
| Subtitles, DVB | 241/241 (`subtitle-dvb`) | 0.8514 (241/241 pass) at `f8e1c4d9` | heap ≤0.7118, total RSS ≤0.8630 (241/241 lower) at `f8e1c4d9` |
| Subtitles, DVD | 228/228 (`subtitle-dvd`) and 5/5 (`subtitle-dvd-host`, gated on this target) | `subtitle-dvd` 0.9752 (228/228 pass; median of the cases 0.1909); `subtitle-dvd-host` 0.6668 (5/5 pass) at `f8e1c4d9` | `subtitle-dvd` heap ≤0.9657, total RSS ≤0.8882 (228/228 lower); `subtitle-dvd-host` heap ≤0.2037, total RSS ≤0.8105 (5/5 lower) at `f8e1c4d9` |
| Closed captions | 461/461 (`subtitle-cc`, the fold and the prepared packets' times) | 0.8229 (461/461 pass) at `f8e1c4d9`; whole worker runs over the 112 inputs at `adaa4bc7` take 0.909 of main's instructions in sum (main at `03cc8d3f`), no input above main beyond the run spread but `h264-rate-two-ts` (+0.60%) | heap ≤0.1267, total RSS ≤0.7895 (461/461 lower) at `f8e1c4d9` |
| Teletext | 72/72 (`subtitle-teletext`) | 0.5229 (72/72 pass) at `f8e1c4d9` | heap ≤0.6256, total RSS ≤0.7890 (72/72 lower) at `f8e1c4d9` |

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
Linux AArch64 only. The closest below it is MP4 forward 1.0811. Capture's worst
bound on ARMv7 at `e5f6c971` is 1.0427. At `f8e1c4d9` Matroska's worst bound
is 0.9708 and DVD's is 0.9752.
Each feature's evidence set lists every case's bound.

The AAC decoder takes 4.8 to 30.3% fewer user-mode instructions on this target at
`4213d042` than at `e43f7a4f` (`perf stat` on CPU 1). LLVM emits no NEON float instruction
for the gate target, for plain loops, `f32::algebraic_*` operations and explicit
four-lane arrays alike, and 32-bit ARM has no post-incrementing float load, so the
changes keep sums in registers, read float streams in blocks with immediate offsets, drop
copies and per-band table lookups, and compute the SBR QMF analysis as a fold and two
32-point DCT-IVs. Only that last change moves the output: SBR output changes at rounding
level, and no aac lane vector's error against FFmpeg grows by more than 1.74 times
(`al_sbr_cm_48_5.1`, 10% of its limit). Its gated AAC cases pass with room: `aac-lc-5.1`
0.9635 and `aac-lc-stereo` 0.9158, against 1.1622 and 1.0065 at `a73350ca`, and AAC in MOV
1.0062, against 1.1105. The three HE-AAC workloads stayed open at that revision, at 1.2875 to
1.2933, from 1.6507 to 1.7985. FFmpeg's ARMv7 build runs SBR's QMF windowing, subband
shuffles, HF generation, gain filtering and noise and its s16 conversion as NEON code, 19.6%
of its cycles on `aac-he-stereo`; the decoder ran that work scalar, and the QMF synthesis
alone took 21.7% of its cycles. On Linux AArch64 the same changes take 3.1 to 10.7% of the AAC
workloads' instructions away. Evidence sets `aac-thumb2-speed-armv7` and
`aac-thumb2-speed-aarch64` hold the counts of every commit, the profiles, the grades and
the code-shape probes.

The codec bounds are gated on this target. At `f9fd4030` and `906d0a36`, all 20 workloads of
`workloads.json` and all 10 of `file-workloads.json` are within the ceiling. The three
HE-AAC workloads reached it, from 1.2853 to 1.3037 at `c68c9667` to 1.0498 to 1.0731, when
the SBR QMF banks' windowing and subband shuffles were written with `std::simd`
(codec-aac's `simd` feature, which the gate build enables with the pinned nightly for the
AAC decode path only). LLVM emits NEON code for those four-lane forms, where it emits none
for the scalar loops, and the decoder takes 21 to 24% fewer user-mode instructions and 17
to 19% fewer cycles on these workloads. Each lane does the scalar code's operations in the
same order: the 39 AAC vectors and the timing workloads decode byte-identically to the
scalar build on this target, and the workloads on Linux AArch64, where the same change
takes 6 to 8% of the instructions away. A vector form of HF generation's patch loop,
assembled lane by lane, was slower on both targets and stays scalar.

At `81edf4c0` the three HE-AAC workloads measure 1.0032 (`aac-he-stereo`), 1.0163
(`aac-hev2-stereo`) and 1.0182 (`aac-he-5.1`), from 1.0647, 1.0751 and 1.0510 at `906d0a36`.
Each timing stopped after 20 pairs on the ceiling then in force, its upper bound already below
the present [ceiling](quality.md#gates). A
stage account at `2a6a07a7` named three items the reference runs as NEON code and the
decoder ran scalar: the device path's s16 conversion (about 6% of the cycles of both stereo
workloads, against 0.5% in FFmpeg's NEON conversion), the covariance sums of HF generation
(about 3.5% of `aac-he-stereo`, three times the reference's, and summed again for every
patch that shares a source band) and on HE-AAC v2 the Parametric Stereo
hybrid filters (about 4.5%). The stereo pass-through of `triergon_session::mix` now converts
from the samples' bits with integer operations, which LLVM vectorises for this target; each
source band's predictor is computed once a frame; and with codec-aac's `simd` feature the
covariances of four source bands, and the hybrid filters of four slots, are summed per
vector, each lane in the scalar code's order. Against `2a6a07a7` the gate build takes 4.6%
fewer user-mode instructions and 5.5% fewer cycles on `aac-he-stereo`, 7.3% and 5.6% on
`aac-hev2-stereo` and 1.6% and 2.6% on `aac-he-5.1`; the other decoders' stereo workloads,
which pass through the same conversion, take 2 to 9% fewer cycles. Output is
unchanged: the workloads' digests, and every aac-lane vector decoded by the AAC decode
example at `2a6a07a7` and `9d70b166`, are byte-identical on both targets. Linux AArch64
keeps the floating-point conversion; there the HE-AAC workloads take 0.7 to 2.9% fewer
cycles.

The other decoders' multichannel workloads run as many instructions as at `2a6a07a7`, with
cycles from 2.4% fewer (DTS) to 4% more (`ac3-4.0`, `eac3-7.1`, `vorbis-q3-5.1`): their code
is unchanged, and where it lies in the codec worker, linked without a trained order, moved.
On `vorbis-q3-5.1` the extra cycles are in the shared inverse MDCT. Their bounds at `81edf4c0`
(`vorbis-q3-5.1` 0.9904, `eac3-7.1` 0.9622, `ac3-5.1-448` 0.9206) are those of `f9fd4030`
(0.9877, 0.9607, 0.9186), against 0.9544, 0.9283 and 0.8943 at `906d0a36`. Evidence sets
`aac-sbr-ps-simd-armv7` and `aac-sbr-ps-simd-aarch64` hold the stage account, the counts of
every commit, the profiles, a quick timing round, the tests and the byte-for-byte check;
`codec-worker-placement-armv7` holds the other decoders' counts and profiles.

The nightly builds the video decoders slower on this target, so the rest of the codec
worker stays on stable. At `7f32411b`, with the whole codec worker built by the nightly,
the six video workloads measured 0.6298 to 1.1694 (`h264-high-1080p` 1.1694), against 0.6044
to 1.1198 at `c68c9667`; at `f9fd4030`, with only `codec-aac-worker` on the nightly, they
measure 0.5995 to 1.1344. On Linux AArch64, `h264-high-1080p` measured 0.9835 at
`c68c9667`, 0.9696 at `7f32411b`, 0.9735 at `f9fd4030` and 0.9774 at `906d0a36`.

`h264-high-1080p` measures 1.0397 at `906d0a36`, from 1.1344 at `f9fd4030`. At `256aa348` the
decoder spent 7.5% of its cycles on this workload in the integer division routines
(`__aeabi_uidiv` and its kin), which stand in for the divide instruction the ARMv7 baseline
lacks: the slice loop, neighbour derivation, deblocking and the co-located lookup split
macroblock addresses by the picture width, and the neighbour location took `% maxW`. Those
splits now multiply by ceil(2^31 / width) and shift (`triergon_codec::Divisor`, exact for
every address the parameter sets allow), and xW and yW take a mask. The decoder takes 4.5%
fewer user-mode instructions and 8.6% fewer cycles on `h264-high-1080p` and
`h264-main-1920x800`, and 8.0% fewer cycles on `h264-high-720p`; on Linux AArch64, which
divides in hardware, instructions rise 0.03 to 0.11% and cycles fall 0.4 to 0.8%. Output is
unchanged. The largest decoder item left is bi-prediction (`weight_bi`, 9.6% of cycles),
whose cost is loads from the reference frames; the reference spends under 1.5% there.
Evidence set `h264-division-7c534afb` holds the stage account, counts and profiles.

The MP4 forward case `progressive-ten-sequential-batches` (a front `moov` and ten
`moof`/`mdat` pairs of three packets) measures 0.9013 (95% 0.8945 to 0.9100) at `419a8493`,
from 1.0573 (1.0418 to 1.0671) at `1c64b775`. At `2a6a07a7` an operation took 565.6K
user-mode instructions and 255.7K cycles, against the reference's 592.0K and 237.8K: fewer
instructions at a lower rate. 29% of the cycles were in the C library, 11% in two `memcpy`
calls of the player moving 576 of the 640 bytes of an engine step on 32-bit ARM: it took
each engine's step out of its result with `?` on every poll, and most polls yield. The player
now matches each step where the engine wrote it, and only an event moves. An operation
takes 460.6K instructions and 214.1K cycles (−18.6% and −16.3%); the C library's share is
20%. The remaining items above 4% of cycles are the 64-bit division routines in the
exact time arithmetic each packet's clocks go through (7%), and `memcpy` in the
progressive engine's merge (4%). Counted against `2a6a07a7`, the Matroska legacy AAC-LC and
MPEG-TS progressive cases on both targets, and an audio clock and a raw Annex B case on
ARMv7, keep their user-mode instructions within ±0.06%, and on ARMv7 every trace of both
playback workers is byte-identical (289 cases).
Evidence set `mp4-forward-step-in-place-419a8493` holds the stage account, counts,
profiles and trace identities.

The AC-3, E-AC-3, DTS and MPEG audio workloads reached the ceiling between `4213d042` and
`c68c9667`. The changes were to code shape, not to the instruction set. The library calls
for `floor`, `round` and integer division are gone: AC-3 decouples with integer coupling
coordinates and rounds AHT values without `roundf`. Divisions by a run-time divisor go
through `triergon_codec::divide`, which divides exactly in double precision on 32-bit ARM.
DTS and MPEG audio Layer II split block and grouped codes with a reciprocal. Sums and
reader state stay in registers, mirrored streams are loaded once, and loops run without
index checks. The DTS, MPEG audio and AC-3 synthesis windows run in register-sized groups.
Two changes are ARMv7 only, both bit-identical. AC-3's long transform windows each output
in its last pass, which takes 23% fewer cycles here and would take 45% more on Linux
AArch64, where the separate loops vectorise. The shared DCT-IV runs its post-twiddle from
both ends. Two changes round differently, on both targets. AC-3 and E-AC-3 compute the
inverse transform with radix-4 and radix-8 passes, and MPEG audio Layer III computes its
36-point IMDCT through a 9-point complex DFT. Every other change leaves the output
bit-identical.

No error against FFmpeg moves on 80 pinned vectors (AC-3, E-AC-3, DTS, MPEG audio, Opus
and Vorbis), or on the 25 regenerated Layer I and II vectors. On 27 of the 28 regenerated
Layer III vectors the error moves at rounding level. RMS grows by at most 1.08 times on
this target and 1.09 times on Linux AArch64. Maximum error grows by at most 1.22 and
1.50 times. No Layer III vector ends above 8.0% of the RMS limit or 18.7% of the maximum
limit on either target.

At `4213d042` the worst of these ARMv7 bounds were `mp2-256-48k` 1.8358 and
`dts-core-es-6.1` 1.8323. At `0ac17828`, before the AAC decoder's speed changes were
merged, the highest were `vorbis-q3-5.1` 1.0503, `eac3-stereo-spx` 1.0273 and `eac3-7.1`
1.0190, all within the ceiling.
Evidence sets `audio-speed-0ac17828-TARGET` hold the per-vector errors before and after,
the profiles and the notes, and `gate-run-TARGET-0ac17828-*` the gate runs at that
revision.

Before this work, at `4cbd1e86`, built as A32 with NEON
(`armv7-unknown-linux-gnueabihf`, `-C target-feature=+neon`) and timed against the same
reference, the three E-AC-3 workloads were within the ceiling (1.1231 to 1.1821). Their
cause was the Thumb-2 encoding. The two AC-3 (1.2190 and 1.2657), three HE-AAC (1.4763 to
1.5747), two DTS (1.6030 and 1.6343) and three MPEG audio workloads (1.5358 to 1.6114)
stayed above it. Those decoders ran 1.6 to 2.3 times the reference's user-mode
instructions. SBR and its QMF synthesis took 47% of HE-AAC's cycles, and the polyphase
synthesis 61 to 72% of MPEG audio's. The library `floor` took 12% of AC-3's cycles, and the
library integer division 7.6% of DTS's (evidence set `codec-armv7-causes`). Earlier, on a c6g.4xlarge pinned to CPU 1, in the same user
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

Capture conversion must stay within one of FFmpeg's on every sample. At capture widths
of at least 16 that are multiples of 4, FFmpeg's ARMv7 build averages each 2×2 block's
RGB before computing chroma. The AArch64 build filters chroma vertically. At
`a34b98d0` the default width-dependent `Session::open` matches the ARMv7 build and
`Session::open_filtered` matches the AArch64 build byte for byte on all 15 cases.
[Capture session constructors](../crates/capture/README.md)

On the Linux AArch64 gate image, the filtered conversion also matches the
pinned reference at 84 short-frame width, height and orientation combinations
and at 480×270 and 960×540 in both orientations. The extra `1 << 8` rounding
term was recovered by fitting the converter to the reference's emitted luma
bytes. At RGB (0, 85, 170) the reference produces luma 76. Without that term
the converter produces 75.
[`capture-filtered-probes-aarch64`](../harness/evidence/sets/capture-filtered-probes-aarch64.json)
holds these checks at `e12863c8`.

On the 32-bit ARMv7 gate image, `Session::open` matches the pinned conversion
sample for sample at 42 combinations of widths 2 through 14, heights 2, 4 and 6,
and both orientations. The same check passes at 480×270 and 960×540 in both
orientations, where it exercises the block path.
[`capture-native-widths-armv7`](../harness/evidence/sets/capture-native-widths-armv7.json)
holds these checks at `a34b98d0`.

Compared with `f8e1c4d9`, the filtered conversion changes the capture output
folds on all 15 Linux AArch64 cases. All 15 ARMv7 output folds and all
reference folds on both targets match their earlier values.

At `a34b98d0`, the 15 capture cases passed conversion, encoding, mux, runtime,
peak heap and kernel-bound total RSS on each c6g.4xlarge target. The worst
runtime bounds are 1.0519 on Linux AArch64 and 1.0536 on 32-bit ARMv7.

Earlier, at `001d40b6`, all 15 cases per target passed parity, runtime, peak
heap and kernel-bound total RSS. The runtime upper bounds were within the present [ceiling](quality.md#gates).

At `e5f6c971`, all 15 cases per c6g.4xlarge target pass conversion, encoding,
mux, runtime, peak heap and kernel-bound total RSS. The input
hashes and output folds match `a34b98d0`. The ARMv7 workloads are 480 or 960
pixels wide and use the block converter.

The Linux AArch64 evidence combines shards 1, 4 and 5 of run
`20261118-172954` with both shards of run `20261118-185150`. A lost SSH
connection left the original shards 2 and 3 without archives. The second
run measured their seven cases at the same revision. Run `20261118-185012`
covered all 15 ARMv7 cases.

In that comparison, the sole bound above the present [ceiling](quality.md#gates) was AArch64 `c04-motion-960` at 1.0595 after the
validated 20-pair early stop. Its bound was 1.0519 with 30 pairs at `a34b98d0`
and 1.0385 at `001d40b6`. Both recent revisions use filtered chroma.
On one AArch64 host, `cargo xtask count` reports 0.78% fewer user-mode
instructions and 1.20% fewer user-mode cycles at `e5f6c971` than at `a34b98d0`.
The input and worker hashes match their gate measurements. The cause of the
difference between the gate runs remains unresolved.

At `c30b9c80`, all 15 capture cases per c6g.4xlarge target pass conversion,
encoding, mux, paired runtime, peak heap and kernel-bound total peak RSS.
The encoded output folds match `e5f6c971` on all cases. Reusing source rows
reduces motion search work. The 32-bit ARM input layout removes one transform
transpose and preserves both rounding passes.

The runtime upper bound for AArch64 `c04-motion-960` is
0.9730. The bound for ARMv7 `c14-motion-960-rate255`
is 0.9838. The largest bounds among the other cases
are 0.9595 for `c13-motion-960-6m` on AArch64 and 0.9525 for `c13-motion-960-6m` on ARMv7.

Capture's worst bound here was 1.1669 at `a73350ca` and 1.1865 at `576f088f`. Profiled in user-mode cycles against
FFmpeg on the gate host, the encoder's motion search reduced each 16-lane sum of absolute
differences with 16 lane extracts and spills where it was inlined, built half-sample rows
byte by byte, ran the inverse DCT over all 64 coefficients although 76 to 95% of blocks
hold at most four, and cleared the coarse search's row of differences with a library
`memset` per row. The sums of absolute differences are now out-of-line vector reductions,
half-sample rows are formed whole, the inverse DCT sums the basis rows of only the
coefficients a block holds, with the same rounding points, and the coarse row is a
fixed-size array. `c14-motion-960-rate255` went from 1.175 to 1.069 of FFmpeg's user-mode
cycles and `c13-motion-960-6m` from 1.178 to 1.065, with all 15 stream digests unchanged.
At `e5f6c971`, the worst ARMv7 runtime bound is 1.0427
(`c14-motion-960-rate255`).
The remaining gap is spread over the encoder's DCT, quantisation and search,
which FFmpeg's ARMv7 build runs in ARMv6 and NEON routines (by their symbols
in its profile).

At `a73350ca` 16 `subtitle-dvd` cases were above the present [ceiling](quality.md#gates) here (`vob-split` worst, 1.0869),
while the feature's median of case medians was 0.1918: they are the DVD program streams
with many full-size subpictures, whose time is the run-length decode; most other cases
are small. The worker ran FFmpeg's user-mode instruction count on
them but about ten times its branch misses (10.3 M against 0.9 M): it expanded a picture's
top and bottom field lines interleaved, so the branch between short and long runs
alternated between two lines' patterns, and its bit reader refilled a 64-bit cache on a
data-dependent branch. It now decodes one line at a time and reads each code from the bit
position. At `f8e1c4d9` those 16 cases are at most 0.8184 and the feature's worst bound
is 0.9752 (`fate-dvd-still-frame-1`), with all 233 DVD traces unchanged.

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
functions the training corpus runs together. Linked by the toolchain's default linker
instead, at `e21f800e` (`2a6a07a7` with every trained order removed) on the same images,
whose ordered results are those of `1c64b775` with the same workers, MP4 forward's total
RSS bound rises from 0.9893 (51/51 lower) to 1.1158 (0/51), MP4 range's from 0.9233
(64/64) to 1.0920 (0/64), Matroska demux's from 0.9045 (12/12) to 1.0209 (10/12), Matroska
zlib's from 0.9450 (10/10) to 1.0110 (8/10) and Matroska legacy AAC's from 0.9555 (4/4) to
1.0172 (0/4); on Linux AArch64 packet framing's rises from 1.0290 (134/135) to 1.0469
(0/135), and every other item stays lower. The median case of an item holds 88 to 672 KiB
more on ARMv7, and 92 to 608 KiB more on Linux AArch64, without its order.

Making the workers smaller does not close this. The functions the training corpus runs lie
in every 64 KiB window of each ordered worker's code (11 of the media worker's 11 on
ARMv7, 22 of the playback worker's 23), and packed they fill 6 and 16. Built with
`opt-level = "s"` or `"z"` and linked without their orders, the ordered workers' images
are 12 to 23% smaller, and the median training operation of each worker and target
recovers 21 to 64% of the peak resident memory its order saves, some none. At `2b196e9c`,
whose 256-bit time arithmetic is out of line, the ARMv7 playback worker's code is 2.1%
smaller and its unordered MP4 cases hold a median of 104 to 118 KiB less; linked without
the orders at `d18904f9`, MP4 forward (1.0581, 0/51 lower), MP4 range (1.0548, 1/64),
Matroska demux (10/12) and legacy AAC (0/4) still fail on ARMv7, and packet framing
(1.0493, 0/135) on Linux AArch64.

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

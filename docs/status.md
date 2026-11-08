# Status

Everything is experimental. No module has yet passed every gate on its complete scope
(see [quality](quality.md)). Runtime is the worst per-case upper bound of
`T_triergon / T_reference`, graded against the [ceiling](quality.md#gates). Memory is peak
heap and total resident peak RSS relative to the reference. Unless noted, measurements are native
AArch64 Linux.
Containers, framing, subtitles and playback are measured against FFmpeg git revision
`d32b387f` on the 9.0 branch; codecs, decode sessions, capture and the playback MP4
terminal-media matrix against the FFmpeg 9.0.2 release; fonts against FreeType 2.15.0;
shaping against HarfBuzz 14.6.0 and FriBidi 1.0.17
([reference pins](quality.md#reference-and-corpus)).

| Area | Implemented | Verified against the reference | Runtime | Memory | Open |
|---|---|---|---|---|---|
| Core | Charged storage, exact time, input windows, work budgets, neutral container model with one codec identity | 51,235 exact-arithmetic cases against an independent oracle | via dependents | via dependents | fuzzing |
| Matroska / WebM | Incremental reader: lacing, header removal, bounded zlib, Cues and preroll, colour and static HDR, chapters, tags, attachments, side data | 116 workloads in six feature groups, including an external HDR sample, and four host FATE demux comparisons | 1.1590 (116/116 pass) | heap ≤0.627, RSS ≤0.965 | full EBML/Matroska scope |
| Matroska startup and seek | Startup and seek with lazy scan replay | 44 cases, 1,344 packets | 1.1606 (44/44 pass) | heap ≤0.404, total RSS ≤0.849 | — |
| Matroska audio clocks | AAC, AC-3, E-AC-3 startup and forward clocks | 10/10 packet folds (`matroska-audio-clocks`) | 1.4510 (6/10 pass) | heap ≤1.822 (8/10 lower), total RSS ≤1.043 (0/10 lower) | audio seek |
| ISO BMFF / MP4 | Regular and fragmented files, edit lists, seek and range plans, neutral `ContainerReader` over seekable sources, QuickTime compatibility, bounded AAC configuration and regular table recovery, timing, sync and terminal media prefixes; AVC, HEVC, AAC, AC-3, E-AC-3, DTS core, AV1, VP9 | 255 inputs, 1,020 open and index comparisons (972 matching outputs and 48 matching refusal outcomes with compatibility policies) | 1.1273 (972/972 pass) | heap ≤0.4133, total RSS ≤0.7283 (972/972) | seek and range-plan timing; neutral reader not yet compared with FFmpeg |
| MP4 range playback | Regular AVC/AAC startup and backward seek with sample table durations and coded-frame trim | 4 complete open and backward-seek comparisons, 845 packets; AAC duration and trim regressions | 1.0466 (4/4 pass) | heap ≤0.556, total RSS ≤0.945 (4/4 lower) | other range playback profiles |
| Ogg | Pages with CRC and resync, multiplexed and chained streams, Vorbis, Opus and FLAC mappings with canonical configurations, granule timing, pre-skip and end trims, bisection seeking | Packets against the pinned `ffprobe` (`cargo xtask codecs readers`) on 21 FATE Vorbis files and 13 generated Opus, Vorbis and FLAC files (multiplexed, chained, damaged, truncated), in two views: the FFmpeg-compatible `compat::clock` 31/34 pass, 3 differ only by reviewed FFmpeg behaviours; the reader's own timing 7/34 pass, 25 differ only where `compat::clock` reproduces FFmpeg, 2 by reviewed behaviours ([reader rules](../harness/codecs/reader-rules.json), [frozen differences](../harness/codecs/reader-differences.json)) | within the codec workloads | within the codec workloads | `b_vorbis.ogg`, `chain_vorbis.ogg`: a negative last duration FFmpeg prints modulo 2^32; `crc_bad.opus`: the first packet after a page that fails its CRC, which FFmpeg times from its own page and `compat::clock` from the stream start; Theora and Speex untimed; chained seeks not compared with FFmpeg |
| Audio elementary streams | Native FLAC (with or without `fLaC`), MPEG audio with ID3v2/ID3v1/APE tags, Xing/Info/VBRI and LAME gapless trims, ADTS, AC-3/E-AC-3; one shared frame-stream engine | Packets as the readers emit them against the pinned `ffprobe` on 271 FLAC, MPEG audio, ADTS, AC-3 and E-AC-3 files (the IETF FLAC testbench, MP3 conformance, generated and crafted damaged streams): 253/271 pass, 16 differ only by reviewed FFmpeg behaviours, 2 refused by both (testbench faulty `06 - missing streaminfo metadata block`, `07 - other metadata blocks preceding streaminfo metadata block`) | within the codec workloads | within the codec workloads | FLAC `flac_s20_rate768k`, `06 - samplerate 768kHz` (no durations at 768 kHz), `01 - changing samplerate`, `27 - old format variable blocksize file created with Flake 0.11`, `11 - file starting with unparsable data`, `11 - incorrect metadata block length`, `08 - blocksize 65536`, `09 - blocksize 1`; crafted `ac3_tags`, `adts_mid_junk`, `flac_corrupt`, `flac_id3`, `flac_mid_junk`, `mp3_foreign_frame`, `mp3_mid_junk_falsesync`; MP3 `he_free` (free format, refused by FFmpeg); E-AC-3 dependent substreams on real files; ADTS and AC-3 seek cost |
| MPEG-TS | PSI, continuity, PES assembly; TS188 and M2TS192 | 66/97 compat-view operations (`mpegts-pes`: the 272 recorded requests through the public `compat::pes` API); 31 open cases need the MPEG-TS `ContainerReader`: `pcr-timeline-discontinuity`, `pcr-extension-invalid`, `transport-tei`, `transport-scrambled`, `transport-afc-reserved`, `transport-adaptation-overrun`, `transport-partial-final-packet`, `transport-sync-loss`, `psi-crc-invalid`, `psi-pointer-overrun`, `psi-length-over-policy`, `psi-pending-not-current`, `pat-atomic-two-sections`, `pat-incomplete-version`, `pat-network-pid-not-track`, `psi-pointer-finishes-old-starts-new`, `psi-two-sections-one-payload`, `pmt-version-wrap-retypes-pid`, `pmt-opaque-language-descriptors`, `pmt-descriptor-overrun`, `pmt-multi-section-forbidden`, `psi-stuffing-tail-invalid`, `psi-truncated-eof`, `pes-timestamp-marker`, `pes-forbidden-time-flags`, `pes-optional-header-overrun`, `pes-start-prefix-invalid`, `hls-contiguous-pes-carry`, `hls-external-map`, `hls-explicit-discontinuity`, `opcr-without-pcr-invalid` | 1.0029 (66/66 pass) | heap ≤0.073, total RSS ≤0.818 (66/66 lower) | the 31 open cases; the strict source view and its native clock are not compared; access-unit demux, broader profiles |
| MPEG-TS progressive playback | TS188 with AVC and AAC-LC, bounded sequential input and access units before EOF | 3 packet folds and all 5 functional inputs with 10 literal outcomes | 1.1951 (3/3 pass) | heap ≤0.016, total RSS ≤0.648 (3/3 lower) | broader transport and codec profiles |
| HLS segments | Independent TS188 segments with contiguous AVC or HEVC video and AAC, AC-3 or E-AC-3 audio | 71 packet comparisons, 71 measured workloads | 1.1520 (71/71 pass) | heap ≤0.022, total RSS ≤0.727 (71/71 lower) | other profiles |
| Packet framing | AVC/HEVC configuration and NAL framing, AV1, VP9, AC-3, E-AC-3, DTS core | 125 AVC/HEVC reference comparisons; DTS core byte-exact on 37,125 projections and 1,172 FATE packets, recorded without a timing runner | 1.3846 (45/125 pass for AVC/HEVC) | heap ≤0.985 (125/125 lower), total RSS ≤1.048 (0/125 lower) | runtime and RSS optimization; full AVC/HEVC/ADTS grammar |
| Subtitles, text | SRT, WebVTT, ASS passthrough, mov_text and tx3g to ASS, charset handling | 82 mov_text cases, 79 tx3g regressions | not measured | not measured | timing and memory |
| Subtitles, bitmap | PGS, DVB display state and colour lookup, DVD units | 229 PGS cases, replayed by tests from recorded output; 5,300 DVB display sets, recorded; no runner in the repository | PGS 1.11 | PGS heap 0.62, RSS 0.90 | full PGS recovery, DVB and DVD decoding |
| Playback | Matroska, MP4, TS and raw AVC/HEVC feeds; seeking; end-of-file continuation | MP4 terminal-media matrix against recorded FFprobe 9.0.2 output (evidence set `playback-terminal-matrix-ffprobe`): 39 inputs in 13 source geometries (12 nonseekable for every input, 1 seekable for the 31 original inputs), 499 rows, each run in debug and in release. `Mp4Open::new_recovery` 499/499 pass: 497 identical packet lists, and 2 seekable reads of a zero-size fragment sample that the opener refuses in the header, where FFprobe also refuses. `Mp4Open::new` 446/499 pass. Packet folds: MP4 forward 37/38 (`mp4-forward`), TS program 35/35 (`ts-program`), raw Annex B 6/6 (`raw-annex-b`) | MP4 forward 1.6427 (35/37 pass), TS program 1.4695 (33/35 pass), raw Annex B 1.0810 (6/6 pass) | MP4 forward heap ≤3.382 (36/37 lower), total RSS ≤1.025 (18/37 lower); TS program heap ≤0.102, total RSS ≤0.910 (35/35 lower); raw Annex B heap ≤0.127, total RSS ≤0.901 (6/6 lower) | other profiles; legacy Matroska `A_AAC/MPEG4/*` codec IDs are not read as AAC; `mp4-forward` open case `regular-encoded-avc-aac-indexed-seek`: the native MOV view refuses an edit list over reordered samples |
| Codecs | H.264 8-bit 4:2:0; HEVC Main and Main10; AAC LC, HE, HEv2; AC-3 and E-AC-3; DTS core with XCh and X96; storage charged to a budget | 417/417 comparable conformance and FATE vectors | 1.0551 (17/17 pass) | heap 0.92, RSS 0.82 | error concealment, unverified tool paths; AAC charges in-band reconfiguration when it appears, and a refused access unit leaves the decoder unchanged unless an earlier raw data block or Main-prediction element of it was decoded |
| Audio codecs in files | FLAC (RFC 9639, 4 to 32 bits, 1 to 8 channels); Opus (RFC 6716 and 8251: SILK, CELT, hybrid, families 0, 1 and 255); Vorbis I; MPEG-1, 2 and 2.5 audio Layer I, II and III | 315/321 pass through the container readers (FATE Vorbis, Opus and MP3 conformance, the IETF FLAC testbench, generated streams): FLAC bit-exact, the others within the [numeric rules](../harness/codecs/numeric-rules.json) | 1.0834 (10/10 pass) | heap ≤0.904, total RSS ≤0.966 (10/10) | FLAC `01 - changing samplerate`, `02 - increasing number of channels`, `03 - decreasing number of channels` and MP3 `he_mode` change format mid-stream, which the CLI converts; FLAC `flac_3ch_odd_mask`, whose layout FFmpeg refuses on its stereo output; MP3 `he_free` (free format: FFmpeg finds no two consecutive frames, and the decoder refuses free-format frames), refused by both; runtime toward the target for Opus music (1.0301) and 5.1 (1.0834), Vorbis 5.1 (0.9356) and stereo (0.8423) and Opus speech (0.8719); Opus families 2 and 3 |
| Decode sessions | Bounded queues, clock-gated output, flush and seek, RGBA letterbox, stereo mix; container track decoding with trims and start delays | with the codecs | 1.0551 (17/17 pass) | with the codecs | — |
| Fonts | sfnt and collections; `head`, `hhea`, `maxp`, `hmtx`, `OS/2`, `name`; `cmap` formats 0, 4, 6, 10, 12, 13; `glyf`/`loca`; CFF and CFF2 at the default instance; `kern` format 0; loading at a size as libass does with hinting off (unhinted outlines, synthetic bold and oblique, size metrics, kerning, decoration bands, default-load advances where hinting cannot move them); face matching with libass's rules (unit tests only, unverified until the libass lane compares it) | The gate covers libass's default configuration (hinting off). All 65 faces of 51 corpus files (TrueType including Arabic, Hebrew, Indic and Thai fonts, name-keyed and CID-keyed CFF, CFF2, a 10-face collection, four authored fonts; 836,315 glyphs) identical to FreeType: names, cmap, libass's text charmap, the kerning of every listed pair, advances and unscaled outlines (`font-outlines` 51/51); at 256 pixels, size metrics, kerning, every glyph's unhinted outline with its synthetic styles, and default-load advances (`font-hinting-none` 100/102, 2 open: auto-hinter); macOS AArch64 | not measured | not measured | libass's hinting-enabled modes (NATIVE, LIGHT, NORMAL): their nine other sizes are measured, not gated (`font-hinting-modes` 900/918: unhinted outlines 459/459, default-load advances 441/459), and hinted outlines are not compared; the FreeType auto-hinter, which leaves DejaVuMathTeXGyre and authored-cmap-formats open at every size; tricky fonts and instruction-control programs; face matching against libass; `cmap` format 14; runtime and memory; Linux runs |
| Shaping | OpenType layout (GDEF, every GSUB and GPOS lookup type, feature variations at the default instance, `kern` format 0 fallback, fallback mark positioning) with HarfBuzz's default, Arabic, Hebrew, Thai/Lao, Hangul, Indic, Khmer, Myanmar and Universal shapers for horizontal runs; libass's FreeType-backed font functions over a sized face; bidi as FriBidi resolves and reorders it for libass; libass's simple shaping level; Unicode properties generated from pinned UCD files; planning, shaping and bidi bounded by work limits | Identical to HarfBuzz and FriBidi on macOS AArch64. Gating: `shape-libass` 191/200 (9 open: FreeType's auto-hinter sets eight fonts' default-load advances, and one CFF font without GPOS needs hinted extents), 3,029 runs over 25 corpus files and 22 scripts shaped as libass does at 256 pixels with hinting off; `shape-bidi` 24/24; `shape-simple` 6/6; `shape-unicode` 1/1 (every code point). Coverage at units per em: `shape-text` 199/200 and, on demand, `shape-text-all` 1,745/1,746 (101,691 runs over every corpus face), one open case each (CFF extents as HarfBuzz's own interpreter reads them) | not measured | not measured | hinted glyph extents and contour points and auto-hinted advances, which libass reads from FreeType; vertical text and `@` fonts; AAT tables and Apple `kern`, and OpenType `kern` formats 1, 2 and 3 and cross-stream kerning, which HarfBuzz applies (faces that need them are refused); `stch` stretching; variable instances other than the default; runtime and memory; Linux runs |
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

The MP4 range playback row uses the gate build at
`f58e444b72f94bb3962990dd515f28be1aaaa574` on a c8g.large pinned to CPU 1.
All four original cases and 845 packets match, with 30 timing pairs of 50 complete
operations and 30 memory pairs. Every case passes runtime, peak heap and total RSS;
full module and FATE coverage remain pending. The earlier compiler-override
comparison remains archived separately.

| Area | Revision | Raw samples SHA-256 |
|---|---|---|
| Matroska / WebM | `0e28298d038bea072629b3bad6389dcc806b4861` | `a20dcbbe804bfd21ee631cc42f61466d2400d5115abb8ea97dd243ef733ba74f` |
| Matroska startup and seek | `0e28298d038bea072629b3bad6389dcc806b4861` | `a20dcbbe804bfd21ee631cc42f61466d2400d5115abb8ea97dd243ef733ba74f` |
| ISO BMFF / MP4 | `f958c0b9624a8b7c7bd91b7d2a0c30299ae1dbed` | `fd718807e0ec1e6287bcb690226bf7056b79985796e74eba54f43544a034bc2c` |
| ISO BMFF comparison | `0a033ec3406aebd09d191298a546659e02c6b76c` | `f56d3fa78b8cc70222f58c7cfa9a17331b0924a9070c7ff6ecc6dc1f213561bc` |
| ISO BMFF open and index runtime | `ea4b941b4454bcc2ea0cd213999d64a07928ed7c` | `fca8e54cb9b2f0d55acc8ba9be2a5849992ae7996f3c3db551896477c3359525` |
| ISO BMFF open and index memory | `ea4b941b4454bcc2ea0cd213999d64a07928ed7c` | `46cf8ac9041b147c96108de6e09fde26c47e12fd10161f7eee9661db95f8c89a` |
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
| ARMv7: ISO BMFF / MP4 | `9621f6063a2e517fecc1c28757e6bed50b5228a8` | `ca882ff82484e31c537ebb93cbc9899666343715219ae4411914392406259691` |
| ARMv7: packet framing, codec conformance, tests | `9621f6063a2e517fecc1c28757e6bed50b5228a8` | `9eaa9347010b3c4aaf5c35bda715ee4ed1b2c7a14d3329b21976a2414455439e` |

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
| ISO BMFF / MP4 | 1,020/1,020 open and index comparisons (972 matching outputs, 48 matching refusals) | 2.4250 (757/972 pass) | heap ≤0.364 (972/972 lower); total RSS ceiling ≤1.387, observed peaks ≤0.894 |
| Packet framing | 125/125 AVC/HEVC reference comparisons | 0.9195 (125/125 pass) | heap ≤0.765 (125/125 lower); total RSS ceiling ≤1.962, observed peaks ≤1.112 |
| Codecs | 347/347 comparable H.264 and HEVC conformance vectors (of 383 run, 31 are outside the supported profiles and 5 are refused by both); the 71 derived audio streams were not run, because nothing in the repository generates them; the six video timing workloads decode to FFmpeg's digests | 1.7785 (8/17 pass) | no repository runner |
| Capture | 0/15: mux and encoder pass on all 15, conversion fails on all 15 (below) | 3.7355 (2/15 pass) | heap ≤0.564, total RSS ≤0.981 (14/15; `c05-noise-960` has no valid observation) |

Above the ceiling on ARMv7: codecs `h264-high-1080p` 1.5371, `h264-main-1920x800`
1.3155, `dts-core-es-6.1` 1.7268, `dts-core-7.1-master` 1.7785, `ac3-5.1-448` 1.3602,
`ac3-2.0-192` 1.5987, `eac3-7.1` 1.2606, `eac3-5.1-spx` 1.2126, `eac3-stereo-spx`
1.4841; capture `c01-ui-static-960` 3.4321, `c02-ui-scroll-960` 2.3130,
`c03-ui-transition-960` 3.0576, `c04-motion-960` 2.8436, `c06-ui-scroll-960-bottomup`
2.2526, `c07-ui-static-480-bottomup` 3.7355, `c08-ui-scroll-480-bottomup` 2.4663,
`c09-motion-480-bottomup` 2.5628, `c10-ui-transition-480-topdown` 3.3268,
`c11-motion-960-rate1` 1.6835, `c12-motion-960-1m` 2.7349, `c13-motion-960-6m` 2.7882,
`c14-motion-960-rate255` 2.6747; ISO BMFF 215 of 972 outputs, worst
`clock27-u64-minus1-step12-native-index-tiny` 2.4250: 194 read in `tiny` windows from the
fragmented-file (`f-`, `trun18-`, `flag-`, `origin18-`, `follow-flag-`, `discard22-`,
`discard24-`, `modern-iso6-`, `order-fragment-header-`), 64-bit clock (`clock27-`,
`order27-`) and tx3g (`r-subtitle-`) groups, and 21 `clock27-` and `order27-` outputs read
in `ordinary` windows. The raw samples' `report.json` lists each bound.

Capture conversion must stay within ±1 of FFmpeg's on every sample. On
`c01-ui-static-960`, Triergon's conversion output is byte-identical on ARMv7 and
AArch64, while FFmpeg's ARMv7 conversion differs from its AArch64 output. Across the 15
cases FFmpeg's ARMv7 conversion is up to 36 from Triergon's (on the noise cases), so all
15 fail the rule on this target; the encoder and mux pass.

Every Matroska, startup and seek and HLS case has a total RSS ceiling above the
reference. The observed resident peaks are above it in every Matroska feature-group
case except `colour-many-tracks`, `attachments-large` and `attachments-large-late` (112
of 115, up to 1.136), and in 3 of the 44 startup and seek cases:
`seek-hevc-seek-no-default-0-eof` 1.0007, `seek-hevc-seek-no-default-2700-fresh` 1.0095
and `seek-hevc-seek-no-default-2700-eof` 1.0163. Every packet-framing case has its total
RSS ceiling and its observed resident peak above the reference (observed up to 1.112);
every ISO BMFF output has its total RSS ceiling above the reference, and none its observed
peak.

The [measured revisions](#measured-revisions) table names the revision and raw samples
behind each ARMv7 row.

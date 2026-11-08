# Status

Everything is experimental. No module has yet passed every gate on its complete scope
(see [quality](quality.md)). Runtime is the worst per-case upper bound of
`T_triergon / T_reference`, graded against the [ceiling](quality.md#gates). Memory is peak
heap and total resident peak RSS relative to the reference. Unless noted, measurements are native
AArch64 Linux.
Containers, framing, subtitles and playback are measured against FFmpeg git revision
`d32b387f` on the 9.0 branch; codecs, decode sessions and capture against the FFmpeg 9.0.2
release; fonts against FreeType 2.15.0 ([reference pins](quality.md#reference-and-corpus)).

| Area | Implemented | Verified against the reference | Runtime | Memory | Open |
|---|---|---|---|---|---|
| Core | Charged storage, exact time, input windows, work budgets, neutral container model with one codec identity | 51,235 exact-arithmetic cases against an independent oracle | via dependents | via dependents | 32-bit target runs, fuzzing |
| Matroska / WebM | Incremental reader: lacing, header removal, bounded zlib, Cues and preroll, colour and static HDR, chapters, tags, attachments, side data | 116 workloads in six feature groups, including an external HDR sample, and four host FATE demux comparisons | 1.1590 (116/116 pass) | heap ≤0.627, RSS ≤0.965 | full EBML/Matroska scope |
| Matroska startup and seek | Startup and seek with lazy scan replay | 44 cases, 1,344 packets | 1.1606 (44/44 pass) | heap ≤0.404, total RSS ≤0.849 | — |
| Matroska audio clocks | AAC, AC-3, E-AC-3 startup and forward clocks | 10 inputs | 0.17 | heap 0.07, RSS 0.60 | audio seek |
| ISO BMFF / MP4 | Regular and fragmented files, edit lists, seek and range plans, neutral `ContainerReader` over seekable sources, QuickTime compatibility, bounded AAC configuration and regular table recovery, timing, sync and terminal media prefixes; AVC, HEVC, AAC, AC-3, E-AC-3, DTS core, AV1, VP9 | 255 inputs, 1,020 open and index comparisons (972 matching outputs and 48 matching refusal outcomes with compatibility policies) | 1.1273 (972/972 pass) | heap ≤0.4133, total RSS ≤0.7283 (972/972) | seek and range-plan timing; neutral reader not yet compared with FFmpeg |
| MP4 range playback | Regular AVC/AAC startup and backward seek with sample table durations and coded-frame trim | 4 complete open and backward-seek comparisons, 845 packets; AAC duration and trim regressions | 1.0466 (4/4 pass) | heap ≤0.556, total RSS ≤0.945 (4/4 lower) | other range playback profiles |
| MPEG-TS | PSI, continuity, PES assembly; TS188 and M2TS192 | 272-request set (80 field matches, 23 open), recorded; no runner in the repository | not measured | not measured | access-unit demux, broader profiles |
| MPEG-TS progressive playback | TS188 with AVC and AAC-LC, bounded sequential input and access units before EOF | 3 packet folds and all 5 functional inputs with 10 literal outcomes | 1.1951 (3/3 pass) | heap ≤0.016, total RSS ≤0.648 (3/3 lower) | broader transport and codec profiles |
| HLS segments | Independent TS188 segments with contiguous AVC or HEVC video and AAC, AC-3 or E-AC-3 audio | 71 packet comparisons, 71 measured workloads | 1.1520 (71/71 pass) | heap ≤0.022, total RSS ≤0.727 (71/71 lower) | other profiles |
| Packet framing | AVC/HEVC configuration and NAL framing, AV1, VP9, AC-3, E-AC-3, DTS core | 125 AVC/HEVC reference comparisons; DTS core byte-exact on 37,125 projections and 1,172 FATE packets, recorded without a timing runner | 1.3846 (45/125 pass for AVC/HEVC) | heap ≤0.985 (125/125 lower), total RSS ≤1.048 (0/125 lower) | runtime and RSS optimization; full AVC/HEVC/ADTS grammar |
| Subtitles, text | SRT, WebVTT, ASS passthrough, mov_text and tx3g to ASS, charset handling | 82 mov_text cases, 79 tx3g regressions | not measured | not measured | timing and memory |
| Subtitles, bitmap | PGS, DVB display state and colour lookup, DVD units | 229 PGS cases, replayed by tests from recorded output; 5,300 DVB display sets, recorded; no runner in the repository | PGS 1.11 | PGS heap 0.62, RSS 0.90 | full PGS recovery, DVB and DVD decoding |
| Playback | Matroska, MP4, TS and raw AVC/HEVC feeds; seeking; end-of-file continuation | MP4 terminal-media matrix: 994 of 998 rows with `Mp4Open::new_recovery` (39 inputs, 499 source geometries, debug and release); the other 4 are seekable reads of a zero-size fragment sample that FFmpeg also refuses in its header; compared with the FFprobe records in evidence set `playback-terminal-matrix-ffprobe` | not measured | not measured | most profiles unmeasured |
| Codecs | H.264 8-bit 4:2:0; HEVC Main and Main10; AAC LC, HE, HEv2; AC-3 and E-AC-3; DTS core with XCh and X96; storage charged to a budget | 417/417 comparable conformance and FATE vectors | 1.0551 (17/17 pass) | heap 0.92, RSS 0.82 | error concealment, unverified tool paths |
| Decode sessions | Bounded queues, clock-gated output, flush and seek, RGBA letterbox, stereo mix | with the codecs | 1.0551 (17/17 pass) | with the codecs | — |
| Fonts | sfnt and collections; `head`, `hhea`, `maxp`, `hmtx`, `OS/2`, `name`; `cmap` formats 0, 4, 6, 10, 12, 13; `glyf`/`loca`; CFF and CFF2 at the default instance; `kern` format 0; loading at a size as libass does with hinting off (unhinted outlines, synthetic bold and oblique, size metrics, kerning, decoration bands, default-load advances where hinting cannot move them); face matching with libass's rules (unit tests only, unverified until the libass lane compares it) | The gate covers libass's default configuration (hinting off). All 59 faces of 50 corpus files (TrueType including Arabic, Hebrew, Indic and Thai fonts, name-keyed and CID-keyed CFF, CFF2, a 10-face collection, three authored fonts; 836,291 glyphs) identical to FreeType: names, cmap, the kerning of every listed pair, advances and unscaled outlines (`font-outlines` 50/50); at 256 pixels, size metrics, kerning, every glyph's unhinted outline with its synthetic styles, and default-load advances (`font-hinting-none` 98/100, 2 open: auto-hinter); macOS AArch64 | not measured | not measured | libass's hinting-enabled modes (NATIVE, LIGHT, NORMAL): their nine other sizes are measured, not gated (`font-hinting-modes` 882/900: unhinted outlines 450/450, default-load advances 432/450), and hinted outlines are not compared; the FreeType auto-hinter, which leaves DejaVuMathTeXGyre and authored-cmap-formats open at every size; tricky fonts and instruction-control programs; face matching against libass; `cmap` format 14; runtime and memory; Linux runs |
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

ISO BMFF, PGS, Matroska audio clocks and general playback feed timing, plus
codec and decode-session memory, await gate-build measurement with repository runners.

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
| Capture, timing and memory | `ceb5076659e00ca9a2d3f3cb55ee4513d6d0710f` | `b75c8d6e6d64d294a385ec25b176e4ffb8f1a0d3c11b3ca956b3d352ac44c3b4` |

## Platforms

Qualification runs on native AArch64 Linux. 32-bit ARMv7 and x86_64 have no
performance runs yet.

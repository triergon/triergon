# Status

Everything is experimental. No module has yet passed every gate on its complete scope
(see [quality](quality.md)). Runtime is the worst per-case upper bound of
`T_triergon / T_ffmpeg` (ceiling 1.20, target 0.80). Memory is peak heap and total
resident peak RSS relative to FFmpeg. Unless noted, measurements are native AArch64 Linux.
Containers, framing, subtitles and playback are measured against FFmpeg git revision
`d32b387f` on the 9.0 branch; codecs, decode sessions and capture against the FFmpeg 9.0.2
release ([reference pins](quality.md#reference-and-corpus)).

| Area | Implemented | Verified against FFmpeg | Runtime | Memory | Open |
|---|---|---|---|---|---|
| Core | Charged storage, exact time, input windows, work budgets, neutral container model with one codec identity | 51,235 exact-arithmetic cases against an independent oracle | via dependents | via dependents | 32-bit target runs, fuzzing |
| Matroska / WebM | Incremental reader: lacing, header removal, bounded zlib, Cues and preroll, colour and static HDR, chapters, tags, attachments, side data | 116 workloads in six feature groups, including an external HDR sample, and four host FATE demux comparisons | 1.1590 (116/116 within 1.20) | heap ≤0.627, RSS ≤0.965 | full EBML/Matroska scope |
| Matroska startup and seek | Startup and seek with lazy scan replay | 44 cases, 1,344 packets | 1.1606 (44/44 within 1.20) | heap ≤0.404, total RSS ≤0.849 | — |
| Matroska audio clocks | AAC, AC-3, E-AC-3 startup and forward clocks | 10 inputs | 0.17 | heap 0.07, RSS 0.60 | audio seek |
| ISO BMFF / MP4 | Regular and fragmented files, edit lists, seek and range plans, QuickTime compatibility, bounded AAC configuration and regular table recovery, timing, sync and terminal media prefixes; AVC, HEVC, AAC, AC-3, E-AC-3, DTS core, AV1, VP9 | 255 inputs, 1,020 open and index comparisons (972 matching outputs and 48 matching refusal outcomes with compatibility policies) | 1.1273 (972/972 within 1.20) | heap ≤0.4133, total RSS ≤0.7283 (972/972) | seek and range-plan timing |
| MPEG-TS | PSI, continuity, PES assembly; TS188 and M2TS192 | 272-request set (80 field matches, 23 open) | not measured | not measured | access-unit demux, broader profiles |
| HLS segments | Independent TS188 segments with contiguous AVC or HEVC video and AAC, AC-3 or E-AC-3 audio | 71 packet comparisons, 71 measured workloads | 1.1808 (71/71 within 1.20) | heap ≤0.022, total RSS ≤0.722 | other profiles |
| Packet framing | AVC/HEVC configuration and NAL framing, AV1, VP9, AC-3, E-AC-3, DTS core | 125 reference matches for AVC/HEVC; DTS core byte-exact on 37,125 projections and 1,172 FATE packets | 1.1772 (125/125 within 1.20 for AVC/HEVC) | heap ≤0.985, total RSS ≤0.990 on the same 125 cases | full AVC/HEVC/ADTS grammar |
| Subtitles, text | SRT, WebVTT, ASS passthrough, mov_text and tx3g to ASS, charset handling | 82 mov_text cases, 79 tx3g regressions | not measured | not measured | timing and memory |
| Subtitles, bitmap | PGS, DVB display state and colour lookup, DVD units | 229 PGS cases; 5,300 DVB display sets | PGS 1.11 | PGS heap 0.62, RSS 0.90 | full PGS recovery, DVB and DVD decoding |
| Playback | Matroska, MP4, TS and raw AVC/HEVC feeds; seeking; end-of-file continuation | 746 of 998 matrix rows | not measured | not measured | most profiles unmeasured |
| Codecs | H.264 8-bit 4:2:0; HEVC Main and Main10; AAC LC, HE, HEv2; AC-3 and E-AC-3; DTS core with XCh and X96 | 417/417 comparable conformance and FATE vectors | 1.0524 (17/17 within 1.20) | heap 0.92, RSS 0.82 | error concealment, unverified tool paths |
| Decode sessions | Bounded queues, clock-gated output, flush and seek, RGBA letterbox, stereo mix | with the codecs | 1.0524 | with the codecs | — |
| Capture | RGBA to YUV 4:2:0, MPEG-1 video, MPEG-TS mux | 15/15 quality cases against FFmpeg 9.0.2 | 1.1210 (15/15 within 1.20) | heap ≤0.548, total RSS ≤0.978 (15/15) | — |

## Measured revisions

Each row names the source revision a result above was measured on and, where the raw
samples are archived, their SHA-256. Update the row in the same change as the result.

The Matroska and startup and seek rows use the [gate build](quality.md#measuring-performance)
at `0e28298d038bea072629b3bad6389dcc806b4861` on a c8g.large pinned to CPU 1.
The HLS row uses the gate build at `a3e8adb2c1b67488a3168a37145a4bc9cc5aa80d`
on a c8g.large pinned to CPU 1. It covers all 71 cases in `corpus.json`, with
100 timing pairs of 50 operations and 30 memory pairs. Every video unit passes through
`contiguous_feed` inside the measured operation. Codec and capture rows at
`ceb5076` report the higher of two unpinned gate-build passes on one c8g.2xlarge.
The remaining rows retain their earlier measurements.
ISO BMFF, packet framing, PGS, Matroska audio clocks and playback timing, plus
codec and decode-session memory, await gate-build measurement with repository runners.

ISO BMFF open and index measurements use `cargo xtask isobmff-bench`, with
`--subset52` selecting the 52-operation set. Packet framing uses `cargo xtask framing`.

| Area | Revision | Raw samples SHA-256 |
|---|---|---|
| Matroska / WebM | `0e28298d038bea072629b3bad6389dcc806b4861` | `a20dcbbe804bfd21ee631cc42f61466d2400d5115abb8ea97dd243ef733ba74f` |
| Matroska startup and seek | `0e28298d038bea072629b3bad6389dcc806b4861` | `a20dcbbe804bfd21ee631cc42f61466d2400d5115abb8ea97dd243ef733ba74f` |
| ISO BMFF / MP4 | `f958c0b9624a8b7c7bd91b7d2a0c30299ae1dbed` | `fd718807e0ec1e6287bcb690226bf7056b79985796e74eba54f43544a034bc2c` |
| ISO BMFF comparison | `0a033ec3406aebd09d191298a546659e02c6b76c` | `f56d3fa78b8cc70222f58c7cfa9a17331b0924a9070c7ff6ecc6dc1f213561bc` |
| ISO BMFF open and index runtime | `ea4b941b4454bcc2ea0cd213999d64a07928ed7c` | `fca8e54cb9b2f0d55acc8ba9be2a5849992ae7996f3c3db551896477c3359525` |
| ISO BMFF open and index memory | `ea4b941b4454bcc2ea0cd213999d64a07928ed7c` | `46cf8ac9041b147c96108de6e09fde26c47e12fd10161f7eee9661db95f8c89a` |
| HLS segments | `a3e8adb2c1b67488a3168a37145a4bc9cc5aa80d` | `ea55e44697ea603b8ffc82e8e3f2b114888c3b8f9844c5a2dd1c6aa8c914079c` |
| Packet framing | `d225c35c1e2aa8a9b7dc4d61bb673a5728bd0ee6` | `b59476c45d8f31ac4694a264ce49cf12bda9bf9709e5cb6979ece0acb41cc812` |
| Codecs and decode sessions, runtime | `ceb5076659e00ca9a2d3f3cb55ee4513d6d0710f` | `12940dd90cf511dc37b8311946ccc43626f9e5e7a79cd3e7dfbeed68f48c77ed` |
| Capture, timing and memory | `ceb5076659e00ca9a2d3f3cb55ee4513d6d0710f` | `b75c8d6e6d64d294a385ec25b176e4ffb8f1a0d3c11b3ca956b3d352ac44c3b4` |

## Platforms

Qualification runs on native AArch64 Linux. 32-bit ARMv7 and x86_64 have no
performance runs yet.

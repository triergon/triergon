# Status

Everything is experimental. No module has yet passed every gate on its complete scope
(see [quality](quality.md)). Runtime is the worst per-case upper bound of
`T_triergon / T_ffmpeg` (ceiling 1.20, target 0.80). Memory is peak heap and total
resident peak RSS relative to FFmpeg. Unless noted, measurements are native AArch64 Linux
against FFmpeg 9.0.

| Area | Implemented | Verified against FFmpeg | Runtime | Memory | Open |
|---|---|---|---|---|---|
| Core | Charged storage, exact time, input windows, work budgets | 51,235 exact-arithmetic cases against an independent oracle | via dependents | via dependents | 32-bit target runs, fuzzing |
| Matroska / WebM | Incremental reader: lacing, header removal, bounded zlib, Cues and preroll, colour and static HDR, chapters, tags, attachments, side data | 116 workloads in six feature groups, including an external HDR sample, and four FATE demux comparisons | 1.1558 (116/116 within 1.20) | heap ≤0.627, RSS ≤0.966 | full EBML/Matroska scope |
| Matroska startup and seek | Startup and seek with lazy scan replay | 44 cases, 1,344 packets | 1.1920 (44/44 within 1.20) | heap ≤0.404, total RSS ≤0.812 | — |
| Matroska audio clocks | AAC, AC-3, E-AC-3 startup and forward clocks | 10 inputs | 0.17 | heap 0.07, RSS 0.60 | audio seek |
| ISO BMFF / MP4 | Regular and fragmented files, edit lists, seek and range plans, QuickTime compatibility, bounded AAC configuration, timing and sync prefixes; AVC, HEVC, AAC, AC-3, E-AC-3, DTS core, AV1, VP9 | 255 inputs, 1,020 operations (944 match in strict mode, 956 with bounded timing and sync prefixes) | 1.1739 (36/36 within 1.20, including all 12 timing and sync prefix cases) | heap ≤0.294, RSS ≤0.681 on the same 36 cases | 16 unmatched reference successes, full-set measurements |
| MPEG-TS | PSI, continuity, PES assembly; TS188 and M2TS192 | 272-request set (80 field matches, 23 open) | not measured | not measured | access-unit demux, broader profiles |
| HLS segments | Independent TS188 segments: AVC or HEVC with AAC, AC-3 or E-AC-3 | 71 packet comparisons, 10 measured workloads | 1.1817 (10/10 within 1.20) | heap ≤0.022, RSS ≤0.690 | other profiles |
| Packet framing | AVC/HEVC configuration and NAL framing, AV1, VP9, AC-3, E-AC-3, DTS core | 125 cases; DTS core byte-exact on 37,125 projections and 1,172 FATE packets | 1.2395 | heap 0.98, RSS 0.99 | above 1.20; full AVC/HEVC/ADTS grammar |
| Subtitles, text | SRT, WebVTT, ASS passthrough, mov_text and tx3g to ASS, charset handling | 82 mov_text cases, 79 tx3g regressions | not measured | not measured | timing and memory |
| Subtitles, bitmap | PGS, DVB display state and colour lookup, DVD units | 229 PGS cases; 5,300 DVB display sets | PGS 1.11 | PGS heap 0.62, RSS 0.90 | full PGS recovery, DVB and DVD decoding |
| Playback | Matroska, MP4, TS and raw AVC/HEVC feeds; seeking; end-of-file continuation | 120/120 continuation observations; 746 of 998 matrix rows | Matroska AVC/AAC feed within 1.25 | lower | most profiles unmeasured |
| Codecs | H.264 8-bit 4:2:0; HEVC Main and Main10; AAC LC, HE, HEv2; AC-3 and E-AC-3; DTS core with XCh and X96 | 417/417 comparable conformance and FATE vectors | 1.10 | heap 0.92, RSS 0.82 | error concealment, unverified tool paths |
| Decode sessions | Bounded queues, clock-gated output, flush and seek, RGBA letterbox, stereo mix | with the codecs | 1.10 | with the codecs | — |
| Capture | RGBA to YUV 4:2:0, MPEG-1 video, MPEG-TS mux | 15 quality cases | 1.11 | not measured | memory |

## Above the 1.20 ceiling

| Case | Worst upper bound |
|---|---|
| Packet framing: 125-case set | 1.2395 |

## Measured revisions

Each row names the source revision a result above was measured on and, where the raw
samples are archived, their SHA-256. Update the row in the same change as the result.

| Area | Revision | Raw samples SHA-256 |
|---|---|---|
| Matroska / WebM | `cd6ed16d7b37974a8e9d0e13c983965fc6ef7f64` | `ae86dddc2c4ee549c5289429002e145b05fd88fd718a63a413aae909b0af5ddd` |
| Matroska startup and seek | `fffb4a58ad0789338871d3032599e3116e5cb3d5` | `f449a8af7381be72dad4ffaba5615a99a0884a8bfb3a61b19c35f961f171f17a` |
| ISO BMFF / MP4 | `f3c319f437d58d97178f396dda55e49cccb53701` | `331bdde8ebfc4ebeab40709d1bdd02166fba4150583476e6bc4040a801c75474` |
| HLS segments | `763f05cf762dadd293328b33cc4dae54136695d4` | `de885f051789ceeff07556b40fcd3928c9e651d3c6b5ee3b193b8126d82435c1` |

## Platforms

Qualification runs on native AArch64 Linux. 32-bit ARMv7 and x86_64 have no
performance runs yet.

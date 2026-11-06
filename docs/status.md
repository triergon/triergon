# Status

Everything is experimental. No module has yet passed every gate on its complete scope
(see [quality](quality.md)). Runtime is the worst per-case upper bound of
`T_triergon / T_ffmpeg` (ceiling 1.20, target 0.80). Memory is peak heap and total
resident peak RSS relative to FFmpeg. Unless noted, measurements are native AArch64 Linux
against FFmpeg 9.0.

| Area | Implemented | Verified against FFmpeg | Runtime | Memory | Open |
|---|---|---|---|---|---|
| Core | Charged storage, exact time, input windows, work budgets | 51,235 exact-arithmetic cases against an independent oracle | via dependents | via dependents | 32-bit target runs, fuzzing |
| Matroska / WebM | Incremental reader: lacing, header removal, bounded zlib, Cues and preroll, colour and static HDR, chapters, tags, attachments, side data | 116 authored workloads in six feature groups, plus FATE lacing, zlib and zero-length block cases | 1.2411 (114/116 within 1.20) | lower on all 116 | 2 chapter cases above 1.20; full EBML/Matroska scope |
| Matroska startup and seek | Startup and seek with lazy scan replay | 44 cases, 1,344 packets | 1.1933 (44/44 within 1.20) | heap ≤0.404, RSS ≤0.735 | — |
| Matroska audio clocks | AAC, AC-3, E-AC-3 startup and forward clocks | 10 inputs | 0.17 | heap 0.07, RSS 0.60 | audio seek |
| ISO BMFF / MP4 | Regular and fragmented files, edit lists, seek and range plans, QuickTime compatibility; AVC, HEVC, AAC, AC-3, E-AC-3, DTS core, AV1, VP9 | 255 inputs, 1,020 operations (944 match) | diagnostic only: 16 of 24 timed rows within 1.25, worst 1.52 | not measured | timing and memory for the full set |
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
| Matroska: nested chapters (`chapters-depth`, `chapters-many`) | 1.2411, 1.2143 |

The Matroska startup and seek measurements use revision `f2a45541a1b5e23fa87e8641f2918b8d9d2f7c00`.
Raw samples SHA-256 `0ea82b902ea66e99693143471ffcac873dab28fb41ee3601f48dd05527211b51`.

The HLS measurements use revision `763f05cf762dadd293328b33cc4dae54136695d4`.
Raw samples SHA-256 `de885f051789ceeff07556b40fcd3928c9e651d3c6b5ee3b193b8126d82435c1`.

## Platforms

Qualification runs on native AArch64 Linux. 32-bit ARMv7 and x86_64 have no
performance runs yet.

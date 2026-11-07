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
| Core | Charged storage, exact time, input windows, work budgets | 51,235 exact-arithmetic cases against an independent oracle | via dependents | via dependents | 32-bit target runs, fuzzing |
| Matroska / WebM | Incremental reader: lacing, header removal, bounded zlib, Cues and preroll, colour and static HDR, chapters, tags, attachments, side data | 116 workloads in six feature groups, including an external HDR sample, and four FATE demux comparisons | 1.1924 (116/116 within 1.20) | heap ≤0.627, total RSS ≤0.965 | full EBML/Matroska scope, broader FATE coverage |
| Matroska startup and seek | Startup and seek with lazy scan replay | 44 cases, 1,344 packets | 1.1875 (44/44 within 1.20) | heap ≤0.404, total RSS ≤0.845 | — |
| Matroska Cues and preroll | Validated Cue positions, bounded fallback scans and all-track preroll | 8 cases with fresh, repeated and after-EOF seeks | 1.1849 (8/8 within 1.20) | heap ≤0.182, total RSS ≤0.910 | broader Matroska and FATE coverage |
| Matroska audio clocks | AAC, AC-3, E-AC-3 startup and forward clocks | 10 inputs | 0.17 | heap 0.07, RSS 0.60 | audio seek |
| ISO BMFF / MP4 | Regular and fragmented files, edit lists, seek and range plans, QuickTime compatibility, bounded AAC configuration and regular table recovery, timing, sync and terminal media prefixes; AVC, HEVC, AAC, AC-3, E-AC-3, DTS core, AV1, VP9 | 255 inputs, 1,020 operations (944 match in strict mode, 972 with compatibility policies) | 1.1496 (52/52 within 1.20) | heap ≤0.294, total RSS ≤0.711 on the same 52 cases | signed-clock and private-carriage error comparisons, full-set measurements |
| MPEG-TS | PSI, continuity, PES assembly; TS188 and M2TS192 | 272-request set (80 field matches, 23 open) | not measured | not measured | access-unit demux, broader profiles |
| HLS segments | Independent TS188 segments: AVC or HEVC with AAC, AC-3 or E-AC-3 | 71 packet comparisons, 10 measured workloads | 1.2223 (9/10 within 1.20) | heap ≤0.022, total RSS ≤0.739 | AVC/AC-3 at 44.1 kHz runtime, other profiles |
| Packet framing | AVC/HEVC configuration and NAL framing, AV1, VP9, AC-3, E-AC-3, DTS core | 125 cases; DTS core byte-exact on 37,125 projections and 1,172 FATE packets | 1.2395 | heap 0.98, RSS 0.99 | above 1.20; full AVC/HEVC/ADTS grammar |
| Subtitles, text | SRT, WebVTT, ASS passthrough, mov_text and tx3g to ASS, charset handling | 82 mov_text cases, 79 tx3g regressions | not measured | not measured | timing and memory |
| Subtitles, bitmap | PGS, DVB display state and colour lookup, DVD units | 229 PGS cases; 5,300 DVB display sets | PGS 1.11 | PGS heap 0.62, RSS 0.90 | full PGS recovery, DVB and DVD decoding |
| Playback | Matroska, MP4, TS and raw AVC/HEVC feeds; seeking; end-of-file continuation | 120/120 continuation observations; 746 of 998 matrix rows | see format rows | see format rows | most profiles unmeasured |
| Codecs | H.264 8-bit 4:2:0; HEVC Main and Main10; AAC LC, HE, HEv2; AC-3 and E-AC-3; DTS core with XCh and X96 | 417/417 comparable conformance and FATE vectors | 1.10 | heap 0.92, RSS 0.82 | error concealment, unverified tool paths |
| Decode sessions | Bounded queues, clock-gated output, flush and seek, RGBA letterbox, stereo mix | with the codecs | 1.10 | with the codecs | — |
| Capture | RGBA to YUV 4:2:0, MPEG-1 video, MPEG-TS mux | 15/15 quality cases against FFmpeg 9.0.2 | 1.1206 (15/15 within 1.20) | heap ≤0.548, total RSS ≤0.978 (15/15) | — |

## Above the 1.20 ceiling

| Case | Worst upper bound |
|---|---|
| Packet framing: 125-case set | 1.2395 |
| HLS: AVC/AC-3 at 44.1 kHz | 1.2223 |

## Measured revisions

Each row names the source revision a result above was measured on and, where the raw
samples are archived, their SHA-256. Update the row in the same change as the result.

| Area | Revision | Raw samples SHA-256 |
|---|---|---|
| Matroska / WebM | `9f541bc475adbeb7faa2a69be1d364d88c12cde5` | `c5f98c04c695b7f7808f483964cc707ce58805c2a124022bd739653d0ede6553` |
| Matroska FATE demux comparisons | `9f541bc475adbeb7faa2a69be1d364d88c12cde5` | `5f73707d4dc7d8a5689d301dcfd894ea4939523bd54259c900ecb53b8a7aa95a` |
| Matroska startup and seek | `9f541bc475adbeb7faa2a69be1d364d88c12cde5` | `79b4c35e628bf7a3b043046219d4ebf30d3840049c1632546f20ac41545f7196` |
| Matroska Cues and preroll | `9f541bc475adbeb7faa2a69be1d364d88c12cde5` | `79b4c35e628bf7a3b043046219d4ebf30d3840049c1632546f20ac41545f7196` |
| ISO BMFF / MP4 | `f958c0b9624a8b7c7bd91b7d2a0c30299ae1dbed` | `fd718807e0ec1e6287bcb690226bf7056b79985796e74eba54f43544a034bc2c` |
| HLS segments | `634f2bc48e08b885ece227baa2e552bb5ca24239` | `93b671ad84e8fa2f568245439c49ef51449f9eae06aeabe90c86e0f8805e86fe` |
| Capture, timing and memory | `e2cb0bea2c2fae823baef8169888ca84cf8b01f0` | `11f0572cf6d9294023340a35970f8fa5c4f6e75abfdab55d8e1fb67f0228f983` |
| Capture, earlier timing | `1831a2aa54e56d45472e171429a98fe878055449` | `fac200b915c2b9a984e5b91cba29e7a3c016690596f128524f46c8f2cb40b6e6` |

## Platforms

Qualification runs on native AArch64 Linux. 32-bit ARMv7 and x86_64 have no
performance runs yet.

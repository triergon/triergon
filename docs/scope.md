# Scope and acceptance contract

Status: the historical frozen K0–K5 snapshot (116 workloads) passed native Mac ARM64 gates on its recorded source. Current development qualification is native Linux ARM64; later source, complete modules and shipping-target gates remain open. Date: 2026-10-31. See [implementation evidence](implementation.md).

## Owner requirements

- Build independently from format specifications; use existing projects as research/comparison targets.
- Dependencies are exceptions. Each must fit the exact contract (“Cinderella”: fits perfectly), be pure safe Rust through the selected transitive graph, be mature and optimal for the workload, and beat the cost of bootstrapping that bounded component.
- FFmpeg is the feature and performance reference for each selected module.
- Full parity is required for the explicitly selected features/modules. “Modern FFmpeg” is the long-term direction, not the v0 completion claim.
- Initial-delivery runtime ceiling: **`T_rust / T_ffmpeg <= 1.20`**, using the
  upper paired 95% bound for every required case. The owner authorized 1.25 in the
  [runtime decision](../research/decisions/initial-delivery-runtime.md) and lowered it
  to 1.20 in the [2026-11-05 decision](../research/decisions/2026-11-05-runtime-ceiling-1.20.md).
  **20% less execution time**, or a ratio at most **0.80**, remains the optimization
  target; the earlier 80% wording was never an 80% runtime-reduction requirement.
- Lower memory use is required in addition to speed and correctness. No numerical memory-reduction percentage was specified; see the measurable gate in `testing.md`.
- These are acceptance targets, not measured results or guarantees of feasibility. A module that misses them stays experimental.

## First consumer and replacement boundary

Streem runs on 32-bit ARM LG webOS and on desktop simulators. Its hardware player decodes movie video and ordinary audio. The first replacement is container demuxing, metadata extraction and seeking, then the remaining FFmpeg subtitle dependency. This does not replace LG's player, SDL, libass, transport authorization, server negotiation, playback clocks, A/V scheduling or user watch-state reporting.

The current source snapshot is native-jelly-streem-cinema commit `9c7c0fb5f`. Authoritative implementation evidence is in `research/streem-inventory.json`; older FFmpeg design documents and the opening comment of `ff.rs` contain obsolete details (old system-library ABI and raw AAC output). Current build code pins bundled FFmpeg 9.0; AAC output is reframed to ADTS. Several packet adapters already exist in Rust, so do not count them as C functionality that still needs porting.

The generic library must expose source facts and compressed packets. Streem-specific queue limits, HDR policy, presentation, retry strategy and decoder adaptation remain in its integration adapter. No silent loss of subtitle tracks, colour information, timing or codec configuration is an acceptable fallback.

## Modules and feature boundaries

| ID | Module | Selected contract | FFmpeg comparison |
|---|---|---|---|
| M00 | Core primitives | Signed PTS/DTS, explicit unknowns, exact rational rescale, bounded readers, checked lengths, owned/borrowed packet storage and budgets | libavutil time arithmetic and equivalent data/reader work |
| M01 | EBML / Matroska / WebM | Streaming and seekable inputs; Segment/Cluster unknown lengths; SimpleBlock/BlockGroup; all three lacing schemes; codec private data; Cues/SeekHead and bounded missing-index policy; chapters, tags, attachments and track flags; delay/preroll/discard padding; header removal and bounded zlib content compression; colour/HDR and opaque codec side data | Matroska demux/probe/seek and selected FATE cases |
| M02 | Packet adapters | H.264/HEVC length-prefix and Annex-B framing/configuration; AV1 configuration/OBU framing; VP9 framing; AAC ASC/ADTS including core-rate handling; AC3/EAC3 framing and DTS-core extraction where selected | Equivalent FFmpeg bitstream filters/parsers; separately normalize intentional hardware framing |
| M03 | ISO BMFF | Regular MP4/MOV and fragmented MP4; front/end moov; stts/ctts and signed offsets; edit lists; sample descriptions/config changes; stss/random access; codec private data, colour/HDR and subtitle samples | MOV/MP4 demux, edit-list/fragment FATE cases and seek receipts |
| M04 | MPEG-TS | 188-byte TS and selected 192-byte M2TS input; PAT/PMT, continuity, PES assembly, PTS/DTS wrap and discontinuity; selected AVC/HEVC/AAC/AC3/EAC3 carriage | MPEG-TS demux/parser cases; HLS segment continuity tests |
| M05 | Subtitle packets/decoders | Preserve authored ASS and text packets; SRT/WebVTT/mov_text extraction; PGS/VobSub/DVB bitmap decoding and palette/geometry/timing for selected variants | Cue/event and normalized RGBA bitmap comparisons |
| M06+ | Future independent modules | Muxing, audio decode/encode, selected software video codecs, filters, scaling/resampling, protocols and CLI compatibility | Each needs its own frozen feature manifest and the same parity/speed/memory gates |

The table is the intended first release-family scope, not an assertion that any module is implemented. Split profiles into separately named experimental lanes without calling partial coverage full module parity. The exact accepted codec/profile/container combinations must be enumerated before implementation; a library being able to label a codec is not decoding or playback support.

## Original design milestones and current execution

The grouping below records the initial design plan. Current execution follows the
[offboarding ledger](offboarding-target.md): independent format lanes now advance
in parallel against the shared core, while builds and numerical qualification are
coordinated in one queue. All feature, runtime and memory gates remain unchanged.

1. **D0 — design and comparison (this deliverable).** Inventory Streem, audit alternative architectures and licenses, run a small reproducible compatibility spike, define acceptance and dependency gates. No dependency is admitted by this phase.
2. **H0 — shared harness first.** Pin the FFmpeg source/configuration, import selected FATE test identities, audit/fetch sample assets selectively, define result schemas, add canonical packet/metadata/seek comparisons and matched benchmark workers. Report every pending or blocked case.
3. **C0 — core bootstrapping.** Implement M00 with no third-party production dependencies. Establish resource/work accounting, fuzz targets and 32-bit checks before a container reader.
4. **K0 — first vertical slice.** MKV AVC/HEVC + AAC/AC3/EAC3 compressed passthrough, subtitles enumerated, exact codec-private data and rational timestamps, EOF behavior and previous-random-access seeking. This is a named slice of M01/M02, not full Matroska parity. Run through the Streem simulator and then its hardware route with synthetic/authorized fixtures.
5. **K1 — close M01/M02.** Complete the selected Matroska/packet feature manifest, including HDR/side data, compression, lacing and corruption/resource cases. Pass all three gates on every declared release target.
6. **I0/T0 — MP4 then TS.** Complete M03 and M04 separately, retaining the same API. HLS orchestration and HTTP remain owned by Streem; the toolkit only handles segment media and discontinuity facts.
7. **S0 — subtitle independence.** Complete M05, including image cues. Text rasterization remains libass in Streem. Production FFmpeg removal is considered only when all enabled required lanes have accepted replacements.
8. **E0+ — broader toolkit.** Add one codec, muxer or processing family at a time. Do not start an undifferentiated “all codecs” rewrite. Select later work based on users, specifications and a measurable reference contract.

## Migration and full-parity definition

Streem gets an explicit backend selection and a narrow adapter. Unsupported input can select the retained FFmpeg backend before playback starts. Midstream fallback requires a new demux generation and an explicit seek/discontinuity transaction; never concatenate old and new decoder timelines or synthesize EOF from a transport fault.

Full parity for a selected lane means every declared valid combination, metadata/timing property and required seek behavior matches the contract across the complete approved corpus. Missing tests, unknown sample licenses, platform skips and unexplained differences keep the lane unaccepted. Bugs or unsafe/unbounded behavior in the reference are investigated against the specification, not blindly reproduced. Intentional differences need an explicit, reviewed case rule rather than a universal normalizer.

## What this phase has not established

The shared-core K0–K5 snapshot qualified on native Mac ARM64 across all 116
frozen workloads. Those results apply to that source snapshot and the selected
operations. The [offboarding ledger](offboarding-target.md) records later
native Linux receipts and opt-in Streem integration and device verification.
Complete module and shipping qualification remain open. The earlier candidate
spike informs the dependency decision. See [implementation evidence](implementation.md)
for the subsequent slices.

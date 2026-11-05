# Overarching target: remove Streem's runtime FFmpeg dependency

Triergon's goal is full Streem adoption across TV playback, the desktop
simulator, and development capture, followed by removal of FFmpeg from
application runtime and packaging. The shared-core K0–K5 source snapshot
passed its native Mac ARM64 gates on all 116 frozen workloads. Current
adoption requires evidence from the revisions that Streem will pin.

Completion means every enabled Streem application-runtime FFmpeg responsibility
has a qualified replacement, including release playback, simulator playback and
development capture. The integrated application must build and pass its playback
regressions without loading, bundling or launching FFmpeg, and its supported target
devices must be verified. Pure safe Rust and the dependency-admission policy apply to the new
core. Source feature parity, initial-delivery runtime at most 1.25 of equivalent FFmpeg work and
lower peak live heap and total peak RSS remain hard gates for every selected lane.
Missing cases or evidence keep a lane open.
The 0.80 runtime ratio remains the optimization target under the
[owner's later runtime decision](../research/decisions/initial-delivery-runtime.md).

Current development performance qualification is native Linux ARM64, following
the [owner's target decision](../research/decisions/native-linux-qualification.md).
Historical Mac receipts keep their original scope. Actual 32-bit
webOS builds and TV validation remain required before shipping offboarding.

## Adoption sequence

1. Maintain a public Triergon development baseline on `main` and land
   follow-up work through reviewed PRs. Record the exact source, inputs,
   workers and remaining failures for each qualification run.
2. Qualify the remaining library operations and prepare an opt-in Streem
   adapter pinned to a reviewed revision. Start with a defined Matroska
   playback profile and retain the existing backend for unsupported inputs.
   Validate HTTP range I/O, cancellation, backpressure, track changes, seek,
   resume and teardown through the application's normal entry points.
3. Expand the TV route to the remaining enabled formats and subtitles.
   Verify the shipping 32-bit ARM build and test the actual TV playback
   paths, including clocks, routing and audio/subtitle behavior.
4. Adopt and verify the desktop simulator and development capture paths.
   Audit the completed app, package contents and enabled runtime features,
   then validate a clean install without an FFmpeg runtime dependency.

The selected AVC/HEVC video startup and seek operation matched all 44
frozen cases on native Linux ARM64. All 44 passed both memory gates and 43
passed delivery timing. The remaining fresh no-Cues seek was 1.2543× against
the 1.25× ceiling. This receipt covers the immutable-input library route.
See the [selected video evidence](../research/qualification/matroska-video-linux-arm64/README.md).

## Completion ledger

| ID | Required work | Completion evidence |
|---|---|---|
| O00 | Refresh the actual Streem call-site/component inventory | Complete for the recorded revision: [source identities, call sites and responsibility assignments](../research/offboarding/streem-source-inventory/README.md) |
| O10 | Core input, exact time, ownership, budgets and cancellation | Sequential/range/unknown-length input; EOF versus transport failure; retained-capacity and work bounds; malformed/fuzz and target checks |
| O20 | Matroska/WebM | Complete selected M01 contract, including compression, lacing, Cues/preroll, HDR/side data, chapters/tags/attachments and missing-index policy; full three-gate qualification |
| O21 | Packet framing/probing helpers | Selected H.264/HEVC, AV1/VP9, AAC, AC3/EAC3 and DTS configuration/framing/probing parity; preserve existing Rust consumer adapters without copying GPL code into this core |
| O30 | MP4/MOV | Regular and fragmented input, edit lists, signed timing, indexes/preroll, configuration changes, codec private data, colour/HDR and subtitle carriage; full gates |
| O31 | TS/M2TS and elementary streams | PSI/PES/continuity, wrap/discontinuity, selected codec carriage, HLS media segments and enabled raw H.264/HEVC paths; full gates |
| O40 | Subtitle independence | PGS/VobSub/DVB bitmap decode, text/configuration preservation and required charset/side-data behavior; independent output parity and full gates |
| O50 | Streem integration | Backend adapter, track selection, buffers/backpressure, generation changes, retries, accurate seek/resume and teardown; preserve route, auth, clocks and watch state |
| O60 | Target qualification | Simulator/integration tests, native Linux gates, 32-bit ARM build and actual TV tests, with source/binary/input provenance |
| O61 | Desktop simulator playback | Replace external FFmpeg video/audio subprocesses with qualified decoding/output paths; preserve enabled simulator behavior and record platform boundaries |
| O62 | Development capture | Replace the enabled MPEG-1 encoder, TS muxer and RGBA/YUV conversion; freeze each contract and pass the same applicable gates |
| O70 | Runtime/package removal | No enabled application-runtime FFmpeg imports, dynamic loads, subprocesses, staged libraries or unresolved call sites; clean install and end-to-end verification without an installed FFmpeg fallback |

The frozen O00 source inventory records Streem revision
`9c7c0fb5f7316c92ab8086beb19db03d5855dd6e`. The historical K0–K5 receipt qualifies selected packet/header/zlib/Cues/static-colour
and bounded segment-metadata slices. O00's source review is complete for that
revision; all eleven implementation/integration/qualification/removal milestones
remain open. Configured registries and actual package/runtime audits are later gates.
The earlier experimental Root3477 integration froze a 5,095-file snapshot that
passed all 13 native Linux ARM64 functional gates with 3,477 passed, zero
failed and one ignored test per profile. It includes the typed playback
crate with the explicit terminal-MDAT operation, the compact AVC/HEVC views
with packed HEVC NAL descriptors, the native PGS first-event and mov_text
receiver APIs, and capture85.
The [implementation evidence](implementation.md) separates standalone
native PGS numerical qualification from the integrated root's functional
results. D1–D6 and O10–O70 remain open. The current Streem mov_text route,
application integration and complete numerical qualification remain separate
from these selected APIs.

The successor adds forward playback of regular front-moov files across finite
MDATs and incorporates the [desktop decoder workspace](../crates/sim/README.md).
The combined MP4-to-decoder test checks clock gating, repeatable output after
seek and teardown with actual AVC and AAC packets. The standalone D4 native
receipt is tied to its own source and workers. The successor's native
qualification and Streem integration remain open.

## Original-file playback feed

The Matroska library feed supports selected AVC/AAC and HEVC/AC-3/E-AC-3
tracks. HEVC spans use four-byte Annex B start codes and prepend the hvcC
VPS/SPS/PPS on IRAP. Dolby audio borrows the original packet after bounding
and checking syncframe extents. The source packet and its timing survive both
projections. The host regression covers all 52 packets of a mixed HEVC/Dolby
file and checks the HEVC seek and E-AC-3 track switch against stale feed
bindings. Native timing and memory, Streem adoption and device proof remain
open for this feed. [Fixture and oracle](../crates/playback/tests/fixtures/mkv-feed/README.md)

## Raw AVC and HEVC input

The common playback probe selects Matroska, MP4, TS188, M2TS192, AVC
and HEVC from a single source, replaying the accepted prefix without
rewinding. The Annex B reader preserves validated access-unit bytes,
key flags and transport lifecycle. The active decoder configuration reports the source
parameter sets and cropped raster. The host comparison covers 48
authored and 243 intact FATE access units and each stream's initial
configuration. AVC and the optional HEVC timing view expose the fields
physically present in the active SPS. The sink-ready feed carries original
Annex B spans, configuration revisions and an explicit submission clock.
Per-picture presentation timing, Streem adoption and native numerical
qualification remain open.
[Playback API and scope](../crates/playback/README.md#raw-avc-and-hevc-input)

## Range-backed MP4 playback seek

The bounded AAC sample-entry reader extracts the exact AudioSpecificConfig
from an ISO `esds` or QuickTime `wave/esds` description without requiring
a populated sample table. The same descriptor walk serves the encoded
catalogue and the playback adapter. Unit tests cover byte offsets, descriptor
limits and duplicate refusal.

The SourcePhysical player now has a generation-bound sync seek over its
checked sample rows. It preserves selected prefix state and explicitly
requested audio preroll, then resumes video at a container sync sample
in the target's description run. The host fixture checks exact source
payloads and the actual late read offset. Complete range-backed native
playback also supports a generation-bound seek for the pinned MOV profile.
The planner rescales the selected reference anchor for each track. The
engine resumes from those anchors and replays the pinned packet order.
The encoded range view also retains its validated edit plans for seek.
An authored B-frame AVC/AAC fixture checks the selected decode timestamp,
first byte range, and the original bytes and clocks at the landing.
Five FFprobe comparisons cover regular, fragmented, repeated-edit,
description-change and mixed-clock AVC/AAC playback. SourcePhysical can
start with front metadata on a range source and switch to a complete index
of the same entity for seek. The explicit native progressive player can
also reopen the same entity as a complete pinned native range plan and
resume in native seek order. Host tests verify the late range, exact
fragment packets, paused handoff and sink generation in both views.
Overlapping native clock queries, native recovery seeks, Streem adoption
and numerical qualification remain open.
[Playback API and scope](../crates/playback/README.md#range-backed-mp4-sync-seek)

## Additional application-runtime surfaces

The TV release component list does not enable movie software decoders. Its normal
movie decode/output remains LG hardware, and text rendering remains libass. These
are existing consumer/platform responsibilities, not FFmpeg components to rebuild
as part of the initial TV offboarding.

Desktop simulator playback currently launches external FFmpeg for video and audio.
Development capture uses MPEG-1 encoding, TS muxing and swscale paths excluded by
RELEASE=1. These uses are required open milestones O61/O62 in the complete target. They need
replacements and explicit decoder/encoder/conversion feature contracts and gates.
Their presence does not imply the TV release needs a universal software movie
decoder. A platform decoder in a consumer adapter is a separate, declared boundary;
it does not admit unsafe/native dependencies into the safe Rust core.

External FFmpeg remains the correctness/performance oracle in the independent test
harness. Fixture generation and explicitly external development tools are recorded
separately from application runtime dependencies. Removing the application's
runtime dependency does not remove its independent reference oracle.

The final inventory and enabled-feature scan must be refreshed against the actual
integration revision. No milestone can be marked complete solely because a narrow
format slice passes, a library can name a codec, a host build compiles, or FFmpeg
files have been deleted while enabled functionality remains unsupported.

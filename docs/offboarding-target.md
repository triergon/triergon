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
core. Source feature parity, initial-delivery runtime at most 1.20 of equivalent FFmpeg work and
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

The selected AVC/HEVC video startup and seek operation passed all 44
frozen parity, runtime, heap and total peak RSS cases on native Linux ARM64.
The worst runtime upper bound was 1.2397× against the 1.25× ceiling.
The receipt covers the immutable-input library route and retains the
earlier failed comparisons by revision.
See the [latest selected video evidence](../research/qualification/matroska-video-linux-arm64/scan-replay-location/README.md).

The opt-in Streem preview at `c0046d7a3aadc89aa875a7b77f871ec60b350063`
pins Triergon `bf8e7758c4c5ebd6ecb3589285d6eec34fa8e425`. Its 32-bit debug
package, SHA-256 `ec692146c24384b929026c48c6bf5f1ad5c1d9e7899b747b670d2dbbc2e16e02`,
installed on an LG G4 running webOS 10.3.1. The installed executables and
shared library matched the package. Starfish accepted audio and video
feeds from two synthetic AVC/AAC catalog items, one in MP4 and one in
Matroska. Both sessions paused, resumed, sought to 52 seconds and stopped
back to detail after teardown. Each run included HTTP 206 responses, and
Starfish reported frame-ready callbacks after each seek. The app's memory
maps contained no FFmpeg libraries. The device logs are retained privately.
This evidence covers the recorded debug package and these two hardware
playback profiles. Wider media coverage and shipping-build qualification
remain open.

The two-audio-track preview at `5bd1f877a3c264e7f79d532707ee0073bca4f64d`
pins Triergon `942b8ee2f41ab6277c80c0b0b82a8f7e68b9ee09`. Its debug IPK,
SHA-256 `037f7b685de6e3e98372a82b0c8e812f236eb2eafbb5233fe99bda51a14632e7`,
passed two catalog replays on the same G4. Each AVC/AAC file resumed at
12 seconds, switched to the other audio track through the track menu,
paused and resumed, then sought over HTTP ranges. Starfish accepted both
feeds and reported frames after the seek. Both Stop reports named the
new audio track and returned to the item detail after teardown. The IPK
and app process were checked for FFmpeg libraries. Simulator replays
verified the distinct PCM tones before and after the switch, including
a separate cold resume into the secondary track in each container.
These runs cover the recorded MP4 and Matroska profiles. The private
receipts bind the fixtures, exact binaries, and logs.

The feature-length MP4 preview at `a387d2d9c66a9583d9de0e5a8ae4fa910a8ec31f`
pins Triergon `cd66cb9b073e006c11e18f7227fb51bd79c22781`. Its 32-bit
debug IPK, SHA-256 `c895589d5c1f63d33002867c54bea7658499b42d336c42a159f4fc9e97dfb1e8`,
resumed a synthetic two-hour AVC/AAC MP4 at one hour on the same G4.
Starfish accepted both feeds after 12.4 seconds. The forward
seek landed in 2.5 seconds and the frame clock advanced with
a single Starfish load for the session. Pause, Resume, and Stop succeeded,
and the mock server recorded the selected audio track in the Stop report.
The installed binaries matched the IPK, the HTTP server returned ranges,
and the app process maps were FFmpeg-free. The screen-on request was
declined in Active Standby. The simulator decoded the picture and
identified the secondary AAC track by its PCM tone.

The previous range preview needed 64.3 seconds before feeding and 71.1
seconds to recover its forward seek after a reload on the same fixture.
The current route reuses checked track state for regular seeks and
releases the optional cache under pool pressure. The recorded hardware
times are single-fixture observations. The reviewed library tree also
passed 73 native Linux ARM64 functional tests. Full numerical
qualification remains open.

The AV1 and VP9 preview at Streem `b1431a78ec68f1293648c17cdf5ab0f5be563cbc`
pins Triergon `b1fb7ef0275866a8d6fc09fc9bf38db2af116d5a`. Its 32-bit debug IPK,
SHA-256 `c9c7cbf3513a0eaa835394f359b2d51c490cf1150adbea97f738382bc3816677`,
passed four catalog replays on the G4. Each codec used a 144-second synthetic
AAC file in MP4 and Matroska. The tests checked HTTP 206 reads, resume at 12
seconds, pause, a single Starfish Load across the forward seek, accepted audio
and video feeds, frame callbacks after seek and Stop. The process maps
showed zero FFmpeg libraries. The settings service reported SDR while the
set remained in Active Standby. The simulator exercised the same routes
through its video clock sink and decoded 659 Hz AAC before and after seek.
A separate simulator fixture verified AV1 configuration replacement before
the first Matroska video packet. MP4 admission requires consistent VP9 colour
across its sample descriptions for the platform's single sourceInfo envelope.

The reviewed library tree passed 74 native Linux ARM64 functional tests for
MP4 sync and the container playback suite. These receipts cover the selected
application playback paths. Full per-case numerical qualification, remaining
carriage profiles and shipping adoption remain open.

The DTS core preview at Streem `3c16fb4eb74914eed6e70dcd1c813a3468ebd78e`
pins Triergon `4bd3d8f12b877fd71848e9bd718520309f022a19`. Five mock-catalog simulator
replays used Matroska, regular and fragmented MP4, QuickTime and a separate
14-bit packed Matroska file. The cases exercised HTTP 206, the selected
DTS track's PCM, resume, pause, a seek on the same Load and Stop. The
simulator used the FFmpeg-free build. Three refusal cases covered an
absent core, an untimestamped later lace and an edit that needed PCM trim.
The 32-bit debug IPK SHA-256 is
`3352e26c35036af755a68643bd6b25c3dc77f8e73c41c5478068755013b34e62`.
Its four packaged ELF files passed the FFmpeg-free link audit.

The same package ran two DTS catalog fixtures on the G4. Both resumed
at 12 seconds and Starfish accepted the AVC and DTS feeds. The MP4
Back-control replay paused, resumed and sought to the 30-second keyframe
on a single Load. The app returned to detail and the mock Plex server
acknowledged the stopped position and DTS track ID. An earlier MP4
replay was backgrounded before Stop was reported. The Matroska replay
resumed at 32 seconds after two in-place watchdog retries and a fresh
Load. The set remained in Active Standby, so panel picture and audible
output were unobserved.

The reviewed DTS library tree passed 126 native Linux ARM64 functional
tests. Core bytes matched FFmpeg on 37,125 authored packet projections
and all 1,172 packets of the FATE DTS-HD MA sample and its three remuxes.
The edited range view keeps packet clocks and sample-entry trim separate.
Streem admits MP4 DTS edits that can submit whole access units. Full
DTS target coverage and numerical qualification remain open.

The progressive conversion preview at Streem
`15cd9563cc3f16d1810d236523a2b08b53ce813c` retains the same Triergon pin.
Its FFmpeg-free simulator passed AVC/AAC Matroska remux through Plex
and conversion through Jellyfin. Each replay resumed at 12 seconds,
paused and resumed, and sought through a new encoder response. The
simulator decoded distinct PCM tones before and after seek. Two cut
Jellyfin responses, one before and one after producer selection, reached
the transport error path and retired the encoder when leaving playback.

The preview's 32-bit debug IPK, SHA-256
`c0b28af510cf748bea6dd667831dd6c454f775d0027fa2309a5e3e3de3bd47ab`,
passed the FFmpeg-free link audit on all four packaged ELFs. On the G4,
the Plex and Jellyfin replays paused, resumed and each sought through
a new chunked stream at 38 seconds. Starfish accepted audio and video
on both Loads of each replay. The mock providers acknowledged the final
stopped position and the old encoder retirement. Both replays used
sequential reads and the app process maps contained no FFmpeg libraries.
The set remained in Active Standby. The hardware observations cover
Starfish's accepted feeds and advancing playback clock.

The Matroska seek follow-up at Streem
`4d81cb919f4e4daaba2845651c34f111dbcf4a96` credits ongoing validation to
the existing seek watchdog. The same DTS fixture first reproduced the
retries on the conversion preview. The follow-up's 32-bit debug IPK,
SHA-256 `ca2cddc539021a7d0255a928a015c6975be694e4f1c6bccfc3674973b1a8ebd7`,
resumed at 12 seconds on the G4. It accepted native Seek to the
30-second keyframe for a 31.582-second request. HTTP ranges, post-seek
audio and video feeds, and Back teardown all completed on one Load.
The simulator also passed the same fixture. Broader seek and shipping
qualification remain open.

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
| O62 | Development capture | Capture85 passed quality and delivery timing on all 15 selected MPEG-1/TS operations. Peak live heap, total peak RSS and target execution remain open. [Capture evidence](../research/qualification/capture85-linux-arm64/README.md) |
| O70 | Runtime/package removal | No enabled application-runtime FFmpeg imports, dynamic loads, subprocesses, staged libraries or unresolved call sites; clean install and end-to-end verification without an installed FFmpeg fallback |

### Preview coverage and remaining work

The 2026-11-06 snapshot reviews the opt-in Streem preview at
`f73227a57a74c2d181a6c6513435d24e11b7dea1` against the
[42-row O00 inventory](../research/offboarding/streem-source-inventory/scope-matrix.md).
The tally excludes 11 rows for retained Rust or platform responsibilities and
external reference tooling (INV16–21, INV25, INV33, INV36, INV41 and INV42).
A row counts when the preview has an FFmpeg-free implementation for at least
one of its profiles. Every row has equal weight. This gives **27 of 31
responsibilities with some preview coverage (87%)**. Some of those rows
have further profiles, qualification or release work.

| Surface | O00 rows | With preview coverage |
|---|---|---:|
| Input, demux, metadata and probing | INV03–15 | 10/13 |
| Subtitle carriage and decode | INV22–24 | 3/3 |
| Simulator sinks and decoding | INV26–30 | 5/5 |
| Development capture | INV31, INV32, INV34, INV35 | 4/4 |
| Runtime and packaging | INV01, INV02, INV37–40 | 5/6 |

For Original playback, the preview routes selected profiles in **two of five
enabled input families (40%)**, Matroska/WebM and MP4/MOV. MPEG-TS, raw AVC and
raw HEVC are the other three families. The independent HLS and progressive
conversion paths add TS, giving **three of five families with some coverage
across all delivery routes (60%)**.

| Area | Remaining implementation or adoption | Verification still due |
|---|---|---|
| Original playback (O20, O21, O30, O31, O50) | Route Original TS188/M2TS and the existing raw AVC/HEVC reader through Streem. Carry over the existing Dolby Vision route. Cover enabled audio carriage outside the current AAC, AC-3, E-AC-3 and DTS profiles where the platform admits it, including Opus and TrueHD. | Verify selected tracks, timing, seeks, HTTP ranges and teardown across the remaining combinations. |
| Server conversions and HLS (O30, O31, O50) | Finish sequential MP4 and wider TS carriage. The progressive TS188 adapter handles negotiated AVC/AAC responses. The mock explicitly supplies TS and the current Jellyfin progressive profile requests MKV. | Close the progressive TS runtime misses. Verify stock-server selection and G4 playback for the adopted revision. Requalify HLS against the current delivery ceiling. |
| Subtitles (O40) | Finish unresolved DVB receiver, source timing and container cases. Extend the existing text and bitmap paths to the remaining selected variants. | Run independent parity and delivery gates for the full selected matrix and check rendering and lifecycle in the app. |
| Developer probe (INV14 and INV15) | Replace the local `ffprobe` trigger and its filter availability diagnostic, or record an explicit disposition for the trigger. | Verify bounded local inspection and diagnostics on the development build. |
| Desktop simulator and native macOS app (O61, O70) | Wire the FFmpeg-free preview into the native macOS app packager (INV40). The opt-in simulator has selectors for all six legacy codec lanes. Selected AVC High 10 and HEVC Range Extension decode cases remain open. | Close selected decode and output qualification and verify the shipped platform packages and sinks. |
| Capture (O62) | The opt-in build already uses Triergon conversion, MPEG-1 encoding and TS muxing. | Admit the reference to heap and RSS measurement, then verify the development capture session on target. |
| Release switch (O60, O70) | Make the qualified backend the default after the remaining routes are covered. | Verify a clean install, the packaged files and enabled runtime paths without FFmpeg. |

The table below applies the [1.20× v2 cutoff](../research/decisions/2026-11-05-runtime-ceiling-1.20.md)
to retained runtime bounds for selected operations. The original v1 measurements
and verdicts stay in their receipts.

| Selected native operation | Parity or quality | Runtime at or below 1.20× | Lower heap and total peak RSS |
|---|---:|---:|---:|
| [Independent HLS](../research/qualification/hls-av-family-linux-arm64/optimization.md) | 71/71 literals | 9/10 | 10/10 |
| [Matroska video startup and seek](../research/qualification/matroska-video-linux-arm64/scan-replay-location/input-health-results.json) | 44/44 cases | 43/44 | 44/44 |
| [Matroska forward audio clock](../research/qualification/matroska-audio-linux-arm64/README.md) | 10/10 cases | 10/10 | 10/10 |
| [Progressive AVC/AAC TS188](https://github.com/triergon/triergon/pull/27), `84ad176` | 3/3 cases | 1/3 | 3/3 |
| [Capture85](../research/qualification/capture85-linux-arm64/README.md) | 15/15 quality cases | 15/15 | 0/15 admitted |

The 0.80× optimization target remains separate. Framing44 still has an open
RSS gate and a retained runtime bound of 1.2395×. The preview's eight simulator
replays and FFmpeg-free ARM debug package cover the pinned app revision.
G4 playback and release adoption for that pin remain open.

The frozen O00 source inventory records Streem revision
`9c7c0fb5f7316c92ab8086beb19db03d5855dd6e`. The historical K0–K5 receipt qualifies selected packet/header/zlib/Cues/static-colour
and bounded segment-metadata slices. O00's source review is complete for that
revision; all eleven implementation/integration/qualification/removal milestones
remain open. Shipping package and runtime audits are still required.
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
bindings. The combined sink feed and device behavior still need
qualification. [Fixture and oracle](../crates/playback/tests/fixtures/mkv-feed/README.md)

The selected AAC-LC, AC-3 and E-AC-3 startup and forward packet clocks
matched all ten native Linux ARM64 cases and 75 packets. All ten passed
runtime, live heap and total peak RSS. The source clock also retains the
damaged AC-3 control's original bytes and timing. The receipt covers the
selected forward audio clock operation. The debug evidence above covers AAC
playback on the G4. Full audio seek and wider combined playback qualification
remain open.
[Native audio clock evidence](../research/qualification/matroska-audio-linux-arm64/README.md)

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
The encoded range view retains its validated edit plans for seek and
exposes the selected encoded-audio trim on the emitted packet. The plan
must match both the track and movie time bases. An authored B-frame AVC/AAC
fixture checks the selected decode timestamp, first byte range, and the
original bytes, clocks and trim at the landing.
Seek input accounting ends when a complete reference packet reaches the
target time. The original presentation target continues to govern reordered
preroll. [Seek budget regression](https://github.com/triergon/triergon/pull/14)
Selected QuickTime AVC and HEVC entries and the leading AC-3 and E-AC-3
edit profile are also admitted. The Dolby profile validates the full
configuration and requires an edit duration that converts to whole
samples on the configured clock. QuickTime AAC playback requires an
unambiguous AAC configuration. The range path also accepts selected DTS
core descriptions and exposes regular rate-one edit clocks and trim.
Five FFprobe comparisons cover regular, fragmented, repeated-edit,
description-change and mixed-clock AVC/AAC playback. SourcePhysical can
start with front metadata on a range source and switch to a complete index
of the same entity for seek. The explicit native progressive player can
also reopen the same entity as a complete pinned native range plan and
resume in native seek order. Host tests verify the late range, exact
fragment packets, paused handoff and sink generation in both views.
The opt-in Streem debug adapter uses the native range path. Overlapping native
clock queries, native recovery seeks, shipping adoption and full numerical
qualification remain open.
[Playback API and scope](../crates/playback/README.md#range-backed-mp4-sync-seek)

## Independent TS188 HLS segments

Four frozen independent TS188 segment cases matched the pinned FFmpeg
parser and app clock, and passed delivery timing, peak live heap and total
peak RSS on native Linux ARM64. The [HLS receipt](../research/qualification/hls-avc-aac-linux-arm64/README.md)
binds these four AVC/AAC cases to the earlier opt-in integration revision
`3f7547922e7be74ab21baf12f92851a161388c33`.

`HlsMediaSegmentReader` adds negotiated AVC or HEVC with ADTS AAC, AC-3
or E-AC-3 in independent TS188 segments. E-AC-3 admission covers a single
independent substream. The selected reader checks the program map,
preserves source video clocks and seals only after confirmed transport
EOF. The original AVC/AAC reader keeps its existing API and clock views.

The preview at Streem `55567117abf02234bd52e83066bc931f5abf9715` pins
Triergon `4d4c4cfb5326700f01b432344db10cece0e3ac23`. In a local packet
comparison against FFmpeg, 60 synthetic segments across six codec pairs
and six separate DVB-style TS muxes matched payloads in each lane, video
PTS/DTS and key flags, and audio PTS and durations. The development
simulator used the same parser source and passed six Jellyfin playlist
replays with decoded picture and PCM, pause, resume, seek and Stop.

The preview's ARM debug IPK SHA-256 is
`1cae873ed9f3458e9864ef11ff227d9cd19bd273b2c031c6fc5fa2ad2d74448b`.
All four packaged ELFs passed the FFmpeg-free link audit. On the LG G4,
the HEVC/E-AC-3 fixture played from a synthetic Jellyfin catalog through
Starfish. The app paused and resumed at 7.832 seconds. It reopened the
HLS session at 24 seconds, and both feeds and the playback clock resumed.
Back stopped at 34.832 seconds, returned to detail and Jellyfin confirmed
the Stop report. The process had no FFmpeg library mappings. The set
remained in Active Standby, so panel picture and audible output were
unobserved. The private receipts preserve the two earlier incomplete
replays and the final fixture and package hashes.

The follow-up at Streem `b2cb7e4b9022f82bc4c347e8fd08e4f1fd1595ea`
adds documentation to the same playback source. All six simulator replays
passed on that exact app revision and its pinned dependency. The ARM debug
IPK SHA-256 is
`71e3c0ff6da51918442f9d22cef96ed6d2f18e7143241ba871fbcd6015e1db34`.
It passed the link audit and an AVC/AC-3 catalog replay on the same G4.
The app paused and resumed at 8.249 seconds, sought to the segment starting
at 22 seconds and reported Stop at 32.832 seconds after Back. Both feeds
resumed after the seek, the process maps were FFmpeg-free, and the set
remained in Active Standby.

The initial native Linux ARM64 run matched 70 segments and a separate
allocation control. The latest worker matched all 71 literals and passed
runtime, peak live heap and total peak RSS on all ten workloads. The two
DVB runtime upper bounds fell to 1.2254× and
1.1970× of FFmpeg. Eight workloads also passed the 0.80×
optimization target. The receipts retain the earlier misses and exact
source and worker identities. [Initial run](../research/qualification/hls-av-family-linux-arm64/README.md) · [Optimization runs](../research/qualification/hls-av-family-linux-arm64/optimization.md)

The optimized dependency still needs Streem integration and target replay.
O31 also requires the remaining selected TS/M2TS, elementary and HLS profiles.

## Additional application-runtime surfaces

The TV release component list does not enable movie software decoders. Its normal
movie decode/output remains LG hardware, and text rendering remains libass. These
are existing consumer/platform responsibilities, not FFmpeg components to rebuild
as part of the initial TV offboarding.

The default desktop simulator launches external FFmpeg for video and audio.
The opt-in preview uses in-process Triergon decoders. The default development
capture uses FFmpeg for MPEG-1 encoding, TS muxing and pixel conversion.
These paths are excluded by `RELEASE=1` on the TV. The opt-in debug package uses
Triergon for capture. O61 and O62 still require full qualification and adoption
of the selected decoder, encoder, muxer and conversion contracts.
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

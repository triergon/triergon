# Decoder boundary and shared-core handoff questions

Open decoder/capture contracts following the O00 source review. These questions
do not weaken the complete offboarding target. All code coordinates refer to the clean Streem
worktree/revision in `source-files.json`. No implementation choice or
dependency admission is made here.

Path abbreviations in this file resolve exactly as follows: `player/…` means
`rust-modules/src/player/…`; `ff.rs` and `capture.rs` mean the corresponding files
under `rust-modules/src/`. Paths beginning `crates/` or `docs/architecture.md`
instead refer to this safe-media repository.

## Freeze O61 before decoder implementation

| Decision | Source evidence and why a decision is required | Concrete answer needed |
|---|---|---|
| D01: independent software decoding versus consumer OS decoding | `player/sim_video.rs:150–159` launches system FFmpeg; `player/ffi_host.rs:718–721` supplies compressed AUs. An OS-backed decoder is not a pure-safe software decoder. Apple's primary VideoToolbox overview documents hardware codec sessions and CoreVideo buffers, but does not prove a particular profile works on this Mac. | Choose and name the boundary separately per target: independently bootstrapped safe-Rust decode, or an explicit consumer OS decoder adapter. OS FFI stays outside the new core and cannot become a hidden dependency exception. The corresponding selected cases still need parity/time/memory evidence. |
| D02: required video combinations | `player/sim_video.rs:97–99,150–155` treats H265/HEVC as HEVC and every other video payload as raw H264. `ff.rs:3446–3452` can frame H264/HEVC/AV1/VP9 for the TV. | Freeze H264 and HEVC profile/level/bit-depth/chroma/interlace/resolution/rate limits, configuration changes, corruption policy and containers supplying them. Decide explicitly whether AV1/VP9 simulator decoding is added or separately scoped as currently unsupported; do not misreport TV framing as existing simulator decode support. |
| D03: source timing and decoder output association | `player/sim_video.rs:125–136,168–197` puts one PTS per AU into a heap and pops one for each raw frame. Raw pipes carry no timing. This source model is an observation, not proof for arbitrary packet/frame cardinality, missing PTS, repeated fields or reorder depth. | Freeze exact output IDs/source timestamps and many-packets-to-one / one-packet-to-many / no-output handling; unknown PTS/DTS, negative starts, B-frame reordering, dropped/corrupt frames, EOF drain and flush/reset. Keep both source timing and consumer presentation timing. Do not sort canonical demux packets to reproduce the heap. |
| D04: output pixels and colour | `player/sim_video.rs:40–43,147–155` requests a fixed RGBA canvas, scale/decrease, centered pad, passthrough cadence. It does not explicitly select scaler kernel, colour matrix/range/transfer, chroma location, SAR/rotation or HDR mapping. Pinned FFmpeg `doc/filters.texi` documents scale's pixel-format conversion and automatic colour/range choices. | Freeze exact dimensions/crop/pad and pixel layout/alpha; SAR, rotation, interlace, HDR10/HLG/DV handling, tone map versus explicit unsupported result, rounding and per-format numeric tolerances. Treat decode and pixel conversion as separate work, then qualify the complete selected pipeline as well. |
| D05: audio decode and format conversion | `player/sim_audio.rs:49–59,132–139` selects AC3/EAC3/AAC/DTS raw demuxers and stereo output; no explicit rate/sample-format/channel rematrix or output codec. `ff.rs:9397–9401,9826–9837` sends extracted DTS core, not DTS-HD extensions. | Freeze codec variants (including AAC SBR/PS/core rate and DTS core forms), input layouts/rates, output PCM type/rate/layout, downmix coefficients, LFE/clipping/dither and delay/padding. Decide whether Opus/TrueHD simulator sound is separate missing scope; neither is mapped by this caller. |
| D06: OS sound output | `player/sim_audio.rs:121–127` selects Pulse or ALSA only; `Makefile:2104` arms audio for Linux. No CoreAudio branch appears. Pinned `doc/outdevs.texi` distinguishes Pulse, ALSA and AudioToolbox. | Name supported Mac ARM64 and Linux/WSLg output adapters, deterministic device-free PCM capture for correctness, and separate output-device latency checks. No claim that current Mac audio plays successfully is supported by this inventory. A native output API remains consumer-owned. |
| D07: lifetime, EOF and queue bounds | `player/sim_video.rs:83–86,116–139,168–211`; `player/sim_audio.rs:67–98,148–174`. Both create ordinary unbounded channels and own kill/wait behavior. Video holds a future frame against the clock, but feeding queues and pending PTS can still grow. General host flush resets video and audio (`player/ffi_host.rs:643–644`); the native-seek hook explicitly resets only video (`player/ffi_host.rs:650–667`). | Freeze byte/frame/work caps, retained-output accounting, cancellation, starvation, backpressure, child/decoder failure, drain at EOF, pause/resume, every seek/flush/new generation, stop and stale callback rejection. Decide the intended audio behavior for the native-seek hook; no runtime bug reproduction was attempted. Do not reproduce unbounded queues merely for compatibility. |
| D08: reference identity and target | Source resolves `NJ_SIM_FFMPEG`/PATH (`player/sim_video.rs:141–142`, `player/sim_audio.rs:117–118`), while bundled libraries pin 9.0 (`ci/build-ffmpeg.sh:63–64`). The two binaries are not interchangeable references. | Pin a full-feature native Mac ARM64 reference build and operation/configuration for each decoder, filter, rematrix and output experiment. The bundled subtitle-only build cannot be the movie-decoder benchmark. Any installed FFmpeg result is exploratory and separately labelled. No installed executable was run here. |
| D09: matched work and platform boundary | A VideoToolbox call may use hardware while a software FFmpeg reference uses CPU; a demux benchmark excludes movie decode; Pulse wall-clock pacing can dominate computation. | Freeze equal work/resource budget and reference acceleration policy before timing. Record module compute and full-pipeline cost, including transfer/conversion/retention. No speed claim may compare hardware decode with a smaller software reference task or hide work in another process. Platform qualification does not claim an independently implemented codec. |
| D10: supported simulator artifacts | `Makefile:2037–2039,2093–2105` builds/stages media libraries for development; `ci/mkmacapp.py:118–130,267–299` disables default features and explicitly stages libass but no FFmpeg dlopen libraries. | Declare development simulator versus distributable UI `.app` behavior. Decide which artifacts must provide actual media playback, and audit them without installed FFmpeg fallback after integration. Source-only packaging observations do not prove a built package's contents. |

Primary boundary source: [Apple VideoToolbox](https://developer.apple.com/documentation/videotoolbox.md),
content-addressed in the supplemental
`.research/parallel-streem-inventory/primary-sources.json`. Its URL is mutable, not an immutable
codec-capability guarantee. FFmpeg reference documentation is pinned to
`d32b387f2b0a484599d4587d651891f0c63c4238`:
[CLI](https://raw.githubusercontent.com/FFmpeg/FFmpeg/d32b387f2b0a484599d4587d651891f0c63c4238/doc/ffmpeg.texi),
[filters](https://raw.githubusercontent.com/FFmpeg/FFmpeg/d32b387f2b0a484599d4587d651891f0c63c4238/doc/filters.texi),
[output devices](https://raw.githubusercontent.com/FFmpeg/FFmpeg/d32b387f2b0a484599d4587d651891f0c63c4238/doc/outdevs.texi).
These documents clarify reference operations; they are not normative AVC/HEVC/
AAC/AC3/DTS specifications and contain no license to copy implementations.

## Freeze O62 as three components plus integration

| Decision | Evidence | Contract needed |
|---|---|---|
| C01: conversion | `ff.rs:1071–1082,1167–1205`; same-sized RGBA→NV12, signed stride, Rust U/V deinterleave. | Explicit matrix/range/chroma siting/filter/rounding/alpha and stride/plane bounds. Compare both row orientations and every colour channel. Tiny authored RGBA inputs are ready; expected YUV is intentionally pending the rule freeze. |
| C02: encoder | `ff.rs:1009–1018,1208–1223`; fixed rate/GOP/no B frames plus requested bitrate. | MPEG-1 subset, supported geometry/bitrate, latency/recovery and independent decode conformance. Compare quality versus bitrate and equal presets/resources; exact byte equality is not the quality contract. |
| C03: mux | `ff.rs:1089–1149,1224–1233`; TS stream/timebase, custom write callback and flush. | Selected MPEG-TS PSI/PES/PCR/PTS/continuity and restart rules, output-chunk/partial-write/error bounds. Preserve the live `PXR2` MPEG1/TS consumer wire mode. Container byte identity is unnecessary unless separately frozen; independent re-demux and timing are required. |
| C04: session behavior | `capture.rs:713–740,790–806`; restarts on geometry/gap, re-encodes keepalives. | Fresh sequence header/random access at client/restart, advancing timing/continuity during keepalive, disconnect behavior, no stale replay. Keep GLES capture, sockets and JPEG fallback outside the new core. |

No full MPEG-1/MPEG-TS or relevant codec normative spec pack was acquired in this
lane. Record exact editions/access/rights and selected normative clauses before
codec/mux implementation. RFC 8216 §3.2 specifies an HLS segment boundary; it does
not replace ISO/IEC MPEG-TS syntax. ISO BMFF, AVC/HEVC, AAC, AC3/EAC3, DTS, PGS,
DVD/VobSub and DVB subtitle variant packs likewise remain explicit acquisition/
scope work, not inferred from enabled component names.

## Parent-owned shared-core prerequisites

The Matroska/core coordinates below describe the historical K0–K5 preparation
snapshot. The subsequent [shared-core implementation](../../../docs/core-development.md)
is under development and has its own source identity; these rows do not claim it
is absent.

The parent is the only owner of stable input/packet/memory contracts and the build/
qualification queue. This lane's schemas are **semantic scenarios**, not a new
public Rust API. Bind them to the parent's final names/types once supplied.

| Handoff requirement | Observation at preparation snapshot | Ready independent work |
|---|---|---|
| Incremental source capabilities, ranges, work yields and typed terminal outcomes | Current `crates/media/src/matroska.rs:241–242,558,795` uses Read/Read+Seek; architecture sketches are explicitly provisional (`docs/architecture.md:36–45`). | Source/seek/cancel scenario fixtures and consumer transport obligations. Do not fork an input trait here. |
| Immutable packet payload lifetime and retained capacity | `crates/media/src/matroska.rs:66–85,181–191` exposes `PacketData`, capacity and optional signed source times. | Semantic retained-buffer/clone/window fixtures and caller-retention workloads. Parent freezes shared buffer charges and ownership. |
| Stable track/configuration/side-data identities across formats | `crates/media/src/matroska.rs:25–56` has current Matroska fields; no independent cross-format interface is frozen by this lane. | Source evidence and requirement matrix for metadata, attachments, config changes, subtitle slots and codec framing. |
| Seek/discontinuity/generation transaction | `crates/media/src/matroska.rs:203–213,849–879` has current seek receipts; existing Streem owns its native session/clock. | Late-output rejection and flush/preroll scenarios; retain both source and presentation timeline. |
| Budget/cancel propagation through decoder/index/metadata/output retention | `crates/media/src/lib.rs:19–34` current Limits; `docs/architecture.md:43–45` intended work/retained-capacity model. | Enumerate all memory owners; fixtures assert no double charge or early credit. No implementation of the shared accountant in this lane. |

## Acceptance and provenance gaps

- The complete K0–K5 campaign status belongs to the parent's sealed receipt.
  None of those 116 numerical workloads proves an O61/O62 decoder/encoder/mux
  contract or any TV behavior.
- Codec profile enumerations, legal sample provenance, normative editions,
  conformance corpus and numeric output rules are unresolved. Naming a codec or
  classifying it as Rust is not qualification.
- Both full frozen selected parity and each case's upper paired95 runtime ratio
  ≤0.80 remain mandatory, together with lower live heap/attributable RSS and
  absolute bounds, on native Mac ARM64. An OS boundary does not waive the gates.
- The source inventory is current to the observed HEAD/hashes only. Final O70
  requires enabled-feature/loader/subprocess/package scans at the integration
  revision, plus clean installation and playback without installed FFmpeg.
- Keep external pinned reference workers and fixture generation separate from
  the application runtime. Do not delete the independent oracle to claim removal.

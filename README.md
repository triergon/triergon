# Triergon

A modular media toolkit written in safe Rust, built independently from format specifications. The workspace provides experimental container readers, packet framing, metadata, subtitle primitives and a shared correctness and benchmark harness.

The archived qualified source snapshot covers all six frozen Matroska development slices on the shared core (K0–K5, 116 workloads), which pass selected parity, runtime and both memory gates on native Mac ARM64. The [qualification receipt](research/qualification/shared-core-mac-arm64/README.md) retains exact source, workers, inputs and failures under their original project name. This includes bounded colour/static HDR, segment metadata and the intact original HDR vector. The [shared core](docs/core-development.md) and complete format modules remain under development; this selected Matroska qualification does not qualify later source or close complete modules. Target integration and full Streem offboarding remain open.

Root3477 retains the explicit nonseekable MP4 terminal-MDAT view, compact
AVC/HEVC configuration views, the native PGS first-event packet API and the
selected native mov_text packet-to-ASS API. It adds the packed HEVC NAL
descriptor and capture85's MPEG-1 luma optimizations. Native Linux ARM64
debug and release each recorded 3,477 passed, zero failed and one ignored
test on the adopted composition. See the separately scoped PGS receiver
qualification in [implementation and evidence](docs/implementation.md).

The standalone capture85 worker passed all 15 selected six-factor quality
cases. Its nominal and auxiliary TS outputs matched the approved capture84
run byte-for-byte. The capture76 and capture84 five-factor nonpasses remain
part of their original receipts. The exact capture85 worker also passed the
runtime delivery gate on all 15 cases. Heap, RSS and complete capture
qualification remain open. See the
[source-scoped capture status](research/capture/STATUS.md).

The first consumer is Streem, whose LG hardware decodes movie video/audio. The initial replacement target is its bundled FFmpeg demux/metadata/subtitle dependency, with accurate seeking and bounded untrusted-input handling. A broader FFmpeg-like toolkit is a separate long-term roadmap.

The selected design direction is an independently bootstrapped core. The current workspace has no third-party packages. Any future production dependency must be pure safe Rust throughout its selected graph, fit the contract precisely, and justify reuse against implementing the bounded component ourselves. Symphonia and other projects are comparison targets. Every selected module must match its declared FFmpeg feature set and use less measured memory. Under the [current delivery policy](research/decisions/initial-delivery-runtime.md), the paired 95% upper runtime ratio must be at most 1.20 per required case (lowered from 1.25 on 2026-11-05); 0.80 remains the optimization target. New numerical qualification runs use native Linux ARM64. Qualification receipts name their exact selected slice, corpus, source, binaries and target. The reviewed mixed-compression case uses an explicitly augmented native reference for a feature missing in pinned FFmpeg; its result is labelled separately.

On a native Linux development host with Rust nightly, Python 3, a C compiler, make, pkg-config, zlib development headers and NASM on x86, prepare the external test reference and fixtures once, then run the checks:

```sh
CARGO_INCREMENTAL=0 make prepare-check
CARGO_INCREMENTAL=0 make check
```

Preparation builds the pinned FFmpeg reference and caches the hash-verified required HDR vector locally. Small authored canonical streams are included so the test inputs remain identical across encoder versions. Preparation does not run performance qualification or add FFmpeg to the Rust dependency graph. The mandatory external vector stays intact and outside version control.

- [Overarching Streem offboarding target](docs/offboarding-target.md)
- [Scope and milestones](docs/scope.md)
- [Implementation, commands and current evidence](docs/implementation.md)
- [Bounded zlib and Cues/preroll status](docs/zlib-cues.md)
- [Bounded colour and static HDR](docs/colour-hdr.md)
- [Segment metadata development](docs/segment-metadata.md)
- [Specification baseline and feature map](specs/README.md)
- [Architecture and decision record](docs/architecture.md)
- [Project comparison and source evidence](docs/project-comparison.md)
- [Dependency admission](docs/dependency-policy.md)
- [Testing and benchmark contract](docs/testing.md)
- [License and provenance policy](docs/licensing.md)
- [Implementation backlog](docs/backlog.md)
- [Reproducible research](research/README.md)

The project lives at [triergon/triergon](https://github.com/triergon/triergon). Streem remains unchanged. Downloaded upstream sources, specification caches, large generated media and active experiments stay outside version control; archived research receipts retain their original names and provenance.

## License

Triergon's independently authored source code is available under your choice
of the [MIT License](LICENSE-MIT) or the [Apache License 2.0](LICENSE-APACHE).
See the [license scope and provenance policy](docs/licensing.md) for fixtures
and other third-party material.

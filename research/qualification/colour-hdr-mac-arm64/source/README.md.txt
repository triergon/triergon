# safe-media — safe Rust media toolkit

Working name for a safe Rust media toolkit. All five frozen Matroska development slices (K0–K4, 67 cases) pass their selected parity, runtime and memory gates on native Mac ARM64. The [current receipt](research/qualification/colour-hdr-mac-arm64/README.md) preserves exact source, binaries, inputs and the two owner-approved zlib comparison rules. Complete M01/M02 and shipping-TV qualification remain pending.

The first consumer is Streem, whose LG hardware decodes movie video/audio. The initial replacement target is its bundled FFmpeg demux/metadata/subtitle dependency, with accurate seeking and bounded untrusted-input handling. A broader FFmpeg-like toolkit is a separate long-term roadmap.

The selected design direction is an independently bootstrapped core. The current workspace has no third-party packages. Any future production dependency must be pure safe Rust throughout its selected graph, fit the contract precisely, and justify reuse against implementing the bounded component ourselves. Symphonia and other projects are comparison targets. Every selected module must match its declared FFmpeg feature set, take at most 80% of FFmpeg’s execution time, and use less measured memory. Qualification receipts name their exact selected slice, corpus, source, binaries and target. The reviewed mixed-compression case uses an explicitly augmented native reference for a feature missing in pinned FFmpeg; its result is labelled separately.

- [Overarching Streem offboarding target](docs/offboarding-target.md)
- [Scope and milestones](docs/scope.md)
- [Implementation, commands and current evidence](docs/implementation.md)
- [Bounded zlib and Cues/preroll status](docs/zlib-cues.md)
- [Bounded colour and static HDR](docs/colour-hdr.md)
- [Specification baseline and feature map](specs/README.md)
- [Architecture and decision record](docs/architecture.md)
- [Project comparison and source evidence](docs/project-comparison.md)
- [Dependency admission](docs/dependency-policy.md)
- [Testing and benchmark contract](docs/testing.md)
- [License and provenance policy](docs/licensing.md)
- [Implementation backlog](docs/backlog.md)
- [Reproducible research](research/README.md)

Streem remains unchanged. This repository is local and has no remote. No commit or publication has been requested. Upstream source snapshots and generated fixtures are excluded from version control.

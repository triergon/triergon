# ADR-0003: Shared container model; comparison views stay outside the libraries

**Status:** Proposed. Draft for owner review; nothing is implemented.
**Date:** 2026-11-05.
**Builds on:** [ADR-0001](architecture.md) (API semantics sketch), [ADR-0002](dependency-policy.md).

## Context

ADR-0001 sketched one consumer-facing vocabulary: `SourceCapabilities`, `ReadStep`,
`Track`, `Packet`, `SeekRequest`/`SeekReceipt` and `Budget`. The resource and input
half exists in `triergon-core`: `Budget`, `BudgetVec`, `Bytes`, `WorkBudget`, `Time`,
`TimeBase`, `Error`, `InputCursor`/`InputWindow`/`InputRequest` and `Step<T>`. Every
format crate depends on it. That layer is the project's strongest shared design and
is not in question here.

The container half was never written. Each format grew its own vocabulary:

| Concern | Matroska (`triergon`) | ISO BMFF (`triergon-isobmff`) | MPEG-TS (`triergon-mpegts`) |
|---|---|---|---|
| Step | `incremental::ParseStep` (adds `Backpressure`, `Finished`) | `SampleStep`, `BoxStep`, `IndexStep`, `FragmentRandomAccessStep` | `AssemblyStep`, `TableStep`, `NativeInputStep` |
| Track | `matroska::Track` (Matroska fields, `kind: u64`, `sample_rate: f64`) | `TrackHeader`, `SampleDescription`, `SourceTrack` | PMT stream entries |
| Packet | `matroska::Packet` (`pts_ns`/`dts_ns: Option<i64>` plus `BlockTiming`), `typed::Packet` | `SourcePacket { sample, payload: Bytes, .. }` | `Pes`, `NativePesPacket` |
| Seek result | `matroska::SeekReceipt` (ns, preroll, `discard_until_ns`) | `SourceSeekReceipt` (sample ordinals, preroll samples), `Mov9NativeSeekReceipt` | none yet |
| Reader layers | `matroska::Demuxer<R>`, `typed::Reader`, `typed::BlockingReader` | cursors and planners | assemblers |

Across the workspace there are 23 public `*Step` enums. Core's `Step<T>` is
imported by source files in only five crates (framing, playback, ISO BMFF, MPEG-TS
and Matroska). One reason is that it has no backpressure outcome, so
`media::incremental::ParseStep` had to add one. The consumer, `triergon-playback`,
defines ten more step enums of its own (`PumpStep`, `BackendStep`, `StartupStep`,
`MatroskaDiscoveryStep` and others) to bridge the per-format shapes.

The FFmpeg 9 comparison projections are also part of the library public API:
`Colour::ffmpeg9`, `BlockTiming::ffmpeg9`, ISO BMFF's exported `Mov9*` clock, cursor
and seek types, MPEG-TS's public `native_*` modules, and the subtitle crates'
`native_*` re-exports. At `4d4c4cf`, library sources contain about 4,600
identifiers named `native*`, `Mov9*` or `ffmpeg9*`, mostly in ISO BMFF, subtitles,
MPEG-TS and playback. Some exist only for the differential harness. Others are
pinned FFmpeg behaviour that playback ships deliberately. Today both kinds share
one public namespace with version-suffixed names, so a consumer cannot tell them
apart.

Consequences today: a Streem adapter needs a separate integration per container.
The harness needs a separate trace adapter per container. The same fact can be
expressed differently: Matroska time in nanoseconds, ISO BMFF time in sample
ordinals. Each new container lane can invent another shape before qualification
freezes it.

A uniform *shape* alone would not fix this. Third-party Rust demux frameworks
with one `Demuxer` trait still fabricate DTS, misreport random-access flags or
allocate per packet. The value is a uniform *contract*, enforced by the harness.

## Decision

### 1. One step type in core

Extend `triergon_core::Step<T>` with `Backpressure { requested, available }`. It
becomes the only outcome type returned across a crate boundary by any resumable
reader. Format-local step enums may remain internal. `End(SourceLocation)` keeps its
current meaning: end of a source entity, which is not necessarily logical EOF.

### 2. A container vocabulary module in core

Add `triergon_core::container`. ADR-0001 prefers a module until a crate boundary is
stable. The vocabulary is format-neutral; format detail travels in typed extensions:

- **`TrackId`**: stable per reader generation. **`MediaKind`**: `Video`, `Audio`,
  `Subtitle`, `Data`, `Unknown`.
- **`Track`**:
  - Identity: id, kind, source codec identifier (the container's original string or
    tag) and an optional normalized codec enum.
  - Configuration: bytes as a charged `Bytes` span, plus the track `TimeBase`.
  - Optional source facts: duration, codec delay, preroll, dimensions, sample rate as
    an exact rational and channels.
  - Language and title with provenance (explicit, specification default or absent).
  - Disposition flags as `Option<bool>`, so an absent element is distinct from false.
  - `extensions`: a typed per-format handle (Matroska `TrackExtensions`, ISO BMFF
    `SampleDescription`, TS PMT entry) plus the bounded original source metadata.
- **`Packet`**:
  - Payload and identity: `track`, `payload: Bytes` (shared, charged, never one
    owned `Vec` per packet), `source: SourceRange` (entity, offset, length) for
    provenance.
  - Timing as `Option<i64>` ticks in the track `TimeBase`: `pts`, `dts`, `duration`.
    Exact `Time` is derived on demand. This avoids `i128`/`u128` arithmetic per
    packet on 32-bit ARM.
  - Random access as `RandomAccess::{Sync, NotSync, Unknown}`, not a bool.
  - Trim as `Trim { start, end }`, both optional.
  - Epochs: `configuration_epoch` and `clock_epoch`, as in TS `SourceEpochs` and
    Matroska CodecState.
  - Flags (`corrupt`) and a typed side-data extension.
- **`Event`**: `Track`, `Configuration { track, epoch, bytes }`, `Packet`, `Metadata`
  (shared documents), `Discontinuity`, `Seek(SeekReceipt)`.
- **`SeekRequest`**: track, target `Time`, policy (`PreviousRandomAccess`, `Exact`,
  `PlaybackPreroll`) and scan budget.
- **`SeekReceipt`**: requested time, landing time and random-access time, preroll,
  `discard_until`, method (`Index`, `BoundedScan`, `Restart`), scanned bytes,
  `replays_from_start` and missing-preroll facts. This merges Matroska's
  `SeekReceipt` with ISO BMFF's `SourceSeekReceipt`; sample ordinals stay in the ISO
  BMFF extension.

### 3. Semantic rules that hold for every format

These are part of the contract. Harness traces check them.

1. Never fabricate a value the source does not carry. DTS stays `None` in Matroska.
   Durations and random-access flags are `None`/`Unknown` unless derived by a
   documented rule. A derived value records its origin.
2. Unknown is never a sentinel and never repeated. Laced frames without a duration
   get `None` timestamps after the first, which Matroska already does.
3. Specification defaults are materialized only with provenance. Absent and default
   remain distinguishable for re-muxing and for comparison.
4. No container fact may be silently dropped: subtitle tracks, colour, timing,
   configuration and attachments. Unsupported fields go to typed or opaque
   extensions.

### 4. One reader interface, static dispatch

A `ContainerReader` trait mirrors Matroska `typed::Reader`, the existing template:

- Input: `provide(&InputWindow)`, `finish`, `fail`, `reset`.
- Work: `step(&mut WorkBudget, &AtomicBool) -> Result<Step<Event>>`.
- Seeking and state: `request_seek(SeekRequest)`, `tracks()`, `capabilities()`.

Readers are concrete types used through generics, or through one closed enum at the
consumer boundary. There is no global registry and no `dyn` in the per-packet path.
One generic blocking adapter in core, `BlockingReader<R: Read + Seek, C:
ContainerReader>`, replaces the per-format adapters. The legacy
`matroska::Demuxer<R>` is retired once nothing selects it.

### 5. Separate shipped reference behaviour from trace-only projections

The `native*`/`Mov9*` code falls into two groups, and they need different homes.

**Shipped reference-compatible behaviour.** `triergon-playback` uses some of it in
production code. Examples are `Mov9PacketCursor` (`crates/playback/src/player.rs`),
`Mov9NativeSeekReceipt`, the encoded edit plan, `NativeInputBootstrap` and
`NativePesPacket`/`NativePesClockV1`. These deliberately reproduce pinned FFmpeg
behaviour, such as malformed-MOV recovery and edit-list clocks, because Streem's
current backend is FFmpeg.
- They stay in the format crates as **named policies** in a `compat` module, e.g.
  `compat::mov::EditPolicy::Reference`, with the pin recorded once per policy
  family.
- They return the neutral `Event`/`Packet` types.
- The `V{n}` suffixes move out of type names and into evidence identifiers.

**Trace-only projections.** These include `ffmpeg9()` views and slices exercised only
by `tests/`, examples or workers: about 59 unused ISO BMFF re-exports, MPEG-TS
`native_packet_completion` and the ISO BMFF operation, transport and header
slices.
- They move behind a non-default `reference-trace` feature, or into a
  `publish = false` crate used by the workers.

In both groups, if a projection needs private parser state, the neutral model is
missing a source fact: add the fact, not the projection. Approved comparison rules
and frozen views stay byte-identical; only their location and names change.

## Migration and gates

Each stage needs its own correctness and numerical evidence. No stage weakens a
parity, runtime or memory gate.

1. Core only: extend `Step`, add `container` with unit and 32-bit tests. No format
   changes.
2. Matroska: `typed::Reader` implements `ContainerReader`. The worker moves to the
   generic blocking adapter. Re-run all 116 frozen K0–K5 workloads with fresh 30-pair
   time, live-heap and RSS evidence on native Linux ARM64. Any packet-path allocation
   or dynamic call the trait adds shows up here, and a regression past a gate blocks
   the stage.
3. ISO BMFF and MPEG-TS move onto the model *before* their module qualification
   freezes the current shapes.
4. Split reference behaviour into `compat` policies and trace-only projections
   (decision 5), keeping comparison outputs byte-identical and recording that
   equivalence as evidence.
5. The Streem adapter (backlog A01) targets the neutral model only.

## Alternatives considered

| Option | Disposition |
|---|---|
| Keep per-format APIs and unify inside the Streem adapter | Rejected. It puts container semantics in the consumer and duplicates harness adapters. |
| A trait-object framework with a codec/container registry (libavformat style) | Rejected. It adds global state and per-packet dynamic dispatch, and a uniform trait does not by itself give a uniform contract. |
| A separate `triergon-container` crate now | Deferred. Split it out when the boundary is stable and independently useful. |
| Exact `Time` on every packet | Rejected for the hot path because of 32-bit cost. Ticks plus a track `TimeBase` keep exactness at lower cost. |

## Consequences

- Benefits: one consumer adapter, one harness trace adapter, consistent unknown and
  default semantics, and a smaller public surface free of reference-specific types.
- Costs: Matroska needs fresh numerical qualification after migration. ISO BMFF and
  MPEG-TS change shape. There is a risk of a lowest-common-denominator model, which
  the typed extensions mitigate.

## Recommended answers to the open questions

These are recommendations for the owner to confirm, not decisions already made.

### Colour/HDR: move a neutral description into core now

Colour facts already arrive from four places with different encodings:
- Matroska `Colour` (`crates/media/src/colour.rs`), using Matroska element codes and
  defaults;
- ISO BMFF track metadata (`crates/isobmff/src/metadata.rs`);
- VP9 configuration (`crates/framing/src/vp9_config.rs` exposes
  `colour_primaries`/`transfer_characteristics`);
- AVC/HEVC VUI handling in `crates/playback/src/raw_video`.

All of these use the ITU-T H.273 (CICP) code space. Streem's HDR policy needs one
answer whichever source supplied it. Leaving colour Matroska-only would make
ISO BMFF and playback invent a second model.

Recommendation:
- Add `container::ColourDescription` with H.273 primaries, transfer and matrix codes,
  range, chroma siting, optional mastering-display and content-light-level values,
  and per-field provenance (`Container`, `Bitstream` or `SpecificationDefault`).
- Container-specific detail stays in the format extension, for example Matroska
  `bits_per_channel` and `cb_subsampling`.
- `Colour::ffmpeg9` moves to the harness-side crate under decision 5.
- Conflicts between container and bitstream values are reported as facts, never
  silently resolved.

### Consumer boundary: a closed enum in a facade crate

Streem receives Matroska directly and TS/fMP4 through HLS, so the reader has to be
chosen at open time from probed bytes. Build-time selection cannot serve that, and a
`dyn ContainerReader` would put dynamic dispatch on every step.

Recommendation:
- A closed `enum AnyReader { Matroska(..), Isobmff(..), Ts(..) }`, with each variant
  behind a Cargo feature.
- Construct it with `open(probe: &[u8], ..)`. It implements `ContainerReader` by
  forwarding: one predictable `match` per step, inlinable, with no allocation.
- Generic code (the harness, format tests) keeps using concrete readers.
- The enum lives in the facade crate described in
  [the refactor plan](refactor-plan.md), not in core, so core never depends on
  formats.

### Legacy `matroska::Demuxer<R>`: retire at stage 2

At `4d4c4cf`, neither the worker (`typed::BlockingReader`) nor playback
(`typed::{Event, Packet, Configuration}`) uses `matroska::Demuxer<R>`. Its remaining
users are `crates/media/tests/matroska.rs` and one test helper in
`crates/worker/src/catalogue.rs`. Keeping it means maintaining a second packet path
that no qualified operation exercises.

Recommendation:
- Port those tests to the typed reader through the generic blocking adapter in
  stage 2, then delete `Demuxer<R>`.
- Streem integration does not exist yet, so nothing external depends on it.

# ADR-0004: Modular architecture target and behaviour-preserving refactor programme

**Status:** Proposed. Draft for owner review; nothing is implemented.
**Date:** 2026-11-05. **Baseline:** `origin/main` at `4d4c4cf`.
**Companion:** [ADR-0003](container-model.md) defines the shared container model that
this programme builds on.

## Why now

The workspace has grown to 15 crates, roughly 210k lines of Rust source and tests, and
10k lines of Python tooling. Each lane was built and qualified on its own, which was
the right way to reach evidence quickly. The result is that the same ideas now exist
in several incompatible forms:

- At least eleven bit readers. There is one in each of the five codec crates, with
  three different overrun policies, plus six in the formats: `media/zlib.rs`,
  `media/typed/hdr10_plus.rs`, `framing/dts_core.rs`, `framing/ac3.rs`,
  `framing/aac_config.rs` and `subtitles/dvb/bits.rs`.
- Two CABAC engines and three FFT/IMDCT implementations.
- Two CRC-32s and about 60 `Limits` structs.
- 23 public `*Step` enums and about 40 step/status enums in total.
- Three identical `AudioFrame` types and two `Picture` types.
- Four worker checksum algorithms. The `fold_bytes` digest exists in two Rust copies
  that have already drifted.
- About 4,600 `native*`/`Mov9*`/`ffmpeg9*` identifiers, with version suffixes in public
  type and module names. About 22 of ISO BMFF's 48 source files carry a `vN` suffix.

The consumer feels this most. `triergon-playback` has a three-way `Engine` enum with
62 per-format match arms (`crates/playback/src/engines.rs`). It decodes Matroska track
codes and TS `stream_type` values itself, and walks MP4 boxes in two places
(`mp4_open.rs`, `progressive.rs`).

The longer each lane qualifies against its current shape, the more evidence must be
re-run to change it. Refactoring before more lanes reach module qualification is
cheaper than refactoring after.

## Target architecture

The layout stays a flat `crates/` directory, which is idiomatic Rust and avoids
rewriting paths in provenance tooling. Layering is by dependency, with each layer
depending only on the layers below it:

```
L0 foundation   triergon-core       budgets, Bytes, Time/TimeBase, input, Step, Error,
                                    container vocabulary, ColourDescription
                triergon-bitstream  bit reader/writer (checked and padded policies),
                                    exp-Golomb, RBSP unescape, CABAC engine, CRC
L1 formats      triergon-matroska   (renamed from package `triergon`, crates/media)
                triergon-isobmff  triergon-mpegts   each implements ContainerReader
                triergon-framing    packet adapters, incl. ADTS and AU splitting
                triergon-subtitles  cue/bitmap decoders
L1 codecs       triergon-codec      decoder traits, Picture/AudioFrame, ChannelLayout,
                                    shared DSP (FFT/DCT-IV/MDCT) once proven bit-exact
                triergon-codec-{aac,ac3,dca,h264,hevc}
L2 composition  triergon            facade: AnyReader enum + curated re-exports, a
                                    Cargo feature per format and codec
                triergon-playback   engine over AnyReader
                triergon-playback-hls, triergon-playback-feeds (codec bridges)
L3 tooling      triergon-harness-protocol, media-worker, d4-worker, capture-worker,
(publish=false) triergon-sim, triergon-capture
```

Rules that keep it clean:

1. **Core stays small and dependency-free.** It holds vocabulary and resources only;
   bit-level parsing lives in `triergon-bitstream`. Core never depends on a format.
2. **Everything allocates through core budgets and can be cancelled.** This now
   includes codecs, which have no core dependency today and allocate frame and DPB
   memory with plain `vec!`.
3. **One error type.** Codecs and formats use core `Error`, with codec- or
   format-specific detail as a variant, so no conversion loses fields. Today
   `media`'s `From<core::Error>` drops `Limit { requested, maximum }`
   (`crates/media/src/error.rs`).
4. **Public API is curated per crate.** Reference-compatible behaviour the product
   ships is exposed as named `compat` policies. Trace-only projections sit behind a
   non-default `reference-trace` feature. No version suffixes in identifiers;
   versions belong to evidence identifiers.
5. **The module tree mirrors the directory tree.** No `#[path]` attributes. Tests
   that need private items live in one `tests` submodule per parent; everything else
   goes in `tests/`.
6. **Static dispatch on hot paths.** Closed enums at boundaries, generics inside.

## Refactor catalogue

Each item lists the evidence (paths as of `4d4c4cf`), the target and the risk.
"Equivalence" means byte-identical worker traces on all frozen corpora and FATE
cases. "Re-qualify" means fresh 30-pair runtime and memory evidence on native
Linux ARM64.

### A. Foundation

| # | Change | Evidence | Gate |
|---|---|---|---|
| A1 | Core `Step` gains `Backpressure`; it becomes the only step type crossing a crate boundary | `media/src/incremental.rs:52` `ParseStep`; 23 public step enums | Equivalence plus re-qualify (touches hot loops) |
| A2 | `core::container` vocabulary and `ColourDescription` | [ADR-0003](container-model.md) | Unit and 32-bit tests |
| A3 | Core `Error` carries typed detail; media's own `Error` is removed | `media/src/error.rs` drops `Limit` fields | Equivalence |
| A4 | `triergon-bitstream`: one reader with an explicit overrun policy, exp-Golomb, `unescape_rbsp`, CABAC engine, CRC-32/CRC-8, big-endian fields | `codec-{aac,ac3,dca,hevc}/src/bits.rs`, `codec-h264/src/bitreader.rs`; RBSP unescape in `h264/src/nal.rs` and `hevc/src/nal.rs`; CRC in `media/typed/side_data.rs` and `mpegts/psi/section.rs` | Per adopting crate: equivalence plus benchmark (hottest code in each decoder) |
| A5 | Shared limits validation helper; `Limits` structs stay per format | about 60 `Limits` structs | Equivalence |

### B. Containers and framing

| # | Change | Evidence | Gate |
|---|---|---|---|
| B1 | Rename package `triergon` (crates/media) to `triergon-matroska`; free the `triergon` name for the facade | `crates/media/Cargo.toml` | Build only |
| B2 | Matroska `typed::Reader` implements `ContainerReader`; one generic blocking adapter; delete `matroska::Demuxer<R>` | Only `media/tests/matroska.rs` and one worker test helper use it | Equivalence plus re-qualify K0–K5 (116 workloads) |
| B3 | ISO BMFF and MPEG-TS readers implement `ContainerReader`; box walking and track classification move from playback into them | `playback/src/mp4_open.rs:196-247,846-963`, `progressive.rs:237-381`, `engines.rs:940-967,1553-1623` | Equivalence on their source and native comparisons |
| B4 | Split `native*` code into shipped `compat` policies and `reference-trace` projections; rename by responsibility (`compat::mov::{recovery, transport, edits, pcm}`) | Native/versioned code is about 13.9k of ISO BMFF's 21.3k lines, 8.4k of MPEG-TS's 13.7k and 4.7k in subtitles. Playback uses 61 of ISO BMFF's 243 exports. `mpegts/src/native_packet_completion.rs` (1,813 lines) is test- and example-only | Equivalence; frozen views byte-identical |
| B5 | Media module tree matches its directories | `#[path]` in `matroska.rs`, `metadata.rs`, `zlib.rs`, `incremental.rs`, `storage.rs`, `content_encoding.rs` | Build only |
| B6 | Move test code out of `src/` | About 14.6k of media's 37.9k `src` lines are test, guard or regression files; ~940-line inline test block in `mpegts/src/native_packet_completion.rs` | Same test count before and after |
| B7 | Framing absorbs duplicate framing: AAC ADTS, the d4-worker Annex-B splitter and DTS sync | `codec-aac/src/adts.rs`, `sim/src/bin/d4-worker.rs:94-170` | Equivalence |
| B8 | Split functions over about 400 lines into one function per state | e.g. `subtitles/pgs/display.rs:595` (891 lines), `framing/nal_packet.rs:1271` (535), `media/typed/reader.rs:928` (493) | Equivalence; benchmark where hot |

### C. Codecs

| # | Change | Evidence | Gate |
|---|---|---|---|
| C1 | `triergon-codec` traits: `VideoDecoder` (send/receive/flush/reset) and `AudioDecoder`; shared `Picture`, `AudioFrame`, `ChannelLayout`/`Speaker` with a documented channel order | Three identical `AudioFrame` (`aac/lib.rs:34`, `ac3/lib.rs:44`, `dca/lib.rs:171`); two `Picture`; `sim/src/adapters.rs` relies on an unwritten rule that each `Speaker` discriminant equals FFmpeg's channel index | Equivalence of decoded output |
| C2 | Codecs take `Budget` and limits; frame pools on `BudgetVec`; `WorkBudget` charged per CTU, macroblock or audio block | Hard-coded limits (`h264/ps.rs:411`, `hevc/ps.rs:8-13`); separate frame pools in h264 and hevc | Equivalence plus re-qualify |
| C3 | HEVC `simd` becomes opt-in; SIMD hooks move from the `Pixel` trait into a `kernels` module; add scalar-vs-SIMD tests that check both give identical output | `default = ["simd"]` needs nightly; the Makefile type-checks the scalar path but never tests it | Both paths tested |
| C4 | Neutral names for reference options and debug items behind a feature | `ac3 DecoderOptions::ffmpeg_compat` (on by default), `ChannelLayout::ffmpeg_name`, `aac debug_*`, `h264 pub mod inspect` | Build only |
| C5 | Split oversized decoder files by stage | `hevc/src/ctu.rs` (2,660 lines), `ac3/src/substream.rs` (2,274; `parse_block` is 405), `dca/src/lib.rs:361` (408-line function) | Equivalence plus benchmark |
| C6 | Shared DSP (FFT/DCT-IV/MDCT) | `aac/fft.rs`, `ac3/imdct.rs`, `dca/dct.rs` | Last and optional: only with bit-exact proof against each current crate |

### D. Composition

| # | Change | Evidence | Gate |
|---|---|---|---|
| D1 | `triergon` facade: `AnyReader` closed enum and curated re-exports, a feature per format | ADR-0003 recommendation | Build only |
| D2 | Playback `Engine` becomes `AnyReader`; per-format logic leaves `engines.rs` | 62 match arms, 2,104 lines | Playback test suite (about 21k lines) plus equivalence |
| D3 | Split `triergon-playback-hls` and `-feeds` (codec bridges) from the engine | Bridges and HLS do not depend on `Engine` | Build plus tests |

### E. Tooling, harness and repository

| # | Change | Evidence | Gate |
|---|---|---|---|
| E1 | `triergon-harness-protocol` crate plus `harness/protocol.h`: one written spec, the JSON writer, the markers, a mode dispatcher, and every digest as a named, versioned algorithm with golden vectors | `quote`/`hex`/`fold_bytes` copied in `worker/src/main.rs`, `isobmff/examples/isobmff_consumer/protocol.rs`, `harness/reference.c` and `container_reference.c`; markers in 13 files; d4 and capture workers use other digests; `d4-worker.rs:2` refers to a missing `harness/d4/reference_decode.c` | Golden vectors; recorded digests unchanged |
| E2 | Toolchain: `rust-toolchain.toml`; `rust-version` set to what the code actually needs (let-chains need 1.88); every crate inherits workspace `[lints]`, edition and `rust-version`; nightly only for the opt-in SIMD lane | `Cargo.toml` says 1.85; `playback` and `subtitles` don't inherit; CI and Makefile use `+nightly` | CI on stable plus one nightly SIMD job |
| E3 | Python as a package: `harness/common.py` (hashing, identity, report saving), snake_case importable generators, `unittest discover`, `memory.py` split into collection and certificate | Four hash helpers; `importlib` loading of `generate-corpus.py`; hand-listed test modules in the Makefile | Python tests |
| E4 | Retire spike-era scripts to `research/spike/`; `check-design` reads constants from `module-gates.json` | `compare-candidates.py`, `generate-fixtures.py`, `build-container-reference.py`, `check-streem-inventory.py` | `check-design` |
| E5 | Evidence storage: summaries and identities stay tracked; raw reports and frozen source snapshots move to a content-addressed archive referenced by their recorded hashes; tooling excludes `research/qualification/**/source/` | `research/qualification` is 97 MB; 1,162 frozen source copies show up in every search | **Owner decision** (immutability policy) |

## Sequencing

Every step preserves behaviour. None changes a parity, runtime or memory threshold,
and every step that touches a hot path re-runs the affected numerical gates.

1. **Phase 0, hygiene (no behaviour change):** E2, C3, C4, B5, B6, E3, E4, E1. Cheap,
   independent, and they make the later diffs readable.
2. **Phase 1, foundation:** A1, A3, A5, then A2 and A4. Adopt the bit reader in the
   formats first, then in the codecs one at a time with benchmarks.
3. **Phase 2, containers:** B1, B2, then B3, B4, B7, B8.
4. **Phase 3, codecs:** C1, C2, C5, then C6 only if bit-exactness is proven.
5. **Phase 4, composition:** D1, D2, D3. Streem integration (backlog A01) then
   targets the facade only.
6. **Phase 5, evidence store:** E5, after the owner decides.

**Coordination:** several feature branches are active (`hq/*` and `codex/*`
worktrees). Run each phase per crate in short, separately reviewed changes, and
announce a short freeze on the crate being moved. Renames (B1, B4) land in their own
commits with no logic changes, so `git log --follow` and reviews stay meaningful.

## What this does not change

- No dependency is admitted. ADR-0002 is unchanged; `triergon-bitstream` and
  `triergon-codec` are owned crates.
- Frozen receipts, raw observations and their claims stay immutable. New source needs
  new identities and fresh qualification, as today.
- FFmpeg-compatible behaviour that playback ships stays. It gets neutral, documented
  policy names instead of version-suffixed type names.

## Owner decisions needed

1. Confirm the three ADR-0003 recommendations: neutral colour in core, `AnyReader`
   in the facade, retire `matroska::Demuxer<R>`.
2. E5 evidence storage: is moving raw reports and source snapshots out of the
   working tree compatible with the immutability policy, if their hashes stay
   tracked?
3. Provenance review already flagged in source: `crates/codec-dca/src/vq.rs` holds
   VQ codebooks recovered black-box from FFmpeg 9.0.2 output. Its header points to
   `research/d4/dca/STATUS.md`, `gen_vq.py` and `recover_vq.py`, none of which is in
   the repository at `4d4c4cf`. That means the generated table cannot be reproduced
   from tracked inputs. The same gap applies to `d4-worker.rs`, which refers to a
   missing `harness/d4/reference_decode.c`.
4. Whether `capture` and `sim` remain workspace members in the product repository
   or move to a development workspace.

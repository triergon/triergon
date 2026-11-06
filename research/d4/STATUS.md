# D4 / O61 desktop decoding replacement — status

Scope owner: this lane. Parent owns Streem edits, target/TV proof and FFmpeg removal.
Nothing here is committed; nothing is qualified until the native gates below pass.

## Selected matrix (frozen from the actual sources, see ADAPTER-CONTRACT.md)

| Lane | Selected | Explicitly refused (typed `Unsupported`) |
|---|---|---|
| H.264 video | Constrained Baseline / Main / High, 8-bit 4:2:0 (and 4:0:0 where FFmpeg decodes it), progressive, PAFF, MBAFF, CAVLC/CABAC, B/direct, weighted prediction, 8x8 + scaling matrices, LTR/MMCO, DPB bumping, cropping, VUI | High 10/4:2:2/4:4:4, bit depth > 8, SVC/MVC enhancement NALs (skipped like FFmpeg's base view) |
| HEVC video | Main, Main 10, Main Still Picture, 4:2:0 8/10-bit, all Main tools (tiles, WPP, dependent slices, SAO, PCM, transform skip, scaling lists, TMVP, weighted prediction, LTR, CRA/BLA/RASL) | RExt, SCC, multi-layer (nuh_layer_id > 0 skipped) |
| AC-3 / E-AC-3 | all acmod/LFE, bsid ≤ 8 incl. alternate syntax, coupling, rematrix, block switching, DRC as FFmpeg applies it; E-AC-3 independent + dependent substreams (7.1), AHT, SPX | bsid 9/10 (FFmpeg decodes: open gap), enhanced coupling (FFmpeg refuses too) |
| AAC (ADTS) | LC, HE-AAC v1 (SBR, implicit), HE-AAC v2 (PS) | ER/LD/ELD/USAC object types (status from the AAC lane) |
| DTS | core (all packings, AMODE 0–9 + LFE), XCh, X96 inside the core frame | XXCh inside the core frame; HD substreams are outside the simulator feed |
| Output | 1920x1080 RGBA letterbox (aspect-decrease scale, centred pad), stereo s16 at decoder rate | — |
| Advertised but not decoded by the simulator | VP9/AV1 routes select the H.264 lane in the source; the replacement reports `Failed` instead of a black picture | — |

## Implementation

- `crates/codec-{h264,hevc,ac3,aac,dca}`: decoder crates (per-lane `research/d4/<lane>/STATUS.md`).
- `crates/sim`: sessions (bounded queues, PTS through reorder, clock gating, generations,
  flush/stop/EOS, explicit failure), `scale.rs` (letterbox + RGBA), `mix.rs` (stereo matrix,
  s16), `load.rs` (frozen Load-payload rules), `adapters.rs` (feature `codecs`),
  `bin/d4-worker.rs` (measurement adapter).
- `harness/d4/`: native reference worker (`reference_decode.c`, same filter chains as the
  CLI, verified byte-identical to CLI output on the development host), observation and
  fitting scripts.

## Frozen rules

`NUMERIC-RULES.json` (before grading), `DOWNMIX-CONTRACT.json` (native observation),
`RGBA-CONTRACT.json` (native observation, conversion arithmetic).

## Open decisions for the owner

1. AC-3/E-AC-3 dither and SPX/AHT noise: FFmpeg's random generator makes the frozen float
   tolerance unreachable for any independent decoder (FFmpeg against itself with another
   seed fails identically). Options: authorise an FFmpeg-compatible generator, or compare
   dither-stripped output plus a statistical noise check.
2. DTS ADPCM and high-frequency VQ codebooks and the perfect-reconstruction filter were
   recovered by black-box observation of FFmpeg output (the spec names them normative but
   prints none). Provenance/licensing acceptance is the owner's call.

3. aac-sce-in-stereo oracle artefact, nightly `std::simd` in codec-hevc, single-core comparison,
   RGBA near-ties: see `HANDOFF.md` (owner decisions 3–6).

## Gates (native Linux ARM64)

Final candidate D2 (`HANDOFF.md`): combined functional gate green (185 suites, 2354 passed,
0 failed, 10 ignored per profile; 1973/1973 M1 replays); TIME 17/17 <= 1.25 (max 1.039, 5/17
<= 0.80); MEM 17/17 heap and RSS < 1; correctness 403/454 pass at both levels, 32 out of set,
5 refused by both, 14 failures recorded with numbers (10 AC-3/E-AC-3 dither, 3 RGBA near-ties,
1 FFmpeg oracle artefact) — no failure excluded. D1 (MEM 14/17) is kept as first-clean history.

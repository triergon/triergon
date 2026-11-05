# D5 / O62 overarching goal: replace Streem's development-capture FFmpeg path

**Status:** active. Opened 2026-11-02 by the independent capture lane
(worktree `d5-o62-capture-replacement-8da468`). Not complete.

## Goal

Replace every FFmpeg responsibility of Streem's enabled development capture
pipeline with clean, pure safe Rust in Triergon:

1. RGBA → YUV 4:2:0 conversion (today: `sws_getContext`/`sws_scale` RGBA→NV12,
   `SWS_BILINEAR`, signed stride, followed by Streem's own NV12→YUV420P split);
2. MPEG-1 video encoding (today: `mpeg1video`, I+P GOP 30, no B frames,
   VBV-constrained rate control at the client-selected bitrate);
3. MPEG-TS muxing (today: `mpegts` muxer over a custom AVIO writer, per-frame
   flush, socket write failure);
4. the session behavior the three components jointly provide to `capture.rs`
   (fresh sequence header/random access per session, advancing timing on
   keepalive re-encodes, rebuild on geometry change or a >1500 ms gap).

The goal is complete only when the **full selected pipeline** and **every
required gate** are verified on native Linux ARM64 for every required case:

| Gate | Requirement |
|---|---|
| Parity | Full declared feature parity with the frozen contract (`contract.json`); every required case passes its predeclared comparison rule |
| Runtime | Paired 95% upper runtime ratio (Rust / FFmpeg, equal work) ≤ 1.25 per required case; ≤ 0.80 remains the optimization target and is reported |
| Memory | Strictly lower peak live heap **and** strictly lower attributable RSS than the matched FFmpeg worker, per required case |
| Safety | `forbid(unsafe_code)`, no FFI, no native runtime library, no third-party production dependency |
| Evidence | Exact source/input/reference/toolchain/binary pins, raw results and retained failures |

Completing O62 additionally requires Streem integration and its own
qualification (O50/O60/O70); this lane does not modify Streem and therefore
cannot close those. A slice pass is never reported as module or TV
qualification.

## Ledger

| ID | Work | State |
|---|---|---|
| G1 | Re-verify the actual Streem capture call sites read-only; record revision and hashes | done — see `contract.json` `streem_source` |
| G2 | Freeze input/output contracts (geometry, rates, pixel layout, matrix/range, alpha, encoder settings, GOP, timestamps, PIDs, packetization, buffering, failure) | done (v1) — `contract.json`; observed-vs-normative split recorded |
| G3 | Colour conversion implementation + reference comparison | see `STATUS.md` |
| G4 | MPEG-TS muxer implementation + independent re-demux | see `STATUS.md` |
| G5 | MPEG-1 encoder (I/P, half-pel ME, VBV rate control) + independent decode, quality/bitrate comparison | see `STATUS.md` |
| G6 | Session/pipeline adapter semantics (C04) | see `STATUS.md` |
| G7 | Pinned FFmpeg 9.0.2 reference worker with Streem's dev configuration | see `STATUS.md` |
| G8 | Native Linux ARM64 correctness, paired runtime and memory gates for every required case | see `STATUS.md` |
| G9 | Hand-back: patch, tests, scope coverage, remaining gaps, shared-core requests | see `STATUS.md` |

`STATUS.md` holds the current, evidence-linked state of each row; this file
only changes if the goal itself changes.

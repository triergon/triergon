# Matroska video sessions with lazy scan replay locations

The source commit `14c03b528db8c681b62b44b5710e02f7de7efb80`
matched all 44 frozen selected cases and all 1,344 packets on native
Linux ARM64. The comparison includes ordered payload bytes, PTS, DTS
and duration. It uses the same manifest, pinned reference, 30 pairs,
50 timing iterations per pair and independent memory observers as the
[preceding receipt](../README.md).

| Gate | Passing cases |
|---|---|
| Selected parity | 44/44 |
| Runtime at most 1.25× | 43/44 |
| Strictly lower peak live heap | 44/44 |
| Strictly lower total peak RSS | 44/44 |
| All delivery gates | 43/44 |
| 0.80× optimization target | 29/44 |

The remaining runtime failure is
`seek-hevc-seek-no-cues-0-fresh` at **1.2515724977139935×**.
Its preceding result was 1.2543000211975848×. Both failures remain
recorded. Worst heap and total RSS upper ratios are 0.403718× and
0.731133×, rounded upward.

The scan still reads and validates the full required source range. It
builds a replay location when a checked block improves the landing, and
validates the immutable scan context once per prefix step call. The
existing zero-work, cancellation, lacing, compression, metadata-state and
late-error controls passed. The host workspace passed 3,663 Rust tests
with ten ignored, 182 harness tests, formatting, Clippy, strict-safe and
design checks. The workspace ARMv7 no-default-features check passed.

The [per-case results and source, input, worker and archive
hashes](results.json) bind this selected operation. The raw measurements,
observer certificates and original failures remain in the retained
native archive. Application adoption and TV validation are open.

A separate diagnostic profile attributed 3.29% of candidate CPU samples
directly to the buffered EBML header helper. The forced-inlining
candidate again passed 44 parity and both memory gates, with 43 runtime
passes and the same fresh no-Cues failure at 1.2525941549145712×. The
candidate was reverted from the PR. The [full failed variant
summary](inline-attempt.json) retains all 44 rows and artifact hashes.

Directly decoding the common short EBML headers passed the host suite
and all 44 native parity and memory cases. Its fresh no-Cues upper runtime
ratio was 1.2566436315641156×, with 43/44 runtime passes. The PR keeps
the lazy scan candidate measured at 1.2515724977139935×.
[Short header experiment and hashes](short-headers-attempt.json)

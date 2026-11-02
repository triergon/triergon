# Ordered checksum implementation update

Both timing workers now calculate each eight-byte polynomial using two bounded
four-byte dot products. The byte recurrence remains `h = 31*h + byte (mod 2^64)`.
Every packet, metadata byte and selected numeric field is consumed in the same
order; trace output, checksums and timed operation boundaries are unchanged.

For four bytes, the largest value is `255*(31^3+31^2+31+1) = 7,849,920`, below
2^23. The eight-byte value is `four(first)*31^4 + four(second)`, at most
7,249,573,818,240. Thus the 32-bit intermediate and 64-bit combination are exact
for all byte values. Existing independent scalar checks remain mandatory. Both
workers receive this implementation change so narrower arithmetic is available
to their native compiler; no SIMD dependency or unsafe Rust is introduced.

The pinned FFmpeg revision, configuration, feature work, all required inputs,
approved two-case views, thresholds and minimum 30 pairs remain unchanged. Both
worker identities change, so old measurements cannot qualify the new workers.
Old clean failures, rejected contamination, outliers and sealed receipts remain
intact. Full source/native parity, scalar checksum agreement and fresh per-case
runtime, live-heap and attributable RSS results are required before acceptance.

The Rust scalar regression and both independent end-to-end scalar checks pass
for the rebuilt workers. The frozen make12 source passes 315 Rust and 51 Python
tests; the [shared-core qualification](../qualification/shared-core-mac-arm64/README.md)
passes all 116 selected source/native workloads and fresh 30-pair time/live-heap/RSS
gates with those worker identities. These are complete-operation measurements;
this note does not isolate a speedup attributable to the checksum alone.

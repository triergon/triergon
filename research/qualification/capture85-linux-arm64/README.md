# Capture85 on native Linux ARM64

The archived whole-session run covered 15 required operations across ten
input assets. The independent quality comparison passed all 15 using the
approved six equal-size comparison points. The timing run recorded 30 pairs
per operation and discarded the first of four sessions in each worker
process. An offline audit checked the outputs and durations of all 900 child
receipts, verified the completion hashes and recomputed each paired bound
with the frozen 4,000 resamples and seed.

All 15 operations met the 1.25× delivery ceiling. Their paired 95% upper
bounds ranged from 0.0864× to 1.1107× of the reference time. Three met the
0.80× optimization target. The reference reported bitrate and buffer
warnings on three inputs. Its original stderr and output hashes are retained.

The 15 capture library source files match the recorded benchmark source
manifest byte for byte at the Streem development pin. The worker, build,
quality and input identities are recorded in the [receipt](receipt.json).
The receipt identifies the retained raw archives by SHA-256. The timing
figures measure the library worker's whole session. Application capture
also includes render readback, scheduling and socket writes.

The memory observer refused admission before running selected memory
cases. It could not certify allocator import coverage for the reference
executable and three linked FFmpeg libraries. The original refusal,
cleanup and prior claims were retained. Neither lower heap nor lower peak
RSS has been established for this worker pair. The combined Streem debug
build and simulator results are application evidence. Real device execution
remains open under [O62](../../../docs/offboarding-target.md).

# K5 metadata qualification and preserved investigations

K5 is qualified on native Mac ARM64: 48 authored profiles and the mandatory intact
`mkv/hdr10tags-both.mkv` pass exact source/native parity, the <=0.80 upper paired
95% runtime gate, and both <1 memory gates. K0–K4 also pass on the same recorded
source. The [receipt](../qualification/segment-metadata-mac-arm64/README.md) retains
all 116 workloads, raw pairs, source/workers/inputs, checks and failures. Complete
modules, target integration and all twelve offboarding milestones remain open.

The intact HDR input's old refusal remains in the immutable
[K4 receipt](../qualification/colour-hdr-mac-arm64/additional-hdr-input.json).
The current input is neither stripped nor substituted. Full upstream HDR decoded
frame-priority tests and sample redistribution rights remain unqualified.

## Memory investigation

The initial 30-pair memory campaign passed source/native parity but failed dense
metadata. Its exact source/build inputs/workers and report remain frozen in ignored
`.research/catalogue-memory-initial/`; the final receipt records their hashes.

| Case | Initial heap upper ratio | Initial RSS upper ratio |
|---|---:|---:|
| 512 chapters | 4.288060051606849 | 1.457142857142857 |
| 512 tags | 1.3887186471456134 | 0.7428571428571429 |

Source nodes now use twelve rather than twenty bytes. The native-view adapter
borrows source strings and keeps a single dictionary entry inline, avoiding a
B-tree allocation for every chapter. All original source bytes, trees, fields and
outputs remain required. Final 512-chapter heap upper ratio is
0.8287590898428336; 512-tag heap upper ratio is 0.5114061967994552. Both RSS gates
also pass. No class, threshold or payload was removed.

## Runtime investigation

A clean 1 MiB attachment run failed at upper ratio 0.8474820869645205. Exact
source/workers/pairs remain frozen in ignored `.research/catalogue-runtime-initial/`.
Separate diagnostic sampling (not qualification timing) identified the full ordered
checksum as the main cost in both adapters, about 96% Rust and 79% native.
Both workers now calculate that same byte polynomial using four independent lanes
for large payloads. Every byte and original order still contribute; independent
Rust and Python scalar checks pass. The first clean new attachment upper ratio,
0.7264960823763555, was retained exactly in the final receipt.

## Checkpoint investigation and final proof

An interrupted replay could previously discard an unvisited later clean observation.
The actual reproduction and before/after tests remain in the receipt. Partial
reports now retain unvisited cached rows separately; duplicate identities fail
closed. All 26 original clean timing observations recovered in acquisition order
remain exactly equal in the final report, including any failures. Source comparisons
run fresh; rebuilt calibration probes require new memory observations. Partial
reports are always unaccepted.

The final host checks pass 50 Rust and 39 Python tests plus formatting, Clippy,
no-default features, release and strict-safe/design checks. Seven specification
snapshots and live primary errata were reviewed. Two upstream reference FATE
cases and three separate packet-demux comparisons pass on this exact identity.
The complete source/API behavior and bounds are in
[segment-metadata.md](../../docs/segment-metadata.md).

Native dictionary, language, chapter and attachment representation remains an
explicit adapter view. Independent source proof retains original BCP47/legacy
values, binary tags, every target UID, all chapter trees and IEEE/date precision.
Remaining packet side data, codec state, resumable input/work, shared retention
budgets and later formats continue toward full offboarding.

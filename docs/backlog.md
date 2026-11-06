# First implementation backlog

The [current Streem coverage and adoption backlog](offboarding-target.md#preview-coverage-and-remaining-work)
tracks preview implementation, open profile coverage and delivery evidence.

This is the original work sequence. H01–H07 and K01 now have experimental implementations. The frozen K0–K5 slices
pass native Mac ARM64 gates, including bounded zlib, Cues/preroll, static colour/HDR
and source segment metadata. K02 still
means completing all selected M01/M02 features and release targets; it is not the
K2 zlib slice. Explicit remaining gaps are in [implementation.md](implementation.md);
no row is release-qualified. Nothing changes the production Streem backend yet.

| ID | Work | Depends on | Done evidence |
|---|---|---|---|
| H01 | Reproducible FFmpeg reference builder and run manifest | Scope | Exact source/config/compiler/binary pins; selected operations available; baseline expected outputs reproduced |
| H02 | Configure-expanded FATE selection and mapping | H01 | Upstream fate-list reconciled with selected modules; every in-scope identity mapped or visibly blocked; initial 177 lexical entries reconciled |
| H03 | Rights-aware content-addressed corpus cache | H02 | Per-asset hashes/origin/license; selective bounded fetch; no silent sample substitution; synthetic and licensed conformance vectors included |
| H04 | Canonical trace protocol and adapters | H01–H03 | Packets, PTS/DTS, metadata, side data, key flags and seek receipts compared without lossy normalization; independent decoder check where needed |
| H05 | Matched persistent benchmark workers | H04 | Identical work, startup separated, allocation/RSS lanes, paired timing/confidence, target/build/cache/thread identities, raw samples; gate fails when unmeasured |
| H06 | Limits, fault transport and fuzz-state runner | H04 | Truncation/length/overflow/decompression/range/EOF/abort regressions; repeatable open/read/seek/reset sequences; no unlimited scans |
| H07 | Strict-safe dependency audit and admission checks | Architecture | Owned crates forbid unsafe; features/macros/target graphs inspected; no dependency automatically admitted from a passing build |
| C01 | Own core types/time/input and budgets | H04, H06, H07 | Exact arithmetic and failure semantics, capacity/work accounting, 32-bit checks and FFmpeg comparison; authorized 1.20 initial-delivery/lower-memory proof;0.80 remains optimization target |
| K01 | Own MKV AVC/HEVC/AAC/AC3/EAC3 vertical slice | C01, H02–H06 | Exact packets/config/timestamps; valid keyframe-preroll seeks; unknown-length input; clearly named slice, not full M01 parity |
| K02 | Complete selected M01/M02 contract | K01 | All required lacing/compression/metadata/side-data/seek/limit cases and both performance gates pass on release targets |
| A01 | Streem opt-in adapter and hardware lane | K01 initially; K02 for acceptance | Existing auth/clocks/queue/route ownership preserved; old backend selectable; isolated synthetic/authorized TV runs with repo lock |
| I01 | Own regular/fragmented MP4 reader | C01 + H0 | Edit-list/signed composition/fragment cases and random access; all chosen features, speed and memory gates |
| T01 | Own TS/M2TS reader | C01 + H0 | PSI/PES/continuity/timestamp wrap and selected segment input; matched FFmpeg gates |
| S01 | Subtitle packet and bitmap independence | K02/I01/T01 as needed | Text/config preservation and PGS/VobSub/DVB reference outputs; finite resources; libass remains outside this replacement |

H01–H07 form the H0 framework milestone. Do not spend the first phase implementing many codecs while deferring the oracle, corpus and performance runner. Freeze each module's profile list, absolute resource budgets and benchmark workload classes before its implementation acceptance campaign.

The design does not provide a trustworthy calendar estimate yet. H0 will expose corpus, reference-operation and target gaps; use that evidence to estimate each module separately. AI-assisted implementation does not change the gate definitions or authorize replacing measured failures with expected future progress.

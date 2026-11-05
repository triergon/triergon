# triergon-mpegts syntax slice

The selected audio/M2TS whole-object checkpoint adds the explicit
`--native-selected-object-v2` operation. Its frozen Linux source passes569
core/TS/example tests per debug/release profile, including18 new format,
namespace, work/cancellation, pressure and retained-owner guards; the existing
input-export helper remains intentionally ignored. All17 selected raw native
objects and independent source traces match byte-for-byte: six finite188-byte
AAC/AC-3/E-AC-3 cases, two video controls and nine finite192-byte video cases.
The unchanged695 registry,21 packet objects,21 prior whole-object and21 PSI
views, entity controls and argument refusals are preserved. Clippy, formatting,
shipping and ARM32 checks pass. Evidence is
`.research/ts-m2ts-audio-closure-v1/handoff-1/` (source411c83d1).
The original TS188/video constructors and operation flags retain their previous
admissions and refusals. Private audio names are explicit archived-native views;
raw source stream types, descriptors, `Unspecified` namespace and unknown clocks
are retained. The observed four-byte M2TS prefix/position view is not a Blu-ray
conformance claim. This checkpoint has no new runtime or heap/RSS qualification;
complete M04, unobserved audio/M2TS combinations, long/nonfinite startup and
broader recovery remain required.

Independent pure safe Rust; only dependency is `triergon-core` at `../core`.

The selected-diagnostic checkpoint passes258 TS and50 core tests in each
debug/release profile, Clippy, shipping checks and both release observers on its
frozen Linux graph. `NativeD32SelectedAvcDiagnosticsV1` retains real rejected
carriers and diagnostics while observing selected complete finite AVC PES data.
Encoded native clocks remain separate from source `PesHeader` and unknown DTS.
TEI, scrambling, invalid source clocks and source epochs remain observable.
The unchanged272-request registry produced272 Rust and281 native observations:
80 declared packet-field matches and zero packet differences. All six earlier
fault discrepancies are closed. The23 nonpass/context rows, one empty observation,
nine multi-entity operation gaps and original17/15 memberships remain required.
Receipts and exact source pins are in
`.research/parallel-ts/selected-faults-v1/ready-v1/`.
Combined public gates and complete native, numerical and target qualification
remain separate.

The earlier bounded DVB finite-PES checkpoint passed222 debug/release tests, Clippy,
the no-default library check and both release observers on native Linux.
All195 predecessors are preserved; the27 new guards first produced7 passes and
20 intended failures. Explicit single-service DVB signalling admits the complete
opaque PES data field, without stripping identifiers/tails or validating inner
subtitle segments. The earlier four finite TS188 AC-3/E-AC-3 carriage views remain.
Source namespaces, raw33/unknown clocks, source payloads and owners stay intact;
no access-unit, duration or source-codec inference follows.

The complete272-row registry was replayed with272 Rust and281 fresh Linux native
calls. Source, ownership, context and causality checks report zero failures;
74 declared packet-field comparisons matched. Six packet discrepancies remained:
PCR discontinuity, invalid PCR extension, TEI, scrambling, PES timestamp markers
and forbidden timestamp flags. The29 nonpass/context rows, one empty observation,
nine multi-entity operation gaps and original17/15 memberships were retained.
No HLS observations were joined. Receipts and the exact remaining cases are in
`.research/parallel-ts/dvb-native-pes-v1/ready-v1/`.
These are source-specific functional results; combined-source gates, complete
native work, runtime and strictly lower heap/RSS qualification remain separate.

## Historical checkpoint notes

The earlier registered/rejection batch: the parent observed the staged21 tests
**11 passed,10 failed**, with the exact nine Unsupported cases and actual OPCR
acceptance; all old130 passed on that graph. Evidence is
`.research/ts-registered-red-1793463982209107000/`. The implementation now admits
the three selected registered finite HEVC/ADTS/LATM contexts through the explicit
constructor while retaining legacy `new`'s H264-only contract. Strict transport
inspection rejects OPCR without PCR and retains the original charged carrier on
the real diagnostic. It checks actual fields/stuffing incrementally: no validation
countdown, repeated full parse or fabricated accepted packet/header.

The opt-in recovery policy consumes the observed complete188-byte OPCR class
through a separate native envelope/encoded-header view. It preserves both native
packets and leaves rejected source clocks/header/time invalid/None. The original
invalid test has only its approved access/type correction; all other old test
semantics and fixture bytes persist. New guards cover hidden owners/deduplication,
work0/1, cancel/pressure, second faults, ordering and source diagnostics8/9.
At that prepared checkpoint, the existing21 regressions and new guards,
Clippy, both release workers and full source/native replay were pending the
parent queue. Its implementation hashes and source expectation version remain in
`.research/parallel-ts/registered-impl-v1/`; the later167/195-test receipts are
separate and do not rewrite that historical HOLD. The previous130/266 proof below covers its frozen
earlier graph and is preserved unchanged.

Continuity/recovery: the parent's actual runtime red was **16 passed, 8 failed**,
exactly the eight predicted failures, with all139 selected files unchanged.
Evidence is `.research/prepared-red-batch1-1793448731454463000/ts-continuity/`;
the verified log SHA is010ae9cf9520f341b4c4aaaf54135eeea40b610e126e756d5245269bdf17c93b.
The narrow source fix now recognizes duplicates before interpreting the flag and
reports an actual counter break separately from a continuous flagged packet.
Continuous partial PSI/PES is retained. Raw clocks, timestamps/unknowns, source
identities/epochs and all24 new plus75 older tests/fixtures are unchanged.
The parent's frozen pre-emission graph now passes **all99 source tests**, with
all143 selected files unchanged. Evidence is
`.research/ts-native-red-observed-1793450994269968000/source99/`; log SHA
053c079637c2e2874bca2ea38b970540374719eae678f6b61b0a9331953fb219.
That source-fix proof precedes the real native observer below. Source-fix
evidence remains preserved in `.research/parallel-ts/continuity-impl-v1/`.

The parent also retained17 actual archived19c native observations in
`.research/ts-continuity-native-v1-observed/`, including the invalid first OPCR
authoring draft. The distinct opt-in bounded native transport/PES observer design
in `.research/parallel-ts/continuity-red-v1/NATIVE-POLICY-DRAFT.md` has now received
parent development approval; it grants no baseline change or case/gate waiver.
The parent's new31-test runtime red was **6 passed, 25 failed**, exactly the
predicted Unsupported emission/header-time entrypoints, with unchanged143 files.
Evidence is `.research/ts-native-red-observed-1793450994269968000/native31-red/`;
log SHA c2abae9efcd0acde8729a181d3bf2b7cee03b3322438d153db416c2478958a63.
The opt-in `native_pes::NativePesObserver` now implements
`ArchivedNativePesPacketsV1` with actual transport/PUSI/data/EOF assembly,
charged owned spans/causes, incremental work/allocation/cancel handling and an
explicit raw-header clock view that reuses the existing calculation. It retains
causal raw33/None/header/source facts even when a native continuation has no
applicable timestamp header. It never clones/splits/repeats complete source PES.
The default strict transport/PSI/PES operation remains separate.

Eight additional actual archived19c probes in
`.research/ts-pes-boundary-native-v1-observed/` establish the finite-body
pre-append boundary and duplicate-continuation damage ordering. Their complete
raw outputs/physical assignments and the verified130-test test-first record are
in `.research/parallel-ts/native-pes-impl-v1/`. The31 native tests and99 source
tests/fixtures retained their original expectations. The parent's actual frozen
run in `.research/ts-native-host-v1-1793455027586572000/` passes **all130 tests**,
including all31 native regressions. Its Clippy run found only the test initializer
at `tests/native_pes_packets.rs:578`; that initializer now retains `pes_bytes: 100`
inside the `ParseLimits` literal. That earlier run did not build the release worker.
The parent's later frozen checkpoint in
`.research/ts-native-composed-host-v1-1793459116821845000/` passes **all130 tests**,
all-target Clippy with warnings denied and **both release examples**. Source
before/after was unchanged. The parent then completed **all266 actual captures**
in `.research/ts-composed-v1-observed/observations.json`:223 exits0 and43 exits1,
with zero NDJSON protocol errors and stable source/input/workers/archived19c
references. Raw watchdog statuses remain223 `invalid-json` and43 `rejected`;
the single-JSON helper's status is separate from explicit line-protocol validation.
Strict source/native comparison and full qualification remain open; execution,
valid NDJSON, empty output and exit0 are not parity or numerical acceptance.
The first native context is one source
entity/fixed source epochs and H.264 stream_type0x1b/stream_id0xe0. Packet-cap,
unproven clock/entity/header/carriage contexts stay explicit nonpasses. No native
reference flags, default packet buffer or original comparator is changed.
Four frozen duplicate/loss differences and all15 current native gaps remain
required. Previous red-staging artifacts/inputs and receipts remain preserved in
`.research/parallel-ts/{continuity-red-v1,native-pes-red-v1}/`.

The parent-reviewed composed adapter is now authored in
`examples/mpegts_native_pes_trace.rs`. It drives the same bounded input cursor and
strict transport/PSI/PES stream, reusing the existing source serializer. Each raw
selected-PID carrier enters the charged native observer before the next carrier;
each owned native result is written and released immediately. There is one
inline observer and no packet replay/output queue. Original source JSON rows
remain unchanged and ordered. Extra `native_pes_*` rows expose the exact archived
pin/worker/default flags, validated scope, full payload, causal header/raw33/None,
physical spans/source ordinals, separate time view, flags/position/side data and
explicit nonpasses. These ordinals are independent of I/O tokens and source
epochs. Unsupported entity/config/clock boundaries never reopen the observer or
flush a segment as logical EOF; source parsing continues after native failure.
The source-PES optional clock and native-packet clock have separate histories.
The diagnostic adapter is compiled and executed in the frozen parent receipts
above. `.research/parallel-ts/composed-native-v1/` preserves its original
pre-compilation plan/checkpoint; current strict review is recorded separately in
`.research/parallel-ts/composed-v1-comparison/`. Every source diagnostic, native
nonpass and unobserved context remains required. All15 historical clock/codec
field differences, all17 full-case dispositions and the unchanged complete
per-case runtime/memory gates remain open. Rust behavior stays on HOLD. This
proof-only README refresh follows the frozen execution graph; it changes no
Rust source, test, fixture or worker.

Clock implementation checkpoint: the parent retained the actual red run for all
22 regressions in `tests/clock_views.rs`: **2 passed, 20 failed**, with both valid
discontinuity controls passing. Evidence is
`.research/ts-clock-red-observed-1793441445237521000/run.log`; source before/after
was unchanged. The 17 unsupported projection cases and three PCR defects are now
implemented. The parent's frozen checkpoint passes **75 TS tests**, Clippy with
warnings denied and the release trace build; evidence is
`.research/ts-clock-host-1793443666396670000/`. Recorded-output checks now match
all **72 required source operations** (64 ordinary, six DVB, two recovery) and
all **64 optional-view source rechecks**, with zero source discrepancies/missing.
Against the unchanged archived native61, the named clock view brings declared
payload/time/codec field agreements from44 to46; **15 declared-field differences
remain**. All17 historical full-case dispositions remain unqualified. The original
PES payload check remains49/61, and three HLS native contexts remain unobserved.
Both fresh recovery-native operations match payload, the explicit PTS-only time
view, program/PCR, position and stream-ID observations; source DTS remains absent
and each exact source diagnostic is retained. Their actual native binding is the
archived worker19c06e53/sourcea045 pair, not the newer common wrapper or D0 worker.
Reports are in `.research/parallel-ts/clock-v4-comparison/`. These host/field
observations do not grant native parity or numerical qualification. This is a
documentation-only update after the test/replay graph was frozen.
Bounded source time uses exact modular arithmetic/core Time. The separate native
view chooses a congruent representative in a declared pin-bound interval; raw33,
source absent DTS, header/payload bytes and source epochs remain observable.
Invalid signal persistence, too-few PCR samples and pending PCR-PID rebinding now
diagnose and quarantine trusted interpretation, without inventing a Timeline.
`clock_is_quarantined()` exposes that state; explicit format discontinuity resets it.
The preceding 53-test/70-source-check evidence below covers the DVB snapshot,
not this changed graph. Parent queue, source/native matrix and checksums are in
`.research/parallel-ts/clock-impl-v1/`. The red checkpoint remains immutable.
No local Cargo/rustc/native/fuzz/benchmark execution occurred.

The DVB-signalling-v1 batch is **HOLD after the parent's green make9 gate**:
all **53 TS tests passed**, including the 13 new regressions and six copied D0
source profiles. Parent-reported make9 passed 276 Rust and 48 Python tests, the
full safety gate and both 51,235-case arithmetic runs. It adds
bounded EN 300 468 `0x59` views, explicit descriptor namespace evidence and a named
single-service D0 codecpar view. Full source/native and numerical qualification
remain open. The parent rebuilt the unique example and froze its worker/source
graph in `.research/ts-dvb-source-v3/`. All **70 actual source checks matched**:
64 ordinary operations and six D0 operations, with zero discrepancies or missing
cases. Source before/after closure was unchanged at collection. The reports are
`.research/parallel-ts/parent-source-comparison-v3.json` and
`.research/parallel-ts/dvb-signalling-v1/parent-d0-source-v3.json`.
The current proof checkpoint is
`.research/parallel-ts/dvb-signalling-v1/checkpoint-source-v3.json`.
The initial implementation checkpoint and 27-file/hash inventory remain historical
snapshots in the same folder; this proof-only documentation update follows the
frozen collection and changes no Rust source, test, fixture or example.

The parent's preceding targeted run passed all **40 TS tests**: ten composed,
21 syntax and nine signalling. Exact evidence is
`.research/parallel-framing-ts-dvb-first.log`. The later make9 result above covers
the DVB changes. This earlier run is signalling-v3 prototype correctness evidence;
native parity, full M04/O31, runtime and memory remain unqualified. Any subsequent
source change needs fresh verification. At that historical signalling-v3 checkpoint
the private clock draft was unimplemented; the later75-test proof above covers
the subsequently implemented clock slice.

The parent also rebuilt the unique `mpegts_source_trace` release target with the
new core/signalling graph and executed all **64 original source operations** again.
`.research/parallel-ts/parent-source-comparison-v2.json` records **64 matches,
0 discrepancies, 0 missing** against the independent oracle. Actual outputs and
worker/graph identity are in `.research/parallel-format-source-v2/{identity.json,
source-recheck.json,ts/observations.json}`; the parent's collection-time graph
recheck was stable. All processes exited 0 while malformed cases retained their
source diagnostics. This source-only result leaves all 17 native gaps unqualified.

The parent's make4 full-workspace gate passed all **31 TS tests** (ten composed
plus 21 syntax), including the narrow composed PSI-pointer recovery fix. This is
executed prototype correctness evidence; subsequent source changes require fresh
verification and remain unqualified. A rejected PAT/PMT pointer becomes an exact
source diagnostic, discards
the affected partial section and permits a later trustworthy PUSI. Low-level push
still rejects with the original charged input. Ownership/invariant/resource errors
retain their existing propagation. No Cargo or native process ran in this lane.

The subsequent signalling slice adds nine original regressions in
`tests/signalling.rs`, included in the 40 passing targeted tests above. The make4
result predates these descriptor changes. Registration/language,
AVC/HEVC and explicit DVB AC-3/E-AC-3 views preserve source fields and borrowed
tails; PMT validation now checks selected video-descriptor syntax and HEVC context.
The targeted unit tests do not establish fresh source/native trace parity or
numerical qualification.

The parent's preceding composed-slice run on 2026-10-31 reported **29 passed,
0 failed**, across the eight composed tests and 21 syntax regressions. The parent
also built the release trace example and executed all **64 source operations**
(61 standalone TS/M2TS cases plus three HLS sequences). The independent comparator
reported **64 observation matches, 0 discrepancies, 0 missing** in
`.research/parallel-ts/parent-source-comparison-v1.json`, using actual outputs in
`.research/ts-source-observed/`. One source process exited 1 for the expected
malformed `psi-pointer-overrun` input; that rejection remains recorded.

The older v1 results precede the example rename and PSI recovery fix; the fresh v2
run above covers the preceding signalling-v3 unique target and all 64 originals,
before the new DVB-signalling-v1 changes. At that older checkpoint both additive
recovery operations still needed actual source/native traces; the fresh v4 proof
above now records them. Native parity, runtime
and memory remain unqualified. This is not complete M04 or TV acceptance, and later behavior
changes require fresh evidence.

The subsequent read-only three-view comparison is
`.research/parallel-ts/recorded-three-view-comparison-v1.json`. It binds actual Rust
outputs to the source observer and archived pinned native outputs: 49/61 payload
observations match, 44/61 match the declared development fields, and all 17 native
discrepancies remain. Three native HLS sequence observations are pending. These
counts include malformed/empty results and are not conformance or parity passes.
The PTS-only native DTS view is parent-reviewed development work, not a user case
waiver; source facts and all original qualification gates remain unchanged.
The additive inputs, oracle checks, proposed packet/metadata views and exact next
queue contract are in `.research/parallel-ts/recovery-native-metadata-checkpoint-v2.md`.

Implemented boundaries:

* `parse_transport`: exact 188/192 source packet, bounded adaptation/private/extension
  regions, raw PCR/OPCR, source offsets, opaque M2TS prefix and shared charged spans.
  TEI/scrambling are observable flags; a coordinator must discard their payload.
* `TransportFramer::poll`: shared `InputCursor`/token/source lifecycle, <=192-byte
  charged carry, work yields, recoverable freeze and entity End. Input generation
  or internal range changes require the corresponding explicit format reset.
* `Continuity`: per-PID payload/adaptation counters, wrap, whole-packet duplicate
  with valid PCR exception, excess duplicates, loss/signalling and damaged input.
* `SectionAssembler`: pointer tails, short headers, multiple sections, bounded
  source extents, incremental PSI CRC and owned zero-copy-frozen section output.
  Bad CRC remains observable; PAT/PMT readers cannot activate it as valid metadata.
* `PatReader`/`PmtReader`: bounded resumable source parsing, ordered opaque descriptor
  loops, original source/physical ranges, network-PID identity, stream/PCR identities,
  and owned charged metadata vectors. Private descriptor semantics need a namespace.
* `Descriptor::{registration,languages,avc_video,hevc_video}` exposes bounded typed
  source views, preserving compatible/profile bits and ordered language roles.
  Explicit `dvb_ac3`/`dvb_eac3` calls preserve optional fields and opaque tails;
  ordinary PMT parsing does not assign a DVB identity to an unscoped private tag.
* `Descriptor::dvb_subtitling` borrows the complete `0x59` TLV and ordered
  eight-byte service entries, with raw language/type and big-endian page IDs.
  Empty bodies remain empty; malformed lengths fail; at most 31 entries fit.
  `PmtReader::new_with_namespace` and `TransportStream::new_with_namespace`
  record explicit caller DVB evidence. The existing constructors use `Unspecified`.
  Scoped validation/summary work stays inside charged descriptor steps. Codec
  identification requires the explicit DVB context, private PES stream type `0x06`
  and nonempty ES-level service signalling; contradictory DVB audio tags are
  reported as conflicting signalling. Program-level descriptors never assign an
  ES codec. Raw TLVs, duplicates, private specifiers and source epochs remain intact.
* `ElementaryStream::native_dvb_single_service_view(D0SingleServiceV1)` returns
  `compID_be + ancillaryID_be + type` and its borrowed source service. It requires
  exactly one eight-byte service in one `0x59` ES TLV; additional ES context,
  duplicates and multi-service cases are explicitly unsupported by this view.
  This is a parent-reviewed development native view bound to the D0 pin. Actual
  native evidence covers the six authored D0 profiles, not every field value or
  multi-service mapping. It adds no payload adapter, timestamp inference or decoder.
* `PesAssembler`: split fixed/optional headers, finite/unbounded-video lengths,
  timestamp/optional-field bounds and markers, header source ranges, raw33 PTS/DTS,
  local clock/configuration epochs and ordered owned payload spans with elementary
  offsets. Work partitioning within a supplied chunk does not multiply span count.
  The result is a PES byte run, never a codec access unit. Unknown DTS stays unknown.

Typical coordinator order: drain packet framer; observe PCR/transport facts; check
TEI/scrambling/continuity; discard damaged partial units; route known PSI/PES PIDs;
drain the relevant assembler; parse source tables; atomically activate a complete
current PAT/PMT version; publish source/epoch changes before affected PES bytes.
Duplicate payload is never re-fed. Non-PCR CC signalling is not a clock reset.
Normal entity End/EndSegment does not call PES `end_of_source` or `discard`.

The composed operation is `TransportStream`: explicit
selected-program routing, atomic complete-current PAT activation, staged PMT route
updates, continuity recovery, program-clock transitions, and PSI/PES drain through
logical EOF. Other PAT programs remain enumerated, but this named first operation
produces PES for the explicitly selected program only. Eight composed regressions
are included in the parent-reported 29/0 result above; the two new pointer-recovery
regressions also passed in the parent's make4 gate.

`mpegts_source_trace 188|192 PROGRAM [--dvb] [--native-clock-v1] FILE... [--discontinuity FILE...]` uses the concrete
shared input cursor and emits source JSON lines, including exact transport/section
bytes, ordered descriptors, complete PES header/payload bytes, source extents and
raw33 time. Ordinary entity boundaries do not flush a PES. A byte-run epoch records
loss/recovery independently of I/O, configuration and clock epochs. Serialization,
file I/O and CLI error allocation are external correctness-adapter work, not the
production parser operation or a runtime/memory measurement.

The optional `--dvb` is an explicit caller-declared transport profile. It adds
ordered service fields and the separately named, unqualified D0 native metadata
view to PMT JSON. The default trace schema is preserved. For example, the parent
can run the rebuilt unique target with
`188 1 --dvb crates/mpegts/tests/data/dvb/dvb2-literals-fields.ts`.
The optional `--native-clock-v1` adds a separately named PES child object containing
the declared archived reference profile, source raw33/epochs/quarantine, native-view
times/origins and the first source reference/interval origin. The default source
schema is retained. A new view object is a new native context; an ordinary entity
boundary or source clock/configuration epoch does not silently reset that context.
The original no-parser PES operation and the default-parser D0 operation remain
distinct. Fresh source/native comparisons are required before any new parity claim.
The full source PES data field still contains `20 00 + segments + ff`; the D0 native
default demux/codec-input observations contain bare segments. Those are distinct
boundaries. Source DTS remains absent even when native DTS equals PTS. M05 owns
segment/envelope interpretation and subtitle decoding; native packet equality is
not established by the five-byte metadata view. All 17 previous native gaps and
all runtime/memory gates remain open.

Retained D0 observation inspection also preserves a new malformed-input difference:
TS emits 101 of the original 106 bare bytes for `dvb-truncated-segment`, omitting
`0f80000100`, in both discovery modes. Matroska retains 106 and the decoder's second
call on the remaining five bytes returns `-1094995529`. The source fixture/test
keeps the entire original PES field. This difference stays unqualified in
`.research/parallel-ts/dvb-signalling-v1/native-bindings.json`; no native run or
source-output normalization was performed here.

The original example is `examples/mpegts_source_trace.rs`; parent builds it with
`--package triergon-mpegts --example mpegts_source_trace`. Its release binary is
`target/release/examples/mpegts_source_trace`, distinct from other packages' examples.
The new composed example is `examples/mpegts_native_pes_trace.rs`, built with
`--package triergon-mpegts --example mpegts_native_pes_trace`; its binary is
`target/release/examples/mpegts_native_pes_trace`. It accepts the same
`188|192 PROGRAM [--dvb] [--native-clock-v1] FILE... [--discontinuity FILE...]`
arguments. Native nonpass exits nonzero after completing available source facts.
Successful collection and empty observations grant no parity or qualification.

Bounded auto-detection/resync/seek, full all-program PES operation and M02 AU framing
remain open. Adaptation extensions and private/SI tables preserve raw bytes; not
every extension semantic is validated. Full ATSC/HDMV and codec framing need the
normative/corpus items in the lane's provenance-and-gaps document. Clock signalling
repetition, missing/boundary PCR and complete selected corruption coverage still
need independent source/native comparisons before acceptance.

Use one shared `Budget` for all accepted input and workspaces. A returned `Bytes`
view charges its whole backing allocation, including hidden prefixes/suffixes.
`BudgetVec` charges extent/metadata/span capacities. Inline fixed arrays do not add
heap allocation. All immutable output can outlive its parser. Rejected push returns
the original Bytes in the shared `PushError`; retryable poll errors preserve state.
Malformed/limit faults are sticky until explicit discard, while cancellation of a
syntax poll itself is non-destructive. The shared cursor owns its sticky I/O failure
and cancellation/recovery lifecycle. Budget pressure must be resolved by releasing
caller retention or an explicit policy/recovery decision, never an unbounded retry
loop or hidden eviction. Corpus qualification must include the chosen limits.

Tests in `tests/regressions.rs` were authored before implementation. They exercise
the independent fixed CRC/PAT words, table/descriptor/length errors, PES/source/time
facts, short reads/work partitions, continuation across source entities, transport
failure/reset, backpressure and retained-capacity lifetime. No third-party fixture
or implementation code was imported. Parent owns workspace registration and all
Cargo/build/test/qualification execution.

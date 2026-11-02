# safe-media-mpegts syntax slice

Independent pure safe Rust; only dependency is `safe-media-core` at `../core`.
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
source change needs fresh verification. The private clock-projection draft is
unimplemented and unqualified.

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
before the new DVB-signalling-v1 changes. Both additive
recovery operations still need actual source/native traces. Native parity, runtime
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

`mpegts_source_trace 188|192 PROGRAM [--dvb] FILE... [--discontinuity FILE...]` uses the concrete
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

The owned example is `examples/mpegts_source_trace.rs`; parent builds it with
`--package safe-media-mpegts --example mpegts_source_trace`. Its release binary is
`target/release/examples/mpegts_source_trace`, distinct from other packages' examples.

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

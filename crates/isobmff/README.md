# triergon-isobmff

Independent safe-Rust syntax/index seam, with only `../core` as a dependency.
Experimental: complete M03, decoder recovery, runtime/memory and shipping-target
qualification remain open.

The v37 implementation passed all 155 all-target definitions in debug and release,
targeted 32, Clippy with warnings denied, the no-default library check and release
example builds on native Linux ARM64. All 116 checkpoint32 test definitions and
their 25 source/helper/example files remain byte-identical. These are private
format gates on a frozen source graph. The implementation and unchanged guards
are now in the shared workspace; whole-workspace and target gates have separate
exact-source receipts. Complete module qualification remains open.

`Mov9NativeIndexCursor`, `Mov9NativeSeekJob` and `Mov9NativeGlobalSeekJob` expose a
separate pinned-MOV compatibility view beside the canonical source planners.
Entries retain original Samples, configuration/ranges, raw and signed clock
fields, API-null state, key/discard bits and edit/origin provenance. Bounded
metadata replay reconstructs per-track state before the existing global packet
selector. The observed equal-native-timebase global seek profile is supported;
null-clock, late/overlapping and unestablished rescale/decoder-recovery semantics
remain explicit refusals. Index distance0 never fabricates a source sync sample.

The v37 open and native-index consumers each have 236 whole-object ordered matches
and 19 required nonpasses per schedule over all 255 inputs. The 34 prior control
pairs and 16 track2 pairs match; eight ordinary track2 reference objects also
match the retained historical reference. No comparison sorts actual candidate or
native packets or normalizes clock fields; source and native views remain
separate. Fresh source/default/physical replay retains 229 strict source matches,
16 source-time unresolved cases and 10 malformed rejections per schedule. Physical
comparison retains the same 16 mismatches. All 828 earlier source-event sequences
and 96 legacy checks remain unchanged; the 16 source dispositions are still open.
Native agreement does not close them.

Exact source, worker, input, raw outcomes and review receipts are under
`.research/parallel-mp4/implementation/native37-final-proof/`. The independently
gated source is associated with native37-parent-1793502238561659368. This README
records the shared implementation beyond that historical gated snapshot; it
does not assert full native/module parity or numerical acceptance.

The later metadata40/core41/packet42 snapshot reuses `InputCursor::read_append`
for reserved metadata and packet buffers, preserving work, cancellation, input
position, binding and final immutable ownership. Core41 exact-clock construction
has a separate51,235-case independent oracle pass in each debug/release profile;
packet42 leaves that arithmetic source unchanged and reuses that proof.
The frozen42 source passes58 core and159 MP4 definitions (217 combined), plus10
standalone consumer tests per debug/release profile, Clippy, no-default and release
builds. All1020 whole-operation rows and510 source-index rows are byte-identical
to41. The1020 rows retain944 ordered full-object matches and76 required nonpasses;
the42 protocol classes comprise21 fresh candidate calls and21 retained pinned
native calls. The original37 source admission and all old16 source dispositions
remain unchanged; source-evidence39 was inactive at that checkpoint.
The frozen42 consumer uses initialized1-byte,12-byte and64KiB scratch tiers at maxima1,
2..=12 and greater than12 respectively, without changing read/seek requests.
Final linked ARM64 instructions show the direct reserved single-byte append and
1-byte refill path; growth and final immutable-owner allocation remain permitted.
Receipts and exact source pins are in
`.research/parallel-mp4/implementation/packet-direct42/variant-contract42.json`,
`GATE1530-RECEIPT42.json`, `CODEGEN42.json` and `source42-association-PARENT.json`.
These gates name a private frozen graph; combined public-tree gates and fresh
numerical/memory qualification remain separate. No complete module acceptance
follows. Earlier scratch-v1 and all first-clean performance failures are retained.

The later explicit source39 approval applies only to the frozen42 worker and its
complete proof chain. It corrects16 checker-domain errors without changing source
bytes, unknown clocks or the12 independently derived native continuity assertions.
All19 native/error cases remain denied. The approval, matrix and retained controls
are in `.research/parallel-mp4/qualification-native42-transfer-v1/`.
It grants no numerical acceptance and does not qualify a later changed worker.

The later43v2 checkpoint passes59 core and159 MP4 definitions plus11 standalone
consumer tests per debug/release profile and fresh51,235 exact-clock cases per
profile. All1530 operation outcomes remain byte-identical to42. Only the hot
`read_append` boundary is forced inline; its body and all policies are unchanged.
The narrow timestamp path uses64-bit division and retains checked128-bit products
and the wide fallback. Requests13..=4096 use initialized4KiB scratch; smaller and
larger tiers preserve the same read/seek requests. Final linked code removes the
hot append result-slot roundtrip with256 additional text bytes. These static facts
are not measured speed, peak-stack or memory results. Exact private source, gates
and code generation are in
`.research/parallel-mp4/implementation/profile-batch43-v2/`.
Combined public gates, current-source admission and numerical qualification remain
separate.

## Historical checkpoint notes

The notes below describe their named earlier snapshots and preparation phases.
Their then-current/pending labels do not supersede the bounded current status above. Raw
failures, source gaps and original receipts remain preserved.

The checkpoint32 snapshot passed 116 all-target regression definitions, Clippy
with warnings denied, the no-default library check and release example builds
on Linux ARM64. All 102 earlier definitions remain unchanged. The 14 new controls
first failed against callable stubs, then passed with the implementation.
No full-module, native seek or numerical qualification follows from these checks.

`SourceSeekPlanner` borrows compact regular/fragmented source indexes, selects a
preceding container sync sample, records explicit available/missing preroll, and
validates every decode-order media range before publishing a plan. It preserves
original sample/configuration identity, unsigned sparse addresses and unknown
source clocks. `TrackFragmentRandomAccess` and `MovieFragmentRandomAccessTail`
retain compact `tfra` rows and checked `mfro` bounds. These are source-only plans
and unverified index hints. Checkpoint32 adds exact rate-one movie/edit lookup,
decode-order composition predecessor selection and compact source `roll`/`prol`
mapping with explicit track/current-fragment/default provenance. It delegates
preroll and media-range validation to the same source planner. Unknown clocks,
ambiguous targets and unsupported group/edit semantics remain explicit outcomes.
Broader inheritance, native seek ordering, postroll and decoder recovery stay open.
The receipt is `m3-candidate32-1793485608259220511` under
`.research/aws-linux-extended1/exports/63.183.208.230/`.

The following clock-boundary account records the earlier checkpoint29 preparation
and its independent observations. Its original held/unexecuted receipts remain
historical; later parent execution and checkpoint31 adoption are separate.

The batch covers the11 remaining new native-view profiles in the actual48 Linux
controls. Source times, exact rational edits, raw header scales and source gaps
remain intact. Eight reference failures, all16 legacy dispositions, all48 frozen
forecasts and every historical raw outcome remain required and preserved.

`Mov9MediaClock` records source and native bases separately. Only the actually
observed unsigned all-ones timescale word projects to native1/1 with a named cause;
other unobserved out-of-range scales remain refused. `Mov9SchedulingClock` keeps
API sentinel and checked rescale-overflow states explicit. An unorderable pending
head retains the incumbent catalogue slot; two exact keys use the existing G6
fold. There is no fabricated timestamp, packet sorting or source-clock wrap.

Native edit points use the last source decode point not after the exact media
start, while discard flags and selection boundaries still use the original edit.
The observed expanded terminal empty edit retains the original source gap and
adds a named native-only endpoint with the prior source identity, freezing the
preceding native duration. This is bounded to the tested terminal expanded/sync
profile; other nonleading-empty combinations remain refused. All added state and
staging stay constant per track and use existing charged storage/work/scan limits.

The postcapture independent input-derived model matches all40 successful native
controls, including the11 new profiles, and all24 retained v9 edit controls.
This is not a Rust runtime result or parity acceptance. Three new runtime test
definitions cover9 scheduling/catalogue controls,2 edit controls, and explicit
source/native scale separation. Apply regression-only files and observe runtime
RED before applying policy files; preserve preformat source and request hashes.
Private request and exact file mappings are in
`.research/parallel-mp4/implementation/clock-boundary29-private/`.

The parent actually passed checkpoint25's83 definitions (73 integration plus10
example), Clippy-Dwarnings and the release trace build on its frozen source.
Fresh v9 captured207 inputs/1035 outcomes with193 complete native-field/order
agreements per schedule, preserving all16 legacy dispositions and other nonpasses.
Those receipts remain historical evidence; changed checkpoint28 needs fresh checks.
The checkpoint25 native cursor
snapshots the immediately preceding source run's last native DTS on entry to each
trun. Its inclusive discard threshold stays fixed throughout that run; a backward
run supplies its own tail to the next run. The source cursor exposes the active
(fragment-slice ordinal, run ordinal) internally in O(1). This uses no source
coordinate uniqueness assumption, index scan, new allocator/input framework or
case-name branch. Per-sample predecessor clock derivation remains separate, and
source times/raw flags remain unchanged.

The parent actual four discard probes refuted all original D0-D3 models, including
a missing-tfdt guard. D4 previous-run tail and D5 previous-fragment tail both fit74
representable fragment captures. The prospective split-run control distinguished
them: actual flags [1,0,4,1,0,0] match D4; D5 predicts packet4 flag5. The original
nonfits and the explicit postcapture/prospective status remain preserved in the
private model reports. This is bounded pinned-native observation support, not an
ISO standard rule or complete M03 qualification.

The83rd example definition now checks six exact authored inputs: the original
actual v8 RED, four actual boundary controls, and the genuine run-partition
control. It checks native flags/clocks/durations/payload association, unchanged
raw source flags/knownness, and predecessor-origin evidence at4096/4096 and1/3.
All60 field vectors and all207 inputs/oracles remain preserved. Checkpoint28
migrates the obsolete fractional-refusal assertion to its actual required native
observation; the original body and real RED remain in the frozen Linux source/log.
No comparator, oracle value, legacy timing disposition or actual packet sequence
is normalized. Parent89-test/Clippy/release checks and a fresh255-input replay are
pending for the new source; no previous receipt qualifies these changes.

The parent actually passed all82 checkpoint20 tests, Clippy-Dwarnings and the
release unique example in `.research/mp4-native-host20-1793456191947296000/`,
with unchanged source. Worker93e2cc3e... produced the actual v8 replay of202 inputs
and1010 outcomes. Both schedules have179 strict source matches,179 physical
matches and175 ordered native matches, preserving13 unresolved/physical timing
cases plus other recorded nonpasses. Additional literal-knownness checks agree
for all72 fragment inputs and46 native-origin events per schedule. The direct
native-field/order/origin audit agrees on187 cases and retains one required flag
mismatch: origin18-three-backward-then-absent outputs5/6 have candidate flag4,
pinned native flag0. Complete reports are in the private v8 assessment; no
full/numerical qualification follows. The parent-only CPU/build/native queue
remains in force; no lane Rust/native execution accompanies this fix.

Checkpoint20 replaced the fragment locator's linear walk with a work-charged
binary search of retained run intervals and checked arithmetic for constant
byte/duration prefixes. Variable prefixes retain their cursor progress. The
parent's actual checkpoint19 run compiled and reached a selected-fields runtime
failure: `test result: FAILED. 2 passed; 1 failed`, with the compact UINT32_MAX
last-sample lookup exhausting the unchanged poll bound. The exact frozen source
and log are in `.research/mp4-native-host19-1793455739306128000/`; Clippy and the
release build did not run after that failure. The subsequent checkpoint20 actual
GREEN is bounded above. Its82 test definitions, work quanta/poll bounds and all60
vectors were retained through that verified locator fix.

Checkpoint19 introduced selected stz2 widths4/8/16, experimental signed trun v1
and a separately labelled native origin/fragment clock view. Before that change,
the parent observed all four
checkpoint18 additive regressions fail at runtime: selected-fields reports
`test result: FAILED. 0 passed; 3 failed`, and the absent-tfdt example reports
`test result: FAILED. 0 passed; 1 failed`. The complete before/after source,
diagnostics and pinned reference captures are preserved in
`.research/mp4-selected18-red-1793454764146107000/`; the lane verification is
`parent-selected18-runtime-red-observation.json` under
`.research/parallel-mp4/implementation/`.

Checkpoint20 contained73 integration plus9 example definitions,82 total. Its four
additive regression bodies and all60 original metadata vectors stayed unchanged.
Two older tests' obsolete refusal expectations had migrated to support against
the same frozen source rows and media bytes. Parent completed the bounded82-test,
Clippy/release and fresh202 replay described above; checkpoint25's83rd regression
and causal flag fix require the next parent validation/replay queue.

MPEG contribution m76694 table9 Original supplies the compact/signed field layouts;
author SDL contribution M2902 supplies MSB-first packed-array interpretation.
Signed trun has additional ISO FDAM3 draft support. These are explicitly
experimental primary-source bases, not final published clause acquisition.
Compact entries stay packed and raw; the unused low nibble for an odd4-bit count
is opaque. Other widths, reserved/version/flag semantics and extra full bytes are
Unsupported, without guessing that other widths or padding are normatively
invalid. Missing array bytes are Truncated. Exact pins and limits are in
`selected-fields-primary-basis18.json`.

The parent's30 prospective native controls all returned0 with the unchanged
`d97ce081...` pinned worker. The independent light review retained all960 model
pair outcomes and their nonfits. Only O2 complete-media start plus predecessor
continuation combined with P5 whole-track signed-raw32 compensation fits all30.
P5 interprets native CTO bits as signed in either trun version and compensates
native PTS by the track's negative minimum; it never changes source version0
unsigned semantics. This is bounded pinned-native evidence, not a standard rule.
Full outcomes are in `selected18-actual-model-discrimination.json`.

`Mov9PacketCursor` owns one core-charged slot/head per track, existing bounded edit
lookahead and no per-sample array. It preflights all source extents with resumable
binary searches, retains u64 addresses, scans compact ctts wire prefixes under
WorkBudget, and applies separate native clocks/flags before a real all-stream
selection. Fragment CTO minima are accumulated during the existing source-range
preflight. It supports cancellation, sticky failures, source/occurrence limits
and held-ready metadata. `Mov9Profile` names the pinned no-parser/no-fill
operation; `Mov9StreamView::for_source` adds explicit major-brand context.
Source samples, version/raw bits, flags, exact Time and physical traversal remain
intact. The example consumes this production iterator; no actual/comparator list
is sorted.

`Mov9PacketOccurrence::clock_origin` records SourceClock, PredecessorEnd evidence,
or CompleteMediaStart. `new` refuses an unknown first origin; only the explicit
`new_complete_source` constructor enables the observed start-zero rule when the
caller establishes the complete original source and ordinal1. Per-track
predecessor state uses the original duration and immediately preceding ordinal,
independently of another track, PTS, header duration or the frozen run discard
threshold.
The example emits actual `native_origin` evidence. Source DTS/CTS and exact Time
stay unknown when wire/caller origins are absent. No shared-core API is added.

An earlier passing bounded host snapshot is checkpoint16, frozen at
`.research/mp4-native-host16-1793450759463621000/`: all70 integration plus8 example
tests passed, as did Clippy with warnings denied and the release trace build.
The frozen worker SHA256 starts `6d2d5893`; selected graph starts `0730d042`.
Its actual v7 replay contains860 outcomes over172 inputs:153 ordered native
matches and zero ordered native mismatches per schedule; four required syntax
refusals, one unresolved source origin, three native-view refusals, four native
reference failures and seven malformed observations remain separately recorded.
The old physical absent-origin comparison still has12 literal differences against
the old authored timeline intent. The additive raw-knownness oracle retains that
intent and source unknownness separately; it does not relabel the old result.
All old172 inputs, source views, reports and failures remain preserved. Mixed
regular/fragment native clocks, edited fragments, unreviewed fractional profiles,
complete recovery/brand/presentation semantics and all full M03/numerical/target
gates remain open. This is the historical checkpoint16/19 boundary; later source
requires affected revalidation against its own identity.

An earlier passing full baseline is the parent's make10 gate (306 Rust / 51 Python,
including 59 M03 integration tests) plus the separate passing example regression.
The frozen v5 unique release example actually produced 352 traces over 88 inputs,
two operations and two schedules, with source/worker unchanged before/after.
Both canonical and physical source comparisons report 59 matches, 19 required
unsupported profiles and ten malformed rejections in each schedule. The former
outside-MDAT classification gap is closed in v5. These are host correctness
observations for that snapshot. Native packet order, fractional edit behavior,
unsupported profiles and numerical gates remain open. This README proof refresh
postdates the frozen capture.

Checkpoint14 added selected signed ctts v1 and moof-relative fragment addressing
after the parent actually observed both request13 regressions fail at runtime.
The exact red log is
`.research/mov-modern-before/idle-run-1793441107279408000/run.log`:
both suites report `test result: FAILED. 0 passed; 1 failed`, with tfhd and ctts
Unsupported respectively (Cargo exit 101). The entire pre-fix crate is frozen beside
that log; lane copies and verified identities are in
`.research/parallel-mp4/implementation/parent-modern-red-observation.json`.
The checkpoint14 production fix and tests pass the parent's 65 integration tests
plus one example test, Clippy with warnings denied and the release trace build.
The frozen source and logs are in
`.research/mp4-modern-host-1793442419925548000/`. Three new definitions cover the
29 independent boundary vectors, and one covers all
13 original modern fragment profiles outside the then-refused signed trun case.
That profile test explicitly requires unknown clocks when tfdt is absent; it does
not fill the unresolved original source-time expectation. Earlier passing gates
and v5 traces do not qualify this changed source.

Parent's make8 and make9 passed all 53 M03 integration tests, including the ten
physical-cursor regressions; the targeted first-pass log is
`.research/mov-interleave-first.log`. The full gates also passed Clippy,
no-default-feature, release and format checks. The fresh unique example was
release-built and actually executed for 352 v4 traces: 88 inputs, default/physical
operations and ordinary/tiny schedules. The parent verified that captured source
graph before and after collection was unchanged. This is correctness evidence for
that recorded snapshot, not full M03 or numerical qualification.

The v4 snapshot predates the appended example regression,
`physical_outside_mdat_is_invalid_before_offset_order_profile_rejection`. That test
actually failed in the parent's `.research/mp4-physical-range-before.log`:
`test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out`.
The exact pre-fix crate and log are retained under
`.research/parallel-mp4/implementation/physical-range-red-before/`.

After that red proof, `new_with_media_extents` and six focused integration
regressions were added. The parent's targeted `--all-targets` run passed all 59
integration tests plus the example regression in
`.research/mp4-physical-range-after.log` (exit 0). The expanded interleaving suite
reports `test result: ok. 16 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out`;
the example reports `test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out`.
This establishes the targeted host regression result for that after-fix snapshot.
Make10 and the actual v5 release-example comparisons above subsequently passed
their stated checks. The earlier 53-test gates and all v4 traces retain their
recorded snapshot bounds; their receipts are unchanged.
Makecheck6's preceding 43-test proof remains in
`.research/parallel-workspace-check-6.log`.
Parent's earlier targeted 42-test run after the path-only fixture correction is
retained in `.research/parallel-mov-dvb-after-fixes.log`; make4 covered 37 tests.
The earlier second workspace run passed 19 tests and built the then-current release
source-trace example. Across all 56 original inputs at both 4096/4096 and 1/3
window/work schedules (112 actual calls), each schedule has 25 exact source-subset
matches, 10 observed malformed rejections, 21 required unsupported cases and zero
source mismatches. These are correctness observations, not full M03 acceptance or
numerical qualification. Those historical trace results do not qualify later code.

Implemented from cached Apple QTFF, Adobe F4V 10.1, ETSI TS 126 244 V18.0.0
and the specifically acquired ISO amendment clauses described below:

- Checked 8/16-byte box framing, u64 endpoints and bounded child traversal.
- Incremental `BoxReader` driving core `InputCursor`, its source/request tokens,
  explicit finish/fail, bounded work and cancellation. Internal range skips retain
  logical generation; new sources/resets require new format reader state. Metadata
  uses charged `BudgetVec<u8>` and recoverable no-copy `freeze` into core `Bytes`.
- `SampleReader` reads a checked self-contained sample extent through the same core
  input, preserving core tokens and charged immutable payload ownership, explicit
  truncation/transport/cancellation, work yields and recoverable freeze backpressure.
- `SourceTrack` / `TrackSampleCursor` join a track's regular and selected fragment
  indexes without expanding them or filling unknown clocks. `PhysicalSampleCursor`
  exposes an explicit physical-address operation across tracks. Its work-bounded
  preflight checks nondecreasing source offsets, increasing per-track ordinals and
  cumulative limits before any output. It retains one charged metadata head per
  track plus one waiting sample; payloads still use `SampleReader`. Equal addresses
  follow supplied track order. Backward per-track offsets stay source-accessible
  but are unsupported by this selected physical merge. This does not establish
  the native reader's default selection policy.
- `PhysicalSampleCursor::new` retains its metadata-only traversal policy.
  `new_with_media_extents` borrows actual self-contained mdat payload extents,
  validates their ordering/nonoverlap under work budget, and uses resumable binary
  searches to check every supplied track's complete sample ranges before returning
  a physical-layout refusal. It adds one pending metadata sample and no copied
  extents, payloads or per-sample allocation. Empty endpoint samples retain
  `Sample::require_within` semantics. The physical file adapter uses this checked
  constructor; valid backward layouts remain Unsupported after bounds validation.
- Raw sample-entry/metadata ownership and descriptions, regular stts/ctts-v0/v1,
  stsc/stsz and experimental packed stz2-4/8/16, stco/co64/stss/stps/sdtp tables. Runs stay compact; validation, sequential
  cursor and source-sample locator yield without per-sample index allocation.
- Exact source ticks and core `Time` conversion. `CompositionOffset` preserves
  version/raw bits/signed interpretation, with absent versus explicit zero distinct.
- elst v0/v1 field preservation and exact rate-one point mapping. No unreviewed
  display-duration, trim, dwell, reverse or non-unit-rate presentation inference.
- Legacy trex/tfhd/trun-v0 plus experimental v1 signed CTO fields, defaults,
  first-sample flags, checked signed data offsets,
  bounded fragment indexes/cursors. ETSI §13.5 supplies unsigned `tfdt` v0/v1 source
  timing; absent origins stay unknown, and conflicting caller origins fail.
  The selected moof-base flag anchors to the enclosing moof when no explicit base
  is present; an explicit base takes precedence, and legacy predecessor inheritance
  remains separate. All byte endpoints remain checked u64 values.
- ETSI §13.4 `sidx` v0/v1 fields and compact work-bounded reference cursor, exact
  movie presentation times, raw SAP declarations and checked sparse u64 addresses.
  Graph resolution, target verification and decoder recovery are still required.

Signed ctts v1 uses the published ISO 14496-12:2008/Amd1:2009 preview
§§8.6.1.3.1-.3. The moof-base numeric bit uses the ISO-authored
14496-12:2008/FDAmd2 preview's addition to §8.8.7.1, with published ETSI named
profile semantics. This is a draft-plus-profile source basis, not acquisition of
the full published/current ISO standard. Source URLs, status, hashes and exact
clauses are in
`.research/parallel-mp4/implementation/modern-primary-source-basis.json`.
The independent 29-vector Python oracle has zero disagreements; its original raw
fixtures now feed `tests/modern_syntax.rs`, including u64 clock/offset boundaries,
unknown origin, signed CTS extrema and compact UINT32_MAX run lookup. All new Rust
results pass on the recorded checkpoint14 source. The later 364 trace executions
retain separate source/native comparisons and unresolved gaps; these host results
do not grant native parity or numerical qualification.

Final published stz2/signed-trun clauses, unselected widths/padding semantics,
unknown flags/versions and full brand/presentation validation remain explicit
gaps. Selected experimental field support passed the bounded checkpoint20 host checks;
source facts and final normative coverage are distinct. The acquired draft conflicts with the earlier compatible
`isom` brand in 14 frozen modern fragment inputs; their bytes and required source
facts remain unchanged. Three new private demux probes declare compatible iso5/iso6
to avoid that specific contradiction; full conformance is still unqualified.
Original complete raw stbl/traf/entry
bytes stay available. A container-sync fact is not decoder recovery proof. External
data-reference resolution and complete track/movie presentation remain open.
`Mov9View`/`Mov9StreamView` are separate, experimental pinned-native projections;
source samples, raw entries and exact clocks stay unchanged. Their edit point
mapping is separate from `Mov9EditCursor`, which selects experimental compatibility
occurrences for the observed regular, nonreordered, positive-delta, rate-one profile.
Sample ranges are verified against the
correct self-contained media extent before reading bytes.

The source trace example drives the real file through bounded core input windows,
retains metadata only, and reads regular and selected fragment samples through core range
requests. It emits exact source clocks, bytes, descriptions, edit fields and separate
format-owned configuration epochs as JSONL. No oracle value is injected. It checks
self-contained dref entries and refuses external references. The acquired moof-base
bit is supported; unknown fields remain refused and absent source clocks are not
filled from expected/native output.
Schema v2 adds raw dependency/fragment flags and source `sidx` reference events.

Separate native-view events carry raw-video/tx3g catalogue and packet projections.
The experimental edit cursor adds preceding-sync selection, observed key-end
postroll, empty/repeated edits, occurrence identities and labelled native duration
bases. It never replaces source decode deltas. The expanded-index terminal cap is
a provisional observation rule; expanded fractional endpoints and unreviewed
reordering/recovery/rate profiles remain unsupported. The
independent native comparator requires real candidate events and mandatory source
checks. It never supplies candidate output from the reference. Twelve original
edit probes were executed by the parent with the pinned worker; raw observations
remain in `.research/new-format-native-observed/` with `mov-edit-probes.json` identities.
The parent executed both candidate schedules for all 12 edit probes: source plus
strict ordered native comparison matches all 12 in each schedule. The four original
edit holdouts also match. The parent then executed all seven additional terminal
boundary probes. Six integer cases match the frozen provisional fields and the
actual default native operation in both v4 schedules. The fractional case remains
required and unsupported: native DTS accumulates 333 ticks per edit, intermediate
durations remain 1000, and terminal duration is 333. Source rational clocks and
decode deltas stay unchanged. These observations do not establish a general
terminal, duration or rounding rule.

The earlier v3 unique-example identities, source and all 68 normal/tiny outputs
are retained in `.research/mov-vendor-source-v3/`, including `frozen/`.
The v4 set of 352 actual traces and identities is retained in
`.research/mov-interleave-source-v4/`. Its recorded candidate worker SHA256 is
`639fa720920b5509d665e95cbb01636dcf7fbd442fd61d456332461c32890516`;
the source graph SHA256 is
`ba07355fd2709cdf5c526411c5151f0483e5bd901704b00ffbe9d65a878e5ce3`.
The original56 strict native comparison still has 25 ordered operation matches, 19 required
unsupported cases, one mismatch, four native failures and seven malformed
observations in each schedule. Both fragment video vendor-tag differences are
resolved with regular-sample context; raw sample entries and native outputs are
unchanged. The remaining `r-multitrack` mismatch is global packet order: current
default native events remain grouped by track. Physical address order also differs
from the actual native default. Neither operation is substituted in the strict
comparator, which compares actual global sequence without sorting. A numeric
selection cutoff is not established by the acquired documentation/observations.

For the other 32 v4 inputs, both schedules have 12 edit operation matches, six
terminal operation matches, one required fractional native-view unsupported case
and 13 scheduling mismatches whose only field difference is global packet order.
All 32 separate physical source views match. The original56 physical view has 27
source matches, 19 required unsupported cases, nine malformed rejections and one
malformed-not-rejected case. That last file emits zero packets and exits
Unsupported: layout-order preflight masks its invalid outside-MDAT address.
The parent observed the example regression fail on this v4 behavior and then pass
after the production bounds-validation fix, requiring Invalid before any output.
The original v4 strict comparator result remains non-pass and is preserved;
the v5 canonical/physical comparisons now reject all ten malformed cases in both
schedules. The exact manifest-bound historical v4 reports and
invocations are recorded in
`.research/parallel-mp4/implementation/v4-actual32-assessment.json`.

Recorded pre-syntax-change v5 evidence is `.research/mov-interleave-source-v5/`, with candidate worker
SHA256 `f154e8fa69bc8a220df44183d97e624b87711e36e3748a92aa9e1a81b3c80596`
and source graph SHA256
`f0ad439f5517a69eac0c114175413ab6dca822c196189288959eed58258cc615`.
All 32 added canonical/physical source views match in both schedules. The default
native view still has 12 earlier edit matches, six integer terminal matches,
one required fractional unsupported case and 13 global-order-only scheduling
mismatches. Including original56 gives 43 ordered native-view matches, 14 global
order mismatches, 19 source-profile unsupported cases, one fractional native-view
unsupported case, four native failures and seven malformed observations per
schedule. These do not establish complete native module parity. Strict comparator
hashes are unchanged. Exact invocations, all 352 trace identities and every case
disposition are in `.research/parallel-mp4/implementation/v5-actual88-assessment.json`.

Parent queue commands (not run by this lane):

```sh
CARGO_INCREMENTAL=0 cargo +nightly test -p triergon-isobmff --all-targets
CARGO_INCREMENTAL=0 cargo +nightly build --release -p triergon-isobmff --example isobmff_source_trace
```

Parent regression recheck (archived red, then targeted after-fix pass):

```sh
CARGO_INCREMENTAL=0 cargo +nightly test -p triergon-isobmff --example isobmff_source_trace physical_outside_mdat_is_invalid_before_offset_order_profile_rejection
```

`isobmff_source_trace INPUT [WINDOW<=4096] [WORK_QUANTUM] [physical]` is a correctness adapter. The
parent may record stdout using 4096/4096 and 1/3 schedules, then run
`.research/parallel-mp4/implementation/compare_source_trace.py` on the resulting
case-named JSONL files. The independent comparator reads the original authored
Fraction oracle and input identities; missing/unsupported/mismatching outputs remain
non-pass. It never synthesizes a missing candidate output.

Use `.research/parallel-mp4/implementation/compare_native_view.py` for the separate
native view. The parent-tested 53-test snapshot includes this edit-iterator result:
`test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out`.
The five edit-iterator definitions cover 12 probes, four original holdouts,
both work schedules, retained source facts, waiting output, cumulative limits and
unreviewed profile rejection. Its ten physical-cursor definitions also passed in
make8/make9. The targeted 59-plus-one run includes all six additional bounds
regressions and the formerly failing example. The subsequent make10/v5 results
above are separate actual observations for their frozen source, with no numerical
M03 claim. No lane Cargo execution is permitted. Rust source/example/tests remain held.
The earlier shared `target/release/examples/source_trace` path was overwritten by
TS. Those 112 calls retain their exact recorded worker SHA; historical receipts are
unchanged. Future runs must hash the freshly built unique executable.

The optional `physical` mode emits schema `triergon.m03.physical-source-trace.v1`
and `physical_sample` events in production cursor order, using the same bounded
input/packet reader and retained raw metadata. It attempts no native operation.
Use `.research/parallel-mp4/implementation/compare_physical_trace.py` for this
separate operation's independent exact source/index check. Its success cannot
satisfy native global parity. Thirteen scheduling challenges and source oracles
are in `.research/parallel-mp4/implementation/interleave-probes/`; the parent
executed their pinned native reference and both v4 candidate modes/schedules.
The lane performed only read-only output comparison and oracle/identity checks.
Use the exact corresponding `--probe-manifest` for each probe family. The default source
comparison still requires configuration/edit/description facts and all required
profiles remain in the full M03 contract.

This source view retains unique decode-order samples and an edit map. It does not
pretend equivalence to native edit-applied packet repetition or DTS shifting. The
parent must freeze equal native/candidate work and compatibility views before any
timing. Serialization, tracing and input hashing are not a demux benchmark. Full
selected parity, delivery runtime upper paired95 <=1.25 (optimization target <=0.80) and lower live heap/RSS remain required.

Regression definitions use original authored fixtures and independent expected
source facts, including >4 GiB virtual co64 offsets. Rights/provenance and exact
fixture hashes are in `tests/fixtures/PROVENANCE.json`. The lane's implementation
record and remaining specification/acquisition blockers are under
`.research/parallel-mp4/implementation/`; cached third-party implementation code is
not used as a source.

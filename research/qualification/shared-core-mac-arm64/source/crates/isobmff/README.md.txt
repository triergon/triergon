# safe-media-isobmff

Independent safe-Rust syntax/index seam, with only `../core` as a dependency.
Experimental; not complete M03, decoder recovery, runtime/memory or target qualification.

Latest evidence is the parent's make10 full green gate (306 Rust / 51 Python,
including 59 M03 integration tests) plus the separate passing example regression.
The frozen v5 unique release example actually produced 352 traces over 88 inputs,
two operations and two schedules, with source/worker unchanged before/after.
Both canonical and physical source comparisons report 59 matches, 19 required
unsupported profiles and ten malformed rejections in each schedule. The former
outside-MDAT classification gap is closed in v5. These are host correctness
observations for that snapshot. Native packet order, fractional edit behavior,
unsupported profiles and numerical gates remain open. This README proof refresh
postdates the frozen capture.

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

Implemented from cached Apple QTFF, Adobe F4V 10.1 and ETSI TS 126 244 V18.0.0
primary specifications:

- Checked 8/16-byte box framing, u64 endpoints and bounded child traversal.
- Incremental `BoxReader` driving core `InputCursor`, its source/request tokens,
  explicit finish/fail, bounded work and cancellation. Internal range skips retain
  logical generation; new sources/resets require new format reader state. Metadata
  uses charged `BudgetVec<u8>` and recoverable no-copy `freeze` into core `Bytes`.
- `SampleReader` reads a checked self-contained sample extent through the same core
  input, preserving core tokens and charged immutable payload ownership, explicit
  truncation/transport/cancellation, work yields and recoverable freeze backpressure.
- `SourceTrack` / `TrackSampleCursor` join a track's regular and legacy fragment
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
- Raw sample-entry/metadata ownership and descriptions, regular stts/ctts-v0,
  stsc/stsz/stco/co64/stss/stps/sdtp tables. Runs stay compact; validation, sequential
  cursor and source-sample locator yield without per-sample index allocation.
- Exact source ticks and core `Time` conversion. `CompositionOffset` preserves
  version/raw bits/signed interpretation, with absent versus explicit zero distinct.
- elst v0/v1 field preservation and exact rate-one point mapping. No unreviewed
  display-duration, trim, dwell, reverse or non-unit-rate presentation inference.
- Legacy trex/tfhd/trun-v0 defaults, first-sample flags, checked signed data offsets,
  bounded fragment indexes/cursors. ETSI §13.5 supplies unsigned `tfdt` v0/v1 source
  timing; absent origins stay unknown, and conflicting caller origins fail.
- ETSI §13.4 `sidx` v0/v1 fields and compact work-bounded reference cursor, exact
  movie presentation times, raw SAP declarations and checked sparse u64 addresses.
  Graph resolution, target verification and decoder recovery are still required.

Unsupported modern ctts/trun v1 box semantics, stz2 and modern
tfhd flags remain explicit specification gaps. Original complete raw stbl/traf/entry
bytes stay available. A container-sync fact is not decoder recovery proof. External
data-reference resolution and complete track/movie presentation remain open.
`Mov9View`/`Mov9StreamView` are separate, experimental pinned-native projections;
source samples, raw entries and exact clocks stay unchanged. Their edit point
mapping is separate from `Mov9EditCursor`, which selects experimental compatibility
occurrences for the observed regular, nonreordered, positive-delta, rate-one profile.
Sample ranges are verified against the
correct self-contained media extent before reading bytes.

The source trace example drives the real file through bounded core input windows,
retains metadata only, and reads regular and sourced legacy-fragment samples through core range
requests. It emits exact source clocks, bytes, descriptions, edit fields and separate
format-owned configuration epochs as JSONL. No oracle value is injected. It checks
self-contained dref entries and refuses external references. Modern fragment flags
stay unsupported; absent source clocks are not filled from expected/native output.
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

Current v5 evidence is `.research/mov-interleave-source-v5/`, with candidate worker
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
CARGO_INCREMENTAL=0 cargo test -p safe-media-isobmff
CARGO_INCREMENTAL=0 cargo build --release -p safe-media-isobmff --example isobmff_source_trace
```

Parent regression recheck (archived red, then targeted after-fix pass):

```sh
CARGO_INCREMENTAL=0 cargo +nightly test -p safe-media-isobmff --example isobmff_source_trace physical_outside_mdat_is_invalid_before_offset_order_profile_rejection
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

The optional `physical` mode emits schema `safe-media.m03.physical-source-trace.v1`
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
selected parity, runtime upper paired95 <=0.80 and lower live heap/RSS remain required.

Regression definitions use original authored fixtures and independent expected
source facts, including >4 GiB virtual co64 offsets. Rights/provenance and exact
fixture hashes are in `tests/fixtures/PROVENANCE.json`. The lane's implementation
record and remaining specification/acquisition blockers are under
`.research/parallel-mp4/implementation/`; cached third-party implementation code is
not used as a source.

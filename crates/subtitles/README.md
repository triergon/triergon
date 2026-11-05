# triergon-subtitles: DVB and strict text source slices

Experimental pure safe Rust. The only dependency is the owned `triergon-core`
path package. No full M05 decoder, FFmpeg parity, performance or lower-memory
qualification is claimed. Workspace registration and Cargo execution belong to
parent/main; this lane does not change the root manifest/lock or build queue.

The independently authored DVB seam follows ETSI EN 300 743 V1.6.1 (2018-10)
§§5.1, 7.2.1–7.2.6, 8 and 10. Source PDF SHA-256:
`1a5b211303397c52b771eb11e8f9f844d88727ecccd3fe0a5e8a6ac961d93e87`.
Official version-qualified source:
<https://www.etsi.org/deliver/etsi_en/300700_300799/300743/01.06.01_60/en_300743v010601p.pdf>.
The ignored lane retains primary texts/hashes, original source art and independent
RGBA/index oracles. No existing decoder implementation was copied or translated.

`text` adds the named strict source-ingestion/carriage contracts
`SubRipStrict1`, `WebVttSourceStrict1`, `Tx3gCarriage1`, `Utf8Packet1`,
`MatroskaWebVttCarriage1` and `OpaqueAss1`. See
[the text contract](src/text/README.md) for exact source/charset/configuration/
timing boundaries, resource policy and explicit compatibility gaps. The original
private text source passed96 tests and all-target Clippy with `-D warnings` in
`.research/subtitles-text-host-v2-1793455830972317000/`. Its public promotion uses
namespace imports and three test import/path substitutions; copied original
fixtures/oracles/pins/resources and private originals are preserved. The parent
subsequently ran the public combination: **161 passed,0 failed**, all-target
Clippy with `-D warnings` passed, and the release library build passed, with
unchanged source at `.research/subtitles-text-promoted-host1-1793458809112021000/`.
This is bounded host correctness/lint/build evidence for that exact source.
A documentation-build result was not supplied in that receipt. Full native
compatibility, complete M05 and numerical/memory qualification remain open.

The later [versioned carriage API](src/text/carriage/README.md) recorded a checkpoint
of **323 passing tests** in debug and release, preserving all 161
existing tests. Its 111 carriage regressions, 40 native line-entry controls,
five retained-owner controls and six Unicode/view unit tests pass, along with
Clippy, the no-default library check and documentation build. The exact Linux
receipt is `extended-feature-expansion-1793485227269489839` under
`.research/aws-linux-extended1/exports/3.122.247.247/`. This source/host evidence
does not establish complete native compatibility or numerical qualification.

## Implemented boundary

`dvb::Raster` owns a private charged region target. Configure source region size,
object origin, nominal region depth, a possibly negative crop rectangle,
nonmodifying behavior, finite resource/work limits, initial fill or immutable
previous pixels, and exact original object/palette facts.

Call `start_field(FieldSpec)` then `step(&[u8], final_input, &mut WorkBudget,
&AtomicBool)`. A step returns exactly consumed bytes and NeedInput, Yield or
FieldComplete. It retains partial bit fields, grammar state, partial map tables,
current row/column and the unwritten suffix of a run. The caller resubmits only
unconsumed bytes. Starvation is not a final field tail. The exact declared field
length excludes ODS header/stuffing; bytes from a subsequent field are not consumed.
A known final incomplete tail is Truncated; unsupported sub-blocks, invalid runs,
nonzero padding, cancellation and resource failures remain explicit errors.

Implemented syntax includes 2/4/8-bit literals and every selected interlaced RLE
branch, string padding, separate string/line ends, all three explicit/default map
families and multiple strings on a line. Depth-reduction receiver modes are
explicitly unsupported. Object Y positions advance by two scan lines per field;
Top and Bottom apply parity 0/1. When an ODS has no bottom data, the enclosing
format decoder replays the original top field bytes with Bottom parity, sharing
source storage. This seam does not infer or buffer an ODS automatically.

Field map tables are explicit initial values; final tables are available after
FieldComplete. The format/state owner chooses their scope/reuse. Page, region,
object, configuration, seek, epoch, generation, source I/O and timestamp semantics
remain outside the low-level syntax seam; the display-state layer owns its local
format state.

## Storage, progress and error guarantees

- `BudgetVec<u8>` reserves before allocation; initialization/copying is charged,
  work-yielding and cancellable. Existing immutable planes are never edited.
- Source run bounds are checked before any of that run is written. Cropping does
  not suppress source validation. Runs never wrap to another source row.
- Clipped/nonmodifying pixels are still charged as decoded work. The default
  Source operation preserves underlying pixels for mapped CLUT entry1; an index0
  write is distinct. The explicit NativeD32 receiver policy is described below.
- Pixel writes/copies are at most 1024 pixels between cancellation checks, and
  maximum encoded runs in this grammar are smaller still. Pending runs can resume
  with a one-unit work allowance. An absolute total work ceiling is additional.
- Unknown/invalid/truncated field syntax or cancellation poisons the transaction
  and releases its private plane; no partial valid-looking output is published.
- After both fields complete, `try_finish` freezes the existing allocation into
  `Bytes`, preserving charged capacity through clones/caller retention. A failed
  freeze restores its returned builder for exact retry after backpressure.
- `start_object` reuses one private region plane across objects before publishing.
  Display-state rendering adds a charged coverage bitset for multiple current
  objects, checking encoded source coordinates rather than bounding boxes alone.
  Prior display-set pixels remain overwritable.
- Output includes region/object/crop/stride/depth plus unchanged source palette
  bytes/default identity and original object bytes. No palette colour, alpha or
  RGBA conversion is silently applied. Cropped output is a viewport projection;
  it is not a reusable full-region cache unless that region was stored in full.
- Palette, object and initial immutable spans must belong to the same shared
  budget. `Bytes::belongs_to` validates this without copying; hidden retained
  backing capacity remains owned/accounted by the core.

The caller can drop a poisoned/finished Raster; earlier emitted Bytes stay valid.
Limits are finite draft policy, not evidence that every selected input fits or
that memory/runtime gates pass. Valid required cases rejected by a proposed limit
remain visible non-passes, never a scope reduction.

## Core input and exact time

The concrete core exports `InputCursor`, `InputToken`, `InputWindow`, `Step`,
`SourceId` and `SourceLocation`, plus `Time` (i128/u128 reduced seconds), `TimeBase`
and explicit `Rounding`. No competing input or time model is introduced here.
The low-level raster consumes bounded field slices from its enclosing parser;
it does not interpret input source completion as an ODS or codec boundary.

A container/packet adapter should use `InputCursor::read_span`, feed bounded spans
without copying, retain any unconsumed span across work yields, and propagate
NeedInput/Yield/Transport/Cancelled separately. Source `Step::End` is an entity
boundary, not a synthetic field end, clear cue or decoder reset. `continue_source`
may keep a logical field/codec state when its enclosing format permits it.
`request_range` preserves the parser generation; it must not by itself flush
palette/configuration state. A logical `reset` changes generation and requires an
explicit format-owned reset/reconfiguration transaction. Current raster state must
not receive new-generation bytes; discard it and reinitialize from authorized
acquisition state. Core exact Time is suitable for a later cue layer; no time
conversion or guessed DVD clock constant enters this raster seam.

## Tests and provenance

`tests/fixtures/` contains only original authored fixture data. The first-object
field bytes and original palette/object bytes were extracted structurally from
this lane's raw DVB fixtures. Expected index planes are copied verbatim from the
independent source-art oracle, not obtained by running this Rust implementation.
The exported set has 21 valid first-object cases; it does not inherit broader
page/palette-update/clear/timing acceptance from the parent fixture corpus.

`tests/raster.rs` was authored before `src/lib.rs`; the ignored test-first record
retains that initial hash. It declares regression cases for tiny input/work
windows, field interleaving/repeated top, maps, clipping, nonmodifying/ragged edges,
source facts, cancellation, truncation, malformed runs/padding, fixed limits,
freeze backpressure and retained output lifetime. The parent compiled the initial
snapshot and reported **11 tests passed, 0 failed**, including all 21 authored
first-object fixture cases, after its minimal `0_usize` integration correction.
The parent subsequently reported **make4 green, including 12 raster tests passed,
0 failed** and all 21 authored first-object cases. That snapshot includes the
compound-object regression, range-pattern/context fixes and the parent's two
`RasterLimits` test initializer fixes. Its full workspace result was 168 Rust
tests, 48 Python tests and both 51,235-case math checks passed. The parent then
compiled the promoted display-state/colour source and passed all **26 DVB tests:
12 state, 2 colour and 12 raster**, recorded in
`.research/parallel-mov-dvb-after-fixes.log`. The state art test exercised all
13 authored cases at work allowances 1, 7 and 4096. This is targeted host
correctness evidence; the later make10 result below covers its integrated tree.
No local Cargo command was run by this lane. Native FFmpeg parity, runtime,
peak live heap and attributable RSS gates remain open.

The next boundary batch adds `tests/boundary_state.rs` (eight tests). The parent
executed it before production changes: **4 passed, 4 failed**, recorded in
`.research/dvb-boundary-state-before.log`. The four failures were acceptance of
empty segment/PES input, overflowing exact-time subtraction during PTS ordering,
and two selected display sets published from one PES packet. The original tests,
production source and log are preserved privately. The narrow fix now uses exact
Time ordering, rejects empty input and tracks selected EOD within each declared
PES packet while preserving its first event. Parent **make10 passed all 34 DVB
tests**, including **8 boundary tests passed, 0 failed**, with the older 26 tests
also passing. The complete gate reported 306 Rust tests, 51 Python tests and both
51,235-case arithmetic checks passed, with formatting/lint/no-default/release/
safety stages recorded in `.research/parallel-workspace-check-10.log`.

The parent subsequently ran the additive configured-source API and its nine
previously authored tests in **make11: 9 passed, 0 failed**. All **43 DVB tests
passed**, alongside the complete 315-Rust/51-Python gate and both 51,235-case
arithmetic checks. Formatting, Clippy, no-default-feature, release and safety
stages passed in `.research/parallel-workspace-check-11.log`. This is source
correctness evidence; native parity and numerical acceptance remain open.

The parent subsequently observed the inactive-background regression: four new
tests compiled and ran, with two passing controls and two failures at the
`DVB background changed without fill` rejection. The original failing source,
tests and log are preserved privately. The narrow correction removes that
rejection to tolerate an inactive encoded background change. A later primary
review corrected the source classification: EN300743 §5.1.5 explicitly forbids
that change when fill is clear. The two change cases are nonconforming source
inputs; their acceptance proves receiver tolerance, not normative conformance.
The earlier primary-conformance claim was wrong and is withdrawn. All four tests and their original source/index/RGBA art
are now promoted under `tests/rcs_fill.rs` and `tests/rcs-fill-fixtures/`. Their
assertions are unchanged. The parent subsequently ran **all47 subtitle tests:
47 passed,0 failed**, and Clippy with `-D warnings` passed. Source before/after
was unchanged; exact logs/results are retained in
`.research/dvb-rcs-fill-green-1793444038077152000/`. This is bounded correctness/
lint evidence, not native parity or numerical acceptance. The earlier36-run D0
trace proof belongs to its original source; a fresh container-wrapper build
awaits coherent parent-owned M01 source.

Twelve further full-DVB nonmodifying regressions passed in the parent's isolated
source-test package and are now promoted as `tests/nonmodifying.rs` with unchanged
assertions/art. The parent subsequently ran all59 promoted regressions with0
failures and unchanged source; that proof precedes the next EOD correction.
All12 new native probes returned0; their flag0 index planes match, while all six
flag1 planes place the remaining CLUT2 at column0 instead of4. Literal/run pairs
agree natively in every depth. All12 full SourceNearest palette views still
differ. At that historical checkpoint, no nonmodifying runtime behavior, native
palette, ordering or tail was changed to accommodate those findings. The later
explicit NativeD32 receiver additions are described below.

Primary §7.2.6 assigns EOD to the ancillary page when shared data is used.
The parent observed three failing ancillary-EOD regressions and three passing
controls before the behavior change. Dispatch now recognizes EOD on either
already-selected page and applies the existing zero-payload check to both.
The six tests and original fixtures are promoted as `tests/ancillary_eod.rs`;
assertions/art are unchanged. The parent subsequently ran all65 regressions:
**65 passed,0 failed**. Clippy for all targets with `-D warnings` and the release
library build also passed, with unchanged source. These historical results are
preserved in `.research/subtitles-eod-host-1793453041434070000/`; they establish
bounded host correctness/lint/build evidence, not native parity or numerical
qualification.

The separate text source passed all96 unchanged tests and all-target Clippy in
`.research/subtitles-text-host-v2-1793455830972317000/`. Its five approved scoped
expectations and seven physical-value ceilings introduce no Box or numerical
waiver. The subsequent public `text` promotion is a separate identity, recorded
in `.research/parallel-subtitles/text-batch-1/promotion-1/checkpoint.json`. Its
public161-test, all-target Clippy and release-library pass is preserved in
`.research/subtitles-text-promoted-host1-1793458809112021000/`; this does not claim
native or numerical acceptance. The 72 authored text fixture/oracle cases and
all96 tests retain their expected values; only test imports and include/module
paths change for the public module.

## Explicit configured packet boundary

`SourceConfiguration::from_matroska_private` preserves the exact five-byte source
CodecPrivate and selects the declared bare-segment envelope.
`SourceConfiguration::from_subtitling_descriptor` preserves the complete tagged
descriptor, selected service index and raw language bytes; its segment/PES-data-
field envelope is explicit. The selected page IDs/type are parsed from primary
source layouts. Parsing identity does not certify all capabilities of that type.
The derived `service_private_view` is a named five-byte source mapping, with no
assumption about arbitrary native decoder extradata.

`SourceClock` retains the original positive numerator/denominator alongside
core's exact reduced `TimeBase`. `PacketTiming` keeps optional raw signed ticks
for PTS/DTS/duration and distinguishes source values from inferred views. Unknown
is `None`; no integer, including `i64::MIN`, becomes an implicit sentinel.
`PacketIdentity::container_position` is a container locator and is not presented
as a physical payload range for an assembled TS packet.

`SourceConfiguration::packet` aliases charged immutable payload/configuration
and computes exact core PTS without an integer-clock rounding step.
`Decoder::new_configured` requires those source facts on accepted packets and
validates pool, service, envelope and PTS consistency before acceptance. Service,
envelope or exact clock changes require `reset_with_configuration` with a new
generation. Compatible metadata-only changes keep their original declaration
with each packet. A retained event's source packets keep the original private/
descriptor bytes, raw clock facts and locator after decoder/input teardown.
Manual legacy packets explicitly set `DvbPacket.source` to `None`.

The nine configured tests include five original authored bodies under both
source configuration forms/clocks, independent index/RGBA art, descriptor service
selection, raw ratios, inferred DTS, unknown versus known sentinel-valued source
time, foreign/malformed metadata, rejection before acceptance, generation reset,
retained configuration lifetime and a display spanning two explicit PES packets.
All nine passed in the parent make11 gate. Full physical source-span/opaque-side-data carriage
and matched native output retention remain integration work; this packet boundary
does not establish a complete container-to-display qualification operation.

## Display-state operation: prototype with parent correctness results

`dvb::Decoder` accepts already validated immutable `DvbPacket` values. Each packet
contains same-pool Bytes, optional exact core Time, logical generation, increasing
sequence and explicit raw-segment/PES-data-field envelope. Segment payloads are
complete within packets; a display set may span packets at segment boundaries
with the same PTS. `provide`, `step`, `finish_input` and explicit `reset` preserve
input starvation, work yield, output pressure, logical completion and acquisition
as distinct states. A source entity End does not manufacture EOD or a clear.

The layer parses DDS/PCS/RCS/CDS and method-0 ODS, preserves private/unsupported
extension spans, owns bounded page/region/CLUT/object state, and publishes atomic
present/update/clear events at EOD. It checks fixed in-epoch region shape, full
RCS coverage at acquisition, RCS versions for palette changes, object/source
bounds, region scan lines and current-object source-code overlap. All modified
regions, including invisible ones, are rendered. Unchanged planes remain shared;
clear changes visibility while retaining region pixels. Events retain source
packets, DDS/PCS/RCS/CDS spans, exact palette components/default identity and charged
immutable index/palette output. Nominal expiry is PTS plus the source timeout;
the presentation layer owns the timer starting at actual first presentation.

Default `ObjectRetention::CurrentDisplay` consumes ODS supplied in the current
display set. The optional bounded `EpochExtension` permits replay of earlier
compressed ODS. The original cached-object research fixture retains its original
expectations and runs under that explicit extension. The acquired primary model
describes cumulative region pixels and rendering ODS as decoded; it does not yet
establish mandatory cross-display ODS replay. Native probes did not perform that
replay. This fixture is not claimed as normative/native conformance.

Every CLUT entry retains original full 8/8/8/8 or reduced 6/4/4/2 Y/Cr/Cb/T facts,
an exact opacity fraction and original CDS span. Palette snapshots materialize two
separately named/charged views. `SourceNearest8` preserves the original source-art
RGBA convention. `StudioUnrepaired8` applies the independently specified studio
inverse without a Y=0 repair, with RGB nearest rounding and alpha floor rounding.
It reproduces the observed transparent `[0,136,0,0]` point and half-opacity alpha
127 while retaining the original `[0,0,0,0]`/alpha-128 view. This is an additive
projection, not a transparent-RGB normalizer or proof of general native equality.
EN300743 §7.2.4 signals opacity at Y=0 and includes an informative legal-Y repair
note; it does not prescribe one universal straight-RGB value for that entry.

`tests/display.rs` has 12 integration tests covering 13 independently
authored state cases and additional pressure/cancel/generation/limit/envelope/time
checks. `tests/colour.rs` has two tests from eight independently specified
Fraction-oracle vectors. Original raster/state expected bytes remain unchanged.
The first parent state run passed 11 tests and failed the retained-charge
assertion: the pool's own live control charge was incorrectly expected to be zero.
The original failure/log are retained privately. The narrow test fix captures
`budget.used()` immediately after `Budget::new`, retains output validation after
decoder drop, and requires exact return to that baseline after every alias drops.
Production was unchanged for this fix. The subsequent parent run recorded:

```text
colour: test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
display: test result: ok. 12 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
raster: test result: ok. 12 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.04s
```

This lane did not execute Cargo. These results do not establish full selected
M05, native parity or numerical acceptance.

The parent separately ran all 13 state sources under original-default and the
documented compute_clut=0 correctness control. All 26 observations are retained;
their decoded outputs are identical between profiles. Differences remain in
literal region order, source-vs-native transparent RGB, cached-ODS replay and
error behavior for in-epoch resize/missing object. No native options profile is
adopted as a performance baseline. Source facts unavailable through AVSubtitle
remain an explicit comparison/adapter gap.

The `pgs` and `dvd` modules provide bounded wire views and indexed RLE expansion.
PGS also implements owned ODS assembly and display-set/cache/palette updates,
with retained source facts and explicitly named native output projections. DVD
adds SPU byte-fragment assembly, VobSub CodecPrivate fields, cooperative
traditional control-program facts and an explicit supplied-palette view. The
separate native crop projection borrows rows from the unchanged indexed plane;
externally supplied narrow-palette indices are validated before publication.
Source state, unsupported command operands and transparent RGB remain retained.
The projection does not establish receiver palette generation or a full decoder. Full DVD control/presentation and extended-variant work,
wall-clock scheduling, complete native compatibility and numerical qualification
remain open. Licensed implementation evidence
and native controls support implementation; unavailable paid texts restrict
normative claims rather than blocking all development. Cross-field map-table lifetime, composition/ancillary ID collisions,
progressive/character/ROM/ACS/disparity/depth-reduction capabilities, text cues,
M01/M03/M04 integration and every numerical gate remain open. M05 stays unqualified.

## Owned context and explicit native DVB receiver view

`context` carries parsed codec/private/name/language/charset-hint facts and ordered
metadata/attachment owners to the unchanged consumer. It preserves absence,
source/configuration identity and complete backing charges. It does not parse
fonts or replace consumer ASS/charset/presentation policy. Its original34 guards
passed private runtime RED1/33, candidate debug/release/Clippy, and the isolated
public integration passed all357 tests, Clippy, no-default library check and docs
at `feature-followups-1793492163312779041/m5-workspace` on native Linux ARM64.

The preceding EN/source receiver contracts describe the default source operation.
The additive `Decoder::new_configured_with_receiver_view` can explicitly select
`ReceiverView::NativeD32`, pinned by `D32_PIN` and `D32_VIEW`. It retains the
original source-nearest palette, encoded CLUT values/segments, whole packets,
configuration and raw clocks, while constructing a separately named native
palette/word view. Transparent RGB is preserved, including unused entries.
Native words and RGBA are charged immutable output, not an after-output repair.

This view tolerates the observed nonempty EOD body and publishes the complete
valid prefix before a short trailing header or at the packet boundary without
inventing an EOD. It then retains the original trailing error/offset and all
original bytes. The default source operation keeps its old EOD/tail errors and
publication behavior. Exactly representable sampled native microsecond fields
are explicit; unknown/nonintegral clocks are left unresolved without rounding.

The parent observed the eleven new guards fail8/pass3 before activation, then
all76 DVB guards passed in debug/release and all-target Clippy passed at
`dvb-resume-1793492267508296369`. The five full explicit-CLUT palette/word cases
and both original malformed-Matroska/native-TS prefix cases are covered. These
are exact-source host proofs. That recorded combined source contained368 guards:
the preceding323,34 owned-context guards and11 native receiver guards. Full
native and numerical acceptance remains separate from these host results.

`NativePalette::all_entries_explicit` is a source fact, not an equality/pass
certificate. The separately constructed default/automatic NativeD32 palettes
cover the bounded observations below; further mapping/composition/state/error
interactions and broader profiles remain required gaps.

The Source operation retains all276 default entries from EN300743 clause10,
their exact opacity/default identity and its original named8-bit projections.
The explicit NativeD32 path additionally constructs independent native RGBA/word
owners when native values differ. Encoded palettes share the studio-unrepaired
owner only when every native entry equals it; other palettes have charged owners. No Source
CLUT fact, source/studio palette, index plane, packet, clock or earlier output is
rewritten; this is not an after-output colour repair.

Eight original whole-source controls and eight additive cardinality/fixed-grid
controls were observed through the same immutable default public-API D0 worker.
They establish per-region automatic palettes when no CDS has been received:
distinct used indices are ranked numerically, independently of frequency and
encounter order. The lowest used index is transparent even when it is nonzero;
single-colour and unused entries are literal zero. Alpha spans0 to255 by integer
rank, green equals alpha, and red/blue equal alpha divided by2. These unusual
colours are preserved receiver facts, not the standard default palette. A CDS
switches the observed family to encoded/fixed defaults; mode change restores
no-CDS automatic behaviour. Shared-family regions retain independent native
palettes and the already established literal reverse-PCS emission order.

The six new guards ran before activation:3 passed/3 failed, exit101, at
`default-clut-red-1793501657999982575`. The failures were exact full native words;
all276 Source/default and retained-owner controls passed. The original assertions,
source art, complete raw native palettes and unused/transparent RGB are unchanged.
The private candidate then passed all107 DVB guards in debug/release, all-target
Clippy with `-D warnings` and the no-default library check at
`default-clut-1/gate-v3`. Ninety-six retained-output source/native field runs at
work1/7/4096 had zero findings and identical work semantics; sixteen additional
logical Budget-pressure runs retained identical output/work and returned exactly
to the live pool baseline. These are functional ownership tests, not heap/RSS
measurements. The paid metadata/pixel cursors survive yield and output pressure;
private native collection storage is released before immutable-owner freeze.

The exact combined399 snapshot passed debug/release, Clippy, the no-default
library check and docs at `default-clut-1/combined399-v1`; its six new guards and
all393 predecessors are included in this workspace. This bounded codec
field proof does not establish equivalent container work, reference retention,
full native compatibility or numerical acceptance. Original8/additive8 raw native
records, original48/13 gaps and all393 preceding guards remain retained and
required. Complete PGS/DVD contracts and qualification remain open.

The encoded NativeD32 view additionally uses an independently authored integer
projection supported by the frozen explicit-CLUT controls. The original D0
public-API worker and its default options are unchanged. Sixteen packed inputs
cover all16,777,216 full8-bit Y/Cr/Cb triples at T=0; discovery controls cover every
reduced6/4/4/2 tuple, every fullY/T pair at neutral chroma, additional chroma/alpha
points, nominal2/4/8 palettes and unused entries. Raw components and all original
source-nearest/studio projections remain separate from native RGBA/words.

Actual paired controls show that a final reduced entry retains its previous
native value; the same entry followed by a full entry applies. A separate native
map of276 charged `ColourSource` values per admitted CLUT family preserves that
state. It exists only for NativeD32 and is released on epoch/reset/decoder drop;
it is not a rendered RGBA cache. The original CDS facts still record every entry.
NativeD32 also accepts the observed Y=0/nonzero-companion tolerance while Source
keeps its original rejection. Such inputs are explicitly nonconforming controls,
not proof of ETSI conformance. No source field or transparent native RGB is erased.

The frozen15 guards produced actual runtime RED4pass/11fail before the three-file
fix. All414 guards subsequently passed debug/release, Clippy and the no-default
library check at `explicit-clut-1/green-v2`, preserving all399 predecessors.
The Rust packet observer also passed all30 unchanged literal source/native
oracles:5,300 display sets and17,044,028 palette slots, including the full cube,
with raw clocks/configuration/order and retained outputs checked after decoder
and input release. Receipts are `explicit-clut-1/rust-matrix-v1`. These are bounded
codec-packet/ownership proofs, not equivalent container/reference-retention work,
allARGB/state combinations, numerical qualification or complete M05 acceptance.
The original input matrices, negative receipts and native records remain retained.

The later reset-policy fix preserves the caller-selected receiver view across
`reset`, `reset_with_configuration` and fatal stream-cache teardown. Stream
state, including effective276-value CLUT maps, is still discarded; fresh palettes
require new acquisition. Retained events keep their original view, source clock,
configuration and charged output owners. No cache, field or public API was added.

Against the old source, all414 preceding guards passed and the seven new guards
ran3pass/4fail, exit101. The one-file fix subsequently passed all421 in debug and
release, all-target Clippy with `-D warnings`, the host no-default library check
and documentation build at `native-reset-1/execution-v7-2-1/native-reset-v7-2-run1`.
Docs retained the pre-existing `colour.rs` link warning. The guards cover healthy
and fatal-error recovery through both reset methods, immediate cache-charge
release, retained aliases, unchanged Source work/bounds/object retention, and
rejected-reset ownership. Earlier colour/palette fixtures and implementation are
unchanged; this batch does not rerun or broaden the historical RGB cube proof.
These are functional reset/ownership results, not whole-workspace/ARM32 shipping,
container/reference-retention equivalence or numerical/full M05 acceptance.

The explicit NativeD32 raster additionally reproduces the twelve complete-CLUT
nonmodifying observations. All six flag1 planes compact the remaining colour2 to
column0; Source keeps its original nonmodifying holes and colour2 at column4.
Two independent 2-to-4 and2-to-8 mapped controls distinguish the decision point:
encoded code1 is suppressed, while encoded code2 mapped to CLUT1 is written.
Both actual raw native planes are `01040303030104030303`, with full palettes,
original input/configuration and clocks retained. These are versioned native
receiver facts, not a reinterpretation of the source contract or conformance.

The fifteen new guards ran before the narrow patch: **9 passed,6 failed**, exit101,
in `dvb-nonmodifying-red-1793497991112652630/runtime-red.log`. The six failures are
the intended flag1 planes; source, mapped-source and logical-row controls pass.
The candidate subsequently passed all **101 DVB tests** in debug and release,
all-target Clippy with `-D warnings` and the no-default library check at
`dvb-nonmodifying-green-1793499733826966925`; source before/after was unchanged.
A per-run suppression decision and separate destination column retain logical
bounds/coverage/decoded-pixel counts/work for every code, including suppressed
runs. Source still tests mapped CLUT1 and always advances. The fifteen guards
and all source/native art are unchanged; only public include/module paths
relocate. The combined393 passed debug/release, all-target Clippy, the no-default
library check and docs at `subtitles-nonmodifying393-1793500180361456104`.
These bounded probes and host results leave full native and numerical
qualification open.

The four original-default order controls now establish reverse
PCS order independently of RCS arrival, region ID or coordinate sorting. The
NativeD32 snapshot traversal constructs that order; no completed output is sorted.
The observed nonascending-PCS receiver tolerance is explicit to NativeD32. Source
PCS ordering/rejection remains unchanged, and both profiles retain window and
nonoverlap validation. A paid pairwise cursor preserves native overlap checks
across yields and pressure without repeatedly scanning the output prefix.

The parent observed the ten new order guards fail4/pass6 before the exact
source patch. The separate candidate then passed new10 plus old76 in debug and
release, all-target Clippy with `-D warnings` and the no-default library check at
`dvb-order-green-1793495506300977483`. Raw four-case inputs, palettes, native
clocks/words/geometry and original source PCS bytes are unchanged. The additional
invalid-overlap/window controls preserve existing bounds; no native conformance
or tolerance is inferred for those unprobed invalid geometries. The new guards
join the preceding368. The combined378 passed debug/release, all-target Clippy,
the no-default library check and docs at `subtitles-order378-1793495735838501975`.
This bounded receiver evidence does not close other state/corruption interactions.
Mandatory cached compressed-ODS replay is still unknown. All original65/96 source
tests, original177+40 text controls and
all old DVB observations remain preserved and required. Full O40/M05/native and
initial1.25/optimization0.80/lower-heap/RSS qualification remain open, as do the
PGS/DVD complete decoder contracts and qualification. No TV or shipping-device result follows.

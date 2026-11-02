# safe-media-subtitles: DVB raster and display-state slice

Experimental pure safe Rust. The only dependency is the owned `safe-media-core`
path package. No full M05 decoder, FFmpeg parity, performance or lower-memory
qualification is claimed. Workspace registration and Cargo execution belong to
parent/main; this lane does not change the root manifest/lock or build queue.

This independently authored seam follows ETSI EN 300 743 V1.6.1 (2018-10)
§§5.1, 7.2.1–7.2.6, 8 and 10. Source PDF SHA-256:
`1a5b211303397c52b771eb11e8f9f844d88727ecccd3fe0a5e8a6ac961d93e87`.
Official version-qualified source:
<https://www.etsi.org/deliver/etsi_en/300700_300799/300743/01.06.01_60/en_300743v010601p.pdf>.
The ignored lane retains primary texts/hashes, original source art and independent
RGBA/index oracles. No existing decoder implementation was copied or translated.

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
- Clipped/nonmodifying pixels are still charged as decoded work. Nonmodifying
  CLUT entry 1 leaves underlying indices intact; an index 0 write is distinct.
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

The additive configured-source API and its nine previously authored tests are
now connected after that gate. Their first compilation/execution remains queued
with the parent. The current 43-test DVB source is therefore newer than the
make10 tree; no passing or numerical result is claimed for this addition.

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
These tests have not run yet. Full physical source-span/opaque-side-data carriage
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

No PGS or DVD/SPU implementation is present: their normative source gaps remain
blockers. Cross-field map-table lifetime, composition/ancillary ID collisions,
progressive/character/ROM/ACS/disparity/depth-reduction capabilities, text cues,
M01/M03/M04 integration and every numerical gate remain open. M05 stays unqualified.

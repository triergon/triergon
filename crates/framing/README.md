# Packet framing development slices

Root3477 retains the compact `AvcConfigViewParser` and
`HvcConfigViewParser` alongside the eager and strict source entry points.
The packed HEVC descriptor stores a full-width payload start and the wire
u16 length. The scanner checks the endpoint before descriptor creation.
Borrowed and retained NAL views preserve exact source spans and owner
lifetimes. On ARM64 the compact descriptor uses 16 bytes per NAL, down
from 24 in Framing44. The two new guards check layout and exact pool
occupancy with a maximum-length NAL and a later full-width range.

The standalone packed-framing gates passed 539 ordinary tests and nine
doctests per profile, including both new guards. Root3477 adds those guards
to the 35 names from the earlier Framing44 overlay. Framing44's separate RSS
campaign did not pass its delivery gate. The separate packed-worker timing
run passed all 125 selected cases at the 1.25× delivery ceiling, with a
worst paired upper bound of 1.2395×. Five reached the 0.80× optimization
target. The same packed worker used less live heap and total peak RSS
on all 125 cases under the approved tight kernel observer. The worst upper
bounds were 0.9842× for heap and 0.9876× for RSS. The older operation-growth
diagnostic remains unresolved on all 125. This qualification applies to the
exact standalone worker and selected operation. The integrated root result
is functional evidence.

## Historical checkpoints

The combined selected AVC/HCRC checkpoint passes165 debug/release framing
regressions, all-target Clippy with warnings denied, no-default release checks
and release examples on native Linux ARM64. All148 DTS predecessors and82
independent AVC records are retained. `AvcConfigParser` implements the acquired
ISO14496-15:2010/incorporated2006FRExt wire grammar; later profiles/corrigenda,
SPS/RBSP and decoder consistency, hvcC and complete AAC/ADTS were open at that checkpoint.
The CPF=0 HCRC clarification preserves unknown CPF as `NotReached` and actual
missing/partial/present CRC bits. Its4152-call unchanged-input replay changes
only14 authorized HCRC presence annotations;4138 stdout records and all stderr
remain byte-identical. Raw/numeric fields, ranges and native24 views are intact.
Exact manifests and host/source evidence are in
`.research/parallel-framing/avc-wire162-adoption-v1/` and
`.research/parallel-framing/dts-hcrc-adoption165-v1/`.
Original793/71/224,784 range,320 capacity, cycle-high24, lifetime90 and EOF90
obligations remain required. Complete selected parity, equivalent native work,
runtime, strictly lower heap/RSS and target qualification remain open.
The older test counts below describe their own historical snapshots.

These are original bounded O21/M02 slices against owned core. The parent's
recorded host snapshot (private local receipt: `../../.research/dts-core-host-1793454330158626000/0.log`)
passes **all 62 framing tests: 13 NAL, 13 AV1/VP9, 13 AC-3/E-AC-3, 11 E-AC-3 BSI
and 12 DTS core**, including all 188 DTS header sources and 17 streams / 544
chunk/work/known-length variants after the test-only TSV quoting correction.
All-target Clippy with warnings denied and the release-library build also return
0. The receipt records identical 61-file source maps before/after that run.
Its canonical implementation bodies and original fixtures remain preserved;
later core/export additions and status prose have separate source identities.
The current checkpoint is described above. The earlier prose-change receipt is in
`.research/parallel-framing/dca-public-assessment-v1/`. Earlier 50-test and
26/39-test snapshots and both DTS red runs remain preserved separately.

The [DTS core implementation](DTS-CORE.md) supplies bounded normal canonical
headers and complete pool-owned frames with unchanged source bytes, explicit
source/generation, work, cancel and retry semantics. The original staged
0/12 red and the first implementation 10/2 red remain evidence; the latter
exposed test-only TSV transport quoting, not a proven production field defect.
The original 188/17 fixture rows and independent expected values are unchanged.
`DTS-CORE-RED.md` and the red feature manifest retain historical staging claims;
`feature-manifest-dts-core-v1.json` is the authored implementation manifest.

The parent also built a separate DCA-only reference and executed all 256 native
observation runs (private local receipt: `../../.research/dca-public-observer-native-v1/observations.json`).
Every run has rc0, a complete-observation summary and zero JSONL protocol errors.
The retained watchdog invalid-json statuses are the known single-JSON/NDJSON
adapter mismatch, not native failures. Independent light Python review verifies
all raw JSONL sequences/hashes and preserves those original statuses. Native
parity outcomes remain **unassigned**: 24/68 stream packet vectors equal the
strict source frame oracle and 44 differ; header-field visibility and rate/clock
identity remain incomplete. Packing, termination, HD assets, coded-audio validity
and full native/numerical qualification stay open. That historical observer proposal was held pending review; later source
adoptions have their own exact-source receipts. No prototype/full-module or
performance acceptance follows from these host tests or observations.

The [E-AC-3 BSI/evidence batch](EAC3-BSI.md) contributes the 11 passing regressions,
with separate complete-BSI/carriage, SID/type/map and tail-boundary controls.
The first red run is preserved separately: its literal-zero teardown assertion
was corrected to require exact equality with the pool's initial control charge,
captured before any frame. Production, fixtures and numerical gates were unchanged.
The batch's selected scope is in `feature-manifest-eac3-bsi-v1.json`.
Flexible mixing definition 3 remains unsupported. No BSID 9/10 support or G01
AVC/hvcC parser is inferred from a native observation or an unacquired grammar.

The earlier [AC-3/E-AC-3 source slice](AC3.md) has 390 authored cases. The pinned
public header API comparison of its 240 prefixes remains **NONPASS**: 166
bsid/frame-size matches, 55 fact-case API errors, seven unexpected accepts and
twelve observed rejections with unresolved error-class distinctions. Every
original input/expectation/nonpass is retained. The parent separately completed 69
SID/type/map diagnostic calls: 36 API successes and 33 API errors, with identical
raw tuples across the three carriage lengths for each control. These observations
do not waive any of the 62 original nonpasses or establish complete native parity.
All native execution belongs to the parent queue; none ran in this lane.

`feature-manifest.json`, `feature-manifest-av1-v1.json` and
`feature-manifest-ac3-v1.json` retain their historical prepared-checkpoint status.
The E-AC-3 BSI and DTS feature manifests also retain their historical authored
checkpoint states. `EAC3-BSI.md`, `DTS-CORE.md` and this README name later actual
host/reference evidence separately, without rewriting those manifests or receipts.
Full selected parity, delivery runtime upper paired95 <=1.20 (optimization target <=0.80) and lower live heap/RSS
remain hard, unqualified gates for these new framing features.

The crate has one owned dependency, `triergon-core`, and forbids unsafe code.
There is no I/O, platform decoder, native library, third-party crate or build
script. Root workspace/lock integration belongs to the parent lane.

## AVC/HEVC configuration and source facts

The new [declared configuration layout draft](DECLARED-CONFIG.md) adds 14
regressions verified on native Linux and a work-bounded validator for externally
declared array/NAL ranges and big-endian length fields. It owns all output
descriptors and preserves raw record bytes and zero-copy source spans. This is
not an ISO avcC/hvcC wire parser. That historical76-test checkpoint passed
Clippy and no-default checks on native Linux. The separate selected AVC wire
parser is described above. hvcC, later AVC grammar and numerical qualification
were open at that checkpoint.

The earlier [DTS source jobs](DTS-SOURCE-JOBS.md) brought that native Linux
checkpoint to 103 passing tests: all 76 existing tests, 14 source inspection tests
and 13 low14-bit packing tests. Clippy, the no-default library check and release
example builds pass. This adds explicit source facts and bounded payload-word
conversion; full 14-bit frame acquisition and all native/numerical gates remain
open. The 71 native acceptance differences have not been closed by these tests.

`Configuration::declared_record` preserves a pool-owned avcC/hvcC record byte for
byte, its declared encoding, and independently declared sample framing. Its
inspection result is always `DeclaredOpaque`. **It does not parse either ISO
record**, infer `lengthSizeMinusOne`, certify validity, or detect a conflicting
caller declaration. A consumer without a separately validated framing fact must
retain the raw record without using it to initialize this reader.

`AnnexBConfigParser` incrementally frames a bounded, already pool-owned Annex-B
configuration. It accepts AVC SPS/PPS types 7/8 and HEVC VPS/SPS/PPS types 32/33/34,
preserving duplicates and order. It returns `AnnexBFramed`, not a complete or
decoder-valid configuration. Parameter-set contents, IDs, relationships, profile,
dimensions and completeness are not parsed. Other configuration NAL types return
explicit `Unsupported`. Output parameter sets share the original immutable
configuration storage; source padding and prefixes remain in `source_bytes()`.

The framing of configuration bytes and the declared framing of media samples are
separate facts. These source readers preserve parameter-set order and bytes.
Packet/demux code supplies source/configuration generations, timestamps and side
data. `NalPacketSession` adds the separately scoped pinned packet projection in
[PACKET-BSF.md](PACKET-BSF.md).

## Incremental NAL input

`NalReader` owns an `InputCursor` for one source entity. It accepts arbitrary
chunk boundaries, including one-byte windows through prefixes, NAL headers and
emulation-prevention sequences. Call `step(work, cancel)` and handle core
`Step::{Ready, NeedInput, Yield, End}`. Answer a request with same-pool
`InputWindow` bytes and its exact token/source/offset. Empty input is starvation;
call `finish(source, token)` to establish completion, or `fail` for transport or
cancellation. Known length does not implicitly finish a source. A new source or
logical reset needs a new reader; there is no cross-entity NAL continuation API.

`LengthSize` declares an unsigned big-endian field of 1, 2, 3 or 4 bytes. This
generic wire contract is deliberately separate from ISO record/sample-entry
restrictions; accepting width 3 does not certify an avcC/hvcC record that names it.
Zero-length NALs, partial prefixes/bodies and excessive sizes are errors.

Each `Ready(NalUnit)` contains a complete pool-owned payload, source identity,
payload offset/length, prefix offset/width or declared length, padding counts,
and fixed-header facts. Length-prefix facts reconstruct the original prefix;
Annex-B facts preserve its observed three/four-byte prefix and zero padding.
An Annex-B reader may consume the next prefix before publishing the preceding
NAL. A later failure does not invalidate previously published NALs. It does not
make an entire access unit or source transactional.

Fixed-header inspection checks minimum length, `forbidden_zero_bit`, and nonzero
HEVC `nuh_temporal_id_plus1`. It preserves AVC `nal_ref_idc`, type, and HEVC type,
layer ID and temporal field. Reserved types and nonzero layers remain facts;
neither extension/header semantics nor decoding support follows from them.

Annex-B extraction follows the literal H.264 B.2 / H.265 B.3 termination patterns:
`00 00 00`, `00 00 01`, and explicit source completion. Runs preceding a prefix
are classified into prefix and leading/trailing zeros. At EOF one or two final
zero bytes remain payload; three or more terminate the NAL and are trailing
padding. No RBSP inference occurs. Nonzero bytes after a terminating zero run
without a prefix are rejected. Access-unit/type-dependent mandatory `zero_byte`
rules are not checked by this framing-only reader; it accepts an initial
three-byte prefix. Full EBSP/RBSP validity, including prohibited payload patterns,
is outside fixed-header inspection.

## Raw Annex B access units

Use `RawAnnexBReader` with an explicitly identified AVC or HEVC source.
It preserves original byte ranges and start-code widths and retains one
access unit plus a lookahead NAL under charged limits. The scatter output
shares the validated NAL payloads. The raw path uses incremental NAL
capacity instead of reserving the configured ceiling for each NAL.

The AVC boundary detector reads bounded SPS, PPS and slice-header prefixes
and compares primary-picture identity. This covers multiple slices and
streams without AUDs. Partitioned, redundant and extended AVC profiles
have explicit refusals. The HEVC reader uses base-layer first-slice and
leading-NAL rules and retains suffix and other-layer bytes in order. The
reader reports coded random-access flags and preserves raw bytes without
adding a presentation clock.
[H.264 §§7.4.1.2.3–4](https://www.itu.int/rec/T-REC-H.264-202408-I/en)
[H.265 §7.4.2.4.4](https://www.itu.int/rec/T-REC-H.265-202601-I/en)

Four authored fixtures compare exact packet extents, key flags and complete
source bytes. Four intact FATE streams add 243 access units. Native Linux
runtime and memory remain open. The playback crate provides the separate
[raw codec probe and decoder configuration](../playback/README.md#raw-avc-and-hevc-input).

## Annex-B output, limits and work

`AnnexBWriter` consumes a complete `NalUnit` and emits a pool-owned
`00 00 00 01 || original NAL bytes`. Work is resumable; `BuildStep` distinguishes
yield, one ready result and completion. Source facts accompany the output.
No NAL byte changes, emulation-prevention edits, parameter-set injection, AUD,
HDR/DV changes or keyframe policy occur. Invalid/opaque payload bytes remain
unchanged, so this adapter alone does not certify a conforming Annex-B stream.

All byte/offset/count arithmetic is checked. Callers set NAL, padding, count,
input-window, config-source/config-count and output-size limits. Reader content,
transport, limit and cancellation errors are sticky. Allocation/backpressure
errors preserve the exact reserve/freeze checkpoint for retry. Rejected external
responses leave parser state intact. Cancel is checked between byte scans and
copies of at most 4096 bytes, and releases in-progress builders.

Each input-byte visit includes constant parsing/copy work in the cursor's charged
unit. Deferred Annex-B payload bytes, output bytes, parameter-set insertions and
reserve/freeze attempts are separately charged. One-unit budgets make progress;
zero budgets do not consume bytes. Allocator/deallocator and bounded list teardown
costs are foundation operations, not a hard wall-clock latency guarantee.

| Operation | Retained storage in this slice |
|---|---|
| Length-prefixed reader | One input window and one declared-length builder, then charged immutable output |
| Annex-B reader | One input window and one `maximum_nal_bytes` scratch allocation per in-progress/output NAL |
| Annex-B config parser | Original config storage, preallocated bounded descriptors, and one temporary NAL scratch; returned NALs share original config storage |
| Annex-B writer | Input NAL plus one exact payload-length+4 output builder, including freeze metadata charged by core |

The Annex-B scratch policy avoids hidden vector-growth copies but can retain much
more capacity than a short NAL needs. This is an explicit memory optimization
gap. Holders of immutable clones keep the full backing allocation charged. A
pool that cannot hold an operation's internal peak needs a larger budget or
smaller declared limits; releasing unrelated outputs cannot fix that situation.

## NAL baseline evidence and parent verification

`fixtures/framing-v1.tsv` has 30 original bit-authored cases, 54 NALs, and 501
input bytes. The independent Python whole-buffer oracle computes payload,
header, offset, prefix and padding expectations. Bodies are opaque authored
bytes, not decoder-conformant media. `tests/framing.rs` contains 13 Rust
regressions: all fixture chunk sizes with work 1/2/5/64, conversion fidelity,
malformed/truncated input, header fields, 255/256 lengths, source lifecycle,
limits, cancellation, reserve/freeze retries and retained ownership across
reader drop and cross-thread release. Test polling loops have finite guards.

The parent's first host-test log (private local receipt: `../../.research/parallel-framing-ts-dvb-first.log`)
records:

```text
test result: ok. 13 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.03s
```

This result covers the authored Rust regressions, including the fixture chunk/work
variations. Suite elapsed time is not numerical performance evidence. It does not
establish source/native full parity, release behavior or memory qualification.

The generator and frozen manifests are in `.research/parallel-framing/`. Its
current v2 is self-contained Python; v1's exact script/manifest/table are kept
separately. Both versions produce the identical TSV. Only the Python generator
and preparation checks ran in this lane; the Rust result above comes from the
parent queue. Parent make8/make9 subsequently passed all 13 NAL and 13 AV1/VP9
regressions plus format/lint/release checks on those source snapshots. Target checks and the
independently pinned reference harness remain pending. No installed FFmpeg was used here.
Historical checkpoint, HOLD and preparation receipts remain unchanged and record
the earlier pre-execution state and README hash. Compiled sources and fixtures
were held at that checkpoint. The new batch adds separate files/receipts without
rewriting those historical artifacts.

Full selected parity, delivery runtime upper paired95 ratio <=1.20 (optimization target <=0.80), lower live heap and
lower attributable RSS remain hard per-case gates on native Linux ARM64. All are
**unmeasured for this slice**; 32-bit webOS and consumer integration are also open.

## Primary sources and provenance

The implementation was authored from specification facts. The promoted O00
inventory is requirements evidence only (INV09/12/15/16); no Streem, FFmpeg or
other implementation was copied or translated. The lane stores exact source
URLs/SHA-256 values and narrow PDF text extracts in its ignored `specs/` folder.
The following editions are deliberately pinned, rather than assumed latest:

- [ITU-T H.264 (08/2024)](https://www.itu.int/rec/T-REC-H.264-202408-S/en),
  7.3.1, 7.4.1/Table 7-1, Annex B (printed pp. 45, 65–67, 311–312).
- [ITU-T H.265 (07/2024)](https://www.itu.int/rec/T-REC-H.265-202407-S/en),
  7.3.1.2, 7.4.2/Table 7-1, Annex B (printed pp. 34, 64–67, 273–274).
- [RFC 6184 §1.3](https://www.rfc-editor.org/rfc/rfc6184#section-1.3) and
  [RFC 7798 §1.1.4](https://www.rfc-editor.org/rfc/rfc7798#section-1.1.4)
  corroborate fixed-header fields. RTP packetization is not implemented.

Acquisition and version selection for hvcC and later AVC profiles/corrigenda
remain open. The selected older AVC grammar is acquired as described above.
Neither a framework's record parser nor recollection of its layout substitutes
for primary source evidence.


## Selected hvcC and AAC source operations

The earlier checkpoints above retain their original scopes and counts.
The additive [hvcC/AAC API contract](HVCC-AAC.md) covers the selected full
hvcC fixed fields and NAL arrays, ordered all-NAL conversion, AAC-LC/HE-AAC/
HE-AACv2 ASC/GASpecific/PCE, ADTS records/streams, and separately named pinned
receiver views. Raw source facts and default behavior remain independent of
those views. All current DTS code, guards, fixtures and documentation remain
unchanged. The complete prerequisite set adds 106 named framing guards
(80 base + 26 compatibility), for an expected 317 framing guards. The
composed adopted1863 + framing source has an expected 1969 passing Rust
tests and its existing one ignored test; this source-only adoption package
has not run that gate. Native and numerical/full-module qualification
remain separate and open.

## Stateful packet projection

The independent `NalPacketSession` and `NalPacketConfigWriter` preserve the source
record and complete packet while producing a separately named pinned d32
AVC/HEVC packet view. Injection origins, opaque caller metadata, receiver epochs,
work, cancellation, ownership and refusals are explicit. Source readers and all
317 existing guards are unchanged. [PACKET-BSF.md](PACKET-BSF.md) defines the
implemented operation and remaining validation/qualification gaps.

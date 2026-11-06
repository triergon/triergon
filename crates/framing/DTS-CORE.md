# Selected canonical DTS core — host green, native parity unassigned

The parent's recorded host run (private local receipt: `../../.research/dts-core-host-1793454330158626000/0.log`)
passes all **62 framing tests**, including all 12 DTS regressions and the original
188 header sources, 17 streams / 544 variants. All-target Clippy with warnings
denied and release-library build also pass, return code 0. The recorded 61-file
source map is identical before/after that run. Its canonical implementation
bodies and original fixtures remain preserved; later core/export additions have
separate source identities. The current checkpoint is described in README.md.
The earlier prose-change receipt is in
`.research/parallel-framing/dca-public-assessment-v1/`.

The staged 0/12 runtime red and first implementation 10/2 red remain immutable.
The latter failed on test-only TSV quoting; its corrected transport decoder keeps
all frozen source bytes, independent expected values and comparison assertions.
`DTS-CORE-RED.md` and the red feature manifest remain historical staging evidence;
`feature-manifest-dts-core-v1.json` records the authored implementation scope.
Neither host green nor native observation completion establishes full parity.

## Selected behavior and primary source

The original code follows ETSI TS102114 V1.6.1 clauses3.4/5.3/5.4.2 and
Tables5-1..5-20. The ignored lane retains exact publisher URL, printed/zero-based
page pins and hashes of web-extracted excerpts. No complete DTS PDF cache or
whole-PDF hash is claimed. No FFmpeg/Streem/other media implementation was copied
or translated, and the sole runtime dependency remains owned triergon-core.

`probe_dts_core_header` inspects at most15 input bytes and returns all35 frozen
raw/derived facts for selected normal canonical MSB-first headers. It preserves
every coded field through the104/120-bit end, HCRC absence versus present-zero,
inactive raw flags, user-defined arrangement codes and nominal versus open
bitrate. Versioned dialog gain is separate from its source nibble. Coding-window
Time uses the core/base-band rate and does not infer an extended/rendered rate.
HCRC is retained without applying its value test, following the acquired clause.

Selected invalid table/fixed/block/sync codes are rejected; termination frames,
incompatible encoder versions, reserved enabled internal-extension codes and a
separate extension-substream sync receive explicit Unsupported. User-defined
channel modes retain unknown channel count. Probe success is not a complete
frame, coded audio, presentation layout or decoder-readiness certificate.

`DtsCoreFrameReader` incrementally acquires exactly FSIZE+1 bytes, including
internal core extensions. False syncs inside the opaque body cannot split a
frame. Each complete pool-owned output retains source, generation, absolute
offset, unchanged bytes and header facts. Multiple frames may change rate/header
facts without rewriting previous outputs. The input is an explicitly declared
adjacent canonical-envelope sequence: there is no arbitrary resynchronization,
carrier unpacking, implicit inter-frame padding or silent tail stripping.

## Work and resource boundaries

The state machine separates every variable copy from allocation and publication:

| Phase | Charged work and retained state |
|---|---|
| Header collection | One unit per source byte copied into fixed15-byte scratch |
| Sync/kind/probe | One unit per fixed operation; probe inspects at most120 bits |
| Reserve | One unit per allocation attempt, with an explicit empty-builder guard |
| Prefix copy | One unit per byte copied from scratch into already reserved storage |
| Body read | One unit per source byte visited; at most4096 bytes retained as a same-pool span |
| Body copy | One unit per copied byte, at most4096 per copy, with partial progress retained |
| Freeze | One unit per charged immutable-owner/freeze attempt |
| Publish | One unit to move the complete owned result to the caller |

Reservation never moves existing prefix/body bytes. Prefix and body extension
check reserved capacity first, so the core grow helper cannot hide a reallocation
copy in a one-unit phase. Input spans are retained until their copied bytes are
committed, preserving exact progress across retryable allocation errors. The
fixed scratch, metadata/Time work, Arc operations and allocator/deallocator are
bounded/foundation costs, not a wall-clock latency guarantee.

Caller limits bound frame bytes, frame count and input windows. Header validity
and payload policy are checked before body allocation. The output-owner lower
bound includes core Bytes owner overhead and Budget control storage; impossible
absolute pools fail explicitly. Actual mutable capacity and immutable ownership
metadata are charged by core. Internal peak includes source backing, builder,
pending aliases and freeze metadata; caller-retained aliases keep whole backing
capacity charged. Budget must accommodate that internal peak. This is resource
accounting, not lower-memory qualification.

Zero work consumes no input or copy bytes. One-unit grants can progress through
all phases. Cancellation is checked before fixed operations and each bounded
copy/read; it drops pending spans, builders, unpublished output and the input
cursor. Sticky invalid/unsupported/limit/truncated/transport/cancel errors retain
only source/request diagnostics and previously published caller-owned outputs.
Backpressure/allocation errors keep the exact reserve/copy/freeze retry position.
Rejected external input responses leave the parser unchanged. Known length does
not imply EOF: explicit completion, starvation and transport failure stay distinct.

## Container packet delivery

`DtsCorePacket` checks the leading sync and core length of a complete
container packet. It borrows canonical big-endian cores and can normalize
the word-swapped and 14-bit packings into caller-owned storage. The output
is bounded by the 14-bit frame-size field. Trailing extension bytes remain
available in the original packet. This projection checks the core envelope
and leaves the coded audio opaque.

## Required unresolved scope and native comparison

14-bit/word-swapped carrier grammar, physical/logical lengths, padding/conversion,
termination SHORT/presentation accounting and extraction from HD asset headers
remain required gaps. The primary audio coding header, audio blocks, internal extension
semantics, auxiliary/timestamp payloads, other CRCs and cross-field/audio validity
remain unvalidated. Encoded opaque fixture bodies do not establish audio conformance.
No G01/AAC grammar or additional BSI feature is inferred or implemented here.

The formats reference remains unchanged. The parent built a separate DCA-only
profile from the same exact source/archive pin and compiled the original public
API observer. Its worker SHA-256 is
`63c770fdcb61de527ddbb6027fc52ab304047ee5f9d5e0afbfa982b41b602e7c`.
All 256 observation runs completed (private local receipt: `../../.research/dca-public-observer-native-v1/observations.json`)
with rc0, complete-observation summaries and zero per-run JSONL protocol errors.
The source/reference before/after maps are unchanged. The retained 256 watchdog
invalid-json statuses are a known single-document-JSON versus NDJSON adapter
mismatch, not timeouts or native failures. Raw statuses and complete raw records
are preserved, with independent sequence/hash review recorded separately.

Native full-operation outcomes remain **unassigned/ungraded**. The strict review
finds 24/68 equal ordered stream frame vectors and 44 different vectors. All 17
stream byte vectors are invariant across chunks; full output metadata is
invariant for 15/17. The 187 emitted header-prefix packets retain constructor
codec defaults and duration 0, so they do not prove native extraction of all 35
source facts. In the rate-change stream, the second source header has 44100 Hz /
512 samples; native reports 48000 Hz / duration 557, with unequal exact times.
No packet trimming, field filling, time rounding or exception turns these into
parity. See `.research/parallel-framing/dca-public-assessment-v1/ASSESSMENT.md`
for exact case/source evidence, scope matrix and the next bounded operation plan.

Full frozen selected parity, equivalent-work delivery runtime upper paired95 <=1.20 (optimization target <=0.80),
lower live heap and lower attributable RSS remain open hard per-case native
Linux ARM64 gates. Separate 32-bit/device and consumer integration proof remains
required. Production and observer source stay HOLD. Parent execution above is
separate from this lane's read-only receipt/Python review; no new dependency,
unsafe, core/root/harness edit, implementation copying, commit or device/account
action is included.

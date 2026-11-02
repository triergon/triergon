# Canonical DTS core: interfaces and regressions staged for parent runtime red

This is an original, intentionally unimplemented DTS checkpoint. The prior
50-test source is preserved in the ignored lane's baseline snapshot. New DTOs,
accessors and signatures are present; format entry points return
`Unsupported("DTS core implementation pending parent runtime red")`. They do not
parse input, acquire frames, synthesize source facts or produce EOF. The tests
classify that diagnostic as pending implementation, distinct from a required
unsupported-format result. A compile failure is not the requested runtime red.

No production parser/framer implementation is included. Parent compilation and
actual runtime red are pending; no Cargo/rustc/rustfmt/native/fuzz/benchmark ran
in this lane. Hold all staged files until the parent reports that result.

## Complete selected contract under test

ETSI TS102114 V1.6.1 clauses3.4,5.3,5.4.2 and Tables5-1..5-20 support normal
canonical MSB-first core bitstream headers and unchanged complete declared frame
envelopes. The future implementation must retain every raw104/120-bit header
field through the dialog/unspecified nibble, optional HCRC presence and value,
user-defined modes, open bitrate and separately derived exact core-window facts.
Legacy HCRC is retained without applying its value test. FSIZE includes internal
core extensions; payload sync patterns do not shorten the declared frame.

`DtsCoreHeader`, `probe_dts_core_header`, `DtsCoreFrameLimits`,
`DtsCoreFrameFacts`, `DtsCoreFrame` and `DtsCoreFrameReader` define the staged
boundary. The frame has no public unchecked constructor. Source/generation/offset
and complete pool-owned bytes accompany its header. Reader methods use existing
core input/source/failure/work types; no shared API or dependency change is needed.

The12 original Rust regressions cover all188 frozen header cases and all17
frame/stream cases. The header test visits every row before its final assertion
and compares all35 raw/derived source-fact keys on success. The stream test visits
all17 sources under chunk1/2/7/31, work1/2/17/4096 and known/unknown length:
544 variants, with exact raw bytes, prior outputs, offsets, generation, source
pool and header facts checked. Their statuses are proposed local-contract
expectations, never predicted native verdicts.

Further tests cover absent/present-zero HCRC, versioned/unspecified dialog words,
core sample duration, open bitrate, user-defined channel modes, header versus
body completion, false payload syncs, zero work, tiny work, cancellation before
acquisition and during partial header/body, sticky transport/errors, foreign
pool/source/generation, replayed offsets, starvation, explicit completion,
allocation/freeze pressure/retry, exact constructor-baseline teardown, retained
views/cross-thread release and frame-byte/count policies. A freeze reservation
failure is tested when a separate freeze charge is needed; prepaid metadata is
also permitted. Every poll loop is bounded.

Both TSVs are byte-for-byte copies of the existing independent Python corpus:

- `fixtures/dts-core-header-author-v1.tsv`:188 rows,
  SHA-256 `6bb339702ec47930cb523246128bc03f668c5587ae5732432c0c26cfaa569261`.
- `fixtures/dts-core-frame-author-v1.tsv`:17 rows,
  SHA-256 `87fb969222680058d06edd5037702eb0dd0e84ead9ae32329ecb2238916a75f6`.

The test-only JSON-subset splitter reads the original frozen expected scalar
and nested frame facts without a JSON dependency. It is not a media oracle or
production JSON parser. No frozen source row, expectation, original generator,
archive or qualification receipt was modified to stage these tests.

## Source proof and required gaps

The ignored lane's `m02-primary-next-v1/primary-proof.json` records exact official
publisher URLs and printed/zero-based PDF pages. DTS is acquired through web
excerpts with exact payload hashes: there is no cached complete DTS PDF or
whole-PDF SHA claim. The excerpt hashes are
`9bb7f71a9c5fe820270b84ca8bbb13afdee115ce4d67a5e823571a8358d5d1ee`
and `2fec6d6cf75fe1a79fd8c7496eea53c83b2bbe3e75ded04987a90babc6426158`.

14-bit/word-swapped carrier grammar, padding/conversion, termination sample
accounting and HD asset/core extraction remain required unresolved gaps. The
selected Unsupported controls do not implement them or waive them from O21.
Primary audio coding headers, encoded audio/internal extension semantics, audio
CRC/auxiliary validation, bitrate/audio cross-field validity and decoding are not
proved by these opaque authored frames. No AVC/hvcC or AAC grammar gap is closed.

## Baseline and claim boundary

The original50 test bodies, existing fixture bytes and production parser/framer
modules are unchanged. `lib.rs` gains only the new module/exports; README prose
identifies this later unrun stage. The old50 green result remains evidence about
its recorded source, not a result for these12 tests or the new exported module.
The first BSI red and subsequent constructor-baseline fix/green remain frozen.

All240 original AC-3 prefixes and all62 original native nonpasses remain intact.
The69 later native observations remain observations and no waiver. Full selected
parity, runtime upper paired95<=0.80, lower live heap and lower attributable RSS
remain unqualified hard per-case native Mac ARM64 gates, with separate32-bit/
device proof required. Staged interfaces and an expected runtime red are neither
full O21 nor numerical qualification.

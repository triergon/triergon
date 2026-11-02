# Packet framing development slices

These are original, bounded O21/M02 slices against `safe-media-core`. The parent's
full `make check` 8 and 9 snapshots passed **all 26 framing regressions: 13 NAL +
13 [AV1/VP9](AV1-VP9.md)**, with format, release, Clippy and safety checks green.
Make8 was the AV1/VP9 batch's first Rust compile/test pass after parent formatting;
make9 also passed all 26. Its 137-case independent Python corpus has run. This is
not full O21, full codec validation, FFmpeg parity, or numerical qualification.
`feature-manifest.json` remains the historical NAL feature/checkpoint record;
`feature-manifest-av1-v1.json` records the new AV1/VP9 boundary and gaps.

The next [AC-3/E-AC-3 batch](AC3.md) adds 13 authored Rust regressions and
390 independently generated Python source-fact cases. Those new Rust regressions
have **not run**; make8/make9 cover the earlier 26 tests only. Its exact selected
features and remaining gaps are in `feature-manifest-ac3-v1.json`. All new audio
parity, runtime and memory gates remain unqualified.

The crate has one owned dependency, `safe-media-core`, and forbids unsafe code.
There is no I/O, platform decoder, native library, third-party crate or build
script. Root workspace/lock integration belongs to the parent lane.

## AVC/HEVC configuration and source facts

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
separate facts. No parameter set is inserted, deduplicated, sorted or rewritten.
Configuration/source generations, packet timestamps and side-data association
remain responsibilities of the parent packet/demux contract.

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

The parent's [first host-test log](../../.research/parallel-framing-ts-dvb-first.log)
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

Full selected parity, runtime upper paired95 ratio <=0.80, lower live heap and
lower attributable RSS remain hard per-case gates on native Mac ARM64. All are
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

Primary ISO/IEC 14496-15 configuration-record syntax acquisition and version
selection remain open. Neither a framework's record parser nor recollection of
its layout substitutes for that primary source in this slice.

# AV1 OBU/av1C and VP9 record slice

This original O21/M02 batch adds AV1 low-overhead/one-OBU framing, AV1 codec
configuration record v1 inspection, and VP9 codec configuration record inspection.
The parent's make8 and make9 snapshots passed **all 13 AV1/VP9 and 13 NAL
regressions**, with format, release, Clippy and safety checks green. Make8 was
this batch's first Rust compile/test pass after parent formatting. This lane ran
only Python preparation; it ran no Rust, native reference, benchmark or fuzz job.
These host results do not close
O21, certify decoded media, or inherit numerical qualification from other graphs.

The crate still depends only on the owned `triergon-core` and forbids unsafe.
Core, root manifests/lock and the shared harness are untouched by this batch.
The earlier AVC/HEVC avcC/hvcC parsing gap **G01 remains unsupported**. AOM's open
AV1 record syntax does not authorize guessing the different ISO AVC/HEVC layouts.

## Source framing and ownership

`Av1ObuReader` accepts core same-pool `InputWindow`s with the current source,
offset and generation token. It emits core `Step<Av1Obu>` events with separate
input requests, work yields and explicitly finished source end. Empty/short
windows do not imply EOF. Transport failure, truncation and cancellation remain
distinct sticky errors. New source/reset transactions use a fresh reader; there
is no cross-entity OBU continuation API in this slice.

Callers explicitly select one packing context:

- `LowOverhead` requires `obu_has_size_field = 1` on every OBU. A missing field is
  unsupported, even when a caller happens to know the source length.
- `OneObu` requires a known, nonempty source entity containing exactly one OBU.
  It derives an absent size from that external envelope; if an internal size is
  present, both lengths must agree. Packet, frame and temporal-unit boundaries
  must not be substituted for an OBU boundary. This is not an AV1 Annex-B parser.

Every complete output preserves the exact wire bytes, including base/extension
header and size encoding, and exposes a pool-owned payload span. Facts include
source, core parser generation, wire/payload offsets and lengths, raw header,
optional extension, optional size field and optional external length. Absent
extension and an explicitly present zero extension are distinct.

LEB128 is limited to eight bytes with a terminating eighth byte and a value no
greater than `u32::MAX`. Redundant zero groups are accepted without shortening;
`Av1Leb128` exposes the original bytes, encoded width and numerical value. Size
arithmetic and source boundaries are checked before payload allocation. On a
32-bit target, unrepresentable wire sizes may return checked overflow; target
execution and its frozen error contract remain pending.

Headers are inspected as source facts. `syntax_issues()` reports nonzero forbidden,
base-reserved and extension-reserved fields; their actual values remain in raw
bytes/accessors. A complete envelope with nonempty issues is **not a conforming
AV1 OBU**. Consumers must apply an explicit acceptance policy. The reader does
not discard unknown OBU types, filter layers/operating points, trim trailing bits,
unescape data, assemble frames, infer keyframes or rewrite metadata.

One unit charges each input-byte visit with its constant classification/copy,
each prefix-byte copy, and each resolve/reserve/freeze attempt. Copies are at
most 4096 bytes before checking cancellation again. One-unit budgets progress;
zero budgets do not consume bytes. Each known wire length is reserved once and
freeze pressure retains the completed builder. There are no growth copies or
payload rescans after allocation pressure. Allocation/deallocation and bounded
descriptor teardown remain foundation costs rather than wall-clock guarantees.

## av1C record inspection

`Av1ConfigParser` receives the already bounded, pool-owned
`AV1CodecConfigurationRecord`, **without** the surrounding box size/type. It
parses the four fixed bytes with work accounting, then frames config OBUs with
the same cursor so offsets remain relative to the original record (first possible
OBU offset 4). Source identity and generation remain associated with every view.

`Av1ConfigFields` exposes all physically present fixed fields and raw bytes:
marker/version, profile/level/tier, high/twelve-bit flags, monochrome, subsampling,
chroma position, three reserved bits, delay presence and the final four bits.
Absent presentation delay is `None`; a present delay value zero is `Some(0)`.
Reserved final bits when delay is absent remain separately observable. Marker and
reserved-bit violations are surfaced as issues, not cleared. Unknown record
versions are explicitly unsupported; their layouts are not guessed.

Empty configOBUs are allowed. If a Sequence Header OBU is present, it must be
first and cannot occur twice. Every config OBU must carry its size field. Other
types are retained as opaque source data. The record's raw bytes, OBU order and
all payload bytes are unchanged. Returned OBU views share the original record;
only one exact-size OBU scratch allocation is needed during parsing. Descriptor
capacity is reserved once from the pool under the explicit OBU-count bound.

This does **not** parse Sequence Header payloads, validate profile/level/colour
relationships, match sequence fields to the record, enforce requirements that
depend on associated samples, validate tile-list carriage, parse metadata, or
certify decoder readiness. Empty fixed-field issues do not imply those checks.
The parent owns packet/configuration epoch association and integration.

## VP9 record inspection

`Vp9ConfigParser` receives a `VPCodecConfigurationRecord` body and an explicitly
declared `Vp9ConfigContext` for the containing FullBox version/24-bit flags. Box
headers are not guessed from bytes. Only context version 1 is supported; v0 and
future versions return unsupported. The parser reads eight fixed bytes with work
accounting, checks the exact declared initialization-data length and exposes
profile, level, bit depth, chroma subsampling, range and all CICP values unchanged.

The full raw record and initialization span are retained in the input pool. A
nonempty initialization field violates the VP9 binding, but is preserved with an
issue rather than dropped. Other issue bits cover nonzero box flags, unknown
profile, invalid bit-depth/chroma field ranges and RGB without 4:4:4. This is a
bounded list of local checks, not complete profile/level/CICP/sample validation.
Unknown colour codes remain source values. No HDR rewrite, VP9 frame header,
superframe index, keyframe/probe or decoder is implemented here.

VP9 inspection does not allocate/copy output data: it aliases a complete owned
record and only reads the fixed prefix. Acquisition backpressure belongs to the
caller that obtains those bytes. AV1 descriptor/builder allocations expose
retryable pool backpressure. Both record parsers yield on small work grants and
poison private state on cancellation without publishing partial configurations.

## Original corpus and parent gates

The independent self-contained Python generator lives at
`.research/parallel-framing/av1-batch/generate.py`. It uses bit-authored fields,
opaque bodies and a separate whole-buffer oracle with positional base-128
arithmetic. It imports no Rust/media implementation and invokes no native tools.
It produced frozen v1 tables and manifest with these bounds:

| Family | Cases | Successful fact extraction | Negative/rejection vectors | Authored wire bytes |
|---|---:|---:|---:|---:|
| AV1 OBU | 77 | 64 | 13 | 2462 |
| AV1 codec record | 38 | 26 | 12 | 207 |
| VP9 codec record | 22 | 9 | 13 | 144 |

These are **137 cases / 2813 wire bytes**. Successful extraction includes source
violations with explicit issue bits; it does not mean valid encoded pictures.
Failed-source hex repros are retained alongside successful cases. Fixtures cover
all OBU types, extension presence/layers/reserved bits, LEB widths 1–8 including
redundant forms and 127/128 boundaries, size-less external OBUs, conflicting
sizes, eight continuing bytes, values above 32 bits, explicit limits, delay
presence, every three-bit reserved value, record versions, sequence ordering,
VP9 context/CICP/initialization preservation and truncated/surplus data.

`tests/av1.rs` contains 13 regressions executed successfully by the parent in
make8 and make9. Its OBU table runs every chunk size
and work 1/2/7/64. Record tests use tiny work/windows and compare independently
generated fields and raw bytes. Ownership checks cover foreign/stale input,
starvation/EOF/failure, reserve/freeze retry checkpoints, cancellation, count
limits, source sharing, teardown and cross-thread release. Poll loops are finite.
The existing 13 NAL regressions and sources are unchanged by the batch.

Only small Python oracle/preparation work ran in this lane. The parent's
make8 (private local receipt: `../../.research/parallel-workspace-check-8.log`) and
make9 (private local receipt: `../../.research/parallel-workspace-check-9.log`) receipts establish the
26-test framing snapshot and completed format/release/Clippy/safety gates.
Target and independent reference harness work remain open. Full selected
parity, delivery runtime upper paired95 <=1.20 (optimization target <=0.80), lower live heap and lower attributable RSS
remain **unqualified and unmeasured** for all new features, with native Linux ARM64
numerical and separate 32-bit/webOS integration proof still required.

## Primary provenance

These source facts were independently implemented from publicly acquired primary
documents. The approved [AV1 v1.0.0 Errata 1 spec tag](https://github.com/AOMediaCodec/av1-spec/tree/4f2ff41559ab4ac241d1957a17e1f1c2814e26d2)
supplies §§4.10.5, 5.2–5.3 and 6.2. The approved PDF identifies that tag; the
unversioned AV1 HTML landing page labels itself a working draft and is not the
normative pin. The [AOM av1-isobmff v1.3.0 deliverable](https://aomediacodec.github.io/av1-isobmff/v1.3.0.html)
supplies §§2.3.3–2.3.4. VP9 record facts come from the
[WebM authoritative binding](https://github.com/webmproject/vp9-dash/blob/b91775b9d2d1b0d83c0ad265aeb61886303071da/VPCodecISOMediaFileFormatBinding.md),
v1.0 (2017-03-31), configuration syntax/semantics.

Exact commit/blob identities, downloaded source hashes and URLs are recorded in
the lane's `av1-batch/spec-sources-v1.json`. Specification archives remain ignored
research material. O00 INV17/IC01–IC04 are requirements context only. No Streem,
FFmpeg, GPL or other implementation was copied or translated, and no new runtime
dependency was admitted. No device, account, publish or commit action occurred.

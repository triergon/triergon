# Declared configuration layout draft

This is an original safe Rust **declared-layout validator**, not an
AVCDecoderConfigurationRecord or HEVCDecoderConfigurationRecord wire parser.
There is no new G01 support claim. The exact base record grammar has not been
acquired in the public primary material inspected so far. The declaration input
must come from a separately reviewed producer; this API cannot establish that
the producer follows ISO/IEC 14496-15. It is not a replacement offboarding path.

`DeclaredConfigParser` accepts a same-pool immutable complete record, source ID,
codec, externally selected sample length size and immutable array declarations.
Array ranges are ordered and nonoverlapping. Each NAL declares a contiguous
one-to-four-byte big-endian length field and payload. The validator checks those
ranges, the actual field integer against payload length, selected policy limits
and the existing primary-backed AVC/HEVC fixed NAL header. Unknown NAL types and
duplicate NALs are retained. A disagreement between the declared array type and
the fixed NAL type is exposed as `array_type_mismatch`; it is never corrected.
Array type and optional completeness stay explicitly **declared** metadata.

The output preserves the entire original record, including gaps, suffixes and
reserved bytes, plus every array and NAL in declared physical order. Array,
length-field and NAL views share the original charged byte owner. A retained
view keeps that complete owner charge after the parser, output, declarations
and other source handles are dropped. Descriptor arrays are pool-owned and
reserved at exact preflight counts while empty. There is no dynamically growing
prefix copy hidden in a work unit and no NAL payload copy or rewrite.

Work charges each array count/range check, each length byte and each descriptor
publication; a single work unit always permits bounded progress. Opaque payload
contents are not scanned. Zero work performs no allocation. Cancellation and
validation failures release parser-owned source/descriptors and remain sticky;
allocation/backpressure errors retain the operation for retry. Publication is
one `Ready` followed by `Complete`. All limits are caller policy, not ISO limits.

The complete record is already owned at entry. External chunk delivery,
generation/reset requests, record-version recognition, profile/level fields,
conditional AVC extensions, HEVC array grammar and ISO length-size eligibility
are **not implemented** here. `inspection()` remains `DeclaredOpaque`.
No completeness, reserved-bit validity, SPS/RBSP or decoder consistency is
certified. The original opaque `Configuration::declared_record` is unchanged.

The source checkpoint adds **14 authored regression functions**, unexecuted in
this lane. They cover all declared field widths, fixed headers, source ordering,
duplicate/unknown NALs, empty arrays, opaque suffixes, mismatches, malformed and
overlapping ranges, limit/foreign-pool failures, work quanta 1/2/3/7/64,
backpressure retry, one-time publication, sticky cancellation and retained-owner
release to the captured pool-control baseline. These are declaration fixtures,
not ISO conformance vectors. The old 62 tests and every original fixture remain
unchanged. All 76 current Rust tests, Clippy and no-default checks pass on native Linux.
These host results do not establish native wire-grammar parity or numerical acceptance.

Primary provenance remains in
`.research/parallel-framing/g01-acquisition/REVIEW-v1.md` and
`REVIEW-v2-3gpp.md`: Apple/W3C/3GPP sources delegate the binary record grammar to
ISO, while the acquired official MPEG edition-8 drafts defer incorporation of
the base text. The later official MPEG w13798 archive is unrelated ARRM material
and is excluded as codec evidence. No implementation source supplied grammar.
Existing fixed-header provenance is unchanged in `src/header.rs` and the NAL
slice documentation. Source-only pre-implementation repro and final hashes are
in `.research/parallel-framing/avc-hevc-records-draft-v1/`.

Complete selected parity, equivalent-work native comparisons, runtime upper
paired95 <=1.25 (optimization target <=0.80) and strictly lower live heap/RSS remain open. Full native parity, timing and numerical acceptance remain open.

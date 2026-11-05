# Selected hvcC and AAC framing APIs

These independently authored safe Rust operations preserve source facts and add
separately named consumer views. They use the existing shared core and introduce
no production dependency. The owner’s 2026-11-02 decision permits licensed
implementation evidence and controlled interoperability observations; this is not
a claim of normative ISO conformance, complete AAC/HEVC decoding or qualified O21.

## hvcC source grammar and conversion

`HvcConfigParser` owns the exact record bytes and source identity, generation and
offset. Version 1 contains the full 23-byte fixed prefix:

| Source field | Bits |
| --- | --- |
| configuration version | 8 |
| profile space, tier, profile IDC | 2, 1, 5 |
| profile compatibility flags | 32 |
| constraint indicator flags | 48 |
| level IDC | 8 |
| reserved pattern, minimum spatial segmentation | 4, 12 |
| reserved pattern, parallelism type | 6, 2 |
| reserved pattern, chroma format | 6, 2 |
| reserved pattern, luma depth minus eight | 5, 3 |
| reserved pattern, chroma depth minus eight | 5, 3 |
| average frame rate | 16 |
| constant frame rate, temporal layers, temporal nesting, length size minus one | 2, 3, 1, 2 |
| array count | 8 |

Each array retains completeness, the reserved bit, the six-bit declared NAL type,
the big-endian 16-bit NAL count and every big-endian 16-bit length. Empty, unknown,
duplicate and interleaved arrays remain ordered source facts. NAL bytes and fixed
headers, exact spans, raw reserved patterns, type disagreements and the opaque
suffix remain accessible. Truncated fields fail; a zero/short/invalid fixed NAL
header does not acquire a decoder-valid certificate. RBSP and parameter-set bodies
remain opaque. Version and selected length-size refusals are explicit; width
three remains available through the pre-existing externally declared framing API.

`HvcAnnexBWriter` converts every source NAL in order, without filtering arrays or
injecting parameter sets. `HvcDecoderConfigWriter` is a distinct pinned receiver
policy: it checks every declared array, including empty arrays, before delegating
to that writer. Types 32, 33, 34, 39 and 40 are eligible. Other types produce a
typed refusal retaining the complete source configuration. This restriction is
not imposed on the generic parser or all-NAL writer.

## ASC, GASpecificConfig and PCE

`AacAscParser` preserves the exact physical bytes and the selected bit extent.
The default byte-record constructor uses the complete byte extent. The explicit
bit-length constructor uses the caller’s actual bound and preserves the physical
backing bytes. Neither constructor infers missing bits, padding or a clock.

The selected grammar retains the five-bit audio object type and its escape form,
indexed or explicit frequency and coded channel configuration. Selected outer
types are AAC-LC (2), explicit SBR (5) and explicit PS (29), with inner AAC-LC for
the latter two. Unsupported object types stay explicit. Frequency indices 0–12
and nonzero 24-bit explicit frequency are distinct source forms; indices 13/14
are invalid. Coded channels 1–7 map to 1, 2, 3, 4, 5, 6 and 8 channels; zero opens
PCE grammar. Other coded configurations remain unsupported.

GASpecificConfig retains `frameLengthFlag`, `dependsOnCoreCoder`, its optional
14-bit delay, `extensionFlag` and optional `extensionFlag3`. The selected core
sample count is 960 or 1024; this does not establish presentation timestamps.
Unselected extension modes remain unsupported.

PCE preserves its tag/profile/frequency fields, all six count fields, optional
mono/stereo/matrix mixdowns, ordered front/side/back/LFE/associated/coupling
elements, alignment bits, comments and exact source bit ranges. Wire limits and
caller limits bound descriptor allocation. Pair flags contribute two channels;
single elements and LFE contribute one; associated/coupling elements contribute
none to that channel count. Duplicate tags, empty configurations and profile or
frequency disagreements remain source facts or explicit issues. A parsed PCE is
not a decoder-ready certificate.

Only the immediate exact post-GA `0x2b7` marker opens the selected sync extension.
The selected extension object type 5 carries SBR presence and extension frequency;
an immediate exact `0x548` marker carries PS presence. There is no arbitrary suffix
scan. Unmatched suffixes remain opaque, while a recognized incomplete extension
fails. Signalling origin, present, absent and unsignalled remain distinct.

`AacAscScopeParser` parses the declared bit view and full physical byte view
independently, retaining each success/refusal and every backing byte. Equal
extents can alias one result. Coexisting PCE descriptors retain their actual
charges; an impossible intrinsic live set returns `Limit`, while external
retention pressure can return retryable `Backpressure`.

`aac_decoder_channel_view` and `aac_decoder_configuration_view` are explicit
consumer decisions over those source facts. The pinned correctness caller’s
indexed-core-frequency and channel-cap choices are supplied policy, not new
source restrictions. An unsignalled native PS/stereo choice never rewrites coded
mono or signalling. Native sample-rate/profile/frame-size defaults do not prove
the corresponding ASC source fields.

## ADTS records, streams and parser carriers

Fixed-header inspection retains all 56 bits, including MPEG version, layer,
protection, profile, frequency, channel configuration, flags, 13-bit frame length,
11-bit fullness and raw-block count. The selected complete-frame parser accepts
exact externally framed AAC-LC, layer-zero, indexed-frequency, single-block
records, with either no CRC or a literal 16-bit CRC. CRC verification, compressed
audio decoding and multi-block error-position grammar are not implemented.
Unsupported fixed fields remain inspectable. Physical shortage, trailing bytes
and header-only/empty-payload issues retain their distinct outcomes.

`AacAdtsWriter` uses explicitly supplied supported parameters and preserves every
compressed access-unit byte. It does not substitute a rendered frequency for the
coded core frequency or silently convert an explicit frequency into an index.
The complete output obeys the 8191-byte wire limit.

`AacAdtsStreamReader` reads complete aligned envelopes from one physical shared
`InputCursor`. Embedded signatures remain payload. Known size is a bound; empty
provision is starvation, and EOF requires explicit finish. Transport and
cancellation remain failures, including after previously retained output.
Reset creates the actual cursor generation and rejects stale callbacks.

`AacAdtsCarrierReader` shares that bounded cursor/growth engine and retains raw
headers, complete carrier bytes, physical offsets and selected-source errors.
Its explicitly named `for_parser_packets` policy can recognize a complete
seven-byte plausible length candidate after an opaque prefix. The prefix and
candidate remain separately ordered outputs. Nonzero layer bits retain the
invalid-source diagnostic and `ParserLengthCandidate` classification. Six bytes
never certify this boundary. Protected multi-block data stays opaque.
`inspection_end` is actual lookahead, not the emitted source end or an audio-frame
certificate. Failure/cancellation never flush pending bytes; explicitly finished
residual bytes stay observable. No media entities are joined or clocks inferred.

All builders grow from actual required bytes under checked caps. Work partitions,
retry credit, retained capacity, freeze ownership, sticky failures and late
cancellation remain explicit. A passing small-pool guard is resource evidence,
not lower-memory or numerical qualification.

## Guards and retained functional evidence

All 211 framing guards already present in the adopted1863 tree, including the
current DTS guards, remain unchanged. This source closure adds these complete
named test sets:

| Test target | Added guards |
| --- | ---: |
| `hvcc_wire` | 22 |
| `hvcc_annexb_whole` | 8 |
| `aac_asc_wire` | 16 |
| `aac_adts_wire` | 13 |
| `aac_adts_stream` | 21 |
| `framing_native_compat` | 26 |
| Total | 106 |

The 80 prerequisites and all 26 compatibility guards are included, together with
both literal fixture files. Their exact source bytes come from the frozen
3894→3901 lineage. The earlier 3901 source passed 317 framing guards within its
own 1827-test/profile graph. This adoption package has not run Cargo: against
the exact adopted1863 baseline, the expected union is 1969 passing tests plus its
one existing ignored test. Parent integration must gate the composed source.

The frozen source/native evidence retains all 1512 original source stdout files,
945 original native captures and 1629 total native observations, together with
every first failure. Separately named receiver views agree on the selected
14 hvcC and 156 ADTS contexts; the old default source comparisons remain intact.
Twenty caller-bit ASC contexts and 30 transport contexts remain unavailable to
the approved byte-only native interfaces. Unsupported AAC modes, ADTS multi-block
validation/CRC verification, HEVC body validation, complete module/native parity,
equivalent-work timing, lower memory and device qualification remain open.

Pinned BSD/MIT/MPL source and licence evidence, the ATSC A/153 Part 8 Table A.1
example provenance, the original contracts and immutable native artifact handles
are supplied separately with the adoption package. No upstream implementation,
reference C adapter, private caller or SDK is part of this library overlay.

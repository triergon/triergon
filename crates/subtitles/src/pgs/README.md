# Bounded PGS packet, object and display-set decoding

SegmentIter borrows packet or SUP framing, preserving raw source clocks and
unknown kinds. ObjectAssembly joins owned ODS fragments with one bounded copy
or a consuming zero-copy view. RasterJob expands complete RLE into an immutable
charged index plane under explicit work and memory limits. Missing last markers
and malformed rows remain explicit errors.

DisplayDecoder processes PCS/PDS/WDS/ODS/END across complete framed packets,
including interleaved object fragments, partial palette updates, cache reuse,
clear events and acquisition/epoch/hard resets. It emits retained display sets;
wall-clock scheduling belongs to the consumer. Unused incomplete objects may
span normal END boundaries; referenced objects must be complete, and explicit
input EOF checks every unfinished assembly.

Outputs preserve source segments, Y/Cr/Cb/alpha, versions, clocks, crop/window/
forced facts and full index planes. The visible view applies explicitly known
bounds; an absent window remains unknown. Native ARGB, placement and palette-
slot carry are named receiver projections and never supply missing source
facts. Unknown source duration remains unknown. Caller-held outputs retain
their charges after decoder reset or teardown.

The implementation is original safe Rust with no new runtime dependency.
Licensed evidence and native observations support development. Complete native
PGS parity, normative conformance, whole-Source numerical qualification and
consumer integration remain open. Preserved strict/recovery disagreements and
native API limits are required nonmatches rather than accepted exceptions.

`NativeCompatibilityDecoder` is a separate, explicit application entry point for
the pinned native first-event packet contract. It shares the bounded receiver
parser/cache/raster engine with full recovery, but constructs no strict Source
decoder and performs no Source ARGB export. `NativeCompatibilityPacket` owns the
original physical packet and clock, final context canvas, receiver observations
and at most the first native event. It has no Source sets or Source-error fields.
Original syntactic references on receiver rectangles are input facts, not a
strict Source-validation certificate.

Later segments in the same packet still update palette/object/presentation state
and final canvas, including updates after an empty first event. Later display
segments do not build another native event. These hidden updates remain available
to the next packet. Caller work/cancellation, finite limits, charged capacities,
pressure retries, sticky failures, reset and retained-output ownership apply to
this route too. Empty drain reports `NativeCompatibilityEof::NoDelayedSubtitle`;
it makes no claim about strict Source finalization.

The existing `NativeRecoveryDecoder`, `IndexedNativeRecoveryDecoder` and strict
default APIs retain their complete Source sets, first Source error and additional
receiver events. Native compatibility does not change those defaults or their
selected workload. Tests retain the complete frozen native transcripts alongside
product ownership/work/error guards; execution receipts name their exact source.

The exact approved first-event receiver operation has separate native Linux
ARM64 qualification. The b545 candidate passed delivery runtime, lower heap
and lower total peak RSS on all 229 memberships, including eight original
refusals. The runtime optimization target passed on 112 memberships. The
independent raw audit is
`.research/pgs-native-mem229-independent-review-v1/ACTUAL-TIME-MEM-REVIEW.json`.
This scope names the pinned native first-event operation, corpus and workers.
Whole Source/recovery qualification remains open. Root3477's result is
functional only.

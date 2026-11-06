# E-AC-3 BSI and native-boundary evidence batch

This opt-in O21/M02 slice extends the existing AC-3/E-AC-3 frame reader with
selected complete E-AC-3 BSI inspection. Its **11 Rust regressions pass** in the
parent's 62-test framing snapshot (private local receipt: `../../.research/dts-core-host-1793454330158626000/0.log`),
together with all-target Clippy with warnings denied and release-library build,
all return code 0. The recorded 61-file source map is unchanged before/after that
run. Its canonical implementation bodies and original fixtures remain preserved;
later core/export additions have separate source identities, and README.md names
the current checkpoint. The earlier 39/50-test snapshots
and the first constructor-control-baseline assertion failure remain preserved.
No Cargo, rustc, rustfmt, native probe, benchmark or fuzz job ran in this lane.
The Matroska qualification does not qualify these framing features.

## Primary grammar and source facts

Frame/BSI syntax is pinned to [ATSC A/52:2018](https://www.atsc.org/wp-content/uploads/2021/04/A52-2018.pdf),
Annex E Table E1.2, printed pp121–125; trailer fields follow Tables E1.5/E1.6,
printed p141. Substream and channel-map semantics follow E2.3.1.2/.6/.8,
printed pp142–145. [ETSI TS 102 366 V1.4.1](https://www.etsi.org/deliver/etsi_ts/102300_102399/102366/01.04.01_60/ts_102366v010401p.pdf)
E.1.2.2 and E.1.2.5/6 corroborate the normal-rate syntax. The ATSC reduced-rate
branch remains explicit; no fscod2 field is invented for the inspected ETSI
edition. Exact primary URLs and retained PDF hashes are in the lane manifest.

`Eac3BsiParser` takes a complete pool-owned `Ac3Frame`. Its existing frame reader
handles arbitrary input chunks, explicit EOF/transport identity and reserve/freeze
backpressure. The inspector starts after the already parsed prefix and preserves
the frame's source, generation and byte offset. It allocates no output storage:
raw frames, BSI groups and additional-data views retain the same charged backing.

Selected mixing syntax covers definitions 0/1/2, mode-dependent mix fields,
programme/external scales, pan information and one/six-block mixing configuration
branches. Exact original mixing bits remain available as a bit span. Flexible
mixing definition 3 is explicitly unsupported; its internal mixing/fill syntax
is not guessed or silently skipped. The earlier frame reader can still retain
the raw complete envelope without this opt-in inspection succeeding.

Informational metadata exposes raw service/copyright/original codes, optional
surround/headphone/surround-ex codes, packed production words and source-rate
presence. Converter synchronization and Type-2 block/size fields retain physical
presence separately from inferred values. For a six-block Type-2 frame, block ID
is inferred true and no physical block-ID bit is manufactured. Reduced-rate
frames do not acquire a physically absent sourcefscod field.

Additional BSI retains its original one-to-64-byte payload as a possibly unaligned
bit span. Group ranges include their presence flag; the additional-data range
excludes its flag/length. `covering_bytes` can include adjacent bits at either
edge, so callers keep the exact range. No payload is repacked, shortened or copied.

Raw auxiliary-presence, optional fourteen-bit auxiliary-length, encoder-info and
CRC2 fields remain visible. BSI parsing cannot cross those mandatory tail fields:
the minimum reserved tail is eighteen bits, or thirty-two when auxiliary-length
is physically present. This is a carriage bound, not proof of auxiliary payload
length agreement, full audio-frame/block syntax or CRC correctness.

Channel-map inspection counts the six paired locations with weight two and uses
ATSC's MSB-first location numbering. `channel_map_matches_coded_count` compares
that raw location count with the coded acmod/LFE count. None remains distinct
from a present zero map. False is a retained local inconsistency; true does not
certify stream grouping, channel topology or decoder readiness. The old prefix
`syntax_issues` values and all frozen prefix expectations remain unchanged.

## Bounds and lifecycle

Callers bound frame bytes and total BSI bits after the syncword. The inspector
checks the input pool and the E-AC-3 family, rejects reserved Type 3, and does not
guess BSID 9/10 or future layouts. The acquired ATSC E2.3.1.6 explicitly excludes
BSIDs 9/10 from its supported decoder set. Those BSIDs remain unsupported.

Variable work is charged by BSI bit. A field resumes correctly after one-bit
grants; the final publication costs one additional unit. Zero work consumes no
bits. Cancellation is checked between bounded transitions and each bit, drops
the retained in-progress frame and is sticky. Content/limit errors are sticky;
no partial BSI object is published. Fixed setup reads at most four already-owned
trailer bytes. Fixed metadata copies and allocator/Arc foundation operations are
not a wall-clock latency guarantee.

There is no new allocation/backpressure point inside the BSI inspector. The
caller or frame reader owns acquisition/storage pressure. An exhausted pool can
still inspect an already-owned frame and produce shared bit spans. Holding a
span retains the whole frame charge and source facts after reader/inspector drop,
including cross-thread final release. Offsets in bit ranges are frame-relative;
the original absolute frame offset remains on `frame().facts()`.

## Independent controls and preserved nonpasses

The original **240 prefixes and 62 native nonpasses are immutable**. This batch
adds separate evidence, never a normalizer or acceptance waiver. The v1 public
API comparison remains NONPASS: 166 two-fact matches, 55 fact-case API errors,
seven unexpected accepts and twelve observed rejections whose error classification
does not match the typed Rust distinction automatically.

The original error partition is 48 cases with substream ID 7 and seven with ID 0.
All nineteen E-AC-3 successes have ID 0. A new independent BSI audit finds complete
selected BSI syntax within the twelve supplied bytes of 43 error cases; twelve
use reserved Type 3 and remain unsupported. Seven of the 43 have custom-map
count mismatches. No missing-BSI or SID cause is inferred from these correlations.

The original generator and a separate whole-buffer bit-string oracle produce:

- **228** complete-carriage/BSI cases, 203 fact extractions and 25 negatives,
  containing 17216 authored bytes. Original E-AC-3 prefixes are also carried
  unchanged inside newly authored full declared envelopes. Opaque audio bodies
  and CRCs are deliberately not conformance media.
- **69** separately versioned diagnostic calls from 23 controls. The first 36
  isolate SID 0–7 or stream type 0–3: source-shape assertions prove that only the
  intended field bits differ. Remaining controls separate maps, compression and
  dual mono. Each is supplied as complete BSI, twelve-byte prefix and full frame.
  **No native outcome is predicted.**
- **Four** independent malformed BSI/tail-overlap source repros, preserving a
  complete selected prefix while later fields cannot fit before the mandatory tail.

The public fixture tables are `eac3-bsi-v1.tsv`, `eac3-diagnosis-v2.tsv` and
`eac3-bsi-boundary-v1.tsv`. Eleven new Rust tests cover all 301 rows with their
separate carriage contexts, tiny chunks/bit grants, zero work, partial fields,
source identity/nonzero frame offsets, exact presence, weighted maps, limits,
mandatory tail boundaries, unsupported syntax, allocation pressure and retained
views/thread release. All poll loops are bounded. These 11 tests pass in the
recorded 62-test host snapshot above; that result is not full native BSI parity.

`generate-v1.py`, `generate-diagnosis-v2.py` and `generate-boundary-v1.py` are
standard-library-only original scripts in `.research/parallel-framing/eac3-bsi-batch`.
They read no media implementation. The first two require only the public frozen
240-row fixture; neither depends on an old ignored run. Each corpus is frozen
with its own generator/hash/count manifest. Failed source repros remain intact.

## Parent observation request and open gates

`diagnosis-v2/manifest.json` preserves the request for 69 unpredicted public-API
calls. The parent subsequently executed all 69 in
`.research/ac3-header-sid-native-v2-observed/`: 36 API successes and 33 API errors.
The fixed SID/type/map controls and three carriage lengths remain separately
recorded; they do not waive any of the 62 original nonpasses.
`native-request-v1.json` separately describes 648 broader
BSI/prefix/frame requests, unchanged. Requests specify actual logical byte counts
and zero API padding; the old native wrapper's twelve-byte bound must not clip
the new experiment. Parent records raw API outputs/errors, reference/compiler/
source/input/binary identities and every nonpass. This lane runs no native tool.

The API exposes only bsid/frame size. Its results cannot establish optional BSI,
configuration, full framing or audio validity, nor independent/dependent grouping.
Full BSI semantics, flexible mix definition 3, audio/auxiliary blocks, CRC checks,
stream assembly, codec-state epochs, decoding, target integration and decoder
readiness remain open. AVC/hvcC G01 remains a separate primary-acquisition gap;
the subsequent DTS source/host/native evidence is a separate slice in `DTS-CORE.md`.

Full selected parity, native Linux ARM64 delivery runtime upper paired95 <=1.20 (optimization target <=0.80), lower live
heap and lower attributable RSS remain hard **unqualified/unmeasured** gates for
this batch. The recorded host checks and 69 diagnostic observations are complete;
full native BSI/equivalent-work parity and fresh numerical/32-bit/device proofs
remain open parent-queue work. The sole production
dependency remains owned core; no unsafe, third-party dependency, core/root/Cargo/
shared-harness edit, native implementation copy, device/account work or commit
is included. This checkpoint is not full O21.

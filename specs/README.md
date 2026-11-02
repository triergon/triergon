# Specification baseline and traceability

Implementation is specification-first. FFmpeg supplies an independent behavioral and
performance reference, not source code to translate. The initial parser was written
from EBML RFC 8794 and Matroska element/block/timestamp documentation. RFC 9559 is
the normative Matroska baseline; it updates RFC 8794. Current schema additions are
pinned separately and do not silently supersede an RFC rule.

`sources.json` records origin and SHA-256 for seven authoritative texts. Cached complete
texts live under ignored `.research/specs/`; their copyright notices remain intact.
No third-party implementation is vendored. Restore/verify with:

```sh
python3 tools/fetch-specs.py
```

A changing explanatory web page must still match its recorded hash; a mismatch needs
review, not an automatic update. Published RFC text is immutable, but errata and later
updates must be checked separately before qualification. This snapshot is not a claim
that the implementation meets every normative MUST.

| Feature | Primary requirement | Implementation / evidence |
|---|---|---|
| Element IDs/sizes, unknown lengths, integers | RFC 8794 §§4–8; RFC 9559 §4.2 updates ID rules | `ebml.rs`; primitive, truncation and mutation tests |
| Segment/Cluster boundaries, defaults and track IDs | RFC 9559 §§5–7; pinned element schema | `matroska.rs`; finite/unknown Cluster cases |
| Block structure and lacing | RFC 9559 §§10.1–10.3 | `lace`; three lacing fixtures and malformed-size tests |
| Random access / references | RFC 9559 §10.4 | Block flags, validated Cues lookup, bounded fallback seek, chained metadata SeekHeads and all-track preroll; all selected fresh/EOF packet comparisons pass; numerical qualification is source-specific |
| Timestamps, codec delay and padding | RFC 9559 §11 plus relevant schema elements | Exact rational nanoseconds checked against an independent oracle; explicit FFmpeg packet-clock adapter checked separately |
| Header-removal content encoding | RFC 9559 §5.1.4.1.31 | Frame/CodecPrivate scopes, per-lace restoration, order and decoded-size bounds; bzip2/lzo and encryption remain outside the selected support |
| Bounded zlib content encoding | RFC 1950 §§2.2–2.3; RFC 1951 §3.2 | Own stored/fixed/dynamic inflater, wrapper/checksum/window/resource/cancellation checks; independent Python/zlib oracle and upstream FATE case |
| Static colour/HDR | RFC 9559 §§5.1.4.1.28.16–40; pinned schema; RFC 8794 §7 scalar defaults/widths | All thirteen integer and ten float source fields, raw extensions, exact IEEE oracle and separate native rational/side-data view; K4 qualified on native Mac ARM64 |
| Segment metadata | RFC 9559 §§5.1.2, 5.1.4.1, 5.1.6–8; RFC 8794 scalar rules | Bounded exact Info/edition/chapter/tag/attachment source trees and track flags/languages; separate native catalogue proof; K5 and mandatory intact HDR input qualified on native Mac ARM64 |
| Codec identities/private configuration | Matroska codec mappings plus RFC 9559 TrackEntry | Opaque packet/private-data preservation, not codec validation or decoding |
| Untrusted-input/resource policy | Own `Limits` and frozen slice manifest | Bounded blocks/metadata/elements/scans; typed incremental jobs, shared retained-capacity accounting and resumable inflater growth implemented; K0–K5 selected operations qualified on the recorded earlier shared-core graph; the later typed adapter passes selected parity, with broader work-bound coverage and numerical/module qualification pending |
| FFmpeg feature compatibility | Pinned reference build, selected FATE mappings | Separate from normative conformance; a disagreement is investigated, never silently normalized |

The initial scope is **container demuxing**. H.264/HEVC/AAC/AC3 codec standards are
required before their later packet-adapter/decoder work; carrying compressed packets
does not claim implementation of those standards. Primary QuickTime/ETSI ISO-media,
transport and DVB subtitle sources now support the selected M03–M05 prototypes.
Their lane records distinguish acquired sources from remaining normative gaps;
the complete modern ISO media and broader bitmap-subtitle scope is still open.
The earlier scope table describes the roadmap, not an acquired or implemented
universal standards pack.

Do not imitate reference bugs to make a test green. Conversely, an apparent spec
advantage does not waive the owner's FFmpeg parity gate: retain both outputs and a
bounded explanation; the lane remains unaccepted pending a reviewed disposition. The owner-approved
empty-PCM view and native combined-encoding reference are recorded in
[the comparison decision](../research/decisions/2026-10-31-zlib-comparisons.md);
source facts and numerical gates remain mandatory.

The 2026-10-31 [primary errata review](../research/qualification/segment-metadata-mac-arm64/specification-errata-review.json) records all eleven current EBML and two Matroska reports, retaining their actual statuses. Selected ID/date/Cues behavior is consistent; reported proposals are not presented as verified normative changes. Complete generic EBML/Matroska conformance remains open.

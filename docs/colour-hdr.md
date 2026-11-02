# Bounded Matroska colour and static HDR

K4 implements all thirteen Colour integer elements and all ten MasteringMetadata
floats from RFC 9559 §§5.1.4.1.28.16–40 and the pinned element schema. The [current qualification receipt](../research/qualification/shared-core-mac-arm64/README.md)
passes the unchanged gates on native Mac ARM64, including K0–K5 requalification. The frozen [contract](../harness/colour-slice.json) contains 33 required
profiles, including SDR, HDR10/PQ, HLG, P3, 32-/64-bit mastering values, empty and
partial metadata, source defaults, nine siting combinations, future numeric codes,
zero/boundary/tiny values, and 64 tracks. No required case is excluded to pass.

`Track::colour` exposes complete source facts, including bit depth, both chroma/Cb
subsampling axes, optional light levels and individual optional mastering fields.
All values remain independent of a renderer or hardware HDR policy. Floating source
values are preserved exactly as f64 (lossless widening for binary32). Raw TrackEntry
bytes remain available, preserving encoding widths, unknown extensions and presence.
Zero-byte numeric elements obey their schema default or zero without a default.

The parser uses fixed-size typed storage within the existing bounded Tracks input.
It adds no decompression, recursive traversal or dynamically growing HDR lists.
Duplicate Colour masters/recognized fields, invalid widths, truncated children,
non-finite values, negative luminance and chromaticity outside [0,1] fail. Unknown
numeric registry codes remain numeric; unknown extensions remain in the bounded raw
TrackEntry. Metadata/track/scan limits still apply. The current reader charges
retained track/packet/catalogue storage to the shared pool. The recorded K0–K5
operations pass fresh numerical qualification with that shared-pool source.
Complete incremental integration, growth work bounds, caller-independent
cancellation during blocking open and qualification of later changes remain open.

The explicit optional `Colour::ffmpeg9()` view reproduces the pinned public FFmpeg
packet-metadata representation. A separate Python EBML oracle checks every complete
source field and raw TrackEntry; the native adapter checks all 32 public colour,
light-level and mastering-rational fields. Both are mandatory. No epsilon or
rounded-JSON comparison is used. The source floats are not replaced by rational
approximations: continued fractions compute the bounded rational view separately.
An explicit exact comparison handles the native rounded half-minimum boundary;
its failing raw output and added required regression remain recorded.

Native observations establish that the pinned no-parser demux operation exposes no
BitsPerChannel through bits_per_raw_sample, omits incomplete light levels, and
omits incomplete/zero primary sets. Its mastering rationals have signed 32-bit
numerators and denominators; the optional view returns an explicit overflow error
where a source value cannot fit that interface. Source parsing still preserves all
finite in-schema float values and full unsigned integer fields. This is a
representation/capability boundary, not permission to erase the source metadata.

The metadata benchmark opens, demuxes and tears down both implementations, consumes
all packet/configuration/timing fields and every canonical colour/HDR field in the
same ordered checksum, and includes rational conversion inside its operation.
No observer runs in speed workers. K4 requires all selected parity, 30 paired
observations, time upper paired 95% ratio <=0.80 and live-heap/attributable RSS
upper ratios <1.0 on native Mac ARM64. Original slices require requalification.

This is static container metadata, not HDR10+ or Dolby Vision bitstream metadata,
decoding, display output or a TV claim. BlockAdditions/codec-side-data carriage,
projection, full M01/M02, other targets and Streem integration remain separate open
work. [K5](segment-metadata.md) now supports bounded chapters, tags and attachments.
The intact upstream `hdr10tags-both.mkv` passes source/native catalogue comparisons
and the same current-source runtime/memory gates as a mandatory K5 input. Its earlier refusal remains
in the sealed K4 receipt. Its full FATE frame-side-data priority operation was not
executed. No content was stripped to turn that input into a pass.

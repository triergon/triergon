# Bounded zlib and Cues/preroll development lane

Both features are implemented in dependency-free safe Rust. Their frozen K2/K3
slice gates pass on native Mac ARM64 for the archived source. The contracts retain
every declared profile; broader module and shipping-target gates remain closed.
The required native Mac thresholds remain a runtime upper paired 95% ratio at most
0.80 and both live-allocation and conservative attributable RSS upper ratios below
1.0, with at least 30 raw pairs per case.

## Compression contract

`zlib::decompress` reads one complete RFC 1950 stream with RFC 1951 stored, fixed
and dynamic blocks. It checks CMF/FLG, code trees, length/distance bounds, stored
length complements and Adler-32. No decoded buffer escapes before checksum success.
Preset dictionaries are explicitly unsupported because no dictionary is supplied by
this selected Matroska carriage. Trailing bytes are rejected in this complete-frame
API; no concatenated-stream interface is claimed.

Output, expansion and work limits are independent. Defaults are 16 MiB per decoded
frame, 1 MiB per decoded CodecPrivate, expansion at most 1024 times that encoding's
input bytes, and 128 Mi work units per encoding. Work units charge blocks, symbols
and decoded output; bounded Huffman-table construction adds fixed work per block.
Decoded private capacities also consume the shared metadata budget across tracks,
so many compressed tracks cannot multiply that budget. Decoded capacities of all laced frames also share one 16 MiB block budget; lacing
cannot multiply the output ceiling. There are at most eight encoding stages. Cancellation is checked between blocks,
between compressed symbols and between checksum chunks. Blocking caller I/O and a
single bounded copy/allocation cannot be asynchronously interrupted.

ContentEncoding orders decode highest first. Frame and CodecPrivate scopes (1/2/3)
are independent; each laced frame is decompressed separately. Mixed header/zlib
chains are decoded according to the source format. A header-only frame retains the
existing packet-buffer view fast path. A single final stored DEFLATE block can also
return a validated owned packet-buffer view, avoiding an extra allocation/copy.
Adler-32 uses independent bounded lanes and equivalent weighted sums that permit compiler vectorization;
compressed-output reservation uses a bounded input-size hint. A checked 64-bit
reservoir amortizes bit input reads across symbols. bzip2/lzo, encryption, external dictionaries
and scope 4 remain explicit unsupported cases. Those fast-path measurements name
the archived decoder. The current charged blocking route uses a persistent inflater
for non-stored streams and exact validated capacity for a single stored stream.
Track, packet, catalogue and inflater output storage now carry shared pool charges.
Its incremental integration, complete growth work bounds and numerical
requalification remain open.

The independent Python/zlib oracle covers valid blocks, window sizes, overlap and
cross-block history; malformed/checksum/truncation, resource and mutation tests
remain separate from successful performance cases. An additional independent EBML
content oracle checks empty outputs, ordered chains and CodecPrivate restoration.
The actual upstream `fate-matroska-zlib-decompression` target and its packet/source
comparisons pass for the externally cached VobSub vector. Redistribution rights
remain unreviewed; no external media or external raw payload is vendored.

## Seek contract

Cues are bounded by raw metadata bytes and an explicit 16,384-entry default limit.
The parser accepts unsorted storage, sorts by track/time and uses binary lookup.
SeekHeads can locate Cues at the Segment tail through bounded chains. Normal open
and packet reading do not scan the file to manufacture an index.

`seek_previous` retains its previous-video-RAP behavior. A validated cue narrows the
scan to the relevant range; an absent index uses the bounded fallback scan. The gap after the last stored cue
is scanned for later uncued RAPs, including requests beyond the end. A cue
is checked against the real Cluster, track, timestamp and keyframe, including any
CueRelativePosition or CueBlockNumber. A forged index never becomes a fabricated
successful landing. CueCodecState changes remain explicitly unsupported.

`seek_for_playback` subtracts the largest declared track SeekPreRoll from the
requested source presentation time, seeks to a preceding video RAP and replays the
whole landing Cluster. This preserves interleaved audio preceding the video block.
At the beginning, preroll is limited by the first available RAP. CodecDelay stays in
the source timestamps. The receipt identifies actual RAP time, index use, scan work,
required preroll, `replays_from_start` and `discard_until_ns`: decode the returned packets, but suppress
presentation until that time. It does not claim to decode or schedule A/V itself.

The fallback is bounded, not a full-file buffer. Hard failures and cancellation
require an explicit new seek/reset before more packets can be consumed; pending
laced packets cannot leak across a failed seek. Repeated seeks, after-EOF seeks,
small scan budgets, index bounds, forged positions, failed-index retries and extreme timestamp arithmetic
have regression coverage. Push-input yielding and audio-only seeks remain outside
this selected API.

The native reference worker uses public FFmpeg APIs: source/configuration is pinned,
all-track preroll comes from AVCodecParameters in samples converted to nanoseconds,
and `av_seek_frame` requests backward video seeking. A fresh seek first reads and
unrefs one packet to establish the pinned demuxer's packet-reader state; after-EOF
seeks already have that state. Priming remains inside timing and memory windows. Operation timing includes
open, metadata/index work, fresh seek and all subsequent packets/teardown. Exact
fresh and after-EOF output comparisons cover six requests per fixture. The same
ordered byte/metadata/timestamp checksum is consumed in both timing workers.

## Reviewed source and decoder-packet comparisons

All ten required K2 profiles pass their selected comparisons, with complete source
frame bytes/CodecPrivate checked against an independent Python EBML/zlib oracle
and exact source time checked separately. All eight K3 profiles match the native
reference across six fresh and after-EOF requests. All 18 cases pass the unchanged
runtime and both memory gates; K0/K1 also pass requalification on the same source.

The owner approved [two explicit rules](../research/decisions/2026-10-31-zlib-comparisons.md):

- `zlib-empty` retains its empty source event and timing. An optional decoder-input
  view matches FFmpeg's zero delivered sample packets for this empty PCM case.
  The source API/default trace remain complete. Decompression and checksum work
  happen before the projection inside operation timing.
- `zlib-mixed` uses **pinned FFmpeg plus native combined-encoding restoration** for
  the selected scope-1 chain. Stock FFmpeg returns compressed bytes and reports
  combined encodings unsupported. The supplement uses the same system zlib and
  includes all restoration work. Its speed result is against that augmented
  pipeline. It does not establish a stock-FFmpeg speed ratio for the missing feature.

The rules bind exact input hashes and the pinned reference revision. Raw native
output and its expected difference are retained; an unexpected difference fails.
Source content/time proof and the selected reference's actual hash are mandatory.
Other cases keep their native reference. No required case or numerical threshold
was removed. The native supplement supports combined scope 1 only; combined private
scopes explicitly fail in the reference and remain outside its qualified capability.

The unknown-Cluster discrepancy was resolved by initializing the native packet
reader before seeking. CodecDelay recovery now uses the native index plus an
ANY-frame seek to include audio preceding the video RAP. At a negative beginning,
the reference reopens at the start to preserve those first frames. All this work
stays inside timing and memory windows. The Rust receipt's `replays_from_start`
comes from source position, so one-time initial sample trimming is not repeated on
later seeks or guessed from a zero timestamp. No recovery packet is dropped to
make the seek comparison pass.

## Archived evidence and current qualification

The first [zlib/Cues receipt](../research/qualification/zlib-cues-mac-arm64/README.md)
retains its failures unchanged. The [checksum iteration](../research/qualification/zlib-cues-iteration-2-mac-arm64/README.md)
passes numerical gates for all 31 matching cases on that source. The
[seek recovery receipt](../research/qualification/seek-recovery-pre-adoption/README.md)
then qualifies all eight K3 cases before the two zlib rules were adopted. None of
these earlier receipts qualifies a changed tree.

The [final approved-comparison receipt](../research/qualification/zlib-cues-approved-mac-arm64/README.md)
qualifies all 34 declared cases on the archived source, each with 30 paired timing
and memory observations. K2's worst upper time/RSS/live-heap ratios are
0.7902 / 0.4118 / 0.6301; K3's are 0.6889 / 0.2500 / 0.1291 (rounded upward).
Every required time upper ratio is <=0.80 and both memory upper ratios are <1.0.
Complete M01/M02, deferred targets and Streem/TV integration remain unqualified.

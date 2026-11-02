# Zlib/Cues comparison investigation history

This records the original source-semantics investigations. Subsequent owner
[approval](../decisions/2026-10-31-zlib-comparisons.md) resolves the two compression
comparisons through explicit rules; the seek discrepancy is resolved by native API
handling and one-time trim semantics. Every required profile and the original
0.80 time / lower-memory limits remain. Current qualification is recorded separately;
the original failed receipts are unchanged.

## Unknown-length Clusters: reference initialization corrected

A fresh FFmpeg open/seek landed one Cluster later than seeking after EOF. Four
public-API experiments compared six targets, fresh and after EOF, on all eight
unchanged fixtures. Reading one packet before the first seek removed all six
unknown-Cluster disagreements. A full `avformat_find_stream_info` scan also removed
them, but is unnecessary for this selected container operation. Changing the seek
API or default stream alone did not fix them.

The reference now primes one packet for a fresh seek, checks its error, discards it
and seeks normally. This work stays inside operation timing and memory accounting.
It does not change source bytes, the requested time, all-track preroll or the
returned packet comparison. A regression first failed on the old reference and
then passed with the correction. This is an empirical reference-protocol fix,
not a claim that the public FFmpeg API universally requires a priming read.

## Empty compressed frame: source event versus decoder packet

The authored `zlib-empty` input contains a valid RFC 1950 stream with zero decoded
bytes. The independent Python zlib oracle agrees with our frame bytes and source
timestamp. The source API retains that event; FFmpeg's packet API emits no packet.
That output distinction is now handled by the approved optional decoder-input
view, while complete source events and timing stay independently verified. A blanket empty-packet filter would also affect codecs with meaningful
empty events and is not an acceptable universal normalization.

## Mixed compression: ordered stages remain required

RFC 9559 section 5.1.4.1.31.2 requires descending ContentEncodingOrder, with unique
orders. The authored fixture uses order 1 zlib and order 0 header restoration.
Independent Python EBML traversal plus zlib decodes the same complete bytes as our
reader. The pinned FFmpeg packet payload instead retains compressed data.

The required chain is retained. Replacing its expected decoded bytes with those
compressed bytes, removing the profile, or comparing it without equivalent
restoration work would not establish full selected-feature parity. An explicitly
identified supplementary reference may be necessary if the pinned native operation
cannot provide this feature; the owner approved that bounded augmented reference in the linked decision.

## Codec delay and preroll: extra audio remains visible

RFC 9559 sections 5.1.4.1.25 and 5.1.4.1.26 separately define subtracting CodecDelay
from source timestamps and the decode interval required after a discontinuity.
Our API replays the whole landing Cluster, retaining an audio frame preceding the
video RAP after its timestamp adjustment. The pinned reference starts at the video
packet instead. For the 7.5 s fixture request these first audio timestamps are 6.95 s
versus 7.2 s in FFmpeg. No frame is silently dropped to hide this distinction.

The reference now uses its native index and an ANY-frame seek to recover the
preceding audio, reopening at a negative beginning where needed. The Rust receipt
identifies replay of the first Cluster from source position; initial trim is not
repeated on later seeks. All eight fixtures match across six fresh/EOF requests.
This selected packet proof does not establish decoded A/V recovery on a device.

Primary specification: [RFC 9559](https://www.rfc-editor.org/rfc/rfc9559.html).
Public reference interface: [FFmpeg 9.0 demuxing API](https://www.ffmpeg.org/doxygen/9.0/group__lavf__decoding.html).

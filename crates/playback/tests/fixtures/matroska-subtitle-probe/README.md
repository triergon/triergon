# Authored subtitle clock controls

Each Matroska file contains the same five authored PGS segments as
`../bitmap/original-pgs-two-pixel.packet`, one segment per block. The video
track contains one 16×16 black VP9 key frame as a stream-discovery control.
The files differ in signed block timestamps and optional BlockDuration.

Expected packet clocks in `continuation.rs` come from FFprobe 9.0.2 with
default stream probing. The controls include unknown timestamps, a first
positive duration, a later known anchor and timing that arrives after the
anchor. The raw Matroska clocks remain available in the typed packet.

Four PGS-only discovery controls vary the first packet that exposes a
valid canvas. One contains an incomplete canvas before the valid one,
one combines two segments in one packet, and one reaches a 32-byte
probe threshold before the canvas. Their expected cutoffs come from
the buffered frame counts in the same FFprobe's discovery trace.

# DVD subpicture packet and indexed RLE primitives

`DvdPacket` borrows exactly one assembled SPU, preserving raw control dates,
packed palette/alpha words, forced/start/stop commands, geometry and field
offsets. Control links and command counts are bounded. Unknown commands retain
the uninterpreted remainder instead of guessing operand lengths.

`DvdRasterJob` expands independently positioned interlaced fields directly into
one charged row-major index plane. Runs yield at a work quantum; retained output
keeps its complete backing charge. Cancellation releases private storage and
allocation pressure can be retried without repeating paid work.

ConfigJob preserves the original VobSub CodecPrivate bytes and cooperatively
reads selected canvas, palette and custom-colour signalling. The explicit
LSB-indexed RGBA view retains transparent RGB; it guesses no missing palette.
Unrelated settings and CodecPrivate timestamps are never applied.

SpuAssembly accepts arbitrary owned byte fragments, including split size words.
A complete input uses a zero-copy view; fragmented input is copied once into one
pre-reserved charged buffer. It returns trailing bytes, separates starvation from
explicit EOF, and preserves paid work through allocation/freeze retries. Reset
releases private state while previously returned packets remain valid. PES and
external packet clocks stay with the consumer.

`ControlProgram::display_controls` yields source show, forced-show and clear
commands in wire order with exact 90 kHz dates. Each command consumes work.
`TraditionalSubpicture::native_presentation` borrows the native receiver
bitmap and combines its millisecond offsets with a caller-provided PTS.
Missing PTS leaves absolute times unknown. An explicit stop at zero
retains a clear boundary. [FFmpeg DVD receiver](https://github.com/FFmpeg/FFmpeg/blob/master/libavcodec/dvdsubdec.c)

Colour-change commands, wider fragment/display behavior, and scheduling
across packets remain open. Strict malformed-row refusals need explicit
native comparison dispositions before complete decoder qualification.
This original safe-Rust implementation uses licensed open evidence and native
controls; it makes no paid-standard conformance or complete parity claim.

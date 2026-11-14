# triergon-elementary

Readers for audio elementary streams: native FLAC, MPEG audio (Layer I, II and III,
with ID3 and APE tags and Xing/Info/VBRI frames), AAC in ADTS, and AC-3/E-AC-3. Each
is a `triergon_core::container::ContainerReader` with one track and does no I/O;
`BlockingReader` drives one from `std::io::Read + Seek`.

```rust
use std::io::Cursor;
use triergon_core::container::{PacketInfo, TrackInfo};
use triergon_elementary::{BlockingReader, Limits, MpegAudio};

// Two MPEG-1 Layer III frames, 128 kbit/s, 44.1 kHz.
let mut frame = vec![0xff, 0xfb, 0x90, 0x64];
frame.resize(417, 0);
let file = [frame.clone(), frame].concat();

let mut reader = BlockingReader::<MpegAudio, _>::open(Cursor::new(file), Limits::default())?;
let first = reader.next_packet()?.expect("a frame");
assert_eq!((first.pts(), first.duration()), (Some(0), Some(1152)));
let track = reader.reader().stream().expect("the track");
assert_eq!((track.codec_tag(), track.sample_rate()), (b"mp3".as_slice(), 44100));
# Ok::<(), triergon_core::Error>(())
```

## Behaviour

All four formats run one frame-stream engine: leading ID3v2 tags, the format's
opening, a sync confirmed by the frames after it, then access units until a trailing
ID3v1 or APEv2 tag or the end. Packet times are samples (`1/sample_rate`); every frame
is a random-access point. Tags are kept as bounded opaque bytes (`Reader::tags`).

| Format | Frame end | Bytes between frames | Configuration | Codec tag |
|---|---|---|---|---|
| FLAC | next header with a valid CRC-8 that the frame's CRC-16 confirms, or that numbers on from a damaged frame | part of the frame before | `fLaC` + STREAMINFO + VORBIS_COMMENT (FFmpeg keeps STREAMINFO and honours the comment's `WAVEFORMATEXTENSIBLE_CHANNEL_MASK`) | `fLaC` |
| MPEG audio | header length; free format runs to the next header | prepended to the next packet | none | `mp1`, `mp2`, `mp3` |
| ADTS | `frame_length` | dropped | none (packets keep their ADTS header) | `adts` |
| AC-3, E-AC-3 | `frmsiz`; an access unit adds the dependent and further substreams after substream 0 | appended to the packet before | none | `ac-3`, `ec-3` |

A frame cut short by the end of the input is delivered with `Packet::is_complete`
false, and so are MPEG audio bytes after the last frame, as FFmpeg 9.0.2 delivers them.

FLAC metadata blocks are kept up to the retention limit (PADDING by length only), and
SEEKTABLE and STREAMINFO are read; a stream whose first block is not STREAMINFO is
refused, as FFmpeg refuses it. A stream without the `fLaC` marker (joined mid-way)
starts at the first frame header that a CRC-16-confirmed frame follows, after any bytes
before it; its track comes from that header and its configuration is empty.
Fixed-blocking frame numbers count the first frame's block size, or each frame's own
when STREAMINFO declares a range of sizes, as FFmpeg counts them.

MPEG audio follows FFmpeg 9.0.2 where the specification leaves room: a first Layer III
frame with a Xing, Info or VBRI description is not delivered; a LAME tag written by
LAME or FFmpeg trims `delay + 529` samples from the first packet and `padding - 529`
from frame `frames - 1`, unless the source holds more than a sixteenth more bytes than
the Info frame states; the reserved emphasis value is accepted.

Seeking reports a receipt and, through `Reader::last_seek`, whether the landing is
exact. FLAC bisects by frame header timestamps within the SEEKTABLE interval (or the
whole file) and reads forward to the frame holding the target. MPEG audio estimates
from the Xing table of contents or the bit rate (`SeekBasis::Toc`, `SeekBasis::Bitrate`)
and counts from the start when the estimate falls past the end. ADTS and AC-3 count
frames from the start (`SeekMethod::Restart`). A playback seek lands
`Format::PREROLL` frames early (two for MPEG audio, one for ADTS and AC-3, none for
FLAC).

## Limits

`Limits` bounds the largest access unit (4 MiB: a verbatim FLAC frame of 65535 samples
of eight 32-bit channels is about 2.2 MB), retained tags and metadata (4 MiB in all),
sync search (1 MiB), input window (64 KiB), SEEKTABLE points (65536) and bisection
jumps (64). Every input-dependent allocation is charged to the reader's `Budget`;
exceeding a limit is a typed error.

## Comparison with FFmpeg

`cargo xtask codecs readers` compares every packet's size, position, timestamps and trim as
the readers deliver them with the pinned `ffprobe -show_packets` of FFmpeg 9.0.2, under the
reviewed rules and dispositions of
[harness/codecs/reader-rules.json](../../harness/codecs/reader-rules.json); gates:
[docs/quality.md](../../docs/quality.md#gates). Over the IETF FLAC decoder testbench, the
MPEG audio conformance bitstreams, streams FFmpeg writes (FLAC, MP3 with and without
Xing/Info and LAME tags, MP2, ADTS, AC-3, E-AC-3) and crafted damaged ones, 253/271 pass
and 2 are refused by both (testbench faulty 06 and 07); the frozen differences of the
others are in [reader-differences.json](../../harness/codecs/reader-differences.json), and
each lies in the scope of one disposition. They differ only where FFmpeg:

- delivers a FLAC frame's duration rescaled to the first frame's rate when the rate
  changes, no duration at 768 kHz, and the last frame of a stream with varying block
  sizes under the fixed flag at the previous end;
- merges a damaged FLAC frame with the three after it into one packet without times;
- delivers the bytes before the first FLAC or AC-3 frame as a packet, and ID3v1 at the
  end of FLAC and AC-3 as part of the last packet (this crate keeps them as tags);
- accepts an MPEG audio frame of another version or sample rate, or a header in junk
  that no frame follows;
- reports ADTS positions after junk as if the junk were not there;
- rejects free-format MPEG audio;
- refuses to open a FLAC stream whose STREAMINFO declares a maximum block size below 16
  (testbench faulty 08 and 09): a decoder decision, so this reader still delivers the
  frames.
- declares the stream STREAMINFO describes when a metadata block runs past the end of the
  file (testbench faulty 11), where this reader reports the stream as truncated.
